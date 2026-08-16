//! Runner unit tests.

use super::*;
use crate::reviewer::{self as rev, Capabilities};
use crate::verdict::{Finding, FindingKind};

/// Serializes every test that touches the real seam environment (directly,
/// by mutating `BASTION_CLAUDE_BIN`, or indirectly, by sealing a run and so
/// reading it) against every other such test. `seams_active()` reads the
/// real process environment, which is global to the test binary, so two
/// scenarios racing here would otherwise leak one test's env var into
/// another's sealed `seams` flag. Every test that seals a run
/// (`ctx.seal = Some(...)`) acquires this at its own top, held for its
/// whole body. A `tokio::sync::Mutex` rather than `std::sync::Mutex`: each
/// guard is held across an `.await`, which clippy's `await_holding_lock`
/// correctly refuses for a blocking mutex.
static SEAM_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The six env vars [`crate::seal::seams_active`] reads, gathered in one
/// place so a test can force them all to a known state.
const SEAM_ENV_VARS: [&str; 6] = [
    crate::backend::claude_code::PROGRAM_ENV,
    crate::backend::codex::PROGRAM_ENV,
    crate::backend::pi::PROGRAM_ENV,
    crate::backend::grok::PROGRAM_ENV,
    crate::backend::muse::PROGRAM_ENV,
    crate::backend::container::ENGINE_ENV,
];

/// Forces every seam env var [`crate::seal::seams_active`] reads to a known
/// state (unset by default, or set via [`Self::set`]) for the guard's
/// lifetime, restoring each var's prior value on drop.
///
/// `seams_active()` reads the real, process-global environment, so any test
/// whose outcome depends on it (directly, by asserting `seal.seams`, or
/// indirectly, by sealing a run and reading it back) must not merely assume
/// the ambient environment is clean: a developer or CI sandbox that already
/// has `BASTION_CODEX_BIN` (or any of the other five) set would otherwise
/// flip `seams_active()` to `true` out from under the test, exactly the
/// failure this guard exists to prevent. Construct it only while already
/// holding [`SEAM_ENV_LOCK`]: mutating process env from a parallel test
/// would otherwise race.
struct SeamEnvGuard {
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl SeamEnvGuard {
    /// Clear every seam env var, remembering each prior value to restore on
    /// drop.
    fn cleared() -> Self {
        // Safety: the caller holds `SEAM_ENV_LOCK` for the guard's whole
        // lifetime, so no other test can observe or mutate these vars
        // concurrently.
        let prior = SEAM_ENV_VARS
            .iter()
            .map(|name| {
                let prior = std::env::var_os(name);
                unsafe {
                    std::env::remove_var(name);
                }
                (*name, prior)
            })
            .collect();
        Self { prior }
    }

    /// Clear every seam env var, then set exactly `name` to `value`. Used by
    /// the one test that asserts a *present* seam is recorded.
    fn cleared_except(name: &'static str, value: &str) -> Self {
        let guard = Self::cleared();
        // Safety: see `cleared`'s safety note; the lock is still held.
        unsafe {
            std::env::set_var(name, value);
        }
        guard
    }
}

impl Drop for SeamEnvGuard {
    fn drop(&mut self) {
        // Safety: see `cleared`'s safety note; the lock is still held for
        // the guard's whole lifetime, including this restoration.
        for (name, value) in &self.prior {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn reviewer(name: &str, mode: Mode) -> Reviewer {
    Reviewer {
        name: name.into(),
        trigger: crate::reviewer::Trigger::Paths(vec!["**".into()]),
        mode,
        backend: rev::Backend::ClaudeCode,
        model: None,
        effort: None,
        timeout: None,
        runner: None,
        env: Default::default(),
        capabilities: Capabilities::default(),
        inputs: Default::default(),
        attestation: None,
        prompt: "p".into(),
    }
}

fn agent_reviewer(name: &str, paths: &[&str]) -> Reviewer {
    let mut reviewer = reviewer(name, Mode::Gate);
    reviewer.trigger = rev::Trigger::Agent(rev::AgentTrigger {
        kind: rev::AgentTriggerKind::Agent,
        prompt: "Run only when the change affects this concern.".into(),
        backend: rev::Backend::Codex,
        model: Some(serde_yaml_ng::from_str("gpt-5.6-luna").unwrap()),
        effort: Some(serde_yaml_ng::from_str("high").unwrap()),
        timeout: None,
        paths: paths.iter().map(|path| (*path).to_string()).collect(),
    });
    reviewer
}

#[test]
fn persisting_a_skip_removes_artifacts_from_an_earlier_run_at_the_same_head() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::with_root(tmp.path().to_path_buf());
    let run = RunId("r-same-head".into());
    let reviewer = agent_reviewer("semantic", &[]);
    let reviewed = Resolved {
        reviewer: reviewer.clone(),
        decision: Decision::Pass,
        summary: "The full review passed.".into(),
        findings: vec![],
        usage: None,
        transcript: Some("full review transcript".into()),
        duration: Duration::from_secs(1),
        replayed: false,
        carried: false,
        scope_digest: None,
        skipped: false,
        trigger: None,
    };
    persist_reviewer(&layout, &run, &reviewed).unwrap();
    assert!(layout.verdict(&run, "semantic").exists());
    assert!(layout.transcript(&run, "semantic").exists());

    let skipped = Resolved {
        decision: Decision::Pass,
        summary: "The concern does not apply.".into(),
        transcript: None,
        skipped: true,
        trigger: Some(TriggerResolution {
            backend: rev::Backend::Codex,
            decision: TriggerDecision::Skip,
            reason: "The concern does not apply.".into(),
            usage: Some(Usage {
                tokens_in: 40,
                tokens_out: 5,
                cache_read: 10,
                cost_usd: Money::from_cents(1),
            }),
            duration_ms: 10,
        }),
        ..reviewed
    };
    persist_reviewer(&layout, &run, &skipped).unwrap();

    assert!(!layout.verdict(&run, "semantic").exists());
    assert!(!layout.transcript(&run, "semantic").exists());
    assert!(layout.meta(&run, "semantic").exists());
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.meta(&run, "semantic")).unwrap()).unwrap();
    assert_eq!(meta["backend"], "codex");
    assert_eq!(meta["usage"]["tokens_in"], 40);
}

#[tokio::test]
async fn an_any_trigger_records_the_concrete_backend_that_ran() {
    let mut gate = agent_reviewer("semantic", &[]);
    let Trigger::Agent(agent) = &mut gate.trigger else {
        panic!("agent_reviewer must build an agent trigger");
    };
    agent.backend = rev::Backend::Any;
    let reviewers = [&gate];
    let (_decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![(
            "semantic-trigger",
            Response::Outcome(pass("skip: the concern does not apply")),
        )]),
    )
    .await;

    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerSkipped {
            trigger: TriggerResolution {
                backend: rev::Backend::ClaudeCode,
                ..
            },
            ..
        }
    )));
}

