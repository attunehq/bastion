//! Attestation: sealing, `bastion attest`, and CI replay/fallback.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;
use crate::github::*;

use bastion::event::RunEvent;
use bastion::store;
use bastion::verdict::Decision;

/// Every local `bastion review` seals its run: `seal.json` lands next to
/// `run.jsonl`, parses, and (since the whole suite drives the fake-agent seams)
/// always records `seams: true`. `TestRepo` dirties the tree by design (an
/// uncommitted edit plus an untracked file, so a reviewer always has files to
/// route), so this run's seal also records `dirty: true`.
#[test]
fn review_seals_its_run() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("sealed-gate", "codex", "gate").behavior("pass")
    ]));
    let run = repo.review(fake);
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);

    let layout = repo.layout();
    let run_id = repo.latest_run_id();
    let seal_path = layout.seal(&run_id);
    assert!(
        seal_path.exists(),
        "expected a seal at {}",
        seal_path.display()
    );

    let seal_json = std::fs::read_to_string(&seal_path).unwrap();
    let seal: serde_json::Value = serde_json::from_str(&seal_json)
        .unwrap_or_else(|e| panic!("seal.json did not parse: {e}\n{seal_json}"));

    assert_eq!(
        seal["seams"],
        serde_json::Value::Bool(true),
        "the suite always runs under the fake-agent seams; seal: {seal_json}"
    );
    assert_eq!(
        seal["dirty"],
        serde_json::Value::Bool(true),
        "TestRepo dirties the tree by design; seal: {seal_json}"
    );
    assert_eq!(
        seal["reviewers"],
        serde_json::json!(["sealed-gate"]),
        "seal: {seal_json}"
    );
    for field in [
        "head_tree",
        "base_tree",
        "patch_id",
        "config_hash",
        "version",
        "mac",
    ] {
        let value = seal[field]
            .as_str()
            .unwrap_or_else(|| panic!("seal.{field} was not a string; seal: {seal_json}"));
        assert!(
            !value.is_empty(),
            "seal.{field} was empty; seal: {seal_json}"
        );
    }
}

/// A review over a fully committed working tree (no uncommitted or untracked
/// changes left dangling) seals `dirty: false`: the dirty flag reflects the
/// actual state of the tree at review time, not a fixed default.
#[test]
fn review_over_a_clean_committed_tree_seals_dirty_false() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("sealed-gate", "codex", "gate").behavior("pass")
    ]));
    // Commit the dirtied files `TestRepo::new` left uncommitted, so the tree is
    // clean when the review runs. `TestRepo` has only one branch (everything
    // lands on `main` directly), so `--base main` would diff HEAD against
    // itself once this commit lands; diff against the parent commit
    // (`HEAD~1`, the fixture's own base commit) instead, so the changeset is
    // still non-empty and the reviewer still routes.
    repo.commit_all("commit the changeset");
    let run = repo.review_base(fake, "HEAD~1", &[]);
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);

    let layout = repo.layout();
    let run_id = repo.latest_run_id();
    let seal_path = layout.seal(&run_id);
    let seal_json = std::fs::read_to_string(&seal_path).unwrap();
    let seal: serde_json::Value = serde_json::from_str(&seal_json)
        .unwrap_or_else(|e| panic!("seal.json did not parse: {e}\n{seal_json}"));

    assert_eq!(
        seal["dirty"],
        serde_json::Value::Bool(false),
        "a fully committed tree must seal dirty: false; seal: {seal_json}"
    );
}

/// `bastion attest` refuses to sign a run that used a test-backend seam: exercising
/// the binary is not the same as a real review, and the refusal names the seam.
#[test]
fn attest_refuses_a_seam_stubbed_run() {
    let Some(fake) = ssh_tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("gate", "codex", "gate").behavior("pass")
    ]));
    let run = repo.review(fake);
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);

    let keys_dir = tempfile::tempdir().unwrap();
    let key_path = generate_ssh_key(keys_dir.path());

    let output = repo.run(fake, &["attest", "--key", key_path.to_str().unwrap()], &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "attesting a seam-stubbed run must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("test seam"),
        "expected the refusal to name the test seam; stderr:\n{stderr}"
    );
}

