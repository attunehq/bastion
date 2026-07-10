//! Accounting, env propagation, and concurrency.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;

use bastion::store;
use bastion::verdict::{Decision, Money};

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
/// seconds; the run's own recorded duration must land well under the serial
/// floor summed from the reviewers' recorded durations.
///
/// Both sides of the comparison come from the run's event stream, measured by
/// the same clock inside the binary, so machine load cannot flake the test: a
/// fixed wall-clock bar used here previously failed under parallel-test
/// subprocess-spawn contention, which stretches real elapsed time without any
/// serialization in the runner. Contention stretches each reviewer's recorded
/// duration too (the spawn wait is inside the per-task measurement), so the
/// serial floor rises at least as fast as the run duration does.
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

    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.started_count(), 8);
    assert_eq!(run.resolved_count(), 8);
    assert_eq!(run.completed().1.passed, 8);

    // Every reviewer really slept its 2s (the sleep is a floor, so each recorded
    // duration is too), which guarantees the serial floor is at least 16s.
    let durations = run.resolved_durations_ms();
    assert_eq!(durations.len(), 8);
    for (name, duration) in names.iter().zip(&durations) {
        assert!(
            *duration >= 2_000,
            "{name} recorded {duration}ms, under its 2s sleep; the recorded \
             durations are not trustworthy"
        );
    }

    // A serialized runner cannot beat the serial floor: its run duration is the
    // sum of the reviewer durations plus overhead. A concurrent runner finishes
    // in about the longest single reviewer, leaving at least 7 reviewers' worth
    // of overlap (14s and up). Demanding 6s of overlap therefore fails any
    // serialized execution by a wide margin while giving the concurrent path
    // wide headroom for persistence and emission overhead.
    let serial_floor: u64 = durations.iter().sum();
    let run_duration = run.completed_duration_ms();
    assert!(
        run_duration + 6_000 <= serial_floor,
        "the run took {run_duration}ms against a serial floor of {serial_floor}ms \
         (per-reviewer: {durations:?}); the reviewers did not run concurrently"
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

/// The spend safety net, end to end: a respawn storm of dead agent launches trips
/// the consecutive-failure breaker and aborts the whole run with a clear error,
/// instead of spending through every reviewer. This is the incident's signature
/// (an agent that launches, dies at zero tokens, and would otherwise keep being
/// retried) reproduced through the real dispatch path and the registry's own
/// `limits:` block.
#[test]
fn a_respawn_storm_of_dead_launches_trips_the_breaker_and_aborts() {
    let Some(fake) = tooling() else { return };

    // Five gate reviewers whose fake agent crashes (exits non-zero with no stdout,
    // the dead-spawn signature), with the breaker set to trip after three in a row.
    let names = ["storm0", "storm1", "storm2", "storm3", "storm4"];
    let reviewers: Vec<Reviewer> = names
        .iter()
        .map(|name| Reviewer::new(name, "codex", "gate").behavior("crash"))
        .collect();
    let repo = TestRepo::new(&registry_with_limits(
        &[("max_consecutive_failures", "3")],
        &reviewers,
    ));

    let run = repo.review(fake);

    assert_eq!(
        run.code,
        Some(1),
        "an aborted run exits non-zero; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("aborted this review"),
        "the abort must be surfaced loudly on stderr; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("produced no output"),
        "the abort must name the dead-spawn breaker; stderr:\n{}",
        run.stderr
    );

    // The run is still persisted with a blocking aggregate (every launched reviewer
    // failed closed), but it is never sealed: an aborted run must not be attestable.
    let (decision, _gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    let run_id = repo.latest_run_id();
    assert!(
        bastion::store::read_seal(&repo.layout(), &run_id)
            .unwrap()
            .is_none(),
        "an aborted run must never seal"
    );
}

/// A registry `limits:` block sets the caps: a run under a generous breaker never
/// trips on the same dead launches that abort it under a strict one, proving the
/// configured cap (not just the built-in default) is what takes effect.
#[test]
fn a_generous_limit_lets_a_run_that_a_strict_one_would_abort_complete() {
    let Some(fake) = tooling() else { return };

    // Two crashing gates: dead launches, but only two, so a breaker that tolerates
    // three consecutive failures never trips. The run completes as an ordinary
    // fail-closed block rather than an abort.
    let reviewers = vec![
        Reviewer::new("crash-a", "codex", "gate").behavior("crash"),
        Reviewer::new("crash-b", "codex", "gate").behavior("crash"),
    ];
    let repo = TestRepo::new(&registry_with_limits(
        &[("max_consecutive_failures", "3")],
        &reviewers,
    ));

    let run = repo.review(fake);

    // A block, not an abort: exit non-zero, but no abort error on stderr.
    assert_eq!(run.code, Some(1), "stderr:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("aborted this review"),
        "two dead launches must not trip a breaker set to three; stderr:\n{}",
        run.stderr
    );
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Block);
    assert_eq!(gates.total, 2);
    assert_eq!(gates.blocked, 2, "both crashing gates fail closed");
    assert_eq!(run.resolved_count(), 2, "every reviewer still resolved");
}
