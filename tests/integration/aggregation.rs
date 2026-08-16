//! Gate aggregation and fail-closed/fail-open scenarios.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;

use std::time::{Duration, Instant};

use bastion::store;
use bastion::verdict::{Decision, FindingKind};

/// All gates pass across both real backends -> the binary exits zero, reports a
/// clean aggregate, and persists an inspectable run.
#[test]
fn all_gates_pass_across_both_backends() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("claude-gate", "claude-code", "gate").behavior("pass"),
        Reviewer::new("codex-gate", "codex", "gate").behavior("pass"),
        Reviewer::new("default-gate", "any", "gate").behavior("pass"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 3);
    assert_eq!(gates.passed, 3);
    assert_eq!(gates.blocked, 0);

    assert_eq!(run.started_count(), 3);
    assert_eq!(run.resolved_count(), 3);

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].verdict, Some(Decision::Pass));
    assert_eq!(runs[0].reviewers, 3);
}

/// Model and effort reach each backend's argv end to end, resolved through the real
/// binary: an explicit per-reviewer value, a value inherited from the registry
/// `defaults` block, the Claude selectors, the Codex ones, Pi's
/// `--model`/`--thinking`, and Grok Build's `--model`/`--reasoning-effort`. The fake
/// agent fails its contract (non-zero exit) if a selector is missing, which would
/// fail the gate closed; a clean `pass` across all five proves the flags arrived.
#[test]
fn model_and_effort_reach_each_backend_through_the_real_binary() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_defaults(
        &[("model", "gpt-5"), ("effort", "high")],
        &[
            // Explicit Codex model/effort overrides the registry default.
            Reviewer::new("codex-explicit", "codex", "gate")
                .model("gpt-5-codex")
                .effort("high")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "gpt-5-codex")
                .env("FAKE_EXPECT_EFFORT", "high"),
            // No model/effort: inherits both from the `defaults` block.
            Reviewer::new("codex-inherits", "codex", "gate")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "gpt-5")
                .env("FAKE_EXPECT_EFFORT", "high"),
            // The Claude selectors (`--model`/`--effort`) on a pinned model; `medium`
            // maps identically on both backends.
            Reviewer::new("claude-explicit", "claude-code", "gate")
                .model("claude-sonnet-4-6")
                .effort("medium")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "claude-sonnet-4-6")
                .env("FAKE_EXPECT_EFFORT", "medium"),
            // The Pi selectors (`--model`/`--thinking`): the model carries its
            // provider in Pi's `provider/id` form, and `xhigh` is a Pi-specific
            // thinking level forwarded verbatim.
            Reviewer::new("pi-explicit", "pi", "gate")
                .model("openai-codex/gpt-5.5")
                .effort("xhigh")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "openai-codex/gpt-5.5")
                .env("FAKE_EXPECT_EFFORT", "xhigh"),
            // The Grok Build selectors (`--model`/`--reasoning-effort`).
            Reviewer::new("grok-explicit", "grok", "gate")
                .model("grok-4.6")
                .effort("xhigh")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "grok-4.6")
                .env("FAKE_EXPECT_EFFORT", "xhigh"),
            // The Muse Code selectors (`--model`/`--reasoning-effort`); `ultra` is
            // a Muse-specific level forwarded verbatim.
            Reviewer::new("muse-explicit", "muse", "gate")
                .model("muse-spark-1.2")
                .effort("ultra")
                .behavior("pass")
                .env("FAKE_EXPECT_MODEL", "muse-spark-1.2")
                .env("FAKE_EXPECT_EFFORT", "ultra"),
        ],
    ));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 6);
    assert_eq!(gates.passed, 6);
}

/// A single blocking gate makes the binary exit non-zero (so an agent loop and CI
/// agree the gate failed), carries its findings, and does not stop the other
/// reviewers from resolving.
#[test]
fn a_blocking_gate_makes_the_binary_exit_nonzero() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("ok-gate", "codex", "gate").behavior("pass"),
        Reviewer::new("bad-gate", "claude-code", "gate").behavior("block"),
    ]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1), "a blocked review must exit 1");
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    assert_eq!(gates.total, 2);
    assert_eq!(gates.passed, 1);
    assert_eq!(gates.blocked, 1);

    let (verdict, _summary, findings, _usage) = run.resolved("bad-gate");
    assert_eq!(verdict, Decision::Block);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, FindingKind::Blocking);
    assert_eq!(findings[0].path, "src/extra.rs");

    assert_eq!(run.resolved("ok-gate").0, Decision::Pass);
}

