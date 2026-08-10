//! Reviewer selection (`--reviewer`) and incremental re-review (carry).
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures;
use crate::fixtures::*;
use crate::github::*;

use bastion::store;
use bastion::verdict::Decision;

/// `--reviewer` narrows the run to the selected subset, and the run is honest
/// about it: marked partial on both ends of the stream and in the run store,
/// and refused by `bastion attest`, so a filtered green can never stand in for
/// a full one.
#[test]
fn reviewer_flag_runs_only_the_selected_subset_and_marks_the_run_partial() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("gate-a", "claude-code", "gate").behavior("pass"),
        Reviewer::new("gate-b", "codex", "gate").behavior("pass"),
        Reviewer::new("gate-c", "pi", "gate").behavior("pass"),
    ]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
            "--reviewer",
            "gate-b",
            "--format",
            "jsonl",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let events = parse_events(&stdout, &stderr);
    let run = fixtures::ReviewRun {
        code: output.status.code(),
        events,
        stderr,
    };

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.started_count(), 1, "only the selected reviewer runs");
    assert_eq!(run.resolved_count(), 1);
    let (decision, ..) = run.resolved("gate-b");
    assert_eq!(decision, Decision::Pass);
    assert!(run.partial(), "a filtered run must be marked partial");
    let (aggregate, gates, _cost) = run.completed();
    assert_eq!(aggregate, Decision::Pass);
    assert_eq!(gates.total, 1);

    // The store remembers it was partial, and it is never sealed, so `bastion
    // attest` refuses it with the partial-specific reason.
    let runs = store::list_runs(&repo.layout()).unwrap();
    assert!(runs[0].partial);
    let attest = repo.run(fake, &["attest"], &[]);
    assert!(!attest.status.success());
    let attest_stderr = String::from_utf8_lossy(&attest.stderr);
    assert!(
        attest_stderr.contains("was partial"),
        "attest must name the partial run as the reason, got:\n{attest_stderr}"
    );
}