fn ctx(reviewers: &[&Reviewer]) -> ExecContext {
    ExecContext {
        run: RunId("r-exec".into()),
        repo_root: PathBuf::from("."),
        branch: "feat".into(),
        base: "main".into(),
        merge_base: "deadbeef".into(),
        changed: u32::try_from(reviewers.len()).unwrap_or(0),
        reviewers: reviewers
            .iter()
            .map(|r| ReviewerRef {
                name: r.name.clone(),
                mode: r.mode,
            })
            .collect(),
        context: ReviewContext::default(),
        seal: None,
        dirty: false,
        replayed: Default::default(),
        attestation: None,
        digest_probe: None,
        partial: false,
        force: false,
        carried: Default::default(),
        scope_digests: Default::default(),
        attestation_fallback: None,
        limits: SpawnLimits::default(),
    }
}

fn pass(summary: &str) -> ReviewOutcome {
    ReviewOutcome {
        verdict: Verdict {
            decision: Decision::Pass,
            summary: summary.into(),
            findings: vec![],
        },
        usage: Some(Usage {
            tokens_in: 100,
            tokens_out: 10,
            cache_read: 40,
            cost_usd: Money::from_cents(5),
        }),
        transcript: Some("t".into()),
    }
}

fn block(summary: &str) -> ReviewOutcome {
    ReviewOutcome {
        verdict: Verdict {
            decision: Decision::Block,
            summary: summary.into(),
            findings: vec![Finding {
                kind: FindingKind::Blocking,
                path: "a.rs".into(),
                line_start: 1,
                line_end: 1,
                detail: "fix".into(),
            }],
        },
        usage: None,
        transcript: Some("t".into()),
    }
}

/// Drive `execute_with` with a per-reviewer outcome map keyed by name.
async fn run_scenario(
    reviewers: &[&Reviewer],
    responses: std::collections::HashMap<String, Response>,
) -> (Decision, Vec<RunEvent>, Layout) {
    run_scenario_with_ctx(reviewers, ctx(reviewers), responses).await
}

/// Like [`run_scenario`], but with a caller-supplied [`ExecContext`], so a
/// test can set `seal` (or anything else `ctx` defaults) without threading a
/// new parameter through every existing scenario call.
async fn run_scenario_with_ctx(
    reviewers: &[&Reviewer],
    ctx: ExecContext,
    responses: std::collections::HashMap<String, Response>,
) -> (Decision, Vec<RunEvent>, Layout) {
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::with_root(tmp.path().to_path_buf());
    // Keep the tempdir alive for the duration by leaking it into the layout's
    // lifetime via a Box; tests read the layout immediately after.
    std::mem::forget(tmp);

    let responses = std::sync::Arc::new(responses);
    let exec = move |req: OwnedRequest| -> ReviewFuture {
        let responses = responses.clone();
        Box::pin(async move {
            match responses.get(&req.reviewer.name).cloned() {
                Some(Response::Outcome(o)) => Ok(o),
                Some(Response::Error(msg)) => Err(color_eyre::eyre::eyre!(msg)),
                Some(Response::Hang(d)) => {
                    tokio::time::sleep(d).await;
                    Ok(pass("late"))
                }
                Some(Response::Panic) => panic!("reviewer task panic"),
                None => Ok(pass("default")),
            }
        })
    };

    let mut events = Vec::new();
    let decision = execute_with(
        reviewers,
        &ctx,
        &layout,
        &mut |e| events.push(e.clone()),
        std::sync::Arc::new(exec),
    )
    .await
    .expect("execute persists");
    (decision, events, layout)
}

#[derive(Clone)]
enum Response {
    Outcome(ReviewOutcome),
    Error(String),
    Hang(Duration),
    Panic,
}

fn responses(pairs: Vec<(&str, Response)>) -> std::collections::HashMap<String, Response> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

#[tokio::test]
async fn an_agent_trigger_can_skip_without_fabricating_a_review_verdict() {
    let gate = agent_reviewer("semantic", &[]);
    let reviewers = [&gate];
    let (decision, events, layout) = run_scenario(
        &reviewers,
        responses(vec![(
            "semantic-trigger",
            Response::Outcome(pass("skip: no persistence boundary changed")),
        )]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerSkipped { reviewer, trigger, .. }
            if reviewer == "semantic"
                && trigger.decision == TriggerDecision::Skip
                && trigger.reason == "no persistence boundary changed"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerResolved { reviewer, .. } if reviewer == "semantic"
    )));
    let completed = events.iter().find_map(|event| match event {
        RunEvent::RunCompleted {
            gates, tokens_in, ..
        } => Some((*gates, *tokens_in)),
        _ => None,
    });
    let (gates, tokens_in) = completed.expect("run completed");
    assert_eq!(
        (gates.total, gates.passed, gates.blocked, gates.skipped),
        (1, 0, 0, 1)
    );
    assert_eq!(tokens_in, 100, "trigger usage counts toward the run");
    assert!(!layout.verdict(&RunId("r-exec".into()), "semantic").exists());
    assert!(
        layout
            .transcript(&RunId("r-exec".into()), "semantic")
            .exists()
    );
}

#[tokio::test]
async fn an_agent_trigger_run_decision_executes_the_full_reviewer() {
    let gate = agent_reviewer("semantic", &[]);
    let reviewers = [&gate];
    let (decision, events, layout) = run_scenario(
        &reviewers,
        responses(vec![
            (
                "semantic-trigger",
                Response::Outcome(pass("run: persistence code changed")),
            ),
            ("semantic", Response::Outcome(pass("reviewed"))),
        ]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerResolved {
            reviewer,
            trigger: Some(trigger),
            ..
        } if reviewer == "semantic" && trigger.decision == TriggerDecision::Run
    )));
    let tokens_in = events.iter().find_map(|event| match event {
        RunEvent::RunCompleted { tokens_in, .. } => Some(*tokens_in),
        _ => None,
    });
    assert_eq!(tokens_in, Some(200));
    let transcript =
        std::fs::read_to_string(layout.transcript(&RunId("r-exec".into()), "semantic")).unwrap();
    assert!(transcript.contains("== agent trigger =="));
    assert!(transcript.contains("== reviewer =="));
}

#[tokio::test]
async fn a_malformed_agent_trigger_decision_runs_the_full_reviewer() {
    let gate = agent_reviewer("semantic", &[]);
    let reviewers = [&gate];
    let (decision, events, _) = run_scenario(
        &reviewers,
        responses(vec![
            ("semantic-trigger", Response::Outcome(pass("maybe"))),
            ("semantic", Response::Outcome(pass("reviewed"))),
        ]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerResolved {
            trigger: Some(trigger),
            ..
        } if trigger.decision == TriggerDecision::Run
            && trigger.reason.contains("malformed")
    )));
}

#[tokio::test]
async fn an_explicit_selection_bypasses_the_agent_trigger() {
    let gate = agent_reviewer("semantic", &[]);
    let reviewers = [&gate];
    let mut context = ctx(&reviewers);
    context.force = true;
    let (decision, events, _) = run_scenario_with_ctx(
        &reviewers,
        context,
        responses(vec![("semantic", Response::Outcome(pass("forced")))]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunEvent::ReviewerResolved { trigger: None, .. }))
    );
}