/// A gate whose backend crashes (non-zero exit) fails closed.
#[test]
fn a_crashing_gate_fails_closed() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("crash-gate", "codex", "gate").behavior("crash")
    ]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1));
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    assert_eq!(gates.blocked, 1);

    let (verdict, summary, findings, _usage) = run.resolved("crash-gate");
    assert_eq!(verdict, Decision::Block);
    assert!(
        summary.contains("did not produce a verdict"),
        "summary was {summary:?}"
    );
    assert!(!findings.is_empty());
}

/// A gate that hangs past its timeout fails closed, AND the hung child is actually
/// killed -- the runner's `kill_on_drop` is what makes a timeout real, so a child
/// that kept running (still using tools / burning tokens) would be a silent bug.
#[test]
fn a_timed_out_gate_fails_closed_and_kills_the_child() {
    let Some(fake) = tooling() else { return };

    let marker = tempfile::tempdir().unwrap();
    let marker_path = marker.path().join("agent-alive.txt");
    let marker_arg = marker_path.to_string_lossy().into_owned();

    let repo = TestRepo::new(&registry(&[Reviewer::new("slow-gate", "codex", "gate")
        .behavior("slow")
        .env("FAKE_SLEEP_MS", "1500")
        .timeout("300ms")]));

    let started = Instant::now();
    // FAKE_MARKER_FILE rides Bastion's environment and is inherited by the child;
    // the fake writes it only after its 1500ms sleep.
    let run = repo.review_base(fake, "main", &[("FAKE_MARKER_FILE", &marker_arg)]);
    let elapsed = started.elapsed();

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    let (verdict, summary, _findings, _usage) = run.resolved("slow-gate");
    assert_eq!(verdict, Decision::Block);
    assert!(summary.contains("timed out"), "summary was {summary:?}");

    // The 300ms timeout bounded the run far below the 1500ms sleep.
    assert!(
        elapsed < Duration::from_secs(15),
        "review took {elapsed:?}; the timeout did not bound the hung child"
    );

    // Wait well past the child's sleep; if it had survived the timeout it would
    // have written the marker by now.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !marker_path.exists(),
        "the timed-out agent child was not killed: it ran to completion and wrote the marker"
    );
}

/// Advisors fail open: an advisor that crashes -- and even one that returns a
/// clean block -- never holds up the merge.
#[test]
fn failing_or_blocking_advisors_never_block() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("the-gate", "codex", "gate").behavior("pass"),
        Reviewer::new("crashy-advisor", "claude-code", "advisor").behavior("crash"),
        Reviewer::new("blocky-advisor", "codex", "advisor").behavior("block"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    // Only the one gate is tallied; advisors never count toward the gate total.
    assert_eq!(gates.total, 1);
    assert_eq!(gates.passed, 1);
    assert_eq!(run.resolved_count(), 3);

    // The blocking advisor is clamped to pass and its blocking finding is recorded
    // as optional end to end, so its persisted row honors the universal invariant
    // (a pass carries no blocking finding) while the advice still surfaces.
    let (verdict, _summary, findings, _usage) = run.resolved("blocky-advisor");
    assert_eq!(verdict, Decision::Pass);
    assert!(!findings.is_empty(), "the advisory finding must surface");
    assert!(
        findings.iter().all(|f| f.kind == FindingKind::Optional),
        "an advisor's blocking finding must be recorded as optional, got: {findings:?}"
    );
}

/// An advisor that hangs past its timeout fails open (skipped), not closed.
#[test]
fn a_timed_out_advisor_is_skipped_not_blocked() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("the-gate", "codex", "gate").behavior("pass"),
        Reviewer::new("slow-advisor", "codex", "advisor")
            .behavior("slow")
            .env("FAKE_SLEEP_MS", "30000")
            .timeout("300ms"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 1);
}

