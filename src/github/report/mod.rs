//! Turning a finished run into GitHub surfaces: a sticky PR comment and check runs.
//!
//! This is the reporting half of the adapter. It reads the same [`RunEvent`]
//! stream the local surface renders and `run.jsonl` persists, and maps it onto two
//! GitHub surfaces described in `docs/developer-guide/github-adapter.md`:
//!
//! - **One sticky PR comment** carrying every reviewer's verdict and *all* of its
//!   findings, blocking and optional alike. Optional findings never gate, so this
//!   is the one place a reader sees them without opening the run artifact; the
//!   comment is upserted in place (matched by a hidden marker) so a re-run rewrites
//!   it rather than stacking duplicates.
//! - **A check run per reviewer plus an always-present aggregate `bastion` check**,
//!   so the PR's checks list shows exactly which reviewers ran and how each landed.
//!   A blocking gate reports `failure`; a passing gate and any advisor report
//!   `success`. Located findings ride along as check annotations.
//!
//! The report renders the run the runner already decided. The runner is what
//! enforces the gate semantics: it fails a gate closed at write time (a crashed or
//! timed-out gate is persisted as a block with a synthetic blocking finding) and
//! clamps every advisor to a pass, recording its findings as optional. So this half does not
//! re-derive the merge decision; it trusts the recorded `run.completed` verdict and
//! each reviewer's recorded row, and draws them onto the two surfaces. The persisted
//! run is a trusted artifact: Bastion's threat model is aligned contributors, not a
//! forged run file (see `docs/user-guide/governance.md`).
//!
//! The one boundary check it keeps is gate-verdict consistency: a gate recorded as a
//! `pass` that still carries a blocking finding contradicts itself, so the report
//! fails it closed rather than publishing a green check off it. The backends already
//! reject such a verdict upstream, so this is a fail-closed safeguard at the boundary,
//! not a recomputation of the gate.
//!
//! All the event-to-markdown and event-to-payload mapping here is pure and unit
//! tested; the only side effects are the [`GitHubApi`] calls in [`report`].

use std::collections::HashMap;
use std::fmt;

use color_eyre::eyre::{Context, Result};

use crate::event::{Gates, RunEvent};
use crate::reviewer::{Backend, Mode};
use crate::verdict::{Decision, Finding, FindingKind, Money, Usage};

use super::PrContext;
use super::client::{ApiRequest, GitHubApi, IssueComment, send_checked};

mod callouts;
mod checks;
mod comment;
mod post;
mod requests;

#[cfg(test)]
mod tests;

use callouts::*;
use checks::*;
use comment::*;
use requests::*;

pub use checks::Conclusion;
pub use post::{CommentAction, ReportSummary, report};

/// The hidden HTML marker that identifies Bastion's own sticky comment, so a
/// re-run finds and rewrites it instead of posting a duplicate. Invisible in the
/// rendered comment.
pub const MARKER: &str = "<!-- bastion-report -->";

/// The hosted walkthrough for creating the dedicated Bastion GitHub App. Linked
/// from the comment footer when the report is posting under the shared
/// `github-actions` identity (see [`SHARED_APP_SLUG`]).
const SETUP_URL: &str = "https://bastion.jessica.black/github-app";

/// The `app.slug` GitHub stamps on check runs created with the default Actions
/// `GITHUB_TOKEN`. Check runs created by a distinct GitHub App carry that app's
/// slug instead and form their own named check suite; ones created under this
/// shared identity cannot, so with other workflows on the commit they cluster
/// beneath one of those. Detecting this slug in a check-run response is how the
/// report decides, on its own, whether to nudge toward a dedicated app.
const SHARED_APP_SLUG: &str = "github-actions";

/// GitHub accepts at most 50 annotations per check-run request. We cap to that and
/// note any overflow in the check summary rather than silently dropping it.
const MAX_ANNOTATIONS: usize = 50;