#[tokio::test]
async fn all_gates_pass_aggregates_to_pass() {
    let g1 = reviewer("g1", Mode::Gate);
    let g2 = reviewer("g2", Mode::Gate);
    let reviewers = [&g1, &g2];
    let (decision, events, layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("g1", Response::Outcome(pass("ok1"))),
            ("g2", Response::Outcome(pass("ok2"))),
        ]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    // started events came from the runner; completed says 2/2.
    let completed = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, verdict, .. } => Some((*gates, *verdict)),
            _ => None,
        })
        .unwrap();
    assert_eq!(completed.1, Decision::Pass);
    assert_eq!(completed.0.total, 2);
    assert_eq!(completed.0.passed, 2);

    // Persisted: run.jsonl, plus per-reviewer artifacts.
    let runs = crate::store::list_runs(&layout).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(layout.transcript(&RunId("r-exec".into()), "g1").exists());
    assert!(layout.verdict(&RunId("r-exec".into()), "g1").exists());
    assert!(layout.meta(&RunId("r-exec".into()), "g1").exists());
}

#[tokio::test]
async fn one_blocking_gate_blocks_the_run() {
    let g1 = reviewer("g1", Mode::Gate);
    let g2 = reviewer("g2", Mode::Gate);
    let reviewers = [&g1, &g2];
    let (decision, _events, _layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("g1", Response::Outcome(pass("ok"))),
            ("g2", Response::Outcome(block("bad"))),
        ]),
    )
    .await;
    assert_eq!(decision, Decision::Block);
}

#[tokio::test]
async fn a_failing_gate_fails_closed() {
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let (decision, events, layout) = run_scenario(
        &reviewers,
        responses(vec![("g1", Response::Error("backend exploded".into()))]),
    )
    .await;
    assert_eq!(decision, Decision::Block);
    // The resolve event carries a block with the failure reason.
    let resolved = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                verdict, summary, ..
            } => Some((*verdict, summary.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(resolved.0, Decision::Block);
    assert!(resolved.1.contains("did not produce a verdict"));
    // No transcript was saved for a crashed gate, but a verdict still was.
    assert!(layout.verdict(&RunId("r-exec".into()), "g1").exists());
    assert!(!layout.transcript(&RunId("r-exec".into()), "g1").exists());
}

#[tokio::test]
async fn a_failing_advisor_is_ignored() {
    let g1 = reviewer("g1", Mode::Gate);
    let a1 = reviewer("a1", Mode::Advisor);
    let reviewers = [&g1, &a1];
    let (decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("g1", Response::Outcome(pass("ok"))),
            ("a1", Response::Error("advisor died".into())),
        ]),
    )
    .await;
    // The failed advisor does not block.
    assert_eq!(decision, Decision::Pass);
    // The tally counts only the one gate.
    let gates = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, .. } => Some(*gates),
            _ => None,
        })
        .unwrap();
    assert_eq!(gates.total, 1);
}

#[tokio::test]
async fn an_advisor_block_does_not_block_the_run() {
    // Even a clean `block` verdict from an advisor is non-blocking, and its
    // blocking finding is recorded as optional so the persisted row satisfies
    // the universal pass-carries-no-blocking invariant while the advice survives.
    let a1 = reviewer("a1", Mode::Advisor);
    let reviewers = [&a1];
    let (decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![("a1", Response::Outcome(block("advisory concern")))]),
    )
    .await;
    assert_eq!(decision, Decision::Pass);

    // The advisor's finding (blocking as emitted) is downgraded to optional.
    let (verdict, findings) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                findings,
                ..
            } if reviewer == "a1" => Some((*verdict, findings.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(verdict, Decision::Pass);
    assert_eq!(findings.len(), 1, "the advisory finding must survive");
    assert!(
        findings.iter().all(|f| f.kind == FindingKind::Optional),
        "an advisor's blocking finding must be recorded as optional, got: {findings:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_gate_blocks() {
    let mut g1 = reviewer("g1", Mode::Gate);
    g1.timeout = Some(Duration::from_secs(1));
    let reviewers = [&g1];
    let (decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![("g1", Response::Hang(Duration::from_secs(60)))]),
    )
    .await;
    assert_eq!(decision, Decision::Block);
    let summary = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .unwrap();
    assert!(summary.contains("timed out"));
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_advisor_is_ignored() {
    let mut a1 = reviewer("a1", Mode::Advisor);
    a1.timeout = Some(Duration::from_secs(1));
    let reviewers = [&a1];
    let (decision, _events, _layout) = run_scenario(
        &reviewers,
        responses(vec![("a1", Response::Hang(Duration::from_secs(60)))]),
    )
    .await;
    assert_eq!(decision, Decision::Pass);
}

#[tokio::test(start_paused = true)]
async fn finished_events_cover_every_task_outcome() {
    let completed = reviewer("completed", Mode::Gate);
    let failed = reviewer("failed", Mode::Gate);
    let mut timed_out = reviewer("timed-out", Mode::Gate);
    timed_out.timeout = Some(Duration::from_secs(1));
    let crashed = reviewer("crashed", Mode::Gate);
    let reviewers = [&completed, &failed, &timed_out, &crashed];

    let (_decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("completed", Response::Outcome(pass("ok"))),
            ("failed", Response::Error("backend failed".into())),
            ("timed-out", Response::Hang(Duration::from_secs(60))),
            ("crashed", Response::Panic),
        ]),
    )
    .await;

    let finished: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ReviewerFinished { reviewer, .. } => Some(reviewer.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 4);
    assert!(finished.contains("completed"));
    assert!(finished.contains("failed"));
    assert!(finished.contains("timed-out"));
    assert!(finished.contains("crashed"));
}

#[tokio::test]
async fn finished_event_arrives_while_another_reviewer_is_still_running() {
    let fast = reviewer("fast", Mode::Gate);
    let slow = reviewer("slow", Mode::Gate);
    let reviewers = [&fast, &slow];
    let context = ctx(&reviewers);
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::with_root(tmp.path().to_path_buf());
    let release_slow = std::sync::Arc::new(tokio::sync::Notify::new());
    let exec_release = release_slow.clone();
    let exec: std::sync::Arc<ReviewFn> = std::sync::Arc::new(move |req: OwnedRequest| {
        let release = exec_release.clone();
        Box::pin(async move {
            if req.reviewer.name == "slow" {
                release.notified().await;
            }
            Ok(pass("ok"))
        })
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut emit = move |event: &RunEvent| {
        tx.send(event.clone()).expect("receiver remains open");
    };
    let run = execute_with(&reviewers, &context, &layout, &mut emit, exec);
    tokio::pin!(run);

    let progress = loop {
        tokio::select! {
            result = &mut run => panic!("run completed before fast progress event: {result:?}"),
            event = rx.recv() => {
                let event = event.expect("event stream remains open");
                if matches!(&event, RunEvent::ReviewerFinished { reviewer, .. } if reviewer == "fast") {
                    break event;
                }
            }
        }
    };
    assert!(matches!(
        progress,
        RunEvent::ReviewerFinished {
            completed: 1,
            total: 2,
            ..
        }
    ));

    release_slow.notify_one();
    assert_eq!(run.await.expect("run completes"), Decision::Pass);
}

#[tokio::test]
async fn persisted_run_jsonl_is_the_full_event_stream() {
    // run.jsonl must contain the started events too, not just resolve/completed,
    // so a replay sees the same sequence the live stream emitted.
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let (_decision, events, layout) = run_scenario(
        &reviewers,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let persisted = crate::store::read_run(&layout, &RunId("r-exec".into())).unwrap();
    assert_eq!(&persisted[1..], events.as_slice());
    assert!(
        matches!(persisted.first(), Some(RunEvent::RunStarted { .. })),
        "stream must open with run.started"
    );
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::ReviewerStarted { .. })),
        "stream must include reviewer.started"
    );
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::ReviewerFinished { .. })),
        "stream must include reviewer.finished"
    );
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::ReviewerResolved { .. })),
        "stream must include reviewer.resolved"
    );
    assert!(
        matches!(persisted.last(), Some(RunEvent::RunCompleted { .. })),
        "stream must close with run.completed"
    );
}

