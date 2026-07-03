//! The parallel, timeout-bounded runner.
//!
//! [`execute`] runs every matched reviewer concurrently, bounds each by its
//! `timeout`, aggregates the results per the merge gate in `docs/developer-guide/design.md`, and
//! emits the full [`RunEvent`] stream. It owns event emission and persistence so
//! [`crate::commands::review`] only has to render the stream and map the aggregate
//! verdict to an exit status.
//!
//! Aggregation is fail-closed for gates and fail-open for advisors: a gate that
//! crashes, times out, or returns an invalid verdict resolves to **block**, never
//! a silent pass; an advisor that does the same is ignored.
//!
//! The backend boundary (the [`Backend`] trait, [`ReviewRequest`]/[`ReviewOutcome`],
//! [`MockBackend`], and dispatch) lives in [`crate::backend`] and is re-exported
//! here for the call sites that predate the split.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Context, Result};
use tokio::task::JoinSet;

use crate::backend::{self, ReviewOutcome, ReviewRequest};
use crate::context::ReviewContext;
use crate::event::{Gates, ReviewerRef, RunEvent, RunId};
use crate::paths::Layout;
use crate::reviewer::{Mode, Reviewer};
use crate::seal::SealBindings;
use crate::verdict::{Decision, Money, Usage, Verdict};

// Re-exports so existing imports (`runner::Backend`, `runner::MockBackend`, ...)
// keep resolving after the backend split.
pub use crate::backend::{Backend, MockBackend};

/// A backend factory: produces the [`ReviewOutcome`] for one owned reviewer.
///
/// Production uses [`backend::dispatch`] (the real subprocess path); tests inject
/// a closure that returns canned outcomes, so the runner's concurrency,
/// timeout, aggregation, and persistence logic is exercised without any agent.
type ReviewFn = dyn Fn(OwnedRequest) -> ReviewFuture + Send + Sync + 'static;

/// A boxed, owned-future review (so it is `Send + 'static` for [`JoinSet`]).
type ReviewFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<ReviewOutcome>> + Send>>;

/// An owned review request, decoupled from any borrow so it can cross into a
/// spawned task.
#[derive(Debug, Clone)]
pub struct OwnedRequest {
    /// The reviewer to execute (owned clone).
    pub reviewer: Reviewer,
    /// The run this review belongs to.
    pub run: RunId,
    /// The repository root.
    pub repo_root: PathBuf,
    /// The base branch.
    pub base: String,
    /// The shared review context (intent, discussion, prior findings). Cloned per
    /// reviewer from the run's [`ExecContext`]; each reviewer's prompt scopes it to
    /// its own concern.
    pub context: ReviewContext,
}

impl OwnedRequest {
    /// Run this request through the real backend dispatch.
    fn dispatch(self) -> ReviewFuture {
        Box::pin(async move {
            let request = ReviewRequest {
                reviewer: &self.reviewer,
                run: &self.run,
                repo_root: &self.repo_root,
                base: &self.base,
                context: &self.context,
            };
            backend::dispatch(&request).await
        })
    }
}