/// GitHub caps a check-run annotation `message` (documented at 64KB). A single
/// oversized finding would 422 the whole report request, so we truncate the inline
/// message well under the limit; the full finding text still rides the sticky comment
/// and the reviewer check summary, so nothing is lost. Measured in characters, which
/// for any UTF-8 byte width stays comfortably below 64KB.
const MAX_ANNOTATION_MESSAGE: usize = 8000;

/// GitHub caps a check-run `output.summary` (and `output.text`) at 65535 bytes. The
/// summary embeds reviewer findings, so a single verbose finding could blow the limit
/// and 422 the whole request, failing an otherwise green job. We cap the assembled
/// summary well under that ceiling and point overflow at the sticky comment, which
/// carries the full text. Measured in characters: even all-4-byte content stays under
/// 65535 bytes.
const MAX_CHECK_SUMMARY: usize = 60000;

/// One reviewer's resolved row, distilled from the event stream.
#[derive(Debug, Clone)]
struct ReviewerRow {
    name: String,
    mode: Mode,
    backend: Option<Backend>,
    decision: Decision,
    summary: String,
    findings: Vec<Finding>,
    duration_ms: u64,
    usage: Option<Usage>,
    /// Whether this verdict was replayed from a signed local attestation rather
    /// than executed fresh (`docs/developer-guide/attestation.md`).
    replayed: bool,
    /// Whether this verdict was carried forward from the branch's previous run
    /// because its trigger-scoped diff was unchanged ([`crate::carry`]). Both
    /// surfaces carry: locally from the branch's prior local run, and in CI from
    /// its own prior CI run when the workflow persists and restores the run
    /// store. The report flags a carried reviewer in its per-reviewer check-run
    /// summary and, at comment level, in the [`carried_callout`].
    carried: bool,
    /// Whether an agent trigger omitted the full reviewer.
    skipped: bool,
}

impl ReviewerRow {
    /// The at-a-glance verdict word for this row. An advisor never gates, so it
    /// reads as `advisory` regardless of the decision the runner clamped to pass.
    fn verdict_word(&self) -> &'static str {
        if self.skipped {
            return "skipped";
        }
        match self.mode {
            Mode::Advisor => "advisory",
            Mode::Gate => self.decision.as_str(),
        }
    }

    /// Whether this row blocks the merge. A gate blocks when it decided to block, or
    /// when its recorded verdict contradicts itself: a `pass` that nonetheless carries
    /// a blocking finding, which mirrors [`crate::verdict::Verdict::is_consistent`].
    /// Such a verdict is not a coherent pass, so the report fails it closed rather than
    /// publishing a green check off it. The backends reject an inconsistent verdict
    /// upstream (see `claude_code.rs` and `codex.rs`), so this is a boundary safeguard,
    /// not a recomputation of the gate.
    ///
    /// Advisors never gate, so they never block, and the `mode == Mode::Gate` guard
    /// is what enforces that here. The runner now normalizes an advisor to a pass
    /// with only optional findings, so a well-formed advisor row carries no blocking
    /// finding at all; the guard still matters as a boundary safeguard, so a row from
    /// an older release (which kept the blocking kind on a clamped advisor pass) or a
    /// hand-edited store never blocks off an advisor's advice.
    fn blocks(&self) -> bool {
        self.mode == Mode::Gate
            && (self.decision == Decision::Block
                || self
                    .findings
                    .iter()
                    .any(|f| f.kind == FindingKind::Blocking))
    }
}