#[tokio::test]
async fn cost_and_tokens_are_summed_across_reviewers() {
    let g1 = reviewer("g1", Mode::Gate);
    let g2 = reviewer("g2", Mode::Gate);
    let reviewers = [&g1, &g2];
    // Each `pass` reports 100 in / 10 out / 40 cached tokens and 5 cents.
    let (_decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("g1", Response::Outcome(pass("a"))),
            ("g2", Response::Outcome(pass("b"))),
        ]),
    )
    .await;
    let (tokens_in, tokens_out, cache_read, cost) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted {
                tokens_in,
                tokens_out,
                cache_read,
                cost_usd,
                ..
            } => Some((*tokens_in, *tokens_out, *cache_read, *cost_usd)),
            _ => None,
        })
        .unwrap();
    assert_eq!(cost, Money::from_cents(10));
    assert_eq!(tokens_in, 200);
    assert_eq!(tokens_out, 20);
    assert_eq!(cache_read, 80);
}

#[tokio::test]
async fn a_reviewer_with_no_usage_contributes_zero_to_the_totals() {
    // A gate that blocks reports no usage (see `block`); a passing gate does.
    // The aggregate should reflect only the reviewer that reported usage, never
    // panic or double-count the missing one.
    let g1 = reviewer("g1", Mode::Gate);
    let g2 = reviewer("g2", Mode::Gate);
    let reviewers = [&g1, &g2];
    let (_decision, events, _layout) = run_scenario(
        &reviewers,
        responses(vec![
            ("g1", Response::Outcome(pass("a"))), // 100/10/40 tokens, 5 cents
            ("g2", Response::Outcome(block("b"))), // no usage reported
        ]),
    )
    .await;
    let (tokens_in, tokens_out, cache_read, cost) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted {
                tokens_in,
                tokens_out,
                cache_read,
                cost_usd,
                ..
            } => Some((*tokens_in, *tokens_out, *cache_read, *cost_usd)),
            _ => None,
        })
        .unwrap();
    assert_eq!(tokens_in, 100);
    assert_eq!(tokens_out, 10);
    assert_eq!(cache_read, 40);
    assert_eq!(cost, Money::from_cents(5));
}

fn seal_bindings(repo_reviewers: &[&str]) -> SealBindings {
    SealBindings {
        head_tree: "head-tree".into(),
        base_tree: "base-tree".into(),
        patch_id: "patch-id".into(),
        config_hash: "config-hash".into(),
        repo_reviewers: repo_reviewers.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[tokio::test]
async fn a_run_with_seal_bindings_produces_a_verifiable_seal_on_disk() {
    // Sealing reads the real process environment (`seams_active()`); hold
    // the lock and force every seam var to unset so the assertion below is
    // deterministic regardless of what the ambient environment carries (see
    // `SeamEnvGuard`'s doc comment).
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.seal = Some(seal_bindings(&["g1"]));

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run)
        .unwrap()
        .expect("a sealed run persists a seal.json");
    assert_eq!(seal.reviewers, vec!["g1".to_string()]);
    assert_eq!(seal.head_tree, "head-tree");
    assert!(!seal.seams, "no seam env var was set for this test");

    // The seal must verify against the run's own persisted resolved events,
    // using the same embedded secret the runner sealed with (sealer and
    // verifier are the same test binary).
    let events = crate::store::read_run(&layout, &run).unwrap();
    let resolved: Vec<serde_json::Value> = events
        .iter()
        .filter(|e| matches!(e, RunEvent::ReviewerResolved { reviewer, .. } if reviewer == "g1"))
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert!(crate::seal::verify(
        crate::seal::embedded_secret(),
        &seal,
        &resolved
    ));
}

#[tokio::test]
async fn an_agent_skip_is_a_sealed_terminal_outcome() {
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let gate = agent_reviewer("semantic", &[]);
    let reviewers = [&gate];
    let mut context = ctx(&reviewers);
    context.seal = Some(seal_bindings(&["semantic"]));

    let (_, _, layout) = run_scenario_with_ctx(
        &reviewers,
        context,
        responses(vec![(
            "semantic-trigger",
            Response::Outcome(pass("skip: no relevant boundary changed")),
        )]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run)
        .unwrap()
        .expect("semantic skip is sealed");
    let events = crate::store::read_run(&layout, &run).unwrap();
    let terminal: Vec<serde_json::Value> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RunEvent::ReviewerSkipped { reviewer, .. } if reviewer == "semantic"
            )
        })
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    assert_eq!(seal.reviewers, ["semantic"]);
    assert!(crate::seal::verify(
        crate::seal::embedded_secret(),
        &seal,
        &terminal,
    ));
}

#[tokio::test]
async fn perturbing_a_persisted_resolved_event_breaks_seal_verification() {
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.seal = Some(seal_bindings(&["g1"]));

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run).unwrap().unwrap();
    let events = crate::store::read_run(&layout, &run).unwrap();
    let mut resolved: Vec<serde_json::Value> = events
        .iter()
        .filter(|e| matches!(e, RunEvent::ReviewerResolved { reviewer, .. } if reviewer == "g1"))
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();

    // Tamper with the persisted event's summary before re-verifying.
    resolved[0]["summary"] = serde_json::Value::String("a different summary".into());
    assert!(!crate::seal::verify(
        crate::seal::embedded_secret(),
        &seal,
        &resolved
    ));
}

#[tokio::test]
async fn a_partial_run_is_marked_partial_and_never_sealed() {
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.partial = true;
    // Bindings are present and would seal a full run; `partial` must win.
    ctx.seal = Some(seal_bindings(&["g1"]));

    let (decision, events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    let run = RunId("r-exec".into());
    assert_eq!(
        crate::store::read_seal(&layout, &run).unwrap(),
        None,
        "a partial run must never seal, or a filtered green becomes attestable"
    );
    // Both the live stream and the persisted run carry the partial flag.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::RunCompleted { partial, .. } if *partial))
    );
    let persisted = crate::store::read_run(&layout, &run).unwrap();
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::RunStarted { partial, .. } if *partial))
    );
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::RunCompleted { partial, .. } if *partial))
    );
}

