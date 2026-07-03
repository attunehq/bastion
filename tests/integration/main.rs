//! End-to-end integration suite.
//!
//! Everything else in this crate is exercised by inline `#[cfg(test)]` modules
//! over real pure functions and the injectable backend seam. This file is the
//! missing top: it drives the *real compiled `bastion` binary* (via
//! `CARGO_BIN_EXE_bastion`) as a black box, each scenario in its own isolated
//! environment -- a throwaway `git` repository, a private `BASTION_DATA_DIR`, and
//! a compiled fake agent standing in for the heavyweight Claude Code / Codex / Pi
//! subprocesses the real backends shell out to.
//!
//! The fake agent ([`fakes::FAKE_AGENT_SRC`]) is compiled once with `rustc` and
//! pointed at through `BASTION_CLAUDE_BIN` / `BASTION_CODEX_BIN` / `BASTION_PI_BIN`,
//! so the binary takes
//! the genuine subprocess path: real spawn, real stdin/argv, real stdout capture, real
//! parse, real fail-closed/fail-open aggregation, real persistence. The fake reads
//! per-reviewer `env` (which Bastion propagates into the child) to choose how to
//! behave -- pass, block, return malformed output, crash, or hang.
//!
//! Crucially, the fake is also a *contract checker*: before it emits anything it
//! validates the invocation it received (the structured-output flags, the piped
//! prompt, the resume/session identifiers on a reprompt) and exits non-zero on any
//! mismatch. A backend that stopped passing the schema, dropped the prompt, or
//! botched a session resume would therefore turn these green tests red, even
//! though the assertions never look at the argv directly.
//!
//! Scenarios that need a toolchain we cannot guarantee (no `rustc`, no `git`)
//! detect-and-skip rather than fail locally, mirroring the existing backend tests;
//! in CI (where `CI` is set) the tools must be present so the suite cannot silently
//! become a no-op.

// This whole target is test code, but clippy's allow-unwrap-in-tests only
// exempts `#[test]` fns and `cfg(test)`, not the helper modules of an
// integration target; in a test, a panic IS the failure report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fakes;
mod fixtures;
mod github;

use std::time::{Duration, Instant};

use bastion::event::RunEvent;
use bastion::store::{self, RunSummary};
use bastion::verdict::{Decision, FindingKind, Money, Verdict};

use fakes::{container_tooling, ssh_tooling, tooling};
use fixtures::{
    Reviewer, TestRepo, event_kind, generate_ssh_key, parse_events, registry,
    registry_with_attestations, registry_with_defaults,
};
use github::{CapturedRequest, FakeGitHub};

// ---------------------------------------------------------------------------
// Core aggregation scenarios (jsonl).
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Reviewer selection (--reviewer) and incremental re-review (carry).
// ---------------------------------------------------------------------------

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
    let first = repo.review(fake);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    assert!(!first.carried("docs-gate"));
    assert!(!first.carried("src-gate"));

    // Second run, nothing changed: the user-level reviewer carries its pass;
    // the repo reviewer stays fresh because its prior seal is seam-tainted.
    let second = repo.review(fake);
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
        &["review", "--base", "main", "--fresh", "--format", "jsonl"],
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
    let after_edit = repo.review(fake);
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
    let first = repo.review(fake);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);

    // Naming both triggered reviewers: full coverage, so not partial, and the
    // named docs-gate executes fresh instead of carrying its eligible pass.
    let output = repo.run(
        fake,
        &[
            "review",
            "--base",
            "main",
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
    let first = repo.review_ci_backend_on_path(
        fake,
        "main",
        source.0,
        source.1,
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    github.finish();
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    assert!(
        !first.carried("ci-gate"),
        "the first run has no prior run to carry from"
    );

    // The seal really did record seams: false; otherwise the carry below could never
    // fire, and this test would be asserting nothing.
    let layout = repo.layout();
    let run_id = store::list_runs(&layout).unwrap()[0].run.clone();
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
    let second = repo.review_ci_backend_on_path(
        fake,
        "main",
        source.0,
        source.1,
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
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
    let third = repo.review_ci_backend_on_path(
        fake,
        "main",
        source.0,
        source.1,
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    github.finish();
    assert!(third.exited_zero(), "stderr:\n{}", third.stderr);
    assert!(
        !third.carried("ci-gate"),
        "an edited scoped file must re-execute the reviewer in CI"
    );
}

/// Model and effort reach each backend's argv end to end, resolved through the real
/// binary: an explicit per-reviewer value, a value inherited from the registry
/// `defaults` block, the Claude selectors, the Codex ones, and Pi's
/// `--model`/`--thinking`. The fake agent fails its contract (non-zero exit) if a
/// selector is missing, which would fail the gate closed; a clean `pass` across all
/// four proves the flags arrived.
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
        ],
    ));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 4);
    assert_eq!(gates.passed, 4);
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

/// The single-reprompt recovery path works end to end on all three backends.
#[test]
fn the_reprompt_recovery_path_works_end_to_end() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("codex-recover", "codex", "gate").behavior("reprompt-recover"),
        Reviewer::new("claude-recover", "claude-code", "gate").behavior("reprompt-recover"),
        Reviewer::new("pi-recover", "pi", "gate").behavior("reprompt-recover"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.passed, 3);
    assert_eq!(run.resolved("codex-recover").0, Decision::Pass);
    assert_eq!(run.resolved("claude-recover").0, Decision::Pass);
    assert_eq!(run.resolved("pi-recover").0, Decision::Pass);
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

