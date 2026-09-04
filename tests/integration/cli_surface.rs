//! Human output, error paths, and the standalone subcommands.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;

use bastion::store;
use bastion::verdict::Decision;

/// Without an explicit base, each branch in a native GitHub PR stack discovers
/// its own direct base through `gh`. Reviewing C sees only C, reviewing B sees
/// only B, and reviewing A sees only A.
#[test]
fn stacked_pull_requests_are_reviewed_as_independent_changesets() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("stack", "codex", "gate")
        .behavior("pass")
        .trigger("stack/**")]));
    repo.commit_all("fixture base");

    repo.checkout_new_branch("stack-a");
    std::fs::create_dir(repo.path().join("stack")).unwrap();
    std::fs::write(repo.path().join("stack/a.txt"), "A\n").unwrap();
    repo.commit_all("A");

    repo.checkout_new_branch("stack-b");
    std::fs::write(repo.path().join("stack/b.txt"), "B\n").unwrap();
    repo.commit_all("B");

    repo.checkout_new_branch("stack-c");
    std::fs::write(repo.path().join("stack/c.txt"), "C\n").unwrap();
    repo.commit_all("C");

    for (branch, base) in [
        ("stack-c", "stack-b"),
        ("stack-b", "stack-a"),
        ("stack-a", "main"),
    ] {
        repo.checkout(branch);
        if branch == "stack-c" {
            // Local review keeps uncommitted work in the selected layer.
            std::fs::write(repo.path().join("stack/c.txt"), "C\nlocal edit\n").unwrap();
        }
        let pull = serde_json::json!({
            "number": 1,
            "title": "stack layer",
            "body": null,
            "author": { "login": "ada" },
            // The remote PR head may lag local, unpushed work. It supplies
            // attestation identity but does not constrain local review.
            "headRefOid": "remote-head",
            "baseRefName": base,
            // The published parent may lag its local branch. Automatic
            // selection still excludes local parent commits already in this child.
            "baseRefOid": repo.revision("main"),
        })
        .to_string();
        let run = repo.review_auto(
            fake,
            &[
                ("BASTION_GH_BIN", fake.to_str().unwrap()),
                ("FAKE_GH_PR_JSON", &pull),
            ],
        );

        assert!(run.exited_zero(), "{branch} stderr:\n{}", run.stderr);
        assert_eq!(run.resolved("stack").0, Decision::Pass);
        assert_eq!(
            run.started_changeset(),
            (1, repo.revision(base).as_str()),
            "{branch} must contain only its own layer"
        );
        if branch == "stack-c" {
            std::fs::write(repo.path().join("stack/c.txt"), "C\n").unwrap();
        }
    }

    // An explicit base wins over automatic PR discovery.
    repo.checkout("stack-b");
    let widened = repo.review_base(fake, "main", &[]);
    assert!(widened.exited_zero(), "stderr:\n{}", widened.stderr);
    assert_eq!(widened.started_changeset(), (2, "main"));

    // A branch without a PR keeps the longstanding `main` default.
    let no_pr = repo.review_auto(
        fake,
        &[
            ("BASTION_GH_BIN", fake.to_str().unwrap()),
            ("FAKE_GH_NO_PR", "1"),
        ],
    );
    assert!(no_pr.exited_zero(), "stderr:\n{}", no_pr.stderr);
    assert_eq!(no_pr.started_changeset(), (2, "main"));

    // An unexpected detection error warns, then behaves like no detected PR.
    let failed_detection = repo.review_auto(
        fake,
        &[
            ("BASTION_GH_BIN", fake.to_str().unwrap()),
            ("FAKE_GH_FAILURE", "1"),
        ],
    );
    assert!(
        failed_detection.exited_zero(),
        "stderr:\n{}",
        failed_detection.stderr
    );
    assert_eq!(failed_detection.started_changeset(), (2, "main"));
    assert!(failed_detection.stderr.contains("using `main`"));

    // A missing `gh` is silent: same fallback as no detected PR, no warning.
    let missing_bin = fake.with_file_name("no-such-gh-binary");
    let missing_gh = repo.review_auto(fake, &[("BASTION_GH_BIN", missing_bin.to_str().unwrap())]);
    assert!(missing_gh.exited_zero(), "stderr:\n{}", missing_gh.stderr);
    assert_eq!(missing_gh.started_changeset(), (2, "main"));
    assert!(
        !missing_gh
            .stderr
            .contains("could not detect a pull request"),
        "a missing gh must not warn, stderr:\n{}",
        missing_gh.stderr
    );

    // Explicit PR selection has the same fallback when neither GitHub source works.
    let output = repo.run(
        fake,
        &[
            "review", "--repo", "acme/app", "--pr", "7", "--format", "jsonl",
        ],
        &[
            ("BASTION_GH_BIN", fake.to_str().unwrap()),
            ("FAKE_GH_FAILURE", "1"),
            ("GITHUB_TOKEN", ""),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let explicit_failed = ReviewRun {
        code: output.status.code(),
        events: parse_events(&String::from_utf8_lossy(&output.stdout), &stderr),
        stderr,
    };
    assert!(explicit_failed.exited_zero());
    assert_eq!(explicit_failed.started_changeset(), (2, "main"));
    assert!(explicit_failed.stderr.contains("without GitHub context"));
}

/// A locally detected PR feeds its discussion into the reviewer prompt, matching
/// the CI `--repo`/`--pr` path.
#[test]
fn local_detection_gathers_pull_request_discussion() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("ctx", "codex", "gate")
        .behavior("pass")
        .env("FAKE_EXPECT_PROMPT_CONTAINS", "grace already approved this")]));
    repo.checkout_new_branch("feature");

    let pull = serde_json::json!({
        "number": 7,
        "title": "stack layer",
        "body": "Deliberate schema change.",
        "author": { "login": "ada" },
        "headRefOid": "remote-head",
        "baseRefName": "main",
        "baseRefOid": repo.revision("main"),
    })
    .to_string();
    let comments = serde_json::json!([
        {
            "id": 1,
            "body": "grace already approved this",
            "user": { "login": "grace" },
            "author_association": "OWNER"
        }
    ])
    .to_string();

    let run = repo.review_auto(
        fake,
        &[
            ("BASTION_GH_BIN", fake.to_str().unwrap()),
            ("FAKE_GH_PR_JSON", &pull),
            ("FAKE_GH_ISSUE_COMMENTS_JSON", &comments),
        ],
    );
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ctx").0, Decision::Pass);
}