/// A `Carried` entry for `name` whose prior event resolved to `verdict`
/// with the given digest.
fn carried_entry(name: &str, verdict: Decision, digest: &str) -> crate::carry::Carried {
    crate::carry::Carried {
        reviewer: reviewer(name, Mode::Gate),
        event: RunEvent::ReviewerResolved {
            run: RunId("r-prior".into()),
            reviewer: name.into(),
            verdict,
            summary: "unchanged since the previous run".into(),
            findings: vec![],
            usage: Some(Usage {
                tokens_in: 999,
                tokens_out: 99,
                cache_read: 0,
                cost_usd: Money::from_cents(50),
            }),
            duration_ms: 1234,
            has_transcript: false,
            replayed: false,
            carried: false,
            scope_digest: Some(digest.into()),
            trigger: None,
        },
    }
}

#[tokio::test]
async fn a_carried_gate_whose_digest_went_stale_mid_run_fails_closed() {
    // The carry decision was made against a pre-run digest; if the scoped
    // content changes while the run executes, the carried pass no longer
    // describes the tree this run reports on, so it must not tally as a
    // pass. The stale stamp here stands in for that mid-run change.
    let (tmp, merge_base, _changed) = probe_repo();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.repo_root = tmp.path().to_path_buf();
    ctx.carried.insert(
        "g2".into(),
        carried_entry("g2", Decision::Pass, "stale-plan-time-digest"),
    );
    ctx.digest_probe = Some(DigestProbe { merge_base });

    let (decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    assert_eq!(decision, Decision::Block, "a stale carry must fail closed");
    let g2 = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer, verdict, ..
            } if reviewer == "g2" => Some(*verdict),
            _ => None,
        })
        .unwrap();
    assert_eq!(g2, Decision::Block);
}

#[tokio::test]
async fn a_carried_advisor_downgrades_a_blocking_finding_to_optional() {
    // A prior advisor row from an older release can carry a clamped pass with a
    // blocking finding. Carrying it must preserve the advice (not fail it as
    // inconsistent) and normalize the finding to optional, so the re-persisted
    // row satisfies the universal invariant.
    let a1 = reviewer("a1", Mode::Advisor);
    let mut entry = carried_entry("a2", Decision::Pass, "digest-1");
    entry.reviewer = reviewer("a2", Mode::Advisor);
    if let RunEvent::ReviewerResolved { findings, .. } = &mut entry.event {
        findings.push(crate::verdict::Finding {
            kind: crate::verdict::FindingKind::Blocking,
            path: "src/a.rs".into(),
            line_start: 1,
            line_end: 1,
            detail: "still worth fixing".into(),
        });
    }
    let reviewers = [&a1];
    let mut ctx = ctx(&reviewers);
    ctx.carried.insert("a2".into(), entry);

    let (decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("a1", Response::Outcome(pass("ok")))]),
    )
    .await;

    assert_eq!(decision, Decision::Pass, "advisors never gate");
    let (verdict, findings) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                findings,
                ..
            } if reviewer == "a2" => Some((*verdict, findings.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(verdict, Decision::Pass);
    assert_eq!(findings.len(), 1, "the advisory finding must survive carry");
    assert!(
        findings.iter().all(|f| f.kind == FindingKind::Optional),
        "the carried advisor's blocking finding must be normalized to optional, got: {findings:?}"
    );
}

#[tokio::test]
async fn a_carried_reviewer_counts_toward_the_gate_without_executing() {
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.carried
        .insert("g2".into(), carried_entry("g2", Decision::Pass, "digest-1"));
    ctx.reviewers.push(ReviewerRef {
        name: "g2".into(),
        mode: Mode::Gate,
    });

    let (decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    assert_eq!(decision, Decision::Pass);
    // The carried reviewer is announced and resolved like the rest.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ReviewerStarted { reviewer, .. } if reviewer == "g2"))
    );
    let (carried_flag, usage, digest) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer,
                carried,
                usage,
                scope_digest,
                ..
            } if reviewer == "g2" => Some((*carried, *usage, scope_digest.clone())),
            _ => None,
        })
        .expect("the carried reviewer resolves");
    assert!(carried_flag);
    assert_eq!(usage, None, "no tokens were spent this run");
    assert_eq!(
        digest.as_deref(),
        Some("digest-1"),
        "the digest is re-stamped so the chain continues next run"
    );
    // Both gates count in the tally.
    let gates = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, .. } => Some(*gates),
            _ => None,
        })
        .unwrap();
    assert_eq!(gates.total, 2);
    assert_eq!(gates.passed, 2);
}

#[tokio::test]
async fn a_carried_repo_reviewer_is_sealed_into_the_new_run() {
    // A carried verdict participates in the new run's seal exactly like a
    // fresh one: the carry planner already verified the prior seal, so the
    // new seal extends the chain rather than dropping the reviewer.
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.seal = Some(seal_bindings(&["g1", "g2"]));
    ctx.carried
        .insert("g2".into(), carried_entry("g2", Decision::Pass, "digest-1"));

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let seal = crate::store::read_seal(&layout, &RunId("r-exec".into()))
        .unwrap()
        .expect("a full run with bindings seals");
    assert_eq!(seal.reviewers, vec!["g1".to_string(), "g2".to_string()]);
}

#[tokio::test]
async fn a_fresh_pass_is_stamped_with_its_scope_digest() {
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.scope_digests.insert("g1".into(), "digest-fresh".into());

    let (_decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let digest = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved { scope_digest, .. } => Some(scope_digest.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(digest.as_deref(), Some("digest-fresh"));
}

/// A throwaway repo for the digest-probe tests: a committed base branch
/// plus one changed file on top, so `carry::scope_digest` has real git
/// state to recompute against.
fn probe_repo() -> (tempfile::TempDir, String, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@bastion.dev",
                "-c",
                "user.name=T",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init"]);
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "base"]);
    git(&["branch", "base"]);
    std::fs::write(dir.join("a.rs"), "fn a() { /* edit */ }\n").unwrap();
    let merge_base = crate::git::merge_base(dir, "base").unwrap();
    let changed = crate::git::changed_files(dir, &merge_base).unwrap();
    (tmp, merge_base, changed)
}