/// Shared context for executing a run's reviewers.
///
/// Carries everything the runner needs to both execute the reviewers and persist
/// the authoritative `run.started` event, so persistence lives entirely in the
/// runner and the command only renders.
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// The run id.
    pub run: RunId,
    /// The repository root.
    pub repo_root: PathBuf,
    /// The branch under review.
    pub branch: String,
    /// The base branch.
    pub base: String,
    /// Number of changed files (for the persisted `run.started`).
    pub changed: u32,
    /// The reviewers that matched and will run (for the persisted `run.started`).
    /// Includes both the reviewers that execute fresh and any that replay
    /// (`replayed`): a replayed reviewer still matched CI's routing, so it
    /// belongs in the plan the way a pending check does.
    pub reviewers: Vec<ReviewerRef>,
    /// The review context handed to every reviewer this run: the author's stated
    /// intent, the surrounding discussion, and each reviewer's prior findings. Empty
    /// when no producer supplied any, which leaves every reviewer's prompt unchanged.
    pub context: ReviewContext,
    /// The git- and config-derived bindings the run should be sealed with, when
    /// the caller could derive them. `None` means this run stays unsealed (a
    /// zero-match fast path, or a caller that failed to derive the bindings and
    /// chose to proceed unsealed rather than fail the review over it).
    pub seal: Option<SealBindings>,
    /// Whether the working tree carried uncommitted or untracked changes when
    /// this review ran ([`crate::git::is_dirty`]), computed once by the caller at
    /// review time. Recorded on the seal so `bastion attest` can refuse a run
    /// that reviewed content HEAD's committed tree does not name. Meaningless
    /// when `seal` is `None`.
    pub dirty: bool,
    /// Reviewers replaying from a verified attestation instead of executing,
    /// keyed by name (`docs/developer-guide/attestation.md`, "Verification and
    /// replay in CI"). Empty for every run before this phase and for any local
    /// review: only the CI surface, with attestations enabled and a verified
    /// bundle, ever populates this.
    pub replayed: std::collections::BTreeMap<String, ReplayedReviewer>,
    /// The attestation this run replayed from, when `replayed` is non-empty.
    /// Drives the single `run.attested` audit-trail event; `None` whenever
    /// `replayed` is empty.
    pub attestation: Option<AttestationAudit>,
    /// The `run.attestation-fallback` event, when the caller already rendered one
    /// (attestations were enabled but the note did not verify or replay).
    /// Carried here so persistence includes it too: the caller renders it to the
    /// live stream directly (before any reviewer has resolved, since it decides
    /// which reviewers execute fresh), so without this the persisted `run.jsonl`
    /// would silently drop the one event that explains why nothing replayed.
    pub attestation_fallback: Option<RunEvent>,
}

/// One reviewer's replayed verdict, carried into [`ExecContext`] so the runner
/// can fold it into the normal tally/persist/report machinery without handing
/// it to the backend `JoinSet`.
#[derive(Debug, Clone)]
pub struct ReplayedReviewer {
    /// The reviewer's own definition, exactly as CI routed it. Carried in full
    /// (not just its name) so persistence (backend, mode, trigger) has the same
    /// fidelity a freshly-executed reviewer's row does.
    pub reviewer: Reviewer,
    /// The bundle's `reviewer.resolved` event for this reviewer, exactly as
    /// attested. Already parsed and checked by
    /// [`crate::attest::replay::plan`] (a well-formed `reviewer.resolved`
    /// event bound to its own reviewer name), so the runner re-derives
    /// verdict, summary, findings, usage, and duration from it with no
    /// further JSON parsing or boundary revalidation; `has_transcript` is
    /// always overridden to `false` (see [`ExecContext::replayed`]'s doc
    /// comment) since there is no local transcript in the CI store.
    pub event: RunEvent,
}

/// Metadata about a run's attestation replay, for the `run.attested` audit-trail
/// event `execute_with` emits once, before `run.completed`, when
/// [`ExecContext::replayed`] is non-empty.
#[derive(Debug, Clone)]
pub struct AttestationAudit {
    /// The SSH public key that signed the attestation.
    pub public_key: String,
    /// When the attestation was signed, as recorded in the bundle.
    pub attested_at: String,
}

/// How long a reviewer with no explicit `timeout` is allowed to run before it is
/// failed closed (gate) or skipped (advisor). Chosen to be generous for a heavy
/// agentic review while still bounding a hung backend.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The fully-resolved result of one reviewer, ready to emit and persist.
struct Resolved {
    reviewer: Reviewer,
    /// The gate decision after applying fail-closed / fail-open policy.
    decision: Decision,
    summary: String,
    findings: Vec<crate::verdict::Finding>,
    usage: Option<Usage>,
    transcript: Option<String>,
    duration: Duration,
    /// Whether this reviewer's outcome counts toward the aggregate gate. Advisors
    /// never do; a failed advisor is ignored entirely.
    counts_as_gate: bool,
    /// Whether this verdict was replayed from a signed local attestation rather
    /// than executed fresh this run.
    replayed: bool,
}

/// Execute the matched reviewers for a run using the real backends.
///
/// Runs them concurrently with per-reviewer timeouts, emits the full event stream
/// via `emit`, persists the run and per-reviewer artifacts under `layout`, and
/// returns the aggregate [`Decision`]. A `block` aggregate maps to a non-zero exit
/// in the caller.
///
/// # Errors
///
/// Returns an error only if persistence fails; backend failures are absorbed into
/// the aggregate per the fail-closed/fail-open policy and never surface as an
/// error here.
pub async fn execute(
    matched: &[&Reviewer],
    ctx: &ExecContext,
    layout: &Layout,
    emit: &mut dyn FnMut(&RunEvent),
) -> Result<Decision> {
    let exec = |req: OwnedRequest| req.dispatch();
    execute_with(matched, ctx, layout, emit, &exec).await
}