/// A discussion fetch that fails after a successful PR lookup keeps the detected
/// base and intent, warns, and continues without comments.
#[test]
fn local_discussion_failure_warns_and_keeps_the_detected_pr() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("ctx", "codex", "gate")
        .behavior("pass")
        .env("FAKE_EXPECT_PROMPT_CONTAINS", "Deliberate schema change.")]));
    repo.checkout_new_branch("feature");

    let pull = serde_json::json!({
        "number": 7,
        "title": "stack layer",
        "body": "Deliberate schema change.",
        "author": { "login": "ada" },
        "headRefOid": "remote-head",
        "baseRefName": "main",
        "baseRefOid": repo.revision("main"),
    })
    .to_string();

    let run = repo.review_auto(
        fake,
        &[
            ("BASTION_GH_BIN", fake.to_str().unwrap()),
            ("FAKE_GH_PR_JSON", &pull),
            ("FAKE_GH_API_FAILURE", "1"),
        ],
    );
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ctx").0, Decision::Pass);
    let base = repo.revision("main");
    assert_eq!(run.started_changeset().1, base.as_str());
    assert!(
        run.stderr.contains("without GitHub discussion"),
        "stderr:\n{}",
        run.stderr
    );
}

/// The default (human) output format renders a readable report and still maps a
/// block to a non-zero exit. Human output is the default a person sees, yet every
/// other scenario uses `--format jsonl`, so this pins the render path directly.
#[test]
fn human_output_renders_and_still_gates() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("hpass", "codex", "gate").behavior("pass"),
        Reviewer::new("hblock", "claude-code", "gate")
            .behavior("block")
            .env("FAKE_SUMMARY", "a human readable block"),
    ]));
    // No --format flag: defaults to human.
    let output = repo.run(fake, &["review", "--base", "main"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a block must still exit 1 in human mode"
    );
    assert!(stdout.contains("PASS "), "missing PASS marker:\n{stdout}");
    assert!(
        stdout.contains("BLOCK hblock: a human readable block"),
        "missing block line:\n{stdout}"
    );
    assert!(
        stdout.contains("[blocking] src/extra.rs:1-1: simulated blocking finding"),
        "missing rendered finding:\n{stdout}"
    );
    assert!(
        stdout.contains("run complete"),
        "missing completion line:\n{stdout}"
    );
}