/// `bastion attest` refuses a run whose seal is missing (deleted, or never sealed).
#[test]
fn attest_refuses_an_unsealed_run() {
    let Some(fake) = ssh_tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("gate", "codex", "gate").behavior("pass")
    ]));
    let run = repo.review(fake);
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);

    let layout = repo.layout();
    let run_id = repo.latest_run_id();
    std::fs::remove_file(layout.seal(&run_id)).expect("seal.json existed");

    let keys_dir = tempfile::tempdir().unwrap();
    let key_path = generate_ssh_key(keys_dir.path());

    let output = repo.run(fake, &["attest", "--key", key_path.to_str().unwrap()], &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "attesting an unsealed run must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not sealed"),
        "expected the refusal to say the run was not sealed; stderr:\n{stderr}"
    );
}

/// A CI-path review (`--repo`/`--pr`) with `attestations: true` but no note on
/// HEAD runs every reviewer fresh *silently*: a missing note is not a refusal, so
/// no `run.attestation-fallback` event is recorded, nothing about attestation is
/// printed, and the report has no line to draw. Only an attestation that was
/// offered and refused warrants surfacing.
#[test]
fn ci_review_without_a_note_runs_fresh_silently() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));
    // Commit the fixture's dirty tree: a dirty CI checkout falls back before any
    // note lookup, and this scenario pins the missing-note (silent) path
    // specifically. The branch marks where the changeset started, since committing
    // on main would otherwise leave nothing to diff against.
    repo.branch("basemark");
    repo.commit_all("head");

    let github = FakeGitHub::start();
    let run = repo.review_ci(fake, "basemark", "acme/app", "1", &ci_env(&github.url));
    github.finish();

    // The reviewer executed for real (no note to replay from).
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ci-gate").0, Decision::Pass);
    assert_eq!(run.started_count(), 1);
    assert_eq!(run.resolved_count(), 1);

    // No fallback event: a missing note is silent, not a refusal.
    assert!(
        run.attestation_fallback_reason().is_none(),
        "a missing note must not emit a fallback event"
    );
    assert!(
        !run.stderr.to_lowercase().contains("attestation"),
        "stderr should never mention attestation for a merely un-attested commit; stderr:\n{}",
        run.stderr
    );

    // ...and nothing persisted in run.jsonl either.
    let layout = repo.layout();
    let run_id = repo.latest_run_id();
    let persisted = store::read_run(&layout, &run_id).unwrap();
    assert!(
        !persisted
            .iter()
            .any(|e| matches!(e, RunEvent::AttestationFallback { .. })),
        "no attestation-fallback event should persist for a missing note"
    );
}

/// With `attestations` absent (the default), a CI-path review never looks up a
/// note and never emits or mentions an attestation fallback: the feature is
/// opt-in.
#[test]
fn ci_review_with_attestations_disabled_never_mentions_attestation() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("ci-gate", "codex", "gate").behavior("pass")
    ]));

    let github = FakeGitHub::start();
    let run = repo.review_ci(fake, "main", "acme/app", "1", &ci_env(&github.url));
    github.finish();

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ci-gate").0, Decision::Pass);
    assert!(
        run.attestation_fallback_reason().is_none(),
        "no fallback event should be emitted when attestations are disabled"
    );
    assert!(
        !run.stderr.to_lowercase().contains("attestation"),
        "stderr should never mention attestation when the switch is off; stderr:\n{}",
        run.stderr
    );
}