#[tokio::test]
async fn the_digest_probe_keeps_a_digest_the_tree_still_matches() {
    let (tmp, merge_base, changed) = probe_repo();
    let g1 = reviewer("g1", Mode::Gate);
    let real = crate::carry::scope_digest(tmp.path(), &merge_base, &g1, &changed).unwrap();

    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.repo_root = tmp.path().to_path_buf();
    ctx.scope_digests.insert("g1".into(), real.clone());
    ctx.digest_probe = Some(DigestProbe { merge_base });

    let (_decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let digest = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved { scope_digest, .. } => Some(scope_digest.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(digest.as_deref(), Some(real.as_str()));
}

#[tokio::test]
async fn the_digest_probe_drops_a_digest_the_tree_no_longer_matches() {
    // A pre-run digest that no longer matches the tree (here: a stale
    // stamp standing in for a tree that changed while the reviewer ran)
    // must not survive onto the resolved event, or a later run could
    // carry a verdict about content the reviewer never judged.
    let (tmp, merge_base, _changed) = probe_repo();
    let g1 = reviewer("g1", Mode::Gate);

    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.repo_root = tmp.path().to_path_buf();
    ctx.scope_digests
        .insert("g1".into(), "stale-pre-run-digest".into());
    ctx.digest_probe = Some(DigestProbe { merge_base });

    let (_decision, events, _layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let digest = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved { scope_digest, .. } => Some(scope_digest.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(digest, None, "a stale digest must be dropped");
}

#[tokio::test]
async fn an_inconsistent_carried_event_fails_closed() {
    // A pass carrying a blocking finding is not a coherent pass; a carried
    // event gets the same consistency check a replayed one does, so a
    // hand-edited (user-level, unsealed) store cannot smuggle one in.
    let reviewers: [&Reviewer; 0] = [];
    let mut ctx = ctx(&reviewers);
    let mut entry = carried_entry("g1", Decision::Pass, "d");
    if let RunEvent::ReviewerResolved { findings, .. } = &mut entry.event {
        findings.push(Finding {
            kind: FindingKind::Blocking,
            path: "a.rs".into(),
            line_start: 1,
            line_end: 1,
            detail: "contradiction".into(),
        });
    }
    ctx.carried.insert("g1".into(), entry);
    ctx.reviewers.push(ReviewerRef {
        name: "g1".into(),
        mode: Mode::Gate,
    });

    let (decision, _events, _layout) =
        run_scenario_with_ctx(&reviewers, ctx, responses(vec![])).await;
    assert_eq!(decision, Decision::Block);
}

#[tokio::test]
async fn a_reviewer_outside_the_repo_set_is_excluded_from_the_seal() {
    // A user-level-only reviewer (not in `repo_reviewers`) must not be sealed:
    // its events are excluded, and if it is the only reviewer that ran, no
    // seal is written at all.
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let a1 = reviewer("a1", Mode::Gate);
    let reviewers = [&a1];
    let mut ctx = ctx(&reviewers);
    ctx.seal = Some(seal_bindings(&["some-other-repo-reviewer"]));

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("a1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    assert_eq!(crate::store::read_seal(&layout, &run).unwrap(), None);
}

#[tokio::test]
async fn no_seal_bindings_leaves_the_run_unsealed() {
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let (_decision, _events, layout) = run_scenario(
        &reviewers,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;
    let run = RunId("r-exec".into());
    assert_eq!(crate::store::read_seal(&layout, &run).unwrap(), None);
}

#[tokio::test]
async fn seams_active_is_recorded_on_the_seal_when_a_backend_seam_env_is_set() {
    // `seams_active()` reads real process env vars, which are process-global
    // and unsafe to mutate from parallel tests; this test is the one place in
    // the suite allowed to set `BASTION_CLAUDE_BIN` for that reason. It holds
    // `SEAM_ENV_LOCK` for the whole window and forces every *other* seam var
    // unset via `SeamEnvGuard`, so the outcome depends only on the one var
    // this test sets, never on whatever the ambient environment carries.
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard =
        SeamEnvGuard::cleared_except(crate::backend::claude_code::PROGRAM_ENV, "/bin/true");

    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut ctx = ctx(&reviewers);
    ctx.seal = Some(seal_bindings(&["g1"]));

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run).unwrap().unwrap();
    assert!(
        seal.seams,
        "the active backend seam must be recorded on the seal"
    );
}

/// Run a couple of throwaway `git` commands against `cwd`, panicking on
/// failure. Mirrors `git::tests::git`, kept local so this module's seal
/// tests do not need to reach into `git`'s private test helpers.
fn git(cwd: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git must be on PATH for this test");
    assert!(status.success(), "git {args:?} failed");
}

/// A throwaway, committed, clean git repository for seal tests that need
/// `ExecContext.repo_root` to point at a real `is_dirty`-able tree instead of
/// the placeholder `"."` most scenarios use.
fn clean_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "grace.hopper@example.com"]);
    git(dir, &["config", "user.name", "Grace Hopper"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-m", "base"]);
    tmp
}

#[tokio::test]
async fn a_clean_working_tree_seals_dirty_false() {
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let repo = clean_repo();
    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut execution = ctx(&reviewers);
    execution.repo_root = repo.path().to_path_buf();
    execution.seal = Some(seal_bindings(&["g1"]));
    execution.dirty = false;

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        execution,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run).unwrap().unwrap();
    assert!(
        !seal.dirty,
        "a clean pre-run sample and a clean tree at seal time must seal dirty: false"
    );
}

#[tokio::test]
async fn a_tree_dirtied_mid_run_still_seals_dirty_true() {
    // `ExecContext.dirty` is only the *pre-run* sample. If the working tree
    // turns dirty while reviewers are still executing (an uncommitted fix
    // written mid-run), the seal must still record dirty: true, or the run
    // would misrepresent itself as attestable against a tree its reviewers
    // never actually saw in full. `seal_run` re-samples at seal time and ORs
    // the two; this test writes the untracked file before `execute_with`
    // runs, standing in for "dirtied sometime before persistence", since the
    // mock backend resolves synchronously and there is no window to inject a
    // write strictly between reviewer start and seal.
    let _seam_lock = SEAM_ENV_LOCK.lock().await;
    let _seam_guard = SeamEnvGuard::cleared();
    let repo = clean_repo();
    std::fs::write(repo.path().join("untracked.txt"), "surprise\n").unwrap();

    let g1 = reviewer("g1", Mode::Gate);
    let reviewers = [&g1];
    let mut execution = ctx(&reviewers);
    execution.repo_root = repo.path().to_path_buf();
    execution.seal = Some(seal_bindings(&["g1"]));
    // The pre-run sample itself reported clean (taken before the write
    // above, in the real flow); the seal-time re-sample is what must catch
    // the dirtied tree.
    execution.dirty = false;

    let (_decision, _events, layout) = run_scenario_with_ctx(
        &reviewers,
        execution,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;

    let run = RunId("r-exec".into());
    let seal = crate::store::read_seal(&layout, &run).unwrap().unwrap();
    assert!(
        seal.dirty,
        "an untracked file present at seal time must seal dirty: true even when the pre-run sample was clean"
    );
}

// -----------------------------------------------------------------------
// Spawn caps: a tripped governor aborts the run
// -----------------------------------------------------------------------

/// A [`CommandRunner`](crate::backend::command::CommandRunner) whose every launch
/// dies at zero tokens (exit 127, no stdout), standing in for the respawn-storm
/// signature so the run-level abort can be exercised through the real governor.
#[derive(Debug, Default)]
struct DeadRunner;

impl crate::backend::command::CommandRunner for DeadRunner {
    async fn run(
        &self,
        _spec: &crate::backend::command::CommandSpec,
    ) -> Result<crate::backend::command::CommandOutput> {
        Ok(crate::backend::command::CommandOutput {
            code: Some(127),
            stdout: String::new(),
            stderr: "command not found".into(),
        })
    }
}

#[tokio::test]
async fn a_spawn_storm_trips_the_breaker_and_aborts_the_run() {
    use crate::backend::command::{CommandRunner as _, CommandSpec};
    use crate::backend::governor::GovernedRunner;

    let g1 = reviewer("g1", Mode::Gate);
    let g2 = reviewer("g2", Mode::Gate);
    let g3 = reviewer("g3", Mode::Gate);
    let reviewers = [&g1, &g2, &g3];
    let mut ctx = ctx(&reviewers);
    // Trip after two consecutive dead launches; three reviewers each launch a dead
    // agent, so the breaker trips well inside the fan-out.
    ctx.limits = SpawnLimits {
        max_consecutive_failures: 2,
        ..SpawnLimits::default()
    };
    // Bindings that would seal a healthy full run; an aborted run must not seal.
    ctx.seal = Some(seal_bindings(&["g1", "g2", "g3"]));

    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::with_root(tmp.path().to_path_buf());
    std::mem::forget(tmp);

    // Drive each reviewer's launch through a GovernedRunner over the run's shared
    // governor (carried on the request), so the real cap logic runs: every launch
    // dies at zero tokens and the shared breaker trips.
    let exec = move |req: OwnedRequest| -> ReviewFuture {
        Box::pin(async move {
            let runner = GovernedRunner::new(DeadRunner, req.governor.clone());
            let out = runner.run(&CommandSpec::new("agent", ".")).await?;
            Err(color_eyre::eyre::eyre!(
                "codex exited with status {}",
                out.code.unwrap_or(-1)
            ))
        })
    };

    let mut events = Vec::new();
    let result = execute_with(
        &reviewers,
        &ctx,
        &layout,
        &mut |e| events.push(e.clone()),
        std::sync::Arc::new(exec),
    )
    .await;

    let err = result.expect_err("a tripped breaker aborts the run with a clear error");
    let msg = format!("{err:#}");
    assert!(msg.contains("aborted this review"), "got: {msg}");
    assert!(
        msg.contains("produced no output"),
        "the abort must name the consecutive-failure cap: {msg}"
    );

    // The run is still persisted and inspectable, its aggregate a block, but it is
    // never sealed: an aborted run must not become attestable.
    let run = RunId("r-exec".into());
    let persisted = crate::store::read_run(&layout, &run).unwrap();
    assert!(
        matches!(
            persisted.last(),
            Some(RunEvent::RunCompleted {
                verdict: Decision::Block,
                ..
            })
        ),
        "an aborted run persists a blocking run.completed"
    );
    assert_eq!(
        crate::store::read_seal(&layout, &run).unwrap(),
        None,
        "an aborted run must never seal"
    );
}

// -----------------------------------------------------------------------
// Attestation replay
// -----------------------------------------------------------------------

/// A `reviewer.resolved` event, as [`crate::attest::replay::plan`] would
/// hand it to the runner after parsing and checking it, for a replay test.
fn attested_event(name: &str, verdict: Decision, summary: &str) -> RunEvent {
    RunEvent::ReviewerResolved {
        carried: false,
        scope_digest: None,
        trigger: None,
        run: RunId("r-attested-elsewhere".into()),
        reviewer: name.into(),
        verdict,
        summary: summary.into(),
        findings: if verdict == Decision::Block {
            vec![Finding {
                kind: FindingKind::Blocking,
                path: "a.rs".into(),
                line_start: 1,
                line_end: 1,
                detail: "fix".into(),
            }]
        } else {
            vec![]
        },
        usage: Some(Usage {
            tokens_in: 50,
            tokens_out: 5,
            cache_read: 0,
            cost_usd: Money::from_cents(2),
        }),
        duration_ms: 12_345,
        has_transcript: true,
        replayed: false,
    }
}

fn attested_skip(name: &str, reason: &str) -> RunEvent {
    RunEvent::ReviewerSkipped {
        run: RunId("r-attested-elsewhere".into()),
        reviewer: name.into(),
        mode: Mode::Gate,
        trigger: TriggerResolution {
            backend: rev::Backend::Codex,
            decision: TriggerDecision::Skip,
            reason: reason.into(),
            usage: Some(Usage {
                tokens_in: 50,
                tokens_out: 5,
                cache_read: 0,
                cost_usd: Money::from_cents(2),
            }),
            duration_ms: 12_345,
        },
        has_transcript: true,
        replayed: false,
    }
}

#[tokio::test]
async fn zero_fresh_reviewers_with_a_full_replay_produces_a_complete_persisted_run() {
    let g1 = reviewer("g1", Mode::Gate);
    let mut ctx = ctx(&[&g1]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1.clone(),
            event: attested_event("g1", Decision::Pass, "replayed pass"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA test@bastion.dev".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    // No reviewers handed to the JoinSet at all: `matched` is empty.
    let (decision, events, layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;

    assert_eq!(decision, Decision::Pass);

    let resolved = events
        .iter()
        .find(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
        .expect("a resolved event exists even with zero fresh reviewers");
    match resolved {
        RunEvent::ReviewerResolved {
            replayed, verdict, ..
        } => {
            assert!(replayed, "the replayed row must carry replayed: true");
            assert_eq!(*verdict, Decision::Pass);
        }
        other => panic!("expected reviewer.resolved, got {other:?}"),
    }

    let attested = events
        .iter()
        .find(|e| matches!(e, RunEvent::AttestationReplayed { .. }))
        .expect("a run.attested event is emitted");
    match attested {
        RunEvent::AttestationReplayed {
            reviewers,
            public_key,
            ..
        } => {
            assert_eq!(reviewers, &["g1".to_string()]);
            assert_eq!(public_key, "ssh-ed25519 AAAA test@bastion.dev");
        }
        other => panic!("expected run.attested, got {other:?}"),
    }

    // The attested event must appear before run.completed.
    let attested_pos = events
        .iter()
        .position(|e| matches!(e, RunEvent::AttestationReplayed { .. }))
        .unwrap();
    let completed_pos = events
        .iter()
        .position(|e| matches!(e, RunEvent::RunCompleted { .. }))
        .unwrap();
    assert!(attested_pos < completed_pos);

    // The run is fully persisted, readable back exactly like a fresh run.
    let persisted = crate::store::read_run(&layout, &RunId("r-exec".into())).unwrap();
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::ReviewerResolved { replayed: true, .. }))
    );
    assert!(
        persisted
            .iter()
            .any(|e| matches!(e, RunEvent::AttestationReplayed { .. }))
    );

    // Persisted per-reviewer artifacts exist (verdict + meta), but no
    // transcript: there is no local transcript for a replayed reviewer.
    let run = RunId("r-exec".into());
    assert!(layout.verdict(&run, "g1").exists());
    assert!(layout.meta(&run, "g1").exists());
    assert!(!layout.transcript(&run, "g1").exists());
}

#[tokio::test]
async fn an_attested_agent_skip_replays_as_a_skip() {
    let g1 = agent_reviewer("g1", &[]);
    let mut ctx = ctx(&[&g1]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1,
            event: attested_skip("g1", "The concern does not apply."),
        },
    );

    let (decision, events, layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;

    assert_eq!(decision, Decision::Pass);
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::ReviewerSkipped {
            reviewer,
            replayed: true,
            trigger: TriggerResolution {
                decision: TriggerDecision::Skip,
                ..
            },
            ..
        } if reviewer == "g1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::RunCompleted {
            gates: Gates {
                total: 1,
                passed: 0,
                blocked: 0,
                skipped: 1,
            },
            tokens_in: 50,
            tokens_out: 5,
            ..
        }
    )));
    assert!(
        !layout.verdict(&RunId("r-exec".into()), "g1").exists(),
        "replay must not invent a verdict artifact for a semantic skip"
    );
}

#[tokio::test]
async fn a_replayed_block_blocks_the_run() {
    // Replay never changes an outcome: a block that was attested still blocks
    // when replayed.
    let g1 = reviewer("g1", Mode::Gate);
    let mut ctx = ctx(&[&g1]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1.clone(),
            event: attested_event("g1", Decision::Block, "replayed block"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    let (decision, events, _layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;
    assert_eq!(decision, Decision::Block);

    let gates = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, .. } => Some(*gates),
            _ => None,
        })
        .unwrap();
    assert_eq!(gates.total, 1);
    assert_eq!(gates.blocked, 1);
}

#[tokio::test]
async fn mixed_replay_and_fresh_reviewers_both_resolve() {
    let g1 = reviewer("g1", Mode::Gate); // replayed
    let g2 = reviewer("g2", Mode::Gate); // fresh
    let mut ctx = ctx(&[&g1, &g2]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1.clone(),
            event: attested_event("g1", Decision::Pass, "replayed pass"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    // Only g2 is handed to the JoinSet; g1 is not in `matched`.
    let (decision, events, _layout) = run_scenario_with_ctx(
        &[&g2],
        ctx,
        responses(vec![("g2", Response::Outcome(pass("fresh pass")))]),
    )
    .await;
    assert_eq!(decision, Decision::Pass);

    let resolved: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer, replayed, ..
            } => Some((reviewer.as_str(), *replayed)),
            _ => None,
        })
        .collect();
    assert_eq!(resolved.len(), 2);
    assert!(resolved.contains(&("g1", true)));
    assert!(resolved.contains(&("g2", false)));

    let gates = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, .. } => Some(*gates),
            _ => None,
        })
        .unwrap();
    assert_eq!(gates.total, 2);
    assert_eq!(gates.passed, 2);
}