/// A missing reviewer registry is a hard error (a non-zero exit with a message),
/// not a fail-closed block and not a silent pass: nothing is persisted.
#[test]
fn a_missing_registry_is_a_hard_error() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::without_registry();
    let output = repo.run(
        fake,
        &["review", "--base", "main", "--format", "jsonl"],
        &[],
    );

    assert_ne!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("run.completed"),
        "no run should be reported"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no reviewer registry found"),
        "stderr:\n{stderr}"
    );
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// A registry at the deprecated `bastion/reviewers.yaml` location still works (the
/// back-compat shim), but the run logs a deprecation warning pointing at the new
/// `.bastion.yaml` root location.
#[test]
fn the_legacy_registry_location_still_works_with_a_deprecation_warning() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new_legacy(&registry(&[
        Reviewer::new("legacy-gate", "codex", "gate").behavior("pass")
    ]));
    // Raise the log level past the suite default (`error`) so the warning is visible.
    let run = repo.review_base(fake, "main", &[("RUST_LOG", "warn")]);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.passed, 1);

    assert!(
        run.stderr.contains("deprecated path"),
        "expected a deprecation warning, stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(".bastion.yaml"),
        "the warning must point at the new location, stderr:\n{}",
        run.stderr
    );
}

/// A repository registry suppresses fallback user-level reviewers by default, so
/// the local run matches the repository-governed reviewer set without extra model
/// calls.
#[test]
fn a_repository_registry_uses_only_repository_reviewers_by_default() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("repo-only", "codex", "gate").behavior("pass")
    ]))
    .with_user_registry(&registry(&[Reviewer::new(
        "user-only",
        "claude-code",
        "gate",
    )
    .behavior("pass")]));

    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 1);
    assert_eq!(run.resolved_count(), 1);
    assert_eq!(run.resolved("repo-only").0, Decision::Pass);
}

/// `--with-user-reviewers` merges a user-level registry with the repository's
/// through the real binary. A reviewer the user keeps in their config dir runs
/// locally even when the repo
/// never defined it, an identical reviewer present in both files is deduplicated,
/// and a same-name reviewer whose config differs survives under the `repo:` scope
/// alongside the user's. This is the local-only path; CI, with no user config dir,
/// sees the repo set alone.
#[test]
fn a_user_registry_merges_with_the_repository_registry() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("repo-only", "codex", "gate").behavior("pass"),
        Reviewer::new("shared-same", "codex", "gate").behavior("pass"),
        Reviewer::new("shared-diff", "claude-code", "gate")
            .behavior("pass")
            .prompt("repo prompt"),
    ]))
    .with_user_registry(&registry(&[
        Reviewer::new("user-only", "claude-code", "gate").behavior("pass"),
        // Byte-for-byte the repo's `shared-same`: deduplicated to one reviewer.
        Reviewer::new("shared-same", "codex", "gate").behavior("pass"),
        // Same name, different prompt: a real collision, so both survive.
        Reviewer::new("shared-diff", "claude-code", "gate")
            .behavior("pass")
            .prompt("user prompt"),
    ]));

    let run = repo.review_with_args(fake, &["--with-user-reviewers"]);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    // repo-only, user-only, shared-same (deduped to one), and the collision kept as
    // shared-diff (user) plus repo:shared-diff (repo): five reviewers, not six.
    assert_eq!(gates.total, 5);
    assert_eq!(gates.passed, 5);
    assert_eq!(run.resolved_count(), 5);

    // The user's own reviewer ran, though the repo registry never mentions it.
    let (user_only, _, _, _) = run.resolved("user-only");
    assert_eq!(user_only, Decision::Pass);
    // The genuine collision kept both copies, with the repo side scoped.
    run.resolved("shared-diff"); // the user's, under the plain name
    run.resolved("repo:shared-diff"); // the repo's, scoped

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs[0].reviewers, 5);
}