// ---------------------------------------------------------------------------
// Containerized reviewers (the `runner` block).
// ---------------------------------------------------------------------------

/// A reviewer with a `runner` runs its backend inside the container engine, end to
/// end: dispatch takes the container branch, resolves the image through the engine
/// (a `dockerfile` build here), and the `docker run` line carries the in-container
/// `claude` invocation that the fake engine re-executes as the agent. A clean pass
/// still passes, proving the whole container wiring (image build, the `docker run`
/// argv, the in-container program name, env forwarding, output capture) is real.
#[test]
fn a_containerized_reviewer_runs_in_the_engine() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e", "claude-code", "gate")
        .behavior("pass")
        .dockerfile("Dockerfile")
        .network()]));
    // The Dockerfile only needs to exist: the fake engine's `build` is a no-op, but
    // image-tag derivation reads the file's bytes.
    std::fs::write(repo.path().join("Dockerfile"), "FROM scratch\n").unwrap();

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.completed().0, Decision::Pass);
    assert_eq!(run.resolved("e2e").0, Decision::Pass);
    // The engine actually ran, and ran the bare in-image `claude` (not a host path):
    // a regression to the native path would never reach the fake engine, so the log
    // would be missing entirely. The `dockerfile` source also builds before it runs:
    // `ensure_image` fires the `build` first, then `docker run` re-execs the agent. A
    // regression that stopped building (or ran before building) would reorder or drop
    // the `build` line.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(
        lines,
        ["build", "claude"],
        "expected a build before the run"
    );
}

/// `backend: any` resolves to Claude Code inside a container too: the container path
/// must pin the bare in-image `claude`, not a host-resolved path. A regression that
/// resolved `any` differently across the native and container paths would surface as a
/// different (or missing) in-container program here.
#[test]
fn a_containerized_any_backend_runs_claude_in_the_engine() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e-any", "any", "gate")
        .behavior("pass")
        .image("ghcr.io/acme/e2e:latest")
        .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("e2e-any").0, Decision::Pass);
    // `any` ran the bare in-image `claude` off the prebuilt image, with no build.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    assert_eq!(logged.lines().collect::<Vec<_>>(), ["claude"]);
}

/// A containerized gate does not launder a block: when the in-container agent
/// blocks, the gate blocks and the binary exits nonzero. Drives the Codex backend
/// off a prebuilt `image` source, so the prompt rides stdin through `docker run -i`
/// and no build step runs.
#[test]
fn a_containerized_gate_still_fails_closed_on_a_block() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e-block", "codex", "gate")
        .behavior("block")
        .image("ghcr.io/acme/e2e:latest")
        .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-block").0, Decision::Block);
    // The block is real: with the fake engine clearing inherited env, the agent saw
    // `FAKE_BEHAVIOR=block` only because Bastion forwarded the reviewer's `env` through
    // the `--env-file`. Had it not crossed, the agent would default to `pass` and this
    // would not block. The bare in-image `codex` ran, off the prebuilt image with no
    // build: an `image` source is used as-is, so the log holds the run and no `build`
    // line.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    assert_eq!(logged.lines().collect::<Vec<_>>(), ["codex"]);
}

/// A containerized gate whose agent never emits a parseable verdict fails closed. The
/// agent returns malformed output on every turn, so the backend reprompts once and
/// still cannot parse a verdict; a gate must then block, exactly as on the native
/// path. This pins the documented fail-closed behavior for containerized reviewers
/// whose first turn is malformed: each `docker run` is a separate `--rm` container, so
/// a real engine cannot resume first-turn session state, and the safe outcome is a
/// block, never a laundered pass. (The fake engine does not model cross-container
/// session loss, so this asserts the always-true fail-closed case, persistent
/// malformed output, rather than a recovery the fake would falsely allow.)
#[test]
fn a_containerized_malformed_gate_fails_closed() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-malformed",
        "codex",
        "gate",
    )
    .behavior("malformed")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
        ],
    );

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-malformed").0, Decision::Block);
}

/// A containerized reviewer's environment is isolated: the container does not
/// inherit Bastion's arbitrary environment, only the reviewer's literal `env` (and
/// the fixed credential allowlist). The fake engine clears inherited env, so this
/// asserts the boundary directly. The host sets `FAKE_SUMMARY=leaked-from-host` on
/// the Bastion process. One reviewer declares its own `FAKE_SUMMARY` and must see
/// that (reviewer env forwarded via `--env-file` reaches the container); a second
/// reviewer declares none and must fall back to the agent's default summary, proving
/// the host value did *not* leak across the boundary. Both observe the value through
/// the summary the agent echoes.
#[test]
fn a_containerized_reviewer_sees_only_forwarded_env() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[
        // Declares `FAKE_SUMMARY`: the forwarded value must cross.
        Reviewer::new("e2e-declared", "claude-code", "advisor")
            .behavior("pass")
            .env("FAKE_SUMMARY", "from-reviewer-env")
            .image("ghcr.io/acme/e2e:latest")
            .network(),
        // Declares no `FAKE_SUMMARY`: the host's value must not leak in, so the agent
        // falls back to its built-in default summary.
        Reviewer::new("e2e-isolated", "claude-code", "advisor")
            .behavior("pass")
            .image("ghcr.io/acme/e2e:latest")
            .network(),
    ]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            // A host-only variable neither reviewer forwards: it must not leak in.
            ("FAKE_SUMMARY", "leaked-from-host"),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    // The declared reviewer env crossed into the container.
    assert_eq!(run.resolved("e2e-declared").1, "from-reviewer-env");
    // The undeclared host variable did not: the agent used its default summary.
    let isolated = run.resolved("e2e-isolated").1;
    assert_eq!(isolated, "fake reviewer verdict");
    assert_ne!(isolated, "leaked-from-host");
}

