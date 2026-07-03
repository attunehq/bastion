//! The parallel, timeout-bounded runner.
//!
//! [`execute`] runs the matched reviewers concurrently, bounds each by its
//! `timeout`, aggregates the results per the merge gate in `docs/developer-guide/design.md`, and
//! emits the full [`RunEvent`] stream. Not every matched reviewer dispatches a
//! backend: a reviewer covered by a verified attestation replays its recorded
//! verdict, and a re-run can carry an unchanged prior pass forward (locally, or in
//! CI from its own prior CI run); both fold into the same tally and stream. It owns event emission and persistence so
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
mod persist;
mod seal;
mod verdicts;

#[cfg(test)]
mod tests;

use persist::{persist_reviewer, persist_run};
use seal::seal_run;
use verdicts::{resolve_carried, resolve_replayed};

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
    /// The reviewers in the run's plan (for the persisted `run.started`).
    /// Includes the reviewers that execute fresh and any that replay
    /// (`replayed`) or carry (`carried`): a replayed or carried reviewer still
    /// matched routing, so it belongs in the plan the way a pending check
    /// does, even though no backend dispatches for it.
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
    /// this review started ([`crate::git::is_dirty`]), a pre-run sample computed
    /// once by the caller before reviewers execute. Sealing re-samples the
    /// working tree again at persist time and ORs the two, so a tree that turns
    /// dirty while reviewers are still running still seals dirty. Recorded on
    /// the seal so `bastion attest` can refuse a run that reviewed content
    /// HEAD's committed tree does not name. Meaningless when `seal` is `None`.
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
    /// Whether this run was deliberately narrowed to a subset of the triggered
    /// reviewers (`bastion review --reviewer`). A partial run persists and
    /// renders as partial, and is never sealed: its aggregate speaks only for
    /// the reviewers that ran, so it must not become attestable as a verdict on
    /// the full triggered set.
    pub partial: bool,
    /// Reviewers carrying their verdict forward from the branch's previous run
    /// because their trigger-scoped diff is unchanged ([`crate::carry`]), keyed
    /// by name. Like `replayed`, these fold into the tally and the persisted
    /// stream without being handed to the backend `JoinSet`. Populated on either
    /// surface (a CI run carries from its own prior CI run just as a local run
    /// does); disjoint from both `replayed` and the fresh set.
    pub carried: std::collections::BTreeMap<String, crate::carry::Carried>,
    /// The current scope digest for each reviewer executing fresh this run,
    /// keyed by name, stamped onto its `reviewer.resolved` event so a later run
    /// can decide whether to carry it. An absent entry (a digest that failed to
    /// compute) leaves the event without one, which simply makes it ineligible
    /// to carry from.
    pub scope_digests: std::collections::BTreeMap<String, String>,
    /// What the runner needs to *recompute* a scope digest after the reviewers
    /// finish. Reviewers judge the live working tree, so a digest sampled
    /// before execution can go stale while they run; when this probe is
    /// present, the runner re-derives each stamped digest post-execution and
    /// drops any that no longer matches, so a later run can never carry a
    /// verdict whose scoped content changed mid-run. `None` (a caller with no
    /// merge base, or a test that stamps digests directly) skips the check
    /// and stamps the pre-run digests as given.
    pub digest_probe: Option<DigestProbe>,
    /// The `run.attestation-fallback` event, when the caller already rendered one
    /// (attestations were enabled but the note did not verify or replay).
    /// Carried here so persistence includes it too: the caller renders it to the
    /// live stream directly (before any reviewer has resolved, since it decides
    /// which reviewers execute fresh), so without this the persisted `run.jsonl`
    /// would silently drop the one event that explains why nothing replayed.
    pub attestation_fallback: Option<RunEvent>,
}

/// The inputs [`crate::carry::scope_digest`] needs beyond the reviewer itself,
/// so the runner can re-derive a digest after execution and confirm the scoped
/// content did not change while the reviewer ran (see
/// [`ExecContext::digest_probe`]).
#[derive(Debug, Clone)]
pub struct DigestProbe {
    /// The base branch the run reviewed against (the diff the prompt names).
    /// The post-run check re-scans the changed-file set against this itself,
    /// so a file created mid-run reaches the recomputed digest.
    pub base: String,
    /// The merge-base commit the run's diffs are taken against.
    pub merge_base: String,
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
    /// Whether this verdict was carried forward from the branch's previous run
    /// ([`crate::carry`]) rather than executed fresh this run.
    carried: bool,
    /// The scope digest to stamp on this reviewer's `reviewer.resolved` event,
    /// when one was computed.
    scope_digest: Option<String>,
}