/// An unknown or untriggered `--reviewer` name is a usage error before anything
/// runs: a typo in a re-run loop must not silently run nothing and exit green.
#[test]
fn reviewer_flag_rejects_unknown_and_untriggered_names() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("src-gate", "claude-code", "gate").behavior("pass"),
        // In the registry, but its trigger does not match the changeset.
        Reviewer::new("docs-gate", "codex", "gate")
            .trigger("docs/**")
            .behavior("pass"),
    ]));
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
            "--reviewer",
            "docs-gate",
            "--reviewer",
            "no-such-reviewer",
            "--format",
            "jsonl",
        ],
        &[],
    );

    assert!(!output.status.success(), "an invalid selection must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not in the reviewer registry: no-such-reviewer"),
        "got:\n{stderr}"
    );
    assert!(
        stderr.contains("not triggered by this changeset: docs-gate"),
        "got:\n{stderr}"
    );
    assert!(
        stderr.contains("triggered reviewers: src-gate"),
        "the error should name what *can* run, got:\n{stderr}"
    );
    // Nothing ran and nothing was persisted: the error precedes the run.
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// The incremental loop end to end: a second review of the same branch carries
/// a prior pass forward when the reviewer's trigger-scoped diff is unchanged,
/// re-executes it once that scoped content (or `--fresh`) says so, and never
/// carries a repository reviewer whose prior seal records an active test seam.
///
/// The carrying reviewer here is deliberately a *user-level* one: this whole
/// suite runs with the `BASTION_*_BIN` seams set, so every seal records
/// `seams: true` and every repository reviewer is (correctly) disqualified from
/// carrying. That refusal is asserted too: `src-gate` stays fresh on the second
/// run even though its scoped diff is also unchanged.
#[test]
fn a_prior_pass_carries_forward_only_while_its_scoped_diff_is_unchanged() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "src-gate",
        "claude-code",
        "gate",
    )
    .behavior("pass")]))
    .with_user_registry(&registry(&[Reviewer::new("docs-gate", "codex", "gate")
        .trigger("docs/**")
        .behavior("pass")]));
    // Change a docs file too, so the user-level reviewer triggers.
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join("docs/note.md"), "first draft\n").unwrap();

    // First run: everything executes fresh.
    let first = repo.review_with_args(fake, &["--with-user-reviewers"]);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    assert!(!first.carried("docs-gate"));
    assert!(!first.carried("src-gate"));

    // Second run, nothing changed: the user-level reviewer carries its pass;
    // the repo reviewer stays fresh because its prior seal is seam-tainted.
    let second = repo.review_with_args(fake, &["--with-user-reviewers"]);
    assert!(second.exited_zero(), "stderr:\n{}", second.stderr);
    assert!(
        second.carried("docs-gate"),
        "an unchanged scoped diff must carry; stderr:\n{}",
        second.stderr
    );
    assert!(
        !second.carried("src-gate"),
        "a seam-tainted prior seal must not carry a repo reviewer"
    );
    let (_, _, _, usage) = second.resolved("docs-gate");
    assert_eq!(usage, None, "a carried verdict spends no tokens");
    assert!(
        second.stderr.contains("carried forward"),
        "the carry is announced on stderr; got:\n{}",
        second.stderr
    );
    let (aggregate, gates, _cost) = second.completed();
    assert_eq!(aggregate, Decision::Pass);
    assert_eq!(gates.total, 2, "a carried gate still counts in the tally");
    assert!(
        !second.partial(),
        "carry is full coverage, not a partial run"
    );

    // `--fresh` opts out entirely.
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
            "--fresh",
            "--with-user-reviewers",
            "--format",
            "jsonl",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let fresh = fixtures::ReviewRun {
        code: output.status.code(),
        events: parse_events(&stdout, &stderr),
        stderr,
    };
    assert!(fresh.exited_zero(), "stderr:\n{}", fresh.stderr);
    assert!(
        !fresh.carried("docs-gate"),
        "--fresh must re-run everything"
    );

    // Touching the scoped content invalidates the carry.
    std::fs::write(repo.path().join("docs/note.md"), "second draft\n").unwrap();
    let after_edit = repo.review_with_args(fake, &["--with-user-reviewers"]);
    assert!(
        !after_edit.carried("docs-gate"),
        "an edited scoped file must re-execute the reviewer"
    );
}

/// The boundary cases of an explicit selection: naming *every* triggered
/// reviewer is a full run (coverage was not reduced, so nothing is marked
/// partial), and naming any reviewer suppresses carry for it (asking for a
/// reviewer by name means asking for it to run), even when an eligible prior
/// pass exists.
#[test]
fn selecting_every_reviewer_is_full_and_a_selection_never_carries() {
    let Some(fake) = tooling() else { return };

    // A user-level reviewer, so a prior pass is carry-eligible despite the
    // suite's seam-tainted seals.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "src-gate",
        "claude-code",
        "gate",
    )
    .behavior("pass")]))
    .with_user_registry(&registry(&[Reviewer::new("docs-gate", "codex", "gate")
        .trigger("docs/**")
        .behavior("pass")]));
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join("docs/note.md"), "first draft\n").unwrap();

    // Seed a prior run whose docs-gate pass would carry on a plain re-run.
    let first = repo.review_with_args(fake, &["--with-user-reviewers"]);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);

    // Naming both triggered reviewers: full coverage, so not partial, and the
    // named docs-gate executes fresh instead of carrying its eligible pass.
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
            "--with-user-reviewers",
            "--reviewer",
            "src-gate",
            "--reviewer",
            "docs-gate",
            "--format",
            "jsonl",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let full = fixtures::ReviewRun {
        code: output.status.code(),
        events: parse_events(&stdout, &stderr),
        stderr,
    };
    assert!(full.exited_zero(), "stderr:\n{}", full.stderr);
    assert!(
        !full.partial(),
        "selecting the whole triggered set is a full run"
    );
    assert!(
        !full.carried("docs-gate"),
        "an explicitly selected reviewer must execute fresh, not carry"
    );
    assert_eq!(full.resolved_count(), 2);
    let (aggregate, gates, _cost) = full.completed();
    assert_eq!(aggregate, Decision::Pass);
    assert_eq!(gates.total, 2);
}