/// `validate` follows the same fallback and explicit-merge rules as `review`.
#[test]
fn validate_merges_user_reviewers_only_when_requested() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("repo-only", "codex", "gate")]))
        .with_user_registry(&registry(&[Reviewer::new(
            "user-only",
            "claude-code",
            "advisor",
        )]));

    let fallback = repo.run(fake, &["validate"], &[]);
    assert!(
        fallback.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    let fallback_stdout = String::from_utf8_lossy(&fallback.stdout);
    assert!(
        fallback_stdout.contains("1 reviewer(s), 1 gate(s), 0 advisor(s)"),
        "stdout:\n{fallback_stdout}"
    );
    assert!(!fallback_stdout.contains("user-only"));

    let merged = repo.run(fake, &["validate", "--with-user-reviewers"], &[]);
    assert!(
        merged.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    let merged_stdout = String::from_utf8_lossy(&merged.stdout);
    assert!(
        merged_stdout.contains("2 reviewer(s), 1 gate(s), 1 advisor(s)"),
        "stdout:\n{merged_stdout}"
    );
    assert!(merged_stdout.contains("repo-only"));
    assert!(merged_stdout.contains("user-only"));
}

/// A repository with no registry of its own still runs the user's personal
/// reviewers locally: discovery finds no repo `.bastion.yaml`, the user-level one
/// supplies the reviewers, and the whole git-root/routing/execution/persistence path
/// runs end to end through the real binary.
#[test]
fn a_user_only_registry_runs_when_the_repo_has_none() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::without_registry().with_user_registry(&registry(&[Reviewer::new(
        "my-personal",
        "claude-code",
        "gate",
    )
    .behavior("pass")]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 1);
    assert_eq!(run.resolved("my-personal").0, Decision::Pass);

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs[0].reviewers, 1);
}

/// Discovery and loading start from the same git root. A registry below that root
/// is not repository policy, even when the command runs from its directory, so the
/// personal fallback remains available.
#[test]
fn a_subdirectory_registry_does_not_suppress_personal_fallback() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::without_registry().with_user_registry(&registry(&[Reviewer::new(
        "my-personal",
        "codex",
        "gate",
    )
    .behavior("pass")]));
    let package = repo.path().join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join(".bastion.yaml"),
        registry(&[Reviewer::new("nested", "codex", "gate").behavior("block")]),
    )
    .unwrap();

    let validation = repo.run_from(fake, &package, &["validate"], &[]);
    let validation_stdout = String::from_utf8_lossy(&validation.stdout);
    assert!(
        validation.status.success(),
        "stdout:\n{validation_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&validation.stderr)
    );
    assert!(
        validation_stdout.contains("my-personal"),
        "stdout:\n{validation_stdout}"
    );
    assert!(
        !validation_stdout.contains("nested"),
        "stdout:\n{validation_stdout}"
    );

    let output = repo.run_from(
        fake,
        &package,
        &["review", "--base", "main", "--format", "jsonl"],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("my-personal"), "stdout:\n{stdout}");
    assert!(!stdout.contains("nested"), "stdout:\n{stdout}");
}