/// The whole run, distilled from its event stream into the shape both surfaces
/// render from.
#[derive(Debug, Clone, Default)]
struct RunDigest {
    branch: Option<String>,
    base: Option<String>,
    changed: u32,
    rows: Vec<ReviewerRow>,
    /// The recorded aggregate verdict from `run.completed`. `None` if the stream
    /// carried no completion event (a truncated run), in which case there is no
    /// decision to report and the aggregate reads as incomplete.
    aggregate: Option<Decision>,
    gates: Option<Gates>,
    cost: Option<Money>,
    duration_ms: Option<u64>,
    /// Total input tokens across reviewers, as recorded on `run.completed`. 0 when
    /// no backend reported usage or the run predates token tracking.
    tokens_in: u64,
    /// Total output tokens across reviewers, as recorded on `run.completed`.
    tokens_out: u64,
    /// Total cache-read input tokens across reviewers, as recorded on
    /// `run.completed`. 0 when no backend reported cache usage.
    cache_read: u64,
    /// The attestation replay audit trail, when a `run.attested` event was
    /// recorded: the reviewers replayed, the attesting key, and when it was
    /// signed. `None` when nothing was replayed.
    attested: Option<AttestedSummary>,
    /// Why an offered attestation was refused, when a `run.attestation-fallback`
    /// event was recorded. `None` when attestation replayed, was never attempted,
    /// or was simply never offered on this commit (a missing note is not a refusal
    /// and records no event), so an un-attested PR draws no attestation line.
    attestation_fallback: Option<String>,
    /// Whether the run was narrowed to a subset of the triggered reviewers
    /// (`bastion review --reviewer`). A partial verdict must never read as a
    /// full one, on any surface.
    partial: bool,
}

/// The replay audit trail folded from a `run.attested` event, for the sticky
/// comment's callout.
#[derive(Debug, Clone)]
struct AttestedSummary {
    reviewers: Vec<String>,
    public_key: String,
    attested_at: String,
}

/// Fold an event stream into a [`RunDigest`].
///
/// `reviewer.started` carries the backend and `run.started` carries each
/// reviewer's mode; both are joined onto the `reviewer.resolved` rows by name so a
/// row knows whether it gated and what ran it.
fn digest(events: &[RunEvent]) -> RunDigest {
    let mut digest = RunDigest::default();
    // Reviewer name -> mode (from run.started) and -> backend (from reviewer.started),
    // joined onto each reviewer.resolved row by name.
    let mut started: HashMap<String, Mode> = HashMap::new();
    let mut backends: HashMap<String, Backend> = HashMap::new();

    for event in events {
        match event {
            RunEvent::RunStarted {
                branch,
                base,
                changed,
                reviewers,
                partial,
                ..
            } => {
                digest.branch = Some(branch.clone());
                digest.base = Some(base.clone());
                digest.changed = *changed;
                // Recorded on both the opening and closing events; OR them so a
                // truncated stream (no run.completed) still reads as partial.
                digest.partial |= *partial;
                started = reviewers.iter().map(|r| (r.name.clone(), r.mode)).collect();
            }
            RunEvent::ReviewerStarted {
                reviewer, backend, ..
            } => {
                // First start wins, matching the prior first-match scan.
                backends.entry(reviewer.clone()).or_insert(*backend);
            }
            RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                summary,
                findings,
                usage,
                duration_ms,
                replayed,
                carried,
                ..
            } => {
                let mode = started.get(reviewer).copied().unwrap_or(Mode::Gate);
                let backend = backends.get(reviewer).copied();
                digest.rows.push(ReviewerRow {
                    name: reviewer.clone(),
                    mode,
                    backend,
                    decision: *verdict,
                    summary: summary.clone(),
                    findings: findings.clone(),
                    duration_ms: *duration_ms,
                    usage: *usage,
                    replayed: *replayed,
                    carried: *carried,
                    skipped: false,
                });
            }
            RunEvent::ReviewerSkipped {
                reviewer,
                mode,
                trigger,
                replayed,
                ..
            } => {
                digest.rows.push(ReviewerRow {
                    name: reviewer.clone(),
                    mode: *mode,
                    backend: Some(trigger.backend),
                    decision: Decision::Pass,
                    summary: trigger.reason.clone(),
                    findings: Vec::new(),
                    duration_ms: trigger.duration_ms,
                    usage: trigger.usage,
                    replayed: *replayed,
                    carried: false,
                    skipped: true,
                });
            }
            RunEvent::RunCompleted {
                verdict,
                gates,
                duration_ms,
                tokens_in,
                tokens_out,
                cache_read,
                cost_usd,
                partial,
                ..
            } => {
                digest.aggregate = Some(*verdict);
                digest.gates = Some(*gates);
                digest.duration_ms = Some(*duration_ms);
                digest.tokens_in = *tokens_in;
                digest.tokens_out = *tokens_out;
                digest.cache_read = *cache_read;
                digest.cost = Some(*cost_usd);
                digest.partial = *partial;
            }
            RunEvent::AttestationReplayed {
                reviewers,
                public_key,
                attested_at,
                ..
            } => {
                digest.attested = Some(AttestedSummary {
                    reviewers: reviewers.clone(),
                    public_key: public_key.clone(),
                    attested_at: attested_at.clone(),
                });
            }
            RunEvent::AttestationFallback { reason, .. } => {
                digest.attestation_fallback = Some(reason.clone());
            }
        }
    }
    digest
}