/// [`execute`] with an injectable backend factory, for tests.
///
/// `exec` produces the review future for one owned request; production passes the
/// real [`backend::dispatch`]. The rest (concurrency, timeouts, aggregation,
/// event emission, and persistence) is identical, so tests cover the real paths.
///
/// # Errors
///
/// Returns an error only if persisting the run fails.
pub async fn execute_with(
    matched: &[&Reviewer],
    ctx: &ExecContext,
    layout: &Layout,
    emit: &mut dyn FnMut(&RunEvent),
    exec: &ReviewFn,
) -> Result<Decision> {
    let run_started = Instant::now();

    // Emit `reviewer.started` for each reviewer up front (the pending-checks
    // equivalent), then launch the ones that must actually execute concurrently.
    // A replayed reviewer still gets a `reviewer.started` (mirroring what
    // actually produced the verdict, per its own definition) so the reported
    // plan matches what ran, but it is never handed to the `JoinSet`: only the
    // non-replayed matched reviewers are. The started events are also retained
    // for persistence so `run.jsonl` is the *full* event stream the docs
    // promise, not just the resolve/completed tail.
    let mut started_events = Vec::with_capacity(matched.len() + ctx.replayed.len());
    for reviewer in matched {
        let event = RunEvent::ReviewerStarted {
            run: ctx.run.clone(),
            reviewer: reviewer.name.clone(),
            mode: reviewer.mode,
            backend: reviewer.backend,
        };
        emit(&event);
        started_events.push(event);
    }
    for replay in ctx.replayed.values() {
        let event = RunEvent::ReviewerStarted {
            run: ctx.run.clone(),
            reviewer: replay.reviewer.name.clone(),
            mode: replay.reviewer.mode,
            backend: replay.reviewer.backend,
        };
        emit(&event);
        started_events.push(event);
    }

    let mut set: JoinSet<(usize, ReviewTaskResult)> = JoinSet::new();
    for (index, reviewer) in matched.iter().enumerate() {
        let request = OwnedRequest {
            reviewer: (*reviewer).clone(),
            run: ctx.run.clone(),
            repo_root: ctx.repo_root.clone(),
            base: ctx.base.clone(),
            context: ctx.context.clone(),
        };
        let timeout = reviewer.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let future = exec(request);
        set.spawn(async move {
            let started = Instant::now();
            let outcome = match tokio::time::timeout(timeout, future).await {
                Ok(result) => match result {
                    Ok(outcome) => TaskOutcome::Ok(outcome),
                    Err(err) => TaskOutcome::Failed(format!("{err:#}")),
                },
                Err(_elapsed) => TaskOutcome::TimedOut,
            };
            (
                index,
                ReviewTaskResult {
                    outcome,
                    duration: started.elapsed(),
                },
            )
        });
    }

    // Collect results as they complete, then restore registry order so the
    // persisted stream is deterministic regardless of completion timing.
    let mut results: Vec<Option<ReviewTaskResult>> = (0..matched.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((index, result)) => results[index] = Some(result),
            Err(join_err) => {
                // A panicked task: we have no index, so we cannot place it. This
                // should not happen (tasks catch their own errors), but if it
                // does, it must not silently drop a gate. Fall through; the
                // corresponding slot stays `None` and is treated as a crash below.
                tracing::error!(error = %join_err, "a reviewer task panicked");
            }
        }
    }

    // Resolve each freshly-executed reviewer, applying fail-closed / fail-open
    // policy, then fold in every replayed reviewer from its attested event. A
    // replayed row is never subject to fail-closed/fail-open: it already carries
    // a real, previously-resolved verdict, so it is reconstructed verbatim
    // (verdict, findings, usage, duration) rather than re-derived.
    let mut resolved = Vec::with_capacity(matched.len() + ctx.replayed.len());
    for (index, reviewer) in matched.iter().enumerate() {
        resolved.push(resolve(reviewer, results[index].take()));
    }
    for replay in ctx.replayed.values() {
        resolved.push(resolve_replayed(replay));
    }

    // Persist per-reviewer artifacts and build the resolve events. The persisted
    // stream opens with the caller's `run.attestation-fallback` (when present,
    // decided and rendered before any reviewer was dispatched) followed by the
    // retained `reviewer.started` events, so a replay sees the same sequence the
    // live `emit` produced.
    let mut events = Vec::with_capacity(started_events.len() + resolved.len() + 3);
    if let Some(fallback) = ctx.attestation_fallback.clone() {
        events.push(fallback);
    }
    events.extend(started_events);
    for item in &resolved {
        persist_reviewer(layout, &ctx.run, item)
            .wrap_err_with(|| format!("persisting reviewer '{}'", item.reviewer.name))?;
        let event = RunEvent::ReviewerResolved {
            run: ctx.run.clone(),
            reviewer: item.reviewer.name.clone(),
            verdict: item.decision,
            summary: item.summary.clone(),
            findings: item.findings.clone(),
            usage: item.usage,
            duration_ms: duration_ms(item.duration),
            has_transcript: item.transcript.is_some(),
            replayed: item.replayed,
        };
        emit(&event);
        events.push(event);
    }

    // The attestation audit trail, once per run, right before `run.completed`:
    // which reviewers replayed, by which key, and when.
    if let Some(audit) = &ctx.attestation
        && !ctx.replayed.is_empty()
    {
        let mut reviewers: Vec<String> = ctx.replayed.keys().cloned().collect();
        reviewers.sort();
        let event = RunEvent::AttestationReplayed {
            run: ctx.run.clone(),
            reviewers,
            public_key: audit.public_key.clone(),
            attested_at: audit.attested_at.clone(),
        };
        emit(&event);
        events.push(event);
    }

    // Aggregate: all gates must pass. A replayed block still blocks the run;
    // replay never changes an outcome.
    let gates = tally(&resolved);
    let aggregate = if gates.blocked == 0 {
        Decision::Pass
    } else {
        Decision::Block
    };
    let usage = total_usage(&resolved);

    let completed = RunEvent::RunCompleted {
        run: ctx.run.clone(),
        verdict: aggregate,
        gates,
        duration_ms: duration_ms(run_started.elapsed()),
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cache_read: usage.cache_read,
        cost_usd: usage.cost_usd,
    };
    emit(&completed);

    // Persist the full stream. The runner owns persistence, so it reconstructs the
    // authoritative `run.started` from the context and prepends it to the resolve
    // and completed events, then writes `run.jsonl` and updates `latest`.
    let mut stream = Vec::with_capacity(events.len() + 1);
    stream.extend(events);
    stream.push(completed);
    persist_run(layout, &ctx.run, ctx, &stream)?;

    seal_run(layout, ctx, &stream);

    Ok(aggregate)
}