/// Git can resolve a worktree even when the process starts outside it. Reviewer
/// selection follows that resolved root, so an unrelated cwd cannot make a
/// repository registry disappear and silently restore personal reviewers.
#[test]
fn an_external_cwd_uses_the_worktree_root_for_reviewer_selection() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("repo-only", "codex", "gate").behavior("pass")
    ]))
    .with_user_registry(&registry(&[
        Reviewer::new("user-only", "codex", "gate").behavior("block")
    ]));
    let external_cwd = tempfile::tempdir().unwrap();
    let git_dir = repo.path().join(".git");
    let git_env = [
        ("GIT_DIR", git_dir.to_str().unwrap()),
        ("GIT_WORK_TREE", repo.path().to_str().unwrap()),
    ];

    let validation = repo.run_from(fake, external_cwd.path(), &["validate"], &git_env);
    let validation_stdout = String::from_utf8_lossy(&validation.stdout);
    assert!(
        validation.status.success(),
        "stdout:\n{validation_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&validation.stderr)
    );
    assert!(
        validation_stdout.contains("repo-only"),
        "stdout:\n{validation_stdout}"
    );
    assert!(!validation_stdout.contains("user-only"));

    let review = repo.run_from(
        fake,
        external_cwd.path(),
        &["review", "--base", "main", "--format", "jsonl"],
        &git_env,
    );
    let review_stdout = String::from_utf8_lossy(&review.stdout);
    assert!(
        review.status.success(),
        "stdout:\n{review_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&review.stderr)
    );
    assert!(
        review_stdout.contains("repo-only"),
        "stdout:\n{review_stdout}"
    );
    assert!(!review_stdout.contains("user-only"));
}

/// `--include` adds to the repository layer but does not claim that a repository
/// registry exists. Personal fallback reviewers still run when it is the only
/// discovered registry.
#[test]
fn an_include_does_not_suppress_personal_fallback() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::without_registry().with_user_registry(&registry(&[Reviewer::new(
        "my-personal",
        "codex",
        "gate",
    )
    .behavior("pass")]));
    std::fs::write(
        repo.path().join("extra.yaml"),
        registry(&[Reviewer::new("included", "codex", "gate").behavior("pass")]),
    )
    .unwrap();

    let run = repo.review_with_args(fake, &["--include", "extra.yaml"]);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved_count(), 2);
    assert_eq!(run.resolved("my-personal").0, Decision::Pass);
    assert_eq!(run.resolved("included").0, Decision::Pass);
}

/// Explicit user-reviewer merging is incompatible with GitHub-source reviews,
/// where personal reviewers must never enter the governed gate.
#[test]
fn a_github_source_rejects_user_reviewer_merging() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("repo", "codex", "gate")]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--repo",
            "attunehq/bastion",
            "--pr",
            "147",
            "--with-user-reviewers",
        ],
        &[],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("`--with-user-reviewers` cannot be used with `--repo`/`--pr`"),
        "stderr:\n{stderr}"
    );
}

/// A repository hint without a PR still cannot be combined with personal
/// reviewers. Reject it rather than silently treating the invocation as local.
#[test]
fn a_repo_hint_rejects_user_reviewer_merging() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("repo", "codex", "gate")]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--repo",
            "attunehq/bastion",
            "--with-user-reviewers",
        ],
        &[],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("`--with-user-reviewers` cannot be used with `--repo`/`--pr`"),
        "stderr:\n{stderr}"
    );
}

/// Actions exports `GITHUB_REPOSITORY` for every process. That ambient fallback
/// is not an explicit GitHub-source request when no PR number is present, so it
/// must not prevent an otherwise local personal-reviewer merge.
#[test]
fn an_ambient_github_repository_allows_local_user_reviewer_merging() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("repo-only", "codex", "gate").behavior("pass")
    ]))
    .with_user_registry(&registry(&[
        Reviewer::new("user-only", "codex", "gate").behavior("pass")
    ]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
            "--with-user-reviewers",
            "--format",
            "jsonl",
        ],
        &[("GITHUB_REPOSITORY", "attunehq/bastion")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("repo-only"), "stdout:\n{stdout}");
    assert!(stdout.contains("user-only"), "stdout:\n{stdout}");
}