/// A provider credential reaches the in-container agent without being listed in the
/// reviewer's `env`. `dispatch` wires `credential_passthrough()` into the container
/// runner, which forwards the fixed allowlist of provider credential names by `-e`.
/// Here `ANTHROPIC_API_KEY` is set on the Bastion process but *not* in the reviewer's
/// `env`; with the fake engine clearing inherited env, the agent can only see it
/// because the credential passthrough forwarded it. The agent echoes it into its
/// summary so the test can observe it crossed. This guards the dispatch wiring: an
/// empty credential list would leave the value absent and fail the assertion.
#[test]
fn a_provider_credential_crosses_into_the_container() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // `FAKE_ECHO_ENV` (a reviewer env) tells the agent to echo `ANTHROPIC_API_KEY`,
    // which is *not* listed in `env`: it can only arrive via credential passthrough.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-cred",
        "claude-code",
        "advisor",
    )
    .behavior("pass")
    .env("FAKE_ECHO_ENV", "ANTHROPIC_API_KEY")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            // A provider credential on the Bastion process, not in the reviewer env.
            ("ANTHROPIC_API_KEY", "cred-sentinel-xyz"),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let summary = run.resolved("e2e-cred").1;
    assert!(
        summary.contains("cred-sentinel-xyz"),
        "the provider credential did not reach the in-container agent; summary: {summary:?}"
    );
}

/// A hung containerized reviewer is timed out closed *and* its container is torn
/// down. `docker run --rm` only removes the container on a clean exit; when Bastion
/// times the reviewer out it kills the engine client, so the runner force-removes the
/// named container itself. The agent sleeps far past the timeout; the gate must still
/// block (the fail-closed guarantee the native timeout path also gives), and the fake
/// engine must have recorded the `rm -f` teardown.
#[test]
fn a_hung_containerized_reviewer_times_out_and_is_torn_down() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-hang",
        "claude-code",
        "gate",
    )
    .behavior("pass")
    .env("FAKE_SLEEP_MS", "5000")
    .timeout("300ms")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // Timed out: the gate fails closed.
    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-hang").0, Decision::Block);
    // The container teardown fired: the engine received `rm -f` for the run's
    // container, so a hung agent cannot keep running detached past the timeout.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.lines().any(|line| line.starts_with("rm:")),
        "expected a container teardown (`rm -f`); engine log:\n{logged}"
    );
}

/// A containerized reviewer that does not opt into `network: true` fails closed
/// before any container work. Bastion cannot scope a container's egress to the model
/// provider yet, so the default `network: false` reads as a restriction it cannot
/// enforce; rather than silently attach general egress, `ExecutionPlan::resolve`
/// rejects it. The gate blocks, the binary exits nonzero, and the engine is never
/// invoked (the failure precedes the image build and `docker run`), so no engine log
/// is written. This is the end-to-end face of the `plan.rs` unit test, proving the
/// resolve-time rejection becomes a real fail-closed block through the binary.
#[test]
fn a_containerized_reviewer_without_network_fails_closed() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // A `runner` block but no `capabilities.network: true`: unrunnable today.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-no-net",
        "claude-code",
        "gate",
    )
    .behavior("pass")
    .image("ghcr.io/acme/e2e:latest")]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // The gate fails closed: a container with the default `network: false` does not run.
    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-no-net").0, Decision::Block);
    // The failure precedes any container work: the engine was never invoked, so no
    // build and no `docker run` were logged.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.is_empty(),
        "the engine must not run for a reviewer rejected at resolve time; engine log:\n{logged}"
    );
}

/// The advisor side of the same resolve-time rejection: a containerized *advisor*
/// without `network: true` is failed *open*, not closed. The same
/// `ExecutionPlan::resolve` error that blocks a gate is, for an advisor, skipped and
/// kept out of the aggregate, so the run still passes and the binary exits zero. This
/// pins that the new preflight error follows the gate/advisor policy split rather than
/// wedging every containerized advisor, and (as on the gate path) never reaches the
/// engine.
#[test]
fn a_containerized_advisor_without_network_is_skipped() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // An advisor with a `runner` but no `capabilities.network: true`.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-no-net-advisor",
        "claude-code",
        "advisor",
    )
    .behavior("pass")
    .image("ghcr.io/acme/e2e:latest")]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // The advisor fails open: it is skipped, the aggregate still passes, exit zero.
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.completed().0, Decision::Pass);
    let resolved = run.resolved("e2e-no-net-advisor");
    assert_eq!(resolved.0, Decision::Pass);
    assert!(
        resolved.1.contains("skipped"),
        "a rejected advisor should be recorded as skipped, got: {:?}",
        resolved.1
    );
    // The engine was never invoked, exactly as on the gate path.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.is_empty(),
        "the engine must not run for an advisor rejected at resolve time; engine log:\n{logged}"
    );
}

// ---------------------------------------------------------------------------
// Accounting, env propagation, and concurrency.
// ---------------------------------------------------------------------------