/// Reconstruct a [`Resolved`] row for a replayed reviewer from its attested
/// `reviewer.resolved` event.
///
/// `replay.event` arrives already parsed and boundary-checked:
/// [`crate::attest::replay::plan`] is the sole producer of a [`ReplayedReviewer`]
/// and only ever hands this function a [`RunEvent::ReviewerResolved`] whose
/// `reviewer` field matches `replay.reviewer.name` (its own key-to-event binding
/// check). There is nothing left here to parse or revalidate at that boundary; a
/// non-`ReviewerResolved` variant reaching this function would be a defect in
/// the planner, not attacker-shaped input, so the fallback arm exists only to
/// keep this total rather than to police untrusted data a second time.
///
/// What *does* stay reviewer-shaped input, and so is checked here rather than
/// trusted: whether the claimed verdict is internally consistent. Fresh
/// execution never reaches [`resolve`] with a `pass` that also carries a
/// blocking finding, because [`backend::extract_verdict`] and the Claude Code
/// backend's own extraction both reject that shape before it ever becomes a
/// [`ReviewOutcome`] (see [`Verdict::is_consistent`]). A signed bundle is
/// attacker-shaped input just like an agent's raw output, so a replayed event
/// gets the identical check: reconstruct the [`Verdict`] the event claims and
/// require it to be consistent before trusting its decision.
fn resolve_replayed(replay: &ReplayedReviewer) -> Resolved {
    let is_gate = replay.reviewer.mode == Mode::Gate;
    match &replay.event {
        RunEvent::ReviewerResolved {
            verdict,
            summary,
            findings,
            usage,
            duration_ms,
            ..
        } => {
            let claimed = Verdict {
                decision: *verdict,
                summary: summary.clone(),
                findings: findings.clone(),
            };
            if !claimed.is_consistent() {
                return fail(
                    &replay.reviewer,
                    is_gate,
                    "the attested reviewer.resolved event was internally inconsistent \
                     (a pass carrying a blocking finding, or a block with none)",
                    Duration::from_millis(*duration_ms),
                );
            }
            Resolved {
                reviewer: replay.reviewer.clone(),
                decision: *verdict,
                summary: summary.clone(),
                findings: findings.clone(),
                usage: *usage,
                // There is no local transcript in the CI store: the bundle carries
                // only the resolved event, never the transcript file.
                transcript: None,
                duration: Duration::from_millis(*duration_ms),
                counts_as_gate: is_gate,
                replayed: true,
            }
        }
        _ => fail(
            &replay.reviewer,
            is_gate,
            "the attested reviewer.resolved event was not a reviewer.resolved event \
             (a defect in the attestation planner, not the bundle)",
            Duration::ZERO,
        ),
    }
}