#[tokio::test]
async fn no_run_attested_event_when_nothing_replayed() {
    // `attestation` set but `replayed` empty (should not happen in practice,
    // but the runner must not emit a vacuous run.attested event).
    let g1 = reviewer("g1", Mode::Gate);
    let mut ctx = ctx(&[&g1]);
    ctx.attestation = Some(AttestationAudit {
        public_key: "k".into(),
        attested_at: "t".into(),
    });

    let (_decision, events, _layout) = run_scenario_with_ctx(
        &[&g1],
        ctx,
        responses(vec![("g1", Response::Outcome(pass("ok")))]),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RunEvent::AttestationReplayed { .. }))
    );
}

/// Build a `reviewer.resolved` event that is internally inconsistent: a `pass`
/// verdict that nonetheless carries a blocking finding. Fresh execution can
/// never produce this shape (the backends reject it before it becomes an
/// outcome), so this simulates a malformed or tampered attestation bundle.
fn inconsistent_pass_event(name: &str) -> RunEvent {
    RunEvent::ReviewerResolved {
        carried: false,
        scope_digest: None,
        trigger: None,
        run: RunId("r-attested-elsewhere".into()),
        reviewer: name.into(),
        verdict: Decision::Pass,
        summary: "claims to pass".into(),
        findings: vec![Finding {
            kind: FindingKind::Blocking,
            path: "a.rs".into(),
            line_start: 1,
            line_end: 1,
            detail: "actually blocks".into(),
        }],
        usage: None,
        duration_ms: 1,
        has_transcript: true,
        replayed: false,
    }
}