/// Reported cost is summed across every reviewer that returned a verdict, across
/// all three backends, exactly; per-reviewer token usage also surfaces on the
/// stream, parsed from each backend's native shape.
#[test]
fn cost_and_token_usage_are_reported_across_backends() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("c1", "claude-code", "gate")
            .behavior("pass")
            .env("FAKE_COST_CENTS", "5")
            .env("FAKE_TOKENS_IN", "1200")
            .env("FAKE_TOKENS_OUT", "80")
            .env("FAKE_CACHE_READ", "600"),
        Reviewer::new("c2", "codex", "gate")
            .behavior("pass")
            .env("FAKE_COST_CENTS", "10")
            .env("FAKE_TOKENS_IN", "900")
            .env("FAKE_TOKENS_OUT", "40")
            .env("FAKE_CACHE_READ", "300"),
        Reviewer::new("c3", "codex", "advisor")
            .behavior("pass")
            .env("FAKE_COST_CENTS", "7"),
        Reviewer::new("c4", "pi", "gate")
            .behavior("pass")
            .env("FAKE_COST_CENTS", "13")
            .env("FAKE_TOKENS_IN", "2000")
            .env("FAKE_TOKENS_OUT", "150")
            .env("FAKE_CACHE_READ", "1000"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (_decision, _gates, cost) = run.completed();
    assert_eq!(cost, Money::from_cents(35));

    // Per-reviewer token usage is parsed from each backend's native shape, including
    // the cache-read figure each backend names differently (Claude's
    // `cache_read_input_tokens`, Codex's `cached_input_tokens`, Pi's `cacheRead`).
    let claude_usage = run.resolved("c1").3.expect("claude usage reported");
    assert_eq!(claude_usage.tokens_in, 1200);
    assert_eq!(claude_usage.tokens_out, 80);
    assert_eq!(claude_usage.cache_read, 600);
    let codex_usage = run.resolved("c2").3.expect("codex usage reported");
    assert_eq!(codex_usage.tokens_in, 900);
    assert_eq!(codex_usage.tokens_out, 40);
    assert_eq!(codex_usage.cache_read, 300);
    let pi_usage = run.resolved("c4").3.expect("pi usage reported");
    assert_eq!(pi_usage.tokens_in, 2000);
    assert_eq!(pi_usage.tokens_out, 150);
    assert_eq!(pi_usage.cache_read, 1000);
    assert_eq!(pi_usage.cost_usd, Money::from_cents(13));

    // The run.completed counter sums tokens across every reviewer (gates and the
    // advisor alike), mirroring how it sums cost. The advisor c3 reports the fake's
    // default 100 in / 10 out and no cache, so the totals are 1200+900+100+2000 in,
    // 80+40+10+150 out, and 600+300+0+1000 cache-read.
    let (tokens_in, tokens_out, cache_read, total_cost) = run.completed_usage();
    assert_eq!(tokens_in, 4200);
    assert_eq!(tokens_out, 280);
    assert_eq!(cache_read, 1900);
    assert_eq!(total_cost, Money::from_cents(35));
}

/// Reviewer `env` is propagated into the agent child and `${...}` inputs are
/// interpolated into the prompt before the agent sees it. The fake asserts the
/// interpolated marker arrived (on all three backends) and fails closed if it did
/// not, so a regression in propagation or interpolation turns this test red.
#[test]
fn env_propagation_and_input_interpolation_reach_the_agent() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("codex-interp", "codex", "gate")
            .behavior("pass")
            .input("preview_url", "http://preview.example/xyz")
            .prompt("Test against ${preview_url} thoroughly.")
            .env("FAKE_EXPECT_PROMPT_CONTAINS", "http://preview.example/xyz"),
        Reviewer::new("claude-interp", "claude-code", "gate")
            .behavior("pass")
            .input("ticket", "ABC-4242")
            .prompt("Review for ticket ${ticket} carefully.")
            .env("FAKE_EXPECT_PROMPT_CONTAINS", "ABC-4242"),
        Reviewer::new("pi-interp", "pi", "gate")
            .behavior("pass")
            .input("module", "auth/session")
            .prompt("Scrutinize the ${module} module closely.")
            .env("FAKE_EXPECT_PROMPT_CONTAINS", "auth/session"),
    ]));
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.passed, 3);
}

/// A reviewer's prior findings reach its prompt on the next run of the same branch,
/// through the real binary. The first run blocks with a finding; the second run, on a
/// new commit of the same branch, is configured so the fake agent fails its contract
/// unless the delivered prompt carries that prior finding's text. A clean pass on the
/// second run proves the prior-findings context was recalled from the run store and
/// rendered into the prompt end to end (recall + render + backend splice).
#[test]
fn prior_findings_reach_the_next_runs_prompt() {
    let Some(fake) = tooling() else { return };

    // Run 1: the reviewer blocks, persisting a finding (the fake's `block` behavior
    // emits `detail: simulated blocking finding`).
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("memory", "claude-code", "gate").behavior("block")
    ]));
    let first = repo.review(fake);
    let (decision, _gates, _cost) = first.completed();
    assert_eq!(decision, Decision::Block, "stderr:\n{}", first.stderr);

    // Reconfigure the same reviewer to pass, but assert (via the fake's contract
    // check) that the prompt now carries the prior finding's text. If the prior
    // findings did not reach the prompt, the fake exits non-zero and the gate fails
    // closed, so the run would block instead of pass.
    std::fs::write(
        repo.path().join(".bastion.yaml"),
        registry(&[Reviewer::new("memory", "claude-code", "gate")
            .env("FAKE_EXPECT_PROMPT_CONTAINS", "simulated blocking finding")]),
    )
    .unwrap();
    // Advance HEAD so the second run gets its own run id (recall looks back at *prior*
    // runs on the branch), then re-dirty so the reviewer still triggers.
    repo.commit_all("iterate");
    std::fs::write(
        repo.path().join("src/memory_probe.rs"),
        "pub fn probe() {}\n",
    )
    .unwrap();

    let second = repo.review(fake);
    assert!(second.exited_zero(), "stderr:\n{}", second.stderr);
    let (decision, gates, _cost) = second.completed();
    assert_eq!(
        decision,
        Decision::Pass,
        "the second run should pass, which it only can if the prior finding reached the \
         prompt (else the fake's contract check fails closed); stderr:\n{}",
        second.stderr
    );
    assert_eq!(gates.passed, 1);
    let (verdict, summary, _findings, _usage) = second.resolved("memory");
    assert_eq!(verdict, Decision::Pass);
    assert!(
        !summary.contains("did not produce a verdict"),
        "the reviewer should have produced a real verdict, not failed closed: {summary}"
    );
}