/// Seal the run when `ctx` carries [`SealBindings`], persisting the result
/// alongside `run.jsonl`.
///
/// Sealing failure is deliberately non-fatal: an unsealed run is simply a run
/// nobody can attest later, not a failed review. Every failure path here (no
/// bindings, zero repo-reviewer events, a persistence error) logs at most a
/// `tracing::warn!` and returns without touching `aggregate`.
fn seal_run(layout: &Layout, ctx: &ExecContext, stream: &[RunEvent]) {
    let Some(bindings) = &ctx.seal else {
        return;
    };

    // Only the repository reviewers this seal covers are eligible: a
    // user-level-only reviewer's verdict never gates anyone else's PR, so it
    // cannot attest anything either (`docs/developer-guide/attestation.md`).
    // Sorting by name makes the digest independent of completion order.
    let mut sealed: Vec<(&str, &RunEvent)> = stream
        .iter()
        .filter_map(|event| match event {
            RunEvent::ReviewerResolved { reviewer, .. }
                if bindings.repo_reviewers.contains(reviewer.as_str()) =>
            {
                Some((reviewer.as_str(), event))
            }
            _ => None,
        })
        .collect();
    sealed.sort_by_key(|(name, _)| *name);

    if sealed.is_empty() {
        return;
    }

    let reviewers: Vec<String> = sealed.iter().map(|(name, _)| (*name).to_string()).collect();
    let events: Vec<serde_json::Value> = match sealed
        .iter()
        .map(|(_, event)| serde_json::to_value(event))
        .collect::<std::result::Result<_, _>>()
    {
        Ok(values) => values,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize resolved events for sealing; run will be unsealed");
            return;
        }
    };

    let seal = crate::seal::seal(
        crate::seal::embedded_secret(),
        crate::version::VERSION,
        bindings,
        crate::seal::seams_active(),
        ctx.dirty,
        reviewers,
        &events,
    );

    if let Err(err) = crate::store::write_seal(layout, &ctx.run, &seal) {
        tracing::warn!(error = %err, "failed to persist run seal; run will be unsealed");
    }
}

/// The raw result of one reviewer task before fail-closed/open policy is applied.
struct ReviewTaskResult {
    outcome: TaskOutcome,
    duration: Duration,
}

/// What a single reviewer task produced.
enum TaskOutcome {
    /// The backend returned a verdict.
    Ok(ReviewOutcome),
    /// The backend ran but failed (bad output, crash, exec error).
    Failed(String),
    /// The reviewer exceeded its timeout.
    TimedOut,
}

/// Apply fail-closed (gate) / fail-open (advisor) policy to one reviewer's raw
/// result, yielding a fully-resolved row.
///
/// A `None` result means the task neither completed nor errored cleanly (a
/// panic); it is treated as a crash, i.e. fail-closed for a gate.
fn resolve(reviewer: &Reviewer, result: Option<ReviewTaskResult>) -> Resolved {
    let is_gate = reviewer.mode == Mode::Gate;
    match result {
        Some(ReviewTaskResult {
            outcome: TaskOutcome::Ok(outcome),
            duration,
        }) => {
            let verdict = outcome.verdict;
            // An advisor never blocks: clamp its decision to pass for aggregation,
            // but keep its findings so they still surface.
            let decision = if is_gate {
                verdict.decision
            } else {
                Decision::Pass
            };
            Resolved {
                reviewer: reviewer.clone(),
                decision,
                summary: verdict.summary,
                findings: verdict.findings,
                usage: outcome.usage,
                transcript: outcome.transcript,
                duration,
                counts_as_gate: is_gate,
                replayed: false,
            }
        }
        Some(ReviewTaskResult {
            outcome: TaskOutcome::Failed(reason),
            duration,
        }) => fail(reviewer, is_gate, &reason, duration),
        Some(ReviewTaskResult {
            outcome: TaskOutcome::TimedOut,
            duration,
        }) => fail(
            reviewer,
            is_gate,
            &format!(
                "timed out after {}s",
                reviewer.timeout.unwrap_or(DEFAULT_TIMEOUT).as_secs()
            ),
            duration,
        ),
        None => fail(
            reviewer,
            is_gate,
            "the reviewer task crashed",
            Duration::ZERO,
        ),
    }
}