/// The single-reprompt recovery path works end to end on every backend.
#[test]
fn the_reprompt_recovery_path_works_end_to_end() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("codex-recover", "codex", "gate").behavior("reprompt-recover"),
        Reviewer::new("claude-recover", "claude-code", "gate").behavior("reprompt-recover"),
        Reviewer::new("pi-recover", "pi", "gate").behavior("reprompt-recover"),
        Reviewer::new("grok-recover", "grok", "gate").behavior("reprompt-recover"),
        Reviewer::new("muse-recover", "muse", "gate").behavior("reprompt-recover"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.passed, 5);
    assert_eq!(run.resolved("codex-recover").0, Decision::Pass);
    assert_eq!(run.resolved("claude-recover").0, Decision::Pass);
    assert_eq!(run.resolved("pi-recover").0, Decision::Pass);
    assert_eq!(run.resolved("grok-recover").0, Decision::Pass);
    assert_eq!(run.resolved("muse-recover").0, Decision::Pass);
}

/// A gate that never produces a parseable verdict, even after the reprompt, fails
/// closed rather than being silently dropped.
#[test]
fn a_persistently_malformed_gate_fails_closed() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "garbage-gate",
        "claude-code",
        "gate",
    )
    .behavior("malformed")]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    let (verdict, summary, _, _) = run.resolved("garbage-gate");
    assert_eq!(verdict, Decision::Block);
    assert!(summary.contains("did not produce a verdict"));
}

/// An internally-inconsistent verdict (a `block` with no blocking finding) is
/// rejected, reprompted, and -- since it stays inconsistent -- fails closed. The
/// gate never trusts a self-contradictory verdict.
#[test]
fn an_inconsistent_verdict_gate_fails_closed() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "inconsistent-gate",
        "codex",
        "gate",
    )
    .behavior("inconsistent")]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("inconsistent-gate").0, Decision::Block);
}

/// The Pi backend runs a reviewer end to end through the real subprocess path: a
/// clean changeset passes, a flawed one blocks with its finding, all via the
/// `pi -p --mode json` protocol the fake agent emulates.
#[test]
fn the_pi_backend_runs_end_to_end() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("pi-pass", "pi", "gate").behavior("pass"),
        Reviewer::new("pi-block", "pi", "gate").behavior("block"),
    ]));
    let run = repo.review(fake);

    // One gate blocks, so the aggregate blocks and the process exits non-zero.
    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("pi-pass").0, Decision::Pass);
    let (verdict, _summary, findings, _usage) = run.resolved("pi-block");
    assert_eq!(verdict, Decision::Block);
    assert!(
        findings
            .iter()
            .any(|f| f.detail.contains("simulated blocking finding")),
        "expected the pi block finding to surface; findings: {findings:?}"
    );
}

/// The Grok Build backend runs a reviewer end to end through the real subprocess
/// path via the `grok -p --output-format json --json-schema` protocol the fake agent
/// emulates.
#[test]
fn the_grok_backend_runs_end_to_end() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("grok-pass", "grok", "gate").behavior("pass"),
        Reviewer::new("grok-block", "grok", "gate").behavior("block"),
    ]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("grok-pass").0, Decision::Pass);
    let (verdict, _summary, findings, _usage) = run.resolved("grok-block");
    assert_eq!(verdict, Decision::Block);
    assert!(
        findings
            .iter()
            .any(|f| f.detail.contains("simulated blocking finding")),
        "expected the grok block finding to surface; findings: {findings:?}"
    );
}

/// The Muse Code backend runs a reviewer end to end through the real subprocess
/// path via the `muse exec --json --yolo` protocol the fake agent emulates.
#[test]
fn the_muse_backend_runs_end_to_end() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("muse-pass", "muse", "gate").behavior("pass"),
        Reviewer::new("muse-block", "muse", "gate").behavior("block"),
    ]));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("muse-pass").0, Decision::Pass);
    let (verdict, _summary, findings, usage) = run.resolved("muse-block");
    assert_eq!(verdict, Decision::Block);
    assert!(
        findings
            .iter()
            .any(|f| f.detail.contains("simulated blocking finding")),
        "expected the muse block finding to surface; findings: {findings:?}"
    );
    // Muse's stream carries no usage, so none is reported.
    assert!(usage.is_none(), "muse reported usage: {usage:?}");
}