/// Reviewers run concurrently, not serially. Eight reviewers each sleep two
/// seconds; awaited concurrently the run finishes in a few seconds, far under the
/// ~16s a serial execution would take.
#[test]
fn reviewers_run_concurrently_not_serially() {
    let Some(fake) = tooling() else { return };

    let names = [
        "slow0", "slow1", "slow2", "slow3", "slow4", "slow5", "slow6", "slow7",
    ];
    let reviewers: Vec<Reviewer> = names
        .iter()
        .map(|name| {
            Reviewer::new(name, "codex", "gate")
                .behavior("slow")
                .env("FAKE_SLEEP_MS", "2000")
        })
        .collect();
    let repo = TestRepo::new(&registry(&reviewers));

    let started = Instant::now();
    let run = repo.review(fake);
    let elapsed = started.elapsed();

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.started_count(), 8);
    assert_eq!(run.resolved_count(), 8);
    assert_eq!(run.completed().1.passed, 8);
    // Serial would be ~16s; concurrent is ~2-3s. 10s catches serialization while
    // leaving generous headroom for a slow/loaded CI box.
    assert!(
        elapsed < Duration::from_secs(10),
        "8x2s reviewers took {elapsed:?}; they did not run concurrently"
    );
}

/// The headline stress scenario: a large, mixed registry across all three backends
/// and both modes, staging passes, blocks, crashes, timeouts, reprompts, and
/// advisory noise all at once. Everything must resolve, the aggregate must block,
/// and every reviewer's artifacts must land on disk.
#[test]
fn a_large_mixed_registry_resolves_every_reviewer_and_persists() {
    let Some(fake) = tooling() else { return };

    let reviewers = vec![
        Reviewer::new("g-claude-pass", "claude-code", "gate").behavior("pass"),
        Reviewer::new("g-codex-pass", "codex", "gate").behavior("pass"),
        Reviewer::new("g-pi-pass", "pi", "gate").behavior("pass"),
        Reviewer::new("g-any-pass", "any", "gate").behavior("pass"),
        Reviewer::new("g-codex-block", "codex", "gate").behavior("block"),
        Reviewer::new("g-claude-crash", "claude-code", "gate").behavior("crash"),
        Reviewer::new("g-codex-timeout", "codex", "gate")
            .behavior("slow")
            .env("FAKE_SLEEP_MS", "30000")
            .timeout("500ms"),
        Reviewer::new("g-claude-recover", "claude-code", "gate").behavior("reprompt-recover"),
        Reviewer::new("g-pi-recover", "pi", "gate").behavior("reprompt-recover"),
        Reviewer::new("a-codex-pass", "codex", "advisor").behavior("pass"),
        Reviewer::new("a-claude-block", "claude-code", "advisor").behavior("block"),
        Reviewer::new("a-pi-block", "pi", "advisor").behavior("block"),
        Reviewer::new("a-codex-crash", "codex", "advisor").behavior("crash"),
    ];
    let total = reviewers.len();
    let repo = TestRepo::new(&registry(&reviewers));
    let run = repo.review(fake);

    assert_eq!(run.code, Some(1), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    assert_eq!(gates.total, 9);
    assert_eq!(
        gates.blocked, 3,
        "block + crash + timeout should each block"
    );
    assert_eq!(gates.passed, 6);

    assert_eq!(run.started_count(), total);
    assert_eq!(run.resolved_count(), total);

    assert_eq!(run.resolved("g-claude-pass").0, Decision::Pass);
    assert_eq!(run.resolved("g-claude-recover").0, Decision::Pass);
    assert_eq!(run.resolved("g-pi-pass").0, Decision::Pass);
    assert_eq!(run.resolved("g-pi-recover").0, Decision::Pass);
    assert_eq!(run.resolved("g-codex-block").0, Decision::Block);

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = &runs[0].run;
    for reviewer in &reviewers {
        assert!(
            layout.verdict(run_id, reviewer.name).exists(),
            "missing verdict.json for {}",
            reviewer.name
        );
        assert!(
            layout.meta(run_id, reviewer.name).exists(),
            "missing meta.json for {}",
            reviewer.name
        );
    }
}

// ---------------------------------------------------------------------------
// Persistence and the read-back surface.
// ---------------------------------------------------------------------------

/// What a blocking run persists round-trips faithfully: the on-disk `run.jsonl` is
/// the full ordered event stream, `verdict.json`/`meta.json` carry the structured
/// result, and `show` replays the same blocking finding the live run emitted.
#[test]
fn a_blocking_run_persists_and_replays_faithfully() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "persisted",
        "claude-code",
        "gate",
    )
    .behavior("block")
    .env("FAKE_SUMMARY", "blocked for a real reason")]));
    let run = repo.review(fake);
    assert_eq!(run.code, Some(1));

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = &runs[0].run;

    // run.jsonl is the full event stream in order. With a single reviewer the
    // exact sequence is pinned, so a reordering (e.g. resolved before started)
    // would be caught, not just a missing-event regression.
    let persisted = store::read_run(&layout, run_id).unwrap();
    let sequence: Vec<&str> = persisted.iter().map(event_kind).collect();
    assert_eq!(
        sequence,
        [
            "run.started",
            "reviewer.started",
            "reviewer.resolved",
            "run.completed"
        ],
        "persisted run.jsonl is not the expected ordered stream"
    );

    // verdict.json carries the structured verdict.
    let verdict_json = std::fs::read_to_string(layout.verdict(run_id, "persisted")).unwrap();
    let verdict: Verdict = serde_json::from_str(&verdict_json).unwrap();
    assert_eq!(verdict.decision, Decision::Block);
    assert!(
        verdict
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::Blocking)
    );

    // meta.json carries the reviewer's backend/mode/trigger (ReviewerMeta is
    // private, so assert via the JSON shape).
    let meta_json = std::fs::read_to_string(layout.meta(run_id, "persisted")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta["backend"], "claude-code");
    assert_eq!(meta["mode"], "gate");
    assert_eq!(meta["trigger"][0], "src/**/*.rs");

    // `show <run>` replays the persisted finding, proving read-back equals the run.
    let show = repo.run(fake, &["show", run_id.as_str(), "--format", "jsonl"], &[]);
    assert!(show.status.success());
    let replay = parse_events(
        &String::from_utf8_lossy(&show.stdout),
        &String::from_utf8_lossy(&show.stderr),
    );
    let replayed_finding = replay.iter().any(|e| match e {
        RunEvent::ReviewerResolved { findings, .. } => findings
            .iter()
            .any(|f| f.detail.contains("simulated blocking finding")),
        _ => false,
    });
    assert!(replayed_finding, "show did not replay the blocking finding");
}