#[tokio::test]
async fn a_replayed_gate_event_with_pass_and_a_blocking_finding_blocks_the_run() {
    // An inconsistent replayed gate event (pass + a blocking finding) must not
    // launder into a passing gate: it routes through the same fail-closed path
    // a crashed fresh execution would.
    let g1 = reviewer("g1", Mode::Gate);
    let mut ctx = ctx(&[&g1]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1.clone(),
            event: inconsistent_pass_event("g1"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    let (decision, events, _layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;
    assert_eq!(
        decision,
        Decision::Block,
        "an inconsistent replayed gate must fail closed"
    );

    let resolved = events
        .iter()
        .find(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
        .expect("a resolved event exists");
    match resolved {
        RunEvent::ReviewerResolved {
            verdict, summary, ..
        } => {
            assert_eq!(*verdict, Decision::Block);
            assert!(
                summary.contains("inconsistent") || summary.contains("did not produce"),
                "summary should explain the fail-closed reason: {summary}"
            );
        }
        other => panic!("expected reviewer.resolved, got {other:?}"),
    }
}

#[tokio::test]
async fn a_consistent_replayed_pass_still_passes() {
    // The straightforward happy path stays intact: a replayed pass with no
    // blocking findings still passes the run.
    let g1 = reviewer("g1", Mode::Gate);
    let mut ctx = ctx(&[&g1]);
    ctx.replayed.insert(
        "g1".to_string(),
        ReplayedReviewer {
            reviewer: g1.clone(),
            event: attested_event("g1", Decision::Pass, "replayed pass"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    let (decision, events, _layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;
    assert_eq!(decision, Decision::Pass);

    let resolved = events
        .iter()
        .find(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
        .expect("a resolved event exists");
    match resolved {
        RunEvent::ReviewerResolved {
            verdict, replayed, ..
        } => {
            assert_eq!(*verdict, Decision::Pass);
            assert!(*replayed);
        }
        other => panic!("expected reviewer.resolved, got {other:?}"),
    }
}

#[tokio::test]
async fn a_replayed_advisor_with_a_blocking_finding_normalizes_to_optional() {
    // A pass-with-a-blocking-finding row would be inconsistent for a gate, but an
    // advisor is normalized before the check: its decision stays pass and the
    // blocking finding becomes optional, so it never trips the universal
    // invariant, never blocks the aggregate, and keeps its advice as an optional
    // finding rather than being dropped through `fail`.
    let a1 = reviewer("a1", Mode::Advisor);
    let mut ctx = ctx(&[&a1]);
    ctx.replayed.insert(
        "a1".to_string(),
        ReplayedReviewer {
            reviewer: a1.clone(),
            event: inconsistent_pass_event("a1"),
        },
    );
    ctx.attestation = Some(AttestationAudit {
        public_key: "ssh-ed25519 AAAA".into(),
        attested_at: "2026-07-01T00:00:00Z".into(),
    });

    let (decision, events, _layout) = run_scenario_with_ctx(&[], ctx, responses(vec![])).await;
    assert_eq!(
        decision,
        Decision::Pass,
        "an advisor never blocks the aggregate"
    );

    let (verdict, findings) = events
        .iter()
        .find_map(|e| match e {
            RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                findings,
                ..
            } if reviewer == "a1" => Some((*verdict, findings.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(verdict, Decision::Pass);
    assert_eq!(
        findings.len(),
        1,
        "the advisory finding is preserved, not dropped through fail"
    );
    assert!(
        findings.iter().all(|f| f.kind == FindingKind::Optional),
        "the replayed advisor's blocking finding must be normalized to optional, got: {findings:?}"
    );

    let gates = events
        .iter()
        .find_map(|e| match e {
            RunEvent::RunCompleted { gates, .. } => Some(*gates),
            _ => None,
        })
        .unwrap();
    assert_eq!(gates.total, 0, "an advisor replay does not count as a gate");
}