/// Whether any gate row blocks the merge (a recorded block, or a self-contradictory
/// gate pass). Used to fail the aggregate closed even when the recorded
/// `run.completed` verdict claims a pass.
fn any_gate_blocks(digest: &RunDigest) -> bool {
    digest.rows.iter().any(ReviewerRow::blocks)
}

/// The aggregate check conclusion for a digest, drawn from the recorded
/// `run.completed` verdict. A recorded pass is a success unless a gate row contradicts
/// itself (then it fails closed); a recorded block is a failure; a run that never
/// completed has no verdict to report, so it reads as a failure (an incomplete run is
/// not a pass).
fn aggregate_conclusion(digest: &RunDigest) -> Conclusion {
    if digest.aggregate == Some(Decision::Pass) && !any_gate_blocks(digest) {
        Conclusion::Success
    } else {
        Conclusion::Failure
    }
}

/// The distinguishable outcomes of a run's aggregate, the classification both the
/// check-run title ([`aggregate_check`]) and the sticky-comment headline
/// ([`status_line`]) branch on. Sharing one taxonomy keeps the two renderings
/// from drifting in what they call a blocked run versus a clean pass; each still
/// renders its own wording from the same shape.
enum AggregateOutcome {
    /// The aggregate is recorded as a pass, but a gate row contradicts itself (a
    /// pass carrying a blocking finding), so the run fails closed.
    BlockedInconsistent,
    /// A clean pass with no gates triggered.
    PassedNoGates,
    /// A clean pass: `passed` of `total` gates passed (with a clean aggregate,
    /// `passed == total`).
    Passed {
        passed: u32,
        total: u32,
        skipped: u32,
    },
    /// Blocked: `passed` of `total` gates passed.
    Blocked {
        passed: u32,
        total: u32,
        skipped: u32,
    },
    /// The run never completed, so there is no verdict to report.
    Incomplete,
}

impl AggregateOutcome {
    /// Classify a digest's aggregate. Mirrors [`aggregate_conclusion`]: a recorded
    /// pass is clean unless a gate row contradicts itself, in which case it fails
    /// closed as [`AggregateOutcome::BlockedInconsistent`].
    fn classify(digest: &RunDigest) -> Self {
        let (passed, total, skipped) = digest
            .gates
            .map_or((0, 0, 0), |g| (g.passed, g.total, g.skipped));
        match digest.aggregate {
            Some(Decision::Pass) if any_gate_blocks(digest) => Self::BlockedInconsistent,
            Some(Decision::Pass) if total == 0 => Self::PassedNoGates,
            Some(Decision::Pass) => Self::Passed {
                passed,
                total,
                skipped,
            },
            Some(Decision::Block) => Self::Blocked {
                passed,
                total,
                skipped,
            },
            None => Self::Incomplete,
        }
    }
}