/// The read-back surface works over a real persisted run, including the explicit
/// run-id forms of `transcript` and `show`, and the deterministic `clean --keep 0`.
#[test]
fn the_read_back_commands_work_over_a_real_run() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("readback", "codex", "gate")
        .behavior("pass")
        .env("FAKE_SUMMARY", "a memorable summary")]));
    let review = repo.review(fake);
    assert!(review.exited_zero(), "stderr:\n{}", review.stderr);

    let run_id = store::list_runs(&repo.layout()).unwrap()[0].run.clone();

    // `runs --format jsonl` lists exactly that run.
    let runs_out = repo.run(fake, &["runs", "--format", "jsonl"], &[]);
    assert!(runs_out.status.success());
    let summaries: Vec<RunSummary> = String::from_utf8_lossy(&runs_out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("runs line is a RunSummary"))
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].run, run_id);
    assert_eq!(summaries[0].verdict, Some(Decision::Pass));

    // `show <run> --format jsonl` re-emits the resolved verdict and the completion.
    let show_out = repo.run(fake, &["show", run_id.as_str(), "--format", "jsonl"], &[]);
    assert!(show_out.status.success());
    let show_events = parse_events(
        &String::from_utf8_lossy(&show_out.stdout),
        &String::from_utf8_lossy(&show_out.stderr),
    );
    let resolved_ok = show_events.iter().any(|e| {
        matches!(
            e,
            RunEvent::ReviewerResolved { reviewer, summary, .. }
                if reviewer == "readback" && summary == "a memorable summary"
        )
    });
    assert!(
        resolved_ok,
        "show did not re-emit the readback reviewer with its summary"
    );
    assert!(
        show_events
            .iter()
            .any(|e| matches!(e, RunEvent::RunCompleted { .. }))
    );

    // `transcript <run> <reviewer>` (explicit two-positional form) prints the saved
    // session, which carries this run's specific summary.
    let transcript_out = repo.run(fake, &["transcript", run_id.as_str(), "readback"], &[]);
    assert!(transcript_out.status.success());
    let transcript = String::from_utf8_lossy(&transcript_out.stdout);
    assert!(
        transcript.contains("a memorable summary"),
        "transcript was {transcript:?}"
    );

    // `clean --keep 0` deterministically prunes every run.
    let clean_out = repo.run(fake, &["clean", "--keep", "0"], &[]);
    assert!(clean_out.status.success());
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// Multiple runs in one data directory: the `latest` pointer advances, `runs`
/// lists newest-first, and `clean --keep 1` prunes exactly the older run.
#[test]
fn multiple_runs_track_latest_and_prune_oldest() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("r", "codex", "gate").behavior("pass")
    ]));

    // First run, against the dirty working tree.
    let first = repo.review(fake);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    let first_id = store::list_runs(&repo.layout()).unwrap()[0].run.clone();

    // Advance HEAD (so the run id changes) and introduce a genuinely new change
    // so the second run actually routes its reviewer rather than being a
    // zero-match pass. Sleep first so the second run's directory mtime is strictly
    // later than the first's, making the newest-first ordering unambiguous even on
    // coarse (1s-resolution) filesystems.
    repo.commit_all("advance");
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(repo.path().join("src/run2.rs"), "pub fn run2() {}\n").unwrap();
    let second = repo.review(fake);
    assert!(second.exited_zero(), "stderr:\n{}", second.stderr);
    assert_eq!(
        second.completed().1.total,
        1,
        "the second run should have routed its gate, not been a zero-match pass"
    );

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs.len(), 2, "two distinct runs should be recorded");
    let newest_id = runs[0].run.clone();
    assert_ne!(
        newest_id, first_id,
        "the newest run should not be the first"
    );

    // `show` with no id resolves to the latest (newest) run.
    let show = repo.run(fake, &["show", "--format", "jsonl"], &[]);
    let show_events = parse_events(
        &String::from_utf8_lossy(&show.stdout),
        &String::from_utf8_lossy(&show.stderr),
    );
    assert!(show_events.iter().any(|e| e.run_id() == &newest_id));

    // `clean --keep 1` removes exactly the older run.
    let clean = repo.run(fake, &["clean", "--keep", "1"], &[]);
    assert!(clean.status.success());
    let clean_stdout = String::from_utf8_lossy(&clean.stdout);
    assert!(
        clean_stdout.contains("removed 1 run(s)"),
        "clean said: {clean_stdout}"
    );
    assert!(
        clean_stdout.contains(first_id.as_str()),
        "clean should name the older run"
    );
    let remaining = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].run, newest_id);
}