/// The incremental loop works in CI too: a second CI review (`--repo`/`--pr`) of the
/// same branch carries a *repository* reviewer's prior pass forward when its
/// trigger-scoped diff is unchanged, exactly as a local re-run does, and re-executes
/// once that content changes. Carry is sound on the CI surface because the prior
/// run's seal verifies and the digest binds the content, not because of where the run
/// happened.
///
/// This drives the backend as `codex` on `PATH` with no override (the real dogfood
/// configuration), so the prior run seals seam-free and its repository reviewer is
/// carry-eligible. The rest of the suite sets the `BASTION_*_BIN` seams, which is
/// exactly why those runs cannot carry a repository reviewer; here the seam-free seal
/// is asserted so the carry below cannot silently pass on a technicality.
#[test]
fn ci_carries_an_unchanged_repo_pass_from_the_prior_ci_run() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("ci-gate", "codex", "gate").behavior("pass")
    ]));
    let source = ("acme/app", "1");

    // First CI run: the reviewer executes for real, and (backend on PATH, no
    // override) its run seals seam-free, so the pass is carry-eligible.
    let github = FakeGitHub::start();
    let first =
        repo.review_ci_backend_on_path(fake, "main", source.0, source.1, &ci_env(&github.url));
    github.finish();
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    assert!(
        !first.carried("ci-gate"),
        "the first run has no prior run to carry from"
    );

    // The seal really did record seams: false; otherwise the carry below could never
    // fire, and this test would be asserting nothing.
    let layout = repo.layout();
    let run_id = repo.latest_run_id();
    let seal_json = std::fs::read_to_string(layout.seal(&run_id)).expect("the first run sealed");
    let seal: serde_json::Value = serde_json::from_str(&seal_json).unwrap();
    assert_eq!(
        seal["seams"],
        serde_json::json!(false),
        "a CI run with the backend on PATH must seal seam-free; seal: {seal_json}"
    );

    // Second CI run, nothing changed: the repository reviewer carries its pass with no
    // backend dispatch, still counting in the gate tally.
    let github = FakeGitHub::start();
    let second =
        repo.review_ci_backend_on_path(fake, "main", source.0, source.1, &ci_env(&github.url));
    github.finish();
    assert!(second.exited_zero(), "stderr:\n{}", second.stderr);
    assert!(
        second.carried("ci-gate"),
        "an unchanged repo reviewer must carry in CI; stderr:\n{}",
        second.stderr
    );
    let (_, _, _, usage) = second.resolved("ci-gate");
    assert_eq!(usage, None, "a carried verdict spends no tokens");
    let (aggregate, gates, _cost) = second.completed();
    assert_eq!(aggregate, Decision::Pass);
    assert_eq!(gates.total, 1, "a carried gate still counts in the tally");
    assert!(
        !second.partial(),
        "carry is full coverage, not a partial run"
    );

    // Editing the scoped content invalidates the carry: the reviewer runs fresh.
    std::fs::write(
        repo.path().join("src/extra.rs"),
        "pub fn extra() {}\npub fn more() {}\n",
    )
    .unwrap();
    let github = FakeGitHub::start();
    let third =
        repo.review_ci_backend_on_path(fake, "main", source.0, source.1, &ci_env(&github.url));
    github.finish();
    assert!(third.exited_zero(), "stderr:\n{}", third.stderr);
    assert!(
        !third.carried("ci-gate"),
        "an edited scoped file must re-execute the reviewer in CI"
    );
}