/// Build the resolved row for a failed/timed-out reviewer: a gate fails closed
/// (block, with a synthetic blocking finding), an advisor fails open (pass).
fn fail(reviewer: &Reviewer, is_gate: bool, reason: &str, duration: Duration) -> Resolved {
    if is_gate {
        Resolved {
            reviewer: reviewer.clone(),
            decision: Decision::Block,
            summary: format!("{} did not produce a verdict: {reason}", reviewer.name),
            findings: vec![crate::verdict::Finding {
                kind: crate::verdict::FindingKind::Blocking,
                path: String::new(),
                line_start: 0,
                line_end: 0,
                detail: format!("reviewer failed to complete: {reason}"),
            }],
            usage: None,
            transcript: None,
            duration,
            counts_as_gate: true,
            replayed: false,
        }
    } else {
        Resolved {
            reviewer: reviewer.clone(),
            decision: Decision::Pass,
            summary: format!("{} skipped (advisor): {reason}", reviewer.name),
            findings: Vec::new(),
            usage: None,
            transcript: None,
            duration,
            counts_as_gate: false,
            replayed: false,
        }
    }
}

/// Tally the gate outcomes for the `run.completed` event.
fn tally(resolved: &[Resolved]) -> Gates {
    let mut total = 0u32;
    let mut passed = 0u32;
    let mut blocked = 0u32;
    for item in resolved {
        if !item.counts_as_gate {
            continue;
        }
        total += 1;
        if item.decision.is_block() {
            blocked += 1;
        } else {
            passed += 1;
        }
    }
    Gates {
        total,
        passed,
        blocked,
    }
}

/// Sum reported usage (input/output tokens and cost) across all reviewers, gates
/// and advisors alike. A reviewer that reported no usage contributes nothing.
fn total_usage(resolved: &[Resolved]) -> Usage {
    resolved
        .iter()
        .filter_map(|item| item.usage)
        .fold(Usage::default(), |acc, u| Usage {
            tokens_in: acc.tokens_in.saturating_add(u.tokens_in),
            tokens_out: acc.tokens_out.saturating_add(u.tokens_out),
            cache_read: acc.cache_read.saturating_add(u.cache_read),
            cost_usd: Money::from_cents(acc.cost_usd.cents().saturating_add(u.cost_usd.cents())),
        })
}

/// Persist one reviewer's saved artifacts: transcript, raw verdict, and metadata.
fn persist_reviewer(layout: &Layout, run: &RunId, item: &Resolved) -> Result<()> {
    let dir = layout.reviewer_dir(run, &item.reviewer.name);
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("creating reviewer directory {}", dir.display()))?;

    if let Some(transcript) = &item.transcript {
        let path = layout.transcript(run, &item.reviewer.name);
        std::fs::write(&path, transcript)
            .wrap_err_with(|| format!("writing {}", path.display()))?;
    }

    // The raw structured verdict, exactly as aggregated.
    let verdict = Verdict {
        decision: item.decision,
        summary: item.summary.clone(),
        findings: item.findings.clone(),
    };
    let verdict_path = layout.verdict(run, &item.reviewer.name);
    std::fs::write(
        &verdict_path,
        serde_json::to_string_pretty(&verdict).wrap_err("serializing verdict")?,
    )
    .wrap_err_with(|| format!("writing {}", verdict_path.display()))?;

    // Per-reviewer metadata: backend, timing, usage, matched trigger.
    let meta = ReviewerMeta {
        backend: item.reviewer.backend,
        mode: item.reviewer.mode,
        duration_ms: duration_ms(item.duration),
        usage: item.usage,
        trigger: item.reviewer.trigger.clone(),
    };
    let meta_path = layout.meta(run, &item.reviewer.name);
    std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).wrap_err("serializing reviewer meta")?,
    )
    .wrap_err_with(|| format!("writing {}", meta_path.display()))?;

    Ok(())
}