/// A changeset that triggers no reviewer is an honest, persisted pass.
#[test]
fn a_changeset_that_triggers_no_reviewer_is_a_clean_pass() {
    let Some(fake) = tooling() else { return };

    // This reviewer only triggers on docs; the dirty tree is all under src/.
    let repo = TestRepo::new(
        "reviewers:\n  - name: docs-only\n    trigger: [docs/**]\n    mode: gate\n    backend: codex\n    prompt: docs review\n",
    );
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 0);
    assert_eq!(run.resolved_count(), 0);

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].reviewers, 0);
}

// ---------------------------------------------------------------------------
// Human output, error paths, and the standalone subcommand.
// ---------------------------------------------------------------------------

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

/// A user-level registry merges with the repository's through the real binary: a
/// reviewer the user keeps in their config dir runs locally even when the repo
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

    let run = repo.review(fake);

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
    let run_id = store::list_runs(&repo.layout()).unwrap()[0].run.clone();

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

// ---------------------------------------------------------------------------
// `bastion github report`: drive the real binary against a fake GitHub.
// ---------------------------------------------------------------------------

#[test]
fn github_report_posts_a_comment_and_checks_for_a_blocked_run() {
    let Some(fake) = tooling() else { return };

    // A single blocking gate, so the run blocks and carries a located finding.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "tenant-isolation",
        "claude-code",
        "gate",
    )
    .behavior("block")]));

    // Persist a real run by driving `bastion review` through the fake agent.
    let review = repo.review(fake);
    assert!(!review.exited_zero(), "a blocking review exits non-zero");

    // Now report that run to a fake GitHub, exercising the real binary's argument
    // parsing, env-driven client, run resolution, and HTTP posting end to end.
    let github = FakeGitHub::start();
    let output = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    assert!(
        output.status.success(),
        "report should succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = github.finish();

    // The sticky comment is upserted: a GET to list, then a POST to create it
    // (the fake returns an empty list, so there is nothing to update in place).
    let list = requests
        .iter()
        .find(|r| r.method == "GET" && r.path.starts_with("/repos/acme/app/issues/7/comments"))
        .expect("a GET listing the PR comments");
    assert!(list.path.contains("per_page=100"));

    let comment = requests
        .iter()
        .find(|r| r.method == "POST" && r.path == "/repos/acme/app/issues/7/comments")
        .expect("a POST creating the sticky comment");
    // The comment carries the hidden marker (for future in-place updates) and the
    // reviewer's blocking finding, so a reader never has to open the artifact.
    assert!(
        comment.body.contains("bastion-report"),
        "marker missing: {}",
        comment.body
    );
    assert!(comment.body.contains("Bastion review"));
    assert!(comment.body.contains("simulated blocking finding"));
    // This repo never installed the bundled skills, so the report folds in the
    // freshness advisory (a GitHub `[!WARNING]` callout) pointing at `skills install`.
    assert!(
        comment.body.contains("[!WARNING]")
            && comment
                .body
                .contains("bundled agent skills are missing or out of date")
            && comment.body.contains("bastion skills install"),
        "the skills advisory is missing from the comment: {}",
        comment.body
    );
    // The fake stamps check runs with the shared `github-actions` app (as the
    // default GITHUB_TOKEN does), so the report detects the missing dedicated app
    // from the check-run response on its own and closes the comment with the nudge.
    assert!(
        comment.body.contains("bastion.jessica.black/github-app"),
        "report should detect the shared app and nudge toward a dedicated one: {}",
        comment.body
    );

    // One check run per reviewer plus the always-present aggregate `bastion` check.
    let checks: Vec<&CapturedRequest> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/repos/acme/app/check-runs")
        .collect();
    assert_eq!(checks.len(), 2, "expected reviewer + aggregate check runs");
    // The reviewer's gate blocked, so its check concludes failure against the head SHA...
    assert!(
        checks
            .iter()
            .any(|c| c.body.contains("bastion / tenant-isolation")
                && c.body.contains(r#""conclusion":"failure""#)
                && c.body.contains("deadcafe")),
        "a failing reviewer check run is missing: {checks:?}"
    );
    // ...and the aggregate reflects the blocked run.
    assert!(
        checks.iter().any(|c| c.body.contains(r#""name":"bastion""#)
            && c.body.contains(r#""conclusion":"failure""#)),
        "the aggregate bastion check is missing: {checks:?}"
    );

    // Once the skills are installed, re-reporting the same run through the command
    // handler no longer folds in the advisory: the handler assesses the working tree
    // itself, so an up-to-date repo produces a clean comment.
    assert!(repo.run(fake, &["skills", "install"], &[]).status.success());
    let github2 = FakeGitHub::start();
    let output2 = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[
            ("GITHUB_API_URL", github2.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    assert!(output2.status.success());
    let requests2 = github2.finish();
    let comment2 = requests2
        .iter()
        .find(|r| r.method == "POST" && r.path == "/repos/acme/app/issues/7/comments")
        .expect("a POST creating the sticky comment");
    assert!(
        !comment2.body.contains("[!WARNING]") && !comment2.body.contains("bundled agent skills"),
        "an up-to-date repo should not carry the skills advisory: {}",
        comment2.body
    );
}

#[test]
fn github_report_with_no_recorded_run_exits_zero_with_a_notice() {
    let Some(fake) = tooling() else { return };

    // A repo whose private data dir holds no runs: we never ran `bastion review`.
    let repo = TestRepo::new(&registry(&[
        Reviewer::new("unused", "claude-code", "gate").behavior("pass")
    ]));

    // Reporting with nothing persisted must not fail the step (it would pile a second
    // error on top of whatever upstream failure left no run). It prints a notice and
    // exits 0. No GitHub is contacted, so no fake server is needed.
    let output = repo.run(
        fake,
        &[
            "github", "report", "--repo", "acme/app", "--pr", "7", "--sha", "deadcafe",
        ],
        &[("GITHUB_TOKEN", "ghs-fake-token")],
    );
    assert!(
        output.status.success(),
        "missing-run report should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nothing to report"),
        "expected a 'nothing to report' notice; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Attestation: sealing, `bastion attest`, and CI replay/fallback.
// ---------------------------------------------------------------------------

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
    let run_id = &store::list_runs(&layout).unwrap()[0].run;
    let seal_path = layout.seal(run_id);
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
    let run_id = &store::list_runs(&layout).unwrap()[0].run;
    let seal_path = layout.seal(run_id);
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
    let run_id = &store::list_runs(&layout).unwrap()[0].run;
    std::fs::remove_file(layout.seal(run_id)).expect("seal.json existed");

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
/// HEAD falls back to a full run: the reviewers still execute for real, a
/// `run.attestation-fallback` event carries a reason naming the missing note, and
/// that reason is both in the persisted run and printed to stderr.
#[test]
fn ci_review_without_a_note_falls_back_and_says_why() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));
    // Commit the fixture's dirty tree: a dirty CI checkout falls back before any
    // note lookup, and this scenario pins the missing-note reason specifically.
    // The branch marks where the changeset started, since committing on main
    // would otherwise leave nothing to diff against.
    repo.branch("basemark");
    repo.commit_all("head");

    let github = FakeGitHub::start();
    let run = repo.review_ci(
        fake,
        "basemark",
        "acme/app",
        "1",
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
    github.finish();

    // The reviewer executed for real (no note to replay from).
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("ci-gate").0, Decision::Pass);
    assert_eq!(run.started_count(), 1);
    assert_eq!(run.resolved_count(), 1);

    // The fallback event is in the JSONL stream, names the missing note, and its
    // reason was also printed to stderr.
    let reason = run
        .attestation_fallback_reason()
        .expect("a run.attestation-fallback event in the jsonl stream");
    assert!(reason.contains("no attestation note"), "reason: {reason}");
    assert!(
        run.stderr.contains(reason),
        "the fallback reason should also be on stderr; stderr:\n{}",
        run.stderr
    );

    // ...and it persisted in run.jsonl, not just the live stream.
    let layout = repo.layout();
    let run_id = &store::list_runs(&layout).unwrap()[0].run;
    let persisted = store::read_run(&layout, run_id).unwrap();
    let persisted_reason = persisted.iter().find_map(|e| match e {
        RunEvent::AttestationFallback { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(persisted_reason.as_deref(), Some(reason));
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
    let run = repo.review_ci(
        fake,
        "main",
        "acme/app",
        "1",
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
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
    let run = repo.review_ci(
        fake,
        "main",
        "acme/app",
        "1",
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
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
    let run = repo.review_ci(
        fake,
        "basemark",
        "acme/app",
        "1",
        &[
            ("GITHUB_API_URL", github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
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
/// comment when the run it reports carried one.
#[test]
fn github_report_carries_the_fallback_notice() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry_with_attestations(&[Reviewer::new(
        "ci-gate", "codex", "gate",
    )
    .behavior("pass")]));

    // Drive the CI-path review so the run persists with a fallback event.
    let review_github = FakeGitHub::start();
    let review = repo.review_ci(
        fake,
        "main",
        "acme/app",
        "7",
        &[
            ("GITHUB_API_URL", review_github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
    );
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
        &[
            ("GITHUB_API_URL", report_github.url.as_str()),
            ("GITHUB_TOKEN", "ghs-fake-token"),
        ],
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
        comment.body.to_lowercase().contains("attest"),
        "the sticky comment should carry the fallback notice: {}",
        comment.body
    );
}