/// Execute the matched reviewers for a run using the real backends.
///
/// Runs them concurrently with per-reviewer timeouts, emits the full event stream
/// via `emit`, persists the run and per-reviewer artifacts under `layout`, and
/// returns the aggregate [`Decision`]. Reviewers supplied through
/// [`ExecContext::replayed`] or [`ExecContext::carried`] dispatch no backend;
/// their recorded verdicts are folded into the same stream and tally. A `block` aggregate maps to a non-zero exit
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
    let mut started_events =
        Vec::with_capacity(matched.len() + ctx.replayed.len() + ctx.carried.len());
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
    // A carried reviewer is announced the same way a replayed one is: it is
    // part of the run's plan even though no backend executes for it.
    for carry in ctx.carried.values() {
        let event = RunEvent::ReviewerStarted {
            run: ctx.run.clone(),
            reviewer: carry.reviewer.name.clone(),
            mode: carry.reviewer.mode,
            backend: carry.reviewer.backend,
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
    let mut resolved = Vec::with_capacity(matched.len() + ctx.replayed.len() + ctx.carried.len());
    for (index, reviewer) in matched.iter().enumerate() {
        let digest = ctx.scope_digests.get(&reviewer.name).cloned();
        resolved.push(resolve(reviewer, results[index].take(), digest));
    }
    for replay in ctx.replayed.values() {
        resolved.push(resolve_replayed(replay));
    }
    for carry in ctx.carried.values() {
        resolved.push(resolve_carried(carry));
    }

    // Reviewers judge the live working tree, so a digest sampled before
    // execution can go stale while they run. Re-derive each stamped digest now
    // and keep it only when it still matches. A mismatch on a *fresh* verdict
    // drops the stamp, which fails safe: the reviewer judged the tree it saw,
    // and the next run simply cannot carry from this one. A mismatch on a
    // *carried* verdict is worse: the prior pass was reused precisely because
    // the digest matched at plan time, so if the scoped content changed
    // mid-run, that pass no longer describes the tree this run reports on.
    // The verdict itself is untrustworthy, so it fails closed (gate) or is
    // skipped (advisor), same as any reviewer that could not produce one.
    if let Some(probe) = &ctx.digest_probe {
        // Re-scan the changed-file set too: a file created mid-run that a
        // trigger matches must reach the recomputed digest, or an addition
        // would be invisible to this check. A failed re-scan recomputes
        // nothing, so every stamped digest mismatches and the check degrades
        // in the fail-safe direction.
        let changed_now = crate::git::changed_files(&ctx.repo_root, &probe.base)
            .map_err(|err| {
                tracing::warn!(error = %err, "could not re-scan changed files post-run");
                err
            })
            .ok();
        for item in &mut resolved {
            if let Some(pre) = item.scope_digest.take() {
                let now = changed_now.as_ref().and_then(|changed| {
                    crate::carry::scope_digest(
                        &ctx.repo_root,
                        &probe.base,
                        &probe.merge_base,
                        &item.reviewer,
                        changed,
                    )
                    .ok()
                });
                if now.as_deref() == Some(pre.as_str()) {
                    item.scope_digest = Some(pre);
                } else if item.carried {
                    tracing::warn!(
                        reviewer = %item.reviewer.name,
                        "the scoped content changed while this run executed; \
                         its carried verdict no longer describes the tree and fails closed"
                    );
                    *item = fail(
                        &item.reviewer,
                        item.counts_as_gate,
                        "the working tree content this reviewer's carried verdict was \
                         scoped to changed while the run executed; re-run to review the \
                         current content",
                        Duration::ZERO,
                    );
                } else {
                    tracing::warn!(
                        reviewer = %item.reviewer.name,
                        "the working tree changed while this reviewer ran; \
                         dropping its scope digest so the next run cannot carry it"
                    );
                }
            }
        }
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
            carried: item.carried,
            scope_digest: item.scope_digest.clone(),
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
        partial: ctx.partial,
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
///
/// `scope_digest` is the current trigger-scoped digest to stamp on the resolved
/// row (see [`ExecContext::scope_digests`]). Only a real verdict gets the
/// stamp: a crash, timeout, or malformed output resolves through [`fail`]
/// without one, so a fail-closed block can never seed a later carry.
fn resolve(
    reviewer: &Reviewer,
    result: Option<ReviewTaskResult>,
    scope_digest: Option<String>,
) -> Resolved {
    let is_gate = reviewer.mode == Mode::Gate;
    match result {
        Some(ReviewTaskResult {
            outcome: TaskOutcome::Ok(outcome),
            duration,
        }) => {
            let verdict = outcome.verdict;
            // An advisor never blocks: clamp its decision to pass and record any
            // blocking finding as optional, so the row still surfaces the advice
            // while satisfying the universal pass-carries-no-blocking invariant.
            let (decision, findings) = clamp_advisor(is_gate, verdict.decision, verdict.findings);
            Resolved {
                reviewer: reviewer.clone(),
                decision,
                summary: verdict.summary,
                findings,
                usage: outcome.usage,
                transcript: outcome.transcript,
                duration,
                counts_as_gate: is_gate,
                replayed: false,
                carried: false,
                scope_digest,
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

/// Normalize a reviewer's judgment to the shape its mode allows, so every
/// resolution path (fresh [`resolve`], [`resolve_replayed`], [`resolve_carried`])
/// records advisors identically.
///
/// A gate's verdict passes through untouched. An advisor never blocks, so its
/// decision is clamped to [`Decision::Pass`] and every [`FindingKind::Blocking`]
/// finding is recorded as [`FindingKind::Optional`]: an advisor's findings are
/// advice, not a merge blocker, so recording them as blocking would be a lie the
/// data model then has to carve exceptions around. Downgrading the finding kind
/// here (rather than clamping only the decision and leaving a blocking finding on
/// a passing row) is what makes [`Verdict::is_consistent`] a *universal* invariant:
/// a persisted `pass` carries no blocking finding, gate or advisor alike, so the
/// consistency check the replay and carry paths apply no longer has to special-case
/// advisor mode (issue #74).
fn clamp_advisor(
    is_gate: bool,
    decision: Decision,
    findings: Vec<crate::verdict::Finding>,
) -> (Decision, Vec<crate::verdict::Finding>) {
    if is_gate {
        return (decision, findings);
    }
    let findings = findings
        .into_iter()
        .map(|mut finding| {
            if finding.kind == crate::verdict::FindingKind::Blocking {
                finding.kind = crate::verdict::FindingKind::Optional;
            }
            finding
        })
        .collect();
    (Decision::Pass, findings)
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
            carried: false,
            scope_digest: None,
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
            carried: false,
            scope_digest: None,
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

/// Whole-millisecond duration, saturating at `u64::MAX`.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