/// Per-reviewer metadata saved alongside the transcript and verdict.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReviewerMeta {
    backend: crate::reviewer::Backend,
    mode: Mode,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    trigger: Vec<String>,
}

/// Persist the run's event stream, prepending the authoritative `run.started`.
fn persist_run(layout: &Layout, run: &RunId, ctx: &ExecContext, tail: &[RunEvent]) -> Result<()> {
    // The store writes `run.jsonl` and updates `latest`. We reconstruct the
    // opening event here so a replayed run is complete; `changed` is recorded by
    // the caller's emitted event, which is the canonical one shown on screen.
    let started = RunEvent::RunStarted {
        run: run.clone(),
        branch: ctx.branch.clone(),
        base: ctx.base.clone(),
        changed: ctx.changed,
        reviewers: ctx.reviewers.clone(),
    };
    let mut events = Vec::with_capacity(tail.len() + 1);
    events.push(started);
    events.extend_from_slice(tail);
    crate::store::write_run(layout, run, &events)
}

/// Whole-millisecond duration, saturating at `u64::MAX`.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
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

    /// The four env vars [`crate::seal::seams_active`] reads, gathered in one
    /// place so a test can force them all to a known state.
    const SEAM_ENV_VARS: [&str; 4] = [
        crate::backend::claude_code::PROGRAM_ENV,
        crate::backend::codex::PROGRAM_ENV,
        crate::backend::pi::PROGRAM_ENV,
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
    /// has `BASTION_CODEX_BIN` (or any of the other three) set would otherwise
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
            trigger: vec!["**".into()],
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

    fn ctx(reviewers: &[&Reviewer]) -> ExecContext {
        ExecContext {
            run: RunId("r-exec".into()),
            repo_root: PathBuf::from("."),
            branch: "feat".into(),
            base: "main".into(),
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
            attestation_fallback: None,
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
            &exec,
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
    }

    fn responses(pairs: Vec<(&str, Response)>) -> std::collections::HashMap<String, Response> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
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
        // Even a clean `block` verdict from an advisor is non-blocking.
        let a1 = reviewer("a1", Mode::Advisor);
        let reviewers = [&a1];
        let (decision, _events, _layout) = run_scenario(
            &reviewers,
            responses(vec![("a1", Response::Outcome(block("advisory concern")))]),
        )
        .await;
        assert_eq!(decision, Decision::Pass);
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

    #[tokio::test]
    async fn persisted_run_jsonl_is_the_full_event_stream() {
        // run.jsonl must contain the started events too, not just resolve/completed,
        // so a replay sees the same sequence the live stream emitted.
        let g1 = reviewer("g1", Mode::Gate);
        let reviewers = [&g1];
        let (_decision, _events, layout) = run_scenario(
            &reviewers,
            responses(vec![("g1", Response::Outcome(pass("ok")))]),
        )
        .await;

        let persisted = crate::store::read_run(&layout, &RunId("r-exec".into())).unwrap();
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
            .filter(
                |e| matches!(e, RunEvent::ReviewerResolved { reviewer, .. } if reviewer == "g1"),
            )
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert!(crate::seal::verify(
            crate::seal::embedded_secret(),
            &seal,
            &resolved
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
            .filter(
                |e| matches!(e, RunEvent::ReviewerResolved { reviewer, .. } if reviewer == "g1"),
            )
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

    // -----------------------------------------------------------------------
    // Attestation replay
    // -----------------------------------------------------------------------

    /// A `reviewer.resolved` event, as [`crate::attest::replay::plan`] would
    /// hand it to the runner after parsing and checking it, for a replay test.
    fn attested_event(name: &str, verdict: Decision, summary: &str) -> RunEvent {
        RunEvent::ReviewerResolved {
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
    async fn an_inconsistent_replayed_advisor_stays_fail_open() {
        // Mirror the fresh-execution advisor policy exactly: a broken/inconsistent
        // result never blocks the run for an advisor, only a gate. This mirrors
        // `fail`'s advisor branch, which resolves to `Decision::Pass` and does not
        // count toward the gate tally.
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
            "an advisor never blocks the aggregate, even on a broken replay"
        );

        let gates = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { gates, .. } => Some(*gates),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            gates.total, 0,
            "a failed advisor replay does not count as a gate"
        );
    }
}