/// A registry split across files reviews like one file: the root's `include:`
/// pulls in a second registry whose reviewer reads its prompt from a markdown
/// file, and both reviewers execute through the real binary and gate together.
#[test]
fn an_included_registry_with_a_prompt_file_reviews_like_one_file() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&format!(
        "include: [reviewers/more.yaml]\n{}",
        registry(&[Reviewer::new("root-pass", "codex", "gate").behavior("pass")])
    ));
    std::fs::create_dir_all(repo.path().join("reviewers/prompts")).unwrap();
    std::fs::write(
        repo.path().join("reviewers/more.yaml"),
        "reviewers:\n  - name: incl-block\n    trigger: [src/**/*.rs]\n    mode: gate\n    backend: claude-code\n    env:\n      FAKE_BEHAVIOR: block\n    prompt: {file: prompts/incl.md}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("reviewers/prompts/incl.md"),
        "Block everything, for the test.\n",
    )
    .unwrap();

    let run = repo.review(fake);

    assert_eq!(run.code, Some(1), "the included gate's block must gate");
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    assert_eq!(gates.total, 2, "both files' reviewers ran");
    assert_eq!(run.resolved("root-pass").0, Decision::Pass);
    assert_eq!(run.resolved("incl-block").0, Decision::Block);
}

/// `--include` merges an extra registry file into the repository layer for the
/// run, as if the root file's `include:` listed it: its reviewer executes
/// alongside the repository's.
#[test]
fn an_include_flag_merges_an_extra_registry_into_the_run() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("repo-r", "codex", "gate").behavior("pass")
    ]));
    std::fs::write(
        repo.path().join("extra.yaml"),
        registry(&[Reviewer::new("extra-r", "claude-code", "gate").behavior("pass")]),
    )
    .unwrap();

    let run = repo.review_with_args(fake, &["--include", "extra.yaml"]);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 2, "the --include reviewer joined the run");
    run.resolved("repo-r");
    run.resolved("extra-r");
}

/// An invalid registry (here, duplicate reviewer names) is a hard error surfaced
/// to the user, never swallowed into a pass.
#[test]
fn an_invalid_registry_is_a_hard_error() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(
        "reviewers:\n  - name: dup\n    trigger: [src/**]\n    mode: gate\n    backend: codex\n    prompt: one\n  - name: dup\n    trigger: [src/**]\n    mode: gate\n    backend: codex\n    prompt: two\n",
    );
    let output = repo.run(
        fake,
        &["review", "--base", "main", "--format", "jsonl"],
        &[],
    );

    assert_ne!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("run.completed"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate reviewer name"),
        "stderr:\n{stderr}"
    );
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// `bastion validate` parses the registry without running a reviewer: a well-formed
/// file exits zero with a summary, and a malformed one exits non-zero naming the
/// problem, the same load-time errors a review would hit. No model call is made, so
/// no fake-agent behavior is exercised; the binary never reaches a backend.
#[test]
fn validate_reports_valid_and_invalid_registries_without_a_review() {
    let Some(fake) = tooling() else { return };

    // Valid: exit 0, a summary on stdout, no run recorded (validate persists nothing).
    let ok = TestRepo::new(
        "reviewers:\n  - name: a\n    trigger: [src/**]\n    mode: gate\n    prompt: p\n  - name: b\n    trigger: [docs/**]\n    mode: advisor\n    prompt: p\n",
    );
    let output = ok.run(fake, &["validate"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a valid registry must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("is valid"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("2 reviewer(s), 1 gate(s), 1 advisor(s)"),
        "stdout:\n{stdout}"
    );
    // The per-reviewer detail lines name each reviewer with its mode and backend.
    assert!(
        stdout.contains("- a (gate, backend: any"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("- b (advisor, backend: any"),
        "stdout:\n{stdout}"
    );
    assert!(
        ok.run(fake, &["runs", "--format", "jsonl"], &[])
            .status
            .success(),
        "validate must not have recorded a run"
    );
    assert!(
        store::list_runs(&ok.layout()).unwrap().is_empty(),
        "validate persists nothing"
    );

    // Invalid (duplicate name): non-zero exit, the error names the duplicate.
    let bad = TestRepo::new(
        "reviewers:\n  - name: dup\n    trigger: [src/**]\n    mode: gate\n    prompt: p\n  - name: dup\n    trigger: [src/**]\n    mode: gate\n    prompt: p\n",
    );
    let output = bad.run(fake, &["validate"], &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "an invalid registry must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate reviewer name"),
        "stderr:\n{stderr}"
    );

    // Invalid (unknown field): rejected too, so a typo cannot slip through silently.
    let typo = TestRepo::new(
        "reviewers:\n  - name: typo\n    trigger: [src/**]\n    mode: gate\n    bakend: codex\n    prompt: p\n",
    );
    let output = typo.run(fake, &["validate"], &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "an unknown field must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field `bakend`"),
        "stderr:\n{stderr}"
    );
}

/// A base that does not resolve is a hard error (git fails), not a block.
#[test]
fn an_unresolvable_base_is_a_hard_error() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("g", "codex", "gate").behavior("pass")
    ]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "does-not-exist-branch",
            "--format",
            "jsonl",
        ],
        &[],
    );

    assert_ne!(output.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("run.completed"));
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// Read-back commands on an empty data directory error cleanly, and unknown run /
/// reviewer ids report a clear not-found error rather than succeeding.
#[test]
fn read_back_errors_are_clear() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("r", "codex", "gate").behavior("pass")
    ]));

    // Empty data dir: nothing recorded yet.
    let show_empty = repo.run(fake, &["show"], &[]);
    assert_ne!(show_empty.status.code(), Some(0));
    let empty_stderr = String::from_utf8_lossy(&show_empty.stderr);
    assert!(
        empty_stderr.contains("no runs recorded yet"),
        "stderr:\n{empty_stderr}"
    );

    // After a real run, unknown ids are not-found errors.
    let run = repo.review(fake);
    assert!(run.exited_zero());
    let run_id = repo.latest_run_id();

    let bad_run = repo.run(fake, &["show", "no-such-run"], &[]);
    assert_ne!(bad_run.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&bad_run.stderr).contains("no such run"));

    let bad_reviewer = repo.run(
        fake,
        &["transcript", run_id.as_str(), "no-such-reviewer"],
        &[],
    );
    assert_ne!(bad_reviewer.status.code(), Some(0));
    let bad_reviewer_stderr = String::from_utf8_lossy(&bad_reviewer.stderr);
    assert!(
        bad_reviewer_stderr.contains("no saved transcript"),
        "stderr:\n{bad_reviewer_stderr}"
    );
}