/// A dirty CI checkout never replays, regardless of what note exists: the
/// reviewers see uncommitted content no attestation's committed bindings name, so
/// the run falls back before any note lookup and the reason says why.
#[test]
fn ci_review_with_a_dirty_checkout_falls_back_without_note_lookup() {
    let Some(fake) = tooling() else { return };

    // TestRepo::new leaves the tree dirty by design; that is the scenario here.
    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));

    let github = FakeGitHub::start();
    let run = repo.review_ci(fake, "main", "acme/app", "1", &ci_env(&github.url));
    github.finish();

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ci-gate").0, Decision::Pass);
    let reason = run
        .attestation_fallback_reason()
        .expect("a run.attestation-fallback event in the jsonl stream");
    assert!(
        reason.contains("uncommitted or untracked"),
        "expected the reason to name the dirty checkout; reason: {reason}"
    );
}

/// A garbage (non-bundle) note on HEAD is a read failure, not a crash: the CI path
/// falls back to a full run and the fallback reason reflects that the note could
/// not be understood.
#[test]
fn ci_review_with_a_garbage_note_falls_back() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));
    // Commit first: a dirty checkout falls back before the note is even read, and
    // this scenario pins the unreadable-note reason specifically. The branch
    // marks where the changeset started.
    repo.branch("basemark");
    repo.commit_all("head");
    repo.write_garbage_note("not a bastion attestation bundle");

    let github = FakeGitHub::start();
    let run = repo.review_ci(fake, "basemark", "acme/app", "1", &ci_env(&github.url));
    github.finish();

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ci-gate").0, Decision::Pass);
    let reason = run
        .attestation_fallback_reason()
        .expect("a run.attestation-fallback event in the jsonl stream");
    assert!(
        reason.contains("unreadable") || reason.contains("signature"),
        "expected the reason to name an unparseable or unverifiable note; reason: {reason}"
    );
}

/// The registry schema accepts both the top-level `attestations: true` switch and
/// a per-reviewer `attestation: never` opt-out, and a review still routes and runs
/// that reviewer normally: the policy changes CI replay eligibility only, never
/// local routing.
#[test]
fn validate_accepts_attestation_schema() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "never-replayed",
        "codex",
        "gate",
    )
    .behavior("pass")
    .attestation_never()]));

    let validate = repo.run(fake, &["validate"], &[]);
    assert!(
        validate.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let stdout = String::from_utf8_lossy(&validate.stdout);
    assert!(stdout.contains("is valid"), "stdout:\n{stdout}");

    let run = repo.review(fake);
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("never-replayed").0, Decision::Pass);
}

/// `bastion github report` folds the attestation-fallback notice into the sticky
/// comment as a `[!WARNING]` block when the run it reports refused an offered
/// attestation. Here the refusal is a dirty CI checkout (the fixture leaves the
/// tree dirty), a genuine rejection rather than a merely absent note.
#[test]
fn github_report_carries_the_fallback_notice() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));

    // Drive the CI-path review so the run persists with a fallback event.
    let review_github = FakeGitHub::start();
    let review = repo.review_ci(fake, "main", "acme/app", "7", &ci_env(&review_github.url));
    review_github.finish();
    assert!(review.exited_zero(), "stderr:\n{}", review.stderr);
    assert!(review.attestation_fallback_reason().is_some());

    // Install the skills first so the report's assertions isolate the fallback
    // notice from the (already-covered) skills-drift advisory.
    assert!(repo.run(fake, &["skills", "install"], &[]).status.success());

    let report_github = FakeGitHub::start();
    let output = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &ci_env(&report_github.url),
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = report_github.finish();
    let comment = requests
        .iter()
        .find(|r| r.method == "POST" && r.path == "/repos/acme/app/issues/7/comments")
        .expect("a POST creating the sticky comment");
    assert!(
        comment.body.contains("> [!WARNING]")
            && comment.body.contains("Attestation was not honored:"),
        "the sticky comment should carry the fallback notice as a warning block: {}",
        comment.body
    );
}