/// `github codeowners` is a standalone subcommand (no repo/git/agent needed): it
/// prints the governance block, and requires at least one `--owner`.
#[test]
fn github_codeowners_emits_the_policy_block() {
    let Some(fake) = tooling() else { return };
    // Reuse a repo only for a valid working directory; the command reads nothing.
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("r", "codex", "gate").behavior("pass")
    ]));

    let ok = repo.run(
        fake,
        &[
            "github",
            "codeowners",
            "--owner",
            "@acme/platform",
            "--owner",
            "@jess",
        ],
        &[],
    );
    assert!(ok.status.success());
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(
        stdout.contains("/.bastion.yaml @acme/platform @jess"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("/.bastion.yml @acme/platform @jess"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("require human review"), "stdout:\n{stdout}");

    // The owner argument is required.
    let missing = repo.run(fake, &["github", "codeowners"], &[]);
    assert_ne!(missing.status.code(), Some(0));
}

/// `skills install` writes the bundled skill into both default roots; `skills
/// check` then passes, fails closed after a hand edit, and passes again once
/// `install --force` restores the file. End to end through the real binary.
#[test]
fn skills_install_and_check_round_trip() {
    let Some(fake) = tooling() else { return };
    // A repo is needed only as a git working directory; skills touch no reviewers.
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("r", "codex", "gate").behavior("pass")
    ]));

    // Install lands a SKILL.md under each default root.
    let install = repo.run(fake, &["skills", "install"], &[]);
    assert!(
        install.status.success(),
        "install failed; stderr:\n{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let claude = repo.path().join(".claude/skills/using-bastion/SKILL.md");
    let agents = repo.path().join(".agents/skills/using-bastion/SKILL.md");
    assert!(claude.exists(), "expected {}", claude.display());
    assert!(agents.exists(), "expected {}", agents.display());

    // The written file is a real Claude Code skill: front matter first, named.
    let body = std::fs::read_to_string(&claude).unwrap();
    assert!(body.starts_with("---\n"), "body:\n{body}");
    assert!(body.contains("name: using-bastion"), "body:\n{body}");
    assert!(
        body.contains("Generated by `bastion skills install`"),
        "the provenance stamp should be present; body:\n{body}"
    );

    // Right after install, check is green.
    assert!(repo.run(fake, &["skills", "check"], &[]).status.success());

    // A hand edit makes check fail closed (non-zero exit) and report drift.
    std::fs::write(&claude, "tampered\n").unwrap();
    let drifted = repo.run(fake, &["skills", "check"], &[]);
    assert_ne!(drifted.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&drifted.stdout).contains("drifted"),
        "stdout:\n{}",
        String::from_utf8_lossy(&drifted.stdout)
    );

    // Without --force, install refuses to clobber the edited file.
    let no_force = repo.run(fake, &["skills", "install"], &[]);
    assert!(no_force.status.success());
    assert!(
        String::from_utf8_lossy(&no_force.stdout).contains("skipped"),
        "stdout:\n{}",
        String::from_utf8_lossy(&no_force.stdout)
    );
    assert_eq!(std::fs::read_to_string(&claude).unwrap(), "tampered\n");

    // --force restores it, and check is green again.
    assert!(
        repo.run(fake, &["skills", "install", "--force"], &[])
            .status
            .success()
    );
    assert!(repo.run(fake, &["skills", "check"], &[]).status.success());

    // `skills list` names the bundled skill.
    let listed = repo.run(fake, &["skills", "list"], &[]);
    assert!(listed.status.success());
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("using-bastion"),
        "stdout:\n{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// `bastion review` warns on stderr when the bundled skills are missing or stale,
/// so the driving agent is told to refresh them, and the notice clears once they are
/// installed. The advisory is stderr-only: it never pollutes the JSONL event stream
/// on stdout, and it never changes the review's exit status.
#[test]
fn review_warns_on_stderr_when_skills_are_stale() {
    let Some(fake) = tooling() else { return };
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("style", "codex", "advisor").behavior("pass")
    ]));

    // Nothing installed yet: the review still runs, but stderr carries the advisory
    // pointing at `skills install`, and stdout stays pure JSONL (parse_events would
    // panic otherwise).
    let before = repo.review(fake);
    assert!(
        before
            .stderr
            .contains("bundled agent skills are missing or out of date")
            && before.stderr.contains("bastion skills install"),
        "expected a skills advisory on stderr; got:\n{}",
        before.stderr
    );

    // Installing the skills clears the advisory on the next review.
    assert!(repo.run(fake, &["skills", "install"], &[]).status.success());
    let after = repo.review(fake);
    assert!(
        !after.stderr.contains("bundled agent skills"),
        "the advisory should be gone once skills are installed; got:\n{}",
        after.stderr
    );
}

/// The stale-skills advisory is gated on the *repository* having adopted Bastion.
/// A review that runs on the author's user-level reviewers alone (no repo
/// `.bastion.yaml`) must not nudge them to install skills into a project that never
/// configured Bastion, even though the skills are absent. Only the local surface is
/// gated this way; the CI report path is unchanged.
#[test]
fn review_does_not_warn_on_stale_skills_without_a_repo_registry() {
    let Some(fake) = tooling() else { return };

    // No repo registry: the reviewers come solely from the user config dir, and the
    // bundled skills were never installed into this throwaway repo.
    let repo = TestRepo::without_registry().with_user_registry(&registry(&[Reviewer::new(
        "my-personal",
        "claude-code",
        "gate",
    )
    .behavior("pass")]));

    let run = repo.review(fake);

    // The review still runs the user's reviewer to a pass...
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    // ...but stays silent about skills, because the project has not adopted Bastion.
    assert!(
        !run.stderr.contains("bundled agent skills"),
        "a user-only review must not nudge about repo skills; got:\n{}",
        run.stderr
    );
}
