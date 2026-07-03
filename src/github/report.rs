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
//! clamps every advisor to a pass while keeping its findings. So this half does not
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

use std::fmt;

use color_eyre::eyre::{Context, Result, bail};

use crate::event::{Gates, RunEvent};
use crate::reviewer::{Backend, Mode};
use crate::verdict::{Decision, Finding, FindingKind, Money, Usage};

use super::PrContext;
use super::client::{ApiRequest, ApiResponse, GitHubApi, IssueComment};

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
}

impl ReviewerRow {
    /// The at-a-glance verdict word for this row. An advisor never gates, so it
    /// reads as `advisory` regardless of the decision the runner clamped to pass.
    fn verdict_word(&self) -> &'static str {
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
    /// Advisors never gate, so they never block: the runner clamps an advisor to a
    /// pass while keeping its findings, so an advisor pass carrying a blocking finding
    /// is the normal clamped state, not a block.
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
    /// Why an enabled attestation was not honored, when a
    /// `run.attestation-fallback` event was recorded. `None` when attestation
    /// either replayed or was never attempted.
    attestation_fallback: Option<String>,
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
    let mut started: Vec<(String, Mode)> = Vec::new();
    let mut backends: Vec<(String, Backend)> = Vec::new();

    for event in events {
        match event {
            RunEvent::RunStarted {
                branch,
                base,
                changed,
                reviewers,
                ..
            } => {
                digest.branch = Some(branch.clone());
                digest.base = Some(base.clone());
                digest.changed = *changed;
                started = reviewers.iter().map(|r| (r.name.clone(), r.mode)).collect();
            }
            RunEvent::ReviewerStarted {
                reviewer, backend, ..
            } => backends.push((reviewer.clone(), *backend)),
            RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                summary,
                findings,
                usage,
                duration_ms,
                replayed,
                ..
            } => {
                let mode = started
                    .iter()
                    .find(|(name, _)| name == reviewer)
                    .map_or(Mode::Gate, |(_, mode)| *mode);
                let backend = backends
                    .iter()
                    .find(|(name, _)| name == reviewer)
                    .map(|(_, backend)| *backend);
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
                ..
            } => {
                digest.aggregate = Some(*verdict);
                digest.gates = Some(*gates);
                digest.duration_ms = Some(*duration_ms);
                digest.tokens_in = *tokens_in;
                digest.tokens_out = *tokens_out;
                digest.cache_read = *cache_read;
                digest.cost = Some(*cost_usd);
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

// ---------------------------------------------------------------------------
// Sticky PR comment
// ---------------------------------------------------------------------------

/// Render the sticky PR comment body (Markdown), led by the hidden [`MARKER`].
///
/// `suggest_dedicated_app` adds a one-line footer nudge; the caller computes it from
/// the posting identity (see [`report`]).
///
/// `skills_warning` is a pre-rendered Markdown alert (from
/// [`crate::skills::DriftWarning::markdown`]) surfaced just under the headline when
/// the checked-out repo's bundled skills are missing or stale. The advisory appears
/// in the comment so a maintainer can see that agents may be working from stale
/// guidance. It never gates and does not touch any check-run conclusion.
fn comment_body(
    digest: &RunDigest,
    suggest_dedicated_app: bool,
    skills_warning: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(MARKER);
    out.push('\n');
    out.push_str("## Bastion review\n\n");
    out.push_str(&status_line(digest));
    out.push_str("\n\n");

    if let Some(warning) = skills_warning {
        out.push_str(warning);
        out.push('\n');
    }

    if let Some(attested) = &digest.attested {
        out.push_str(&attestation_callout(attested));
        out.push('\n');
    }
    if let Some(reason) = &digest.attestation_fallback {
        out.push_str(&format!("\n_Attestation was not honored: {reason}._\n\n"));
    }

    if digest.rows.is_empty() {
        out.push_str("No reviewers were triggered by this change.\n");
        out.push_str(&footer(suggest_dedicated_app));
        return out;
    }

    out.push_str(&reviewer_table(digest));

    let with_findings: Vec<&ReviewerRow> = digest
        .rows
        .iter()
        .filter(|r| !r.findings.is_empty())
        .collect();
    if !with_findings.is_empty() {
        out.push_str("\n### Findings\n");
        for row in with_findings {
            out.push_str(&format!("\n#### `{}` ({})\n", row.name, row.mode.as_str()));
            for finding in &row.findings {
                out.push_str(&finding_bullet(finding));
            }
        }
    }

    out.push_str(&footer(suggest_dedicated_app));
    out
}

/// The one-line headline: the aggregate decision plus the gate tally, run time,
/// token usage, and cost.
fn status_line(digest: &RunDigest) -> String {
    let reviewers = digest.rows.len();
    let (passed, total) = digest.gates.map_or((0, 0), |g| (g.passed, g.total));
    let timing = digest
        .duration_ms
        .map(|ms| format!(", {}s", ms / 1000))
        .unwrap_or_default();
    // Mirrors the local counter (both call `verdict::format_token_counter`):
    // omitted when no backend reported usage, so a mock or zero-reviewer run stays
    // clean.
    let tokens = crate::verdict::format_token_counter(
        digest.tokens_in,
        digest.tokens_out,
        digest.cache_read,
    )
    .map(|segment| format!(", {segment}"))
    .unwrap_or_default();
    let cost = digest
        .cost
        .filter(|c| c.cents() > 0)
        .map(|c| format!(", {c}"))
        .unwrap_or_default();

    let headline = match digest.aggregate {
        Some(Decision::Pass) if any_gate_blocks(digest) => {
            "**Blocked.** A gate verdict is internally inconsistent (a pass carrying a \
             blocking finding); failing closed."
                .to_string()
        }
        Some(Decision::Pass) => {
            if total == 0 {
                "**Passed.** No gates were triggered.".to_string()
            } else {
                format!("**Passed.** All {total} gate(s) passed.")
            }
        }
        Some(Decision::Block) => format!("**Blocked.** {passed} of {total} gate(s) passed."),
        None => "**Incomplete.** The run did not finish.".to_string(),
    };
    format!("{headline} {reviewers} reviewer(s) ran{timing}{tokens}{cost}.")
}

/// The prominent `[!NOTE]` callout opening the comment when one or more
/// reviewers replayed from a signed local attestation instead of executing
/// fresh in CI (`docs/developer-guide/attestation.md`, "Verification and
/// replay in CI"). Uses the same GitHub alert mechanism as the skills-drift
/// `[!WARNING]` block so both advisories read consistently.
fn attestation_callout(attested: &AttestedSummary) -> String {
    let names = attested
        .reviewers
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "> [!NOTE]\n\
         > {} replayed from a signed local attestation rather than executed fresh: {names}.\n\
         > Attested by {} at {}.\n",
        if attested.reviewers.len() == 1 {
            "1 reviewer was"
        } else {
            "reviewers were"
        },
        truncate_key(&attested.public_key),
        attested.attested_at,
    )
}

/// A truncated rendering of an SSH public key line for display: the key type
/// and a short prefix of the base64 material, plus the comment if the key
/// carries one. Keeps the callout from dumping the full base64 blob inline.
fn truncate_key(public_key: &str) -> String {
    let mut parts = public_key.split_whitespace();
    let kind = parts.next().unwrap_or("key");
    let material = parts.next().unwrap_or("");
    let comment = parts.next();

    let short_material: String = material.chars().take(12).collect();
    match comment {
        Some(comment) if !comment.is_empty() => {
            format!("{kind} {short_material}... ({comment})")
        }
        _ => format!("{kind} {short_material}..."),
    }
}

/// The reviewer summary table.
fn reviewer_table(digest: &RunDigest) -> String {
    let mut out =
        String::from("| Reviewer | Mode | Verdict | Summary |\n| --- | --- | --- | --- |\n");
    for row in &digest.rows {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            row.name,
            row.mode.as_str(),
            row.verdict_word(),
            escape_cell(&row.summary),
        ));
    }
    out
}

/// One finding rendered as a Markdown bullet. A located finding cites its path and
/// line range; a synthetic finding (the fail-closed reviewer-crash marker, which
/// has no path) is rendered without a location.
fn finding_bullet(finding: &Finding) -> String {
    let kind = match finding.kind {
        FindingKind::Blocking => "blocking",
        FindingKind::Optional => "optional",
    };
    if finding.path.is_empty() {
        format!("- **{kind}**: {}\n", finding.detail.trim())
    } else {
        format!(
            "- **{kind}** `{}`: {}\n",
            location(&finding.path, finding.line_start, finding.line_end),
            finding.detail.trim(),
        )
    }
}

/// `path:line` or `path:start-end` for a finding's location.
fn location(path: &str, start: u32, end: u32) -> String {
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

/// Neutralize Markdown table-breaking characters in a free-text cell: pipes would
/// start a new column and newlines would end the row.
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// The trailing note. Always credits Bastion and points at the run artifact; when
/// the report is posting under the shared Actions identity, it also nudges toward a
/// dedicated app so the checks group on their own instead of under a sibling workflow.
fn footer(suggest_dedicated_app: bool) -> String {
    let mut out = String::from(
        "\n<sub>Posted by Bastion. Full transcripts are attached to the workflow run as an artifact.",
    );
    if suggest_dedicated_app {
        out.push_str(&format!(
            " These checks were posted under the shared GitHub Actions app, so with other \
             workflows on the commit they can cluster under one of those; [set up a dedicated \
             app]({SETUP_URL}) to give them their own group."
        ));
    }
    out.push_str("</sub>\n");
    out
}

// ---------------------------------------------------------------------------
// Check runs
// ---------------------------------------------------------------------------

/// A check-run conclusion, limited to the two the adapter emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    /// A passing gate, or any advisor.
    Success,
    /// A blocking gate (or the aggregate when any gate blocked).
    Failure,
}

impl Conclusion {
    fn as_str(self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
        }
    }
}

/// A check-run annotation: a located finding pinned to the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Annotation {
    path: String,
    start_line: u32,
    end_line: u32,
    level: &'static str,
    message: String,
}

/// A fully-resolved check run ready to POST.
#[derive(Debug, Clone)]
struct CheckRun {
    name: String,
    head_sha: String,
    conclusion: Conclusion,
    title: String,
    summary: String,
    annotations: Vec<Annotation>,
}

/// Build the per-reviewer check runs plus the aggregate `bastion` check.
///
/// The aggregate is always present so it can serve as the single stable required
/// check, even when zero reviewers matched (a trivial pass).
fn check_runs(ctx: &PrContext, digest: &RunDigest) -> Vec<CheckRun> {
    let mut checks: Vec<CheckRun> = digest
        .rows
        .iter()
        .map(|row| reviewer_check(ctx, row))
        .collect();
    checks.push(aggregate_check(ctx, digest));
    checks
}

/// The check run for one reviewer, reflecting its recorded row. A gate that blocked
/// concludes failure; a passing gate concludes success; an advisor always concludes
/// success (it never gates) and carries its findings along.
fn reviewer_check(ctx: &PrContext, row: &ReviewerRow) -> CheckRun {
    let (conclusion, decision_word) = match row.mode {
        Mode::Advisor => (Conclusion::Success, "Advisory"),
        Mode::Gate if row.blocks() => (Conclusion::Failure, "Blocked"),
        Mode::Gate => (Conclusion::Success, "Passed"),
    };
    let title = format!("{decision_word}: {}", truncate(&row.summary, 110));

    let annotations = annotations_for(&row.findings);
    let summary = cap_check_summary(reviewer_check_summary(row, &annotations));

    CheckRun {
        name: format!("bastion / {}", row.name),
        head_sha: ctx.head_sha.clone(),
        conclusion,
        title,
        summary,
        annotations,
    }
}

/// The aggregate `bastion` check, reflecting the whole-run gate as the runner
/// recorded it.
fn aggregate_check(ctx: &PrContext, digest: &RunDigest) -> CheckRun {
    let conclusion = aggregate_conclusion(digest);
    let (passed, total) = digest.gates.map_or((0, 0), |g| (g.passed, g.total));
    let title = match digest.aggregate {
        Some(Decision::Pass) if any_gate_blocks(digest) => {
            "Blocked: a gate verdict is internally inconsistent".to_string()
        }
        Some(Decision::Pass) => {
            if total == 0 {
                "No gates triggered".to_string()
            } else {
                format!("{passed}/{total} gates passed")
            }
        }
        Some(Decision::Block) => format!("Blocked: {passed}/{total} gates passed"),
        None => "Incomplete run".to_string(),
    };

    let mut summary = String::new();
    summary.push_str(&status_line(digest));
    summary.push_str("\n\n");
    if digest.rows.is_empty() {
        summary.push_str("No reviewers were triggered by this change.\n");
    } else {
        summary.push_str(&reviewer_table(digest));
    }

    CheckRun {
        name: "bastion".to_string(),
        head_sha: ctx.head_sha.clone(),
        conclusion,
        title,
        summary: cap_check_summary(summary),
        annotations: Vec::new(),
    }
}

/// The Markdown body of a per-reviewer check run: a small metadata block, the
/// reviewer's own summary, and its findings.
fn reviewer_check_summary(row: &ReviewerRow, annotations: &[Annotation]) -> String {
    let backend = row.backend.map_or("unknown", Backend::as_str);
    let mut out = format!(
        "- Mode: {}\n- Agent: {backend}\n- Verdict: {}\n- Duration: {}s\n",
        row.mode.as_str(),
        row.verdict_word(),
        row.duration_ms / 1000,
    );
    if row.replayed {
        out.push_str("- Replayed from an attested local run rather than executed fresh.\n");
    }
    if let Some(usage) = row.usage {
        let cached = if usage.cache_read > 0 {
            format!(", {} cached", usage.cache_read)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "- Tokens: {} in, {} out{cached} ({})\n",
            usage.tokens_in, usage.tokens_out, usage.cost_usd,
        ));
    }
    out.push('\n');
    out.push_str(&row.summary);
    out.push('\n');

    if !row.findings.is_empty() {
        out.push_str("\n**Findings**\n\n");
        for finding in &row.findings {
            out.push_str(&finding_bullet(finding));
        }
    }
    // Annotations are capped per request; if findings overflowed the cap, say so.
    let located = row.findings.iter().filter(|f| is_locatable(f)).count();
    if located > annotations.len() {
        out.push_str(&format!(
            "\n_{} more located finding(s) are listed above but not pinned to the diff (annotation cap)._\n",
            located - annotations.len(),
        ));
    }
    out
}

/// Whether a finding can become a check annotation: it needs a real path and a
/// first line that is at least 1 (GitHub rejects line 0). The synthetic
/// reviewer-crash finding has neither.
fn is_locatable(finding: &Finding) -> bool {
    !finding.path.is_empty() && finding.line_start >= 1
}

/// The annotation `message` for a finding, truncated to [`MAX_ANNOTATION_MESSAGE`]
/// so a single long finding cannot 422 the whole report request. When cut, it points
/// the reader to the sticky comment, which always carries the full finding text.
fn annotation_message(detail: &str) -> String {
    let detail = detail.trim();
    if detail.chars().count() <= MAX_ANNOTATION_MESSAGE {
        return detail.to_string();
    }
    let kept: String = detail.chars().take(MAX_ANNOTATION_MESSAGE).collect();
    format!(
        "{}\n\n(truncated; see the Bastion comment for the full finding.)",
        kept.trim_end()
    )
}

/// Cap an assembled check-run summary at [`MAX_CHECK_SUMMARY`] so a verbose finding
/// cannot push `output.summary` past GitHub's 65535-byte limit and 422 the request.
/// When cut, it points the reader at the sticky comment, which carries every finding
/// in full.
fn cap_check_summary(summary: String) -> String {
    if summary.chars().count() <= MAX_CHECK_SUMMARY {
        return summary;
    }
    let kept: String = summary.chars().take(MAX_CHECK_SUMMARY).collect();
    format!(
        "{}\n\n(truncated; see the Bastion comment for the full findings.)\n",
        kept.trim_end()
    )
}

/// Map a reviewer's locatable findings to check annotations, capped at
/// [`MAX_ANNOTATIONS`] in count and [`MAX_ANNOTATION_MESSAGE`] per message.
fn annotations_for(findings: &[Finding]) -> Vec<Annotation> {
    findings
        .iter()
        .filter(|f| is_locatable(f))
        .take(MAX_ANNOTATIONS)
        .map(|f| Annotation {
            path: f.path.clone(),
            start_line: f.line_start,
            // GitHub requires end_line >= start_line.
            end_line: f.line_end.max(f.line_start),
            level: match f.kind {
                FindingKind::Blocking => "failure",
                FindingKind::Optional => "warning",
            },
            message: annotation_message(&f.detail),
        })
        .collect()
}

/// Truncate `text` to `max` characters, adding an ellipsis marker when cut. Kept
/// ASCII (`...`) to match the house style.
fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

/// `GET` the PR's issue comments (to find an existing sticky comment).
fn comment_list_request(ctx: &PrContext) -> ApiRequest {
    ApiRequest::get(format!(
        "/repos/{}/{}/issues/{}/comments?per_page=100",
        ctx.owner, ctx.repo, ctx.pr
    ))
}

/// `POST` a new issue comment.
fn comment_create_request(ctx: &PrContext, body: &str) -> ApiRequest {
    ApiRequest::post(
        format!(
            "/repos/{}/{}/issues/{}/comments",
            ctx.owner, ctx.repo, ctx.pr
        ),
        serde_json::json!({ "body": body }),
    )
}

/// `PATCH` an existing issue comment in place.
fn comment_update_request(ctx: &PrContext, comment_id: u64, body: &str) -> ApiRequest {
    ApiRequest::patch(
        format!(
            "/repos/{}/{}/issues/comments/{}",
            ctx.owner, ctx.repo, comment_id
        ),
        serde_json::json!({ "body": body }),
    )
}

/// `POST` a completed check run.
fn check_run_request(ctx: &PrContext, check: &CheckRun) -> ApiRequest {
    let annotations: Vec<serde_json::Value> = check
        .annotations
        .iter()
        .map(|a| {
            serde_json::json!({
                "path": a.path,
                "start_line": a.start_line,
                "end_line": a.end_line,
                "annotation_level": a.level,
                "message": a.message,
            })
        })
        .collect();
    ApiRequest::post(
        format!("/repos/{}/{}/check-runs", ctx.owner, ctx.repo),
        serde_json::json!({
            "name": check.name,
            "head_sha": check.head_sha,
            "status": "completed",
            "conclusion": check.conclusion.as_str(),
            "output": {
                "title": check.title,
                "summary": check.summary,
                "annotations": annotations,
            },
        }),
    )
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// What the report did to the sticky comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentAction {
    /// Posted a new comment.
    Created,
    /// Updated the existing sticky comment in place.
    Updated(u64),
}

/// A short account of what the report posted, for the CLI to print.
#[derive(Debug, Clone, Copy)]
pub struct ReportSummary {
    /// What happened to the sticky comment.
    pub comment: CommentAction,
    /// How many check runs were created (per reviewer plus the aggregate).
    pub checks: usize,
}

impl fmt::Display for ReportSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let comment = match self.comment {
            CommentAction::Created => "posted a new PR comment".to_string(),
            CommentAction::Updated(id) => format!("updated PR comment {id}"),
        };
        write!(f, "{comment}; created {} check run(s)", self.checks)
    }
}

/// Post a finished run's results to its pull request.
///
/// Creates a check run per reviewer plus the aggregate `bastion` check, then upserts
/// the sticky comment. Any non-2xx response aborts with a legible error.
///
/// The checks go first on purpose: GitHub stamps each created check run with the
/// `app` that posted it, so the first response tells the report which identity it is
/// acting under. When that is the shared `github-actions` app (the default
/// `GITHUB_TOKEN`, with no dedicated app configured), the checks cannot form their
/// own suite, so the comment closes with a nudge toward setting one up. This is
/// decided here from GitHub's own response, independent of how the workflow is
/// written.
///
/// `skills_warning` is an optional skills-freshness advisory folded into the comment
/// when the repo's bundled skills are missing or stale. It arrives as a parsed
/// [`crate::skills::DriftWarning`] (not raw Markdown), so only an advisory produced
/// by `skills::assess` can reach the comment, never arbitrary caller-supplied text.
/// It is advisory only and leaves every check-run conclusion untouched.
///
/// # Errors
///
/// Returns an error if a GitHub request fails to send or returns a non-2xx status.
pub async fn report<A: GitHubApi + ?Sized>(
    api: &A,
    ctx: &PrContext,
    events: &[RunEvent],
    skills_warning: Option<&crate::skills::DriftWarning>,
) -> Result<ReportSummary> {
    let digest = digest(events);

    let checks = check_runs(ctx, &digest);
    // The identity we posted under, classified from the first check-run response.
    let mut posting_app = PostingApp::Unknown;
    for (i, check) in checks.iter().enumerate() {
        let resp = send_checked(api, &check_run_request(ctx, check)).await?;
        if i == 0 {
            posting_app = PostingApp::from_check_run(&resp.body);
        }
    }

    // Render the parsed advisory to Markdown at the boundary of the private comment
    // builder, so the public API keeps requiring the proof type.
    let warning_md = skills_warning.map(crate::skills::DriftWarning::markdown);
    let body = comment_body(
        &digest,
        posting_app.should_suggest_dedicated_app(),
        warning_md.as_deref(),
    );
    let comment = upsert_comment(api, ctx, &body).await?;

    Ok(ReportSummary {
        comment,
        checks: checks.len(),
    })
}

/// Which GitHub App created the check runs, parsed once from the `app.slug` GitHub
/// stamps on a check-run response. This is the report's posting identity, and it is
/// what decides whether the checks can form their own check suite. Parsing it into a
/// type here keeps the raw `github-actions` string comparison out of [`report`] and
/// names the three cases so none is forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostingApp {
    /// The shared `github-actions` app (the default `GITHUB_TOKEN`). Its check runs
    /// cannot form their own suite, so the report nudges toward a dedicated app.
    SharedActions,
    /// A distinct GitHub App: its check runs get their own named suite, so no nudge.
    Dedicated,
    /// The response carried no readable `app.slug` (an unexpected or fake shape).
    /// Leave the nudge off rather than guess.
    Unknown,
}

impl PostingApp {
    /// Classify the creating app from a check-run creation response. GitHub always
    /// stamps `app.slug` on a real response; a fake or truncated body may not, which
    /// maps to [`PostingApp::Unknown`].
    fn from_check_run(check_run_body: &serde_json::Value) -> Self {
        match check_run_body
            .get("app")
            .and_then(|app| app.get("slug"))
            .and_then(|slug| slug.as_str())
        {
            Some(SHARED_APP_SLUG) => Self::SharedActions,
            Some(_) => Self::Dedicated,
            None => Self::Unknown,
        }
    }

    /// Whether the sticky comment should nudge toward a dedicated app.
    fn should_suggest_dedicated_app(self) -> bool {
        matches!(self, Self::SharedActions)
    }
}

/// Create the sticky comment, or update it in place if one already exists.
async fn upsert_comment<A: GitHubApi + ?Sized>(
    api: &A,
    ctx: &PrContext,
    body: &str,
) -> Result<CommentAction> {
    let listed = send_checked(api, &comment_list_request(ctx)).await?;
    match find_marker_comment(&listed.body)? {
        Some(id) => {
            send_checked(api, &comment_update_request(ctx, id, body)).await?;
            Ok(CommentAction::Updated(id))
        }
        None => {
            send_checked(api, &comment_create_request(ctx, body)).await?;
            Ok(CommentAction::Created)
        }
    }
}

/// Find the id of Bastion's own sticky comment in a comment-list response, by its
/// hidden [`MARKER`].
///
/// Fails closed on a malformed list body rather than collapsing a parse error into
/// "no existing comment": treating an unexpected response shape as "none found"
/// would post a fresh comment on every run, stacking duplicates. A body Bastion
/// cannot parse is an error to surface, not a silent create.
///
/// # Errors
///
/// Returns an error if the response body is not the expected array of comments.
fn find_marker_comment(list_body: &serde_json::Value) -> Result<Option<u64>> {
    let comments: Vec<IssueComment> = serde_json::from_value(list_body.clone())
        .wrap_err("parsing the PR comment list from GitHub")?;
    Ok(comments
        .into_iter()
        .find(|c| c.body.contains(MARKER))
        .map(|c| c.id))
}

/// Send a request and treat any non-2xx status as an error, surfacing GitHub's
/// own message. The fail-closed posture: a reporting call that GitHub rejected is
/// a real failure, not something to swallow.
async fn send_checked<A: GitHubApi + ?Sized>(api: &A, req: &ApiRequest) -> Result<ApiResponse> {
    let resp = api.send(req).await?;
    if !resp.is_success() {
        bail!(
            "GitHub {} {} returned {}: {}",
            req.method.as_str(),
            req.path,
            resp.status,
            resp.error_message().unwrap_or("(no message)"),
        );
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ReviewerRef, RunId};
    use crate::github::client::test_support::RecordingClient;
    use crate::github::client::{ApiResponse, Method};

    fn ctx() -> PrContext {
        PrContext {
            owner: "acme".into(),
            repo: "app".into(),
            pr: 12,
            head_sha: "deadbeef".into(),
        }
    }

    fn finding(kind: FindingKind, path: &str, start: u32, end: u32, detail: &str) -> Finding {
        Finding {
            kind,
            path: path.into(),
            line_start: start,
            line_end: end,
            detail: detail.into(),
        }
    }

    /// A representative run: one blocking gate (with a blocking and an optional
    /// finding), one passing gate, one advisor with an optional finding.
    fn sample_events() -> Vec<RunEvent> {
        let run = RunId("r-1".into());
        vec![
            RunEvent::RunStarted {
                run: run.clone(),
                branch: "feat/cart".into(),
                base: "main".into(),
                changed: 3,
                reviewers: vec![
                    ReviewerRef {
                        name: "tenant-isolation".into(),
                        mode: Mode::Gate,
                    },
                    ReviewerRef {
                        name: "file-responsibility".into(),
                        mode: Mode::Gate,
                    },
                    ReviewerRef {
                        name: "style".into(),
                        mode: Mode::Advisor,
                    },
                ],
            },
            RunEvent::ReviewerStarted {
                run: run.clone(),
                reviewer: "tenant-isolation".into(),
                mode: Mode::Gate,
                backend: Backend::Codex,
            },
            RunEvent::ReviewerResolved {
                run: run.clone(),
                reviewer: "tenant-isolation".into(),
                verdict: Decision::Block,
                summary: "A query reads rows without scoping by tenant id.".into(),
                findings: vec![
                    finding(
                        FindingKind::Blocking,
                        "src/db.ts",
                        88,
                        91,
                        "scope by tenant_id",
                    ),
                    finding(
                        FindingKind::Optional,
                        "src/db.ts",
                        10,
                        10,
                        "consider an index",
                    ),
                ],
                usage: Some(Usage {
                    tokens_in: 1820,
                    tokens_out: 156,
                    cache_read: 900,
                    cost_usd: Money::from_cents(21),
                }),
                duration_ms: 38_000,
                has_transcript: true,
                replayed: false,
            },
            RunEvent::ReviewerResolved {
                run: run.clone(),
                reviewer: "file-responsibility".into(),
                verdict: Decision::Pass,
                summary: "Responsibilities look well separated.".into(),
                findings: vec![],
                usage: None,
                duration_ms: 12_000,
                has_transcript: true,
                replayed: false,
            },
            RunEvent::ReviewerResolved {
                run: run.clone(),
                reviewer: "style".into(),
                verdict: Decision::Pass,
                summary: "A couple of nits.".into(),
                findings: vec![finding(
                    FindingKind::Optional,
                    "src/x.ts",
                    4,
                    4,
                    "rename foo",
                )],
                usage: None,
                duration_ms: 5_000,
                has_transcript: true,
                replayed: false,
            },
            RunEvent::RunCompleted {
                run,
                verdict: Decision::Block,
                gates: Gates {
                    total: 2,
                    passed: 1,
                    blocked: 1,
                },
                duration_ms: 40_000,
                tokens_in: 1820,
                tokens_out: 156,
                cache_read: 900,
                cost_usd: Money::from_cents(21),
            },
        ]
    }

    #[test]
    fn comment_surfaces_every_finding_including_optional() {
        let body = comment_body(&digest(&sample_events()), false, None);
        // Marker for in-place upsert, and the headline.
        assert!(body.starts_with(MARKER));
        assert!(body.contains("**Blocked.** 1 of 2 gate(s) passed."));
        // The table lists all three reviewers with their verdict words.
        assert!(body.contains("| `tenant-isolation` | gate | block |"));
        assert!(body.contains("| `style` | advisor | advisory |"));
        // Both a blocking and an optional finding are rendered, with locations...
        assert!(body.contains("- **blocking** `src/db.ts:88-91`: scope by tenant_id"));
        assert!(body.contains("- **optional** `src/db.ts:10`: consider an index"));
        // ...including the advisor's optional finding, which never gates.
        assert!(body.contains("- **optional** `src/x.ts:4`: rename foo"));
        // No em dashes leaked into generated prose.
        assert!(!body.contains('\u{2014}') && !body.contains('\u{2013}'));
    }

    #[test]
    fn status_line_carries_time_tokens_cache_and_cost_in_order() {
        // The sample run's only reviewer with usage reported 1820 in / 156 out / 900
        // cached, so the aggregate counter mirrors the local one: time, then tokens
        // (with the cache-read figure), then cost.
        let line = status_line(&digest(&sample_events()));
        assert!(
            line.contains("3 reviewer(s) ran, 40s, 1820 in / 156 out / 900 cached tokens, $0.21.")
        );
    }

    #[test]
    fn status_line_omits_tokens_when_none_were_reported() {
        // A zero-reviewer run reports no usage; the counter drops the token segment
        // rather than printing "0 in / 0 out tokens".
        let events = vec![
            RunEvent::RunStarted {
                run: RunId("r".into()),
                branch: "b".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            },
            RunEvent::RunCompleted {
                run: RunId("r".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        let line = status_line(&digest(&events));
        assert!(!line.contains("tokens"), "no token segment: {line}");
    }

    #[test]
    fn comment_handles_zero_reviewers() {
        let events = vec![
            RunEvent::RunStarted {
                run: RunId("r".into()),
                branch: "b".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            },
            RunEvent::RunCompleted {
                run: RunId("r".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        let body = comment_body(&digest(&events), false, None);
        assert!(body.contains("No gates were triggered."));
        assert!(body.contains("No reviewers were triggered"));
        // With the nudge off, the footer carries no dedicated-app note.
        assert!(!body.contains(SETUP_URL));
    }

    #[test]
    fn comment_footer_nudges_to_a_dedicated_app_when_asked() {
        // The nudge rides the footer in both the populated and the zero-reviewer
        // shapes, so a passing trivial run still surfaces it.
        let populated = comment_body(&digest(&sample_events()), true, None);
        assert!(populated.contains(SETUP_URL));
        assert!(populated.contains("shared GitHub Actions app"));

        let empty_events = vec![
            RunEvent::RunStarted {
                run: RunId("r".into()),
                branch: "b".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            },
            RunEvent::RunCompleted {
                run: RunId("r".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        assert!(comment_body(&digest(&empty_events), true, None).contains(SETUP_URL));
        // No Unicode dashes slipped into the nudge prose.
        assert!(!populated.contains('\u{2014}') && !populated.contains('\u{2013}'));
    }

    #[test]
    fn comment_folds_in_a_skills_warning_when_given_one() {
        // The advisory rides just under the headline, above the reviewer table, and
        // only when supplied: a `None` warning leaves the comment untouched.
        let digest = digest(&sample_events());
        let warning = "> [!WARNING]\n> skills are stale; run `bastion skills install`.\n";

        let with = comment_body(&digest, false, Some(warning));
        assert!(with.contains("> [!WARNING]"));
        assert!(with.contains("skills are stale"));
        // It sits after the status headline but before the reviewer table.
        let warn_at = with.find("[!WARNING]").unwrap();
        let table_at = with.find("| Reviewer |").unwrap();
        let headline_at = with.find("reviewer(s) ran").unwrap();
        assert!(headline_at < warn_at && warn_at < table_at);

        let without = comment_body(&digest, false, None);
        assert!(!without.contains("[!WARNING]"));
    }

    #[test]
    fn comment_folds_in_a_skills_warning_on_a_zero_reviewer_run() {
        // The zero-reviewer comment returns early through its own branch; the warning
        // is inserted before that branch, so it must still ride a no-reviewer run.
        let events = vec![
            RunEvent::RunStarted {
                run: RunId("r".into()),
                branch: "b".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            },
            RunEvent::RunCompleted {
                run: RunId("r".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        let warning = "> [!WARNING]\n> skills are stale.\n";
        let body = comment_body(&digest(&events), false, Some(warning));
        assert!(body.contains("> [!WARNING]"));
        // It lands after the headline but before the zero-reviewer sentence.
        let warn_at = body.find("[!WARNING]").unwrap();
        let none_at = body.find("No reviewers were triggered").unwrap();
        assert!(warn_at < none_at, "warning must precede the empty-run note");
    }

    /// [`sample_events`] with `tenant-isolation`'s resolved row marked
    /// `replayed: true` and a matching `run.attested` audit event appended.
    fn sample_events_with_replay() -> Vec<RunEvent> {
        let mut events = sample_events();
        for event in &mut events {
            if let RunEvent::ReviewerResolved {
                reviewer, replayed, ..
            } = event
                && reviewer == "tenant-isolation"
            {
                *replayed = true;
            }
        }
        events.push(RunEvent::AttestationReplayed {
            run: RunId("r-1".into()),
            reviewers: vec!["tenant-isolation".into()],
            public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZQ== ada@example.com"
                .into(),
            attested_at: "2026-07-01T12:00:00Z".into(),
        });
        events
    }

    #[test]
    fn comment_opens_with_a_replay_callout_naming_reviewers_key_and_timestamp() {
        let digest = digest(&sample_events_with_replay());
        let body = comment_body(&digest, false, None);
        assert!(body.contains("[!NOTE]"));
        assert!(body.contains("`tenant-isolation`"));
        assert!(body.contains("2026-07-01T12:00:00Z"));
        // The full base64 blob is not dumped verbatim; only a truncated form.
        assert!(!body.contains("AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZQ=="));
        assert!(body.contains("ssh-ed25519"));
        assert!(body.contains("ada@example.com"));

        // The callout sits after the headline, before the reviewer table.
        let headline_at = body.find("reviewer(s) ran").unwrap();
        let note_at = body.find("[!NOTE]").unwrap();
        let table_at = body.find("| Reviewer |").unwrap();
        assert!(headline_at < note_at && note_at < table_at);
    }

    #[test]
    fn comment_has_no_callout_when_nothing_replayed() {
        let digest = digest(&sample_events());
        let body = comment_body(&digest, false, None);
        assert!(!body.contains("[!NOTE]"));
    }

    #[test]
    fn comment_notes_a_fallback_reason_in_one_unobtrusive_line() {
        let mut events = sample_events();
        events.push(RunEvent::AttestationFallback {
            run: RunId("r-1".into()),
            reason: "no attestation note found on HEAD".into(),
        });
        let body = comment_body(&digest(&events), false, None);
        assert!(body.contains("no attestation note found on HEAD"));
        assert!(!body.contains("[!NOTE]"), "a fallback is not a replay");
    }

    #[test]
    fn replayed_reviewer_check_summary_states_it_was_replayed() {
        let digest = digest(&sample_events_with_replay());
        let checks = check_runs(&ctx(), &digest);
        let replayed_check = checks
            .iter()
            .find(|c| c.name == "bastion / tenant-isolation")
            .unwrap();
        assert!(
            replayed_check
                .summary
                .contains("Replayed from an attested local run")
        );

        // A non-replayed reviewer's summary carries no such note.
        let fresh_check = checks
            .iter()
            .find(|c| c.name == "bastion / file-responsibility")
            .unwrap();
        assert!(!fresh_check.summary.contains("Replayed"));
    }

    #[test]
    fn truncate_key_shortens_the_material_and_keeps_type_and_comment() {
        let long = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZWtleW1hdGVyaWFsaGVyZQ== ada@example.com";
        let short = truncate_key(long);
        assert!(short.starts_with("ssh-ed25519 AAAAC3NzaC1l"));
        assert!(short.contains("..."));
        assert!(short.contains("(ada@example.com)"));
        assert!(short.len() < long.len());
    }

    #[test]
    fn comment_cell_escaping_keeps_the_table_intact() {
        // A summary with a pipe and a newline must not break the row.
        let cell = escape_cell("a | b\nc");
        assert_eq!(cell, "a \\| b c");
    }

    #[test]
    fn check_runs_map_gate_and_advisor_conclusions() {
        let checks = check_runs(&ctx(), &digest(&sample_events()));
        // Per reviewer plus the aggregate.
        assert_eq!(checks.len(), 4);

        let by_name = |name: &str| checks.iter().find(|c| c.name == name).unwrap().clone();

        let blocked = by_name("bastion / tenant-isolation");
        assert_eq!(blocked.conclusion, Conclusion::Failure);
        assert!(blocked.title.starts_with("Blocked:"));
        // Its blocking finding becomes a failure annotation; the optional one a warning.
        assert_eq!(blocked.annotations.len(), 2);
        assert_eq!(blocked.annotations[0].level, "failure");
        assert_eq!(blocked.annotations[1].level, "warning");
        assert_eq!(blocked.head_sha, "deadbeef");
        // The per-reviewer summary lists its token usage, including the cache-read
        // figure when nonzero.
        assert!(
            blocked
                .summary
                .contains("- Tokens: 1820 in, 156 out, 900 cached ($0.21)")
        );

        // The advisor, even with a finding, concludes success and never gates.
        let advisor = by_name("bastion / style");
        assert_eq!(advisor.conclusion, Conclusion::Success);
        assert!(advisor.title.starts_with("Advisory:"));

        // The aggregate reflects the blocked run and carries no annotations.
        let aggregate = by_name("bastion");
        assert_eq!(aggregate.conclusion, Conclusion::Failure);
        assert!(aggregate.annotations.is_empty());
        assert!(aggregate.title.contains("1/2"));
    }

    #[test]
    fn aggregate_reads_incomplete_run_as_failure() {
        // A stream with no run.completed has no recorded verdict, so the aggregate
        // cannot read as a pass: an incomplete run concludes failure.
        let events = vec![RunEvent::RunStarted {
            run: RunId("r".into()),
            branch: "b".into(),
            base: "main".into(),
            changed: 1,
            reviewers: vec![],
        }];
        let checks = check_runs(&ctx(), &digest(&events));
        let aggregate = checks.iter().find(|c| c.name == "bastion").unwrap();
        assert_eq!(aggregate.conclusion, Conclusion::Failure);
        assert_eq!(aggregate.title, "Incomplete run");
    }

    /// Build a run: it starts `started_gates` gates, resolves `rows` of them, and
    /// records `completed` (the aggregate verdict and tally).
    fn recorded_run(
        started_gates: &[&str],
        rows: Vec<RunEvent>,
        completed: (Decision, Gates),
    ) -> Vec<RunEvent> {
        let run = RunId("r-rec".into());
        let mut events = vec![RunEvent::RunStarted {
            run: run.clone(),
            branch: "feat".into(),
            base: "main".into(),
            changed: 1,
            reviewers: started_gates
                .iter()
                .map(|name| ReviewerRef {
                    name: (*name).into(),
                    mode: Mode::Gate,
                })
                .collect(),
        }];
        events.extend(rows);
        events.push(RunEvent::RunCompleted {
            run,
            verdict: completed.0,
            gates: completed.1,
            duration_ms: 1000,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        });
        events
    }

    fn gate_resolved(name: &str, verdict: Decision, findings: Vec<Finding>) -> RunEvent {
        RunEvent::ReviewerResolved {
            run: RunId("r-rec".into()),
            reviewer: name.into(),
            verdict,
            summary: format!("{name} summary"),
            findings,
            usage: None,
            duration_ms: 1000,
            has_transcript: true,
            replayed: false,
        }
    }

    #[test]
    fn clean_pass_with_gates_concludes_success() {
        // A recorded pass: the aggregate and the per-reviewer gate both go green.
        let events = recorded_run(
            &["g1"],
            vec![gate_resolved("g1", Decision::Pass, vec![])],
            (
                Decision::Pass,
                Gates {
                    total: 1,
                    passed: 1,
                    blocked: 0,
                },
            ),
        );
        let digest = digest(&events);
        let checks = check_runs(&ctx(), &digest);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion")
                .unwrap()
                .conclusion,
            Conclusion::Success
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion / g1")
                .unwrap()
                .conclusion,
            Conclusion::Success
        );
    }

    #[test]
    fn recorded_block_concludes_failure() {
        // A recorded block: the aggregate fails and the blocking gate's check fails.
        let events = recorded_run(
            &["g1"],
            vec![gate_resolved(
                "g1",
                Decision::Block,
                vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
            )],
            (
                Decision::Block,
                Gates {
                    total: 1,
                    passed: 0,
                    blocked: 1,
                },
            ),
        );
        let digest = digest(&events);
        let checks = check_runs(&ctx(), &digest);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion")
                .unwrap()
                .conclusion,
            Conclusion::Failure
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion / g1")
                .unwrap()
                .conclusion,
            Conclusion::Failure
        );
        // The comment headline reflects the recorded block.
        assert!(comment_body(&digest, false, None).contains("**Blocked.** 0 of 1 gate(s) passed."));
    }

    #[test]
    fn self_contradictory_gate_pass_fails_closed() {
        // A gate recorded as `pass` that carries a blocking finding contradicts itself
        // (the backends reject this upstream, but the report fails closed at the
        // boundary regardless). Even though run.completed recorded a pass, the gate's
        // own check and the aggregate both fail rather than publishing a green check.
        let events = recorded_run(
            &["g1"],
            vec![gate_resolved(
                "g1",
                Decision::Pass,
                vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
            )],
            (
                Decision::Pass,
                Gates {
                    total: 1,
                    passed: 1,
                    blocked: 0,
                },
            ),
        );
        let digest = digest(&events);
        let checks = check_runs(&ctx(), &digest);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion / g1")
                .unwrap()
                .conclusion,
            Conclusion::Failure
        );
        let aggregate = checks.iter().find(|c| c.name == "bastion").unwrap();
        assert_eq!(aggregate.conclusion, Conclusion::Failure);
        assert!(aggregate.title.contains("internally inconsistent"));
        // The comment headline fails closed rather than claiming a pass.
        assert!(comment_body(&digest, false, None).contains("internally inconsistent"));
    }

    #[test]
    fn trivial_pass_with_a_plan_but_no_reviewers_concludes_success() {
        // The legitimate zero-reviewer run: the plan was announced (one run.started
        // with no reviewers) and recorded a clean pass. The aggregate stays green.
        let events = vec![
            RunEvent::RunStarted {
                run: RunId("r-rec".into()),
                branch: "feat".into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![],
            },
            RunEvent::RunCompleted {
                run: RunId("r-rec".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 1000,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        let digest = digest(&events);
        assert_eq!(
            check_runs(&ctx(), &digest)
                .iter()
                .find(|c| c.name == "bastion")
                .unwrap()
                .conclusion,
            Conclusion::Success
        );
    }

    #[test]
    fn advisor_with_a_blocking_finding_does_not_block() {
        // The runner clamps advisors to pass while keeping their findings, so an advisor
        // row with verdict pass and a blocking finding is legitimate. It never gates: the
        // advisor check concludes success and the recorded pass aggregate stays green.
        let events = vec![
            RunEvent::RunStarted {
                run: RunId("r-rec".into()),
                branch: "feat".into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![ReviewerRef {
                    name: "a1".into(),
                    mode: Mode::Advisor,
                }],
            },
            RunEvent::ReviewerResolved {
                run: RunId("r-rec".into()),
                reviewer: "a1".into(),
                verdict: Decision::Pass,
                summary: "x".into(),
                findings: vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
                usage: None,
                duration_ms: 1000,
                has_transcript: true,
                replayed: false,
            },
            RunEvent::RunCompleted {
                run: RunId("r-rec".into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 0,
                    passed: 0,
                    blocked: 0,
                },
                duration_ms: 1000,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        let digest = digest(&events);
        let checks = check_runs(&ctx(), &digest);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion / a1")
                .unwrap()
                .conclusion,
            Conclusion::Success
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "bastion")
                .unwrap()
                .conclusion,
            Conclusion::Success
        );
    }

    #[test]
    fn oversized_annotation_message_is_truncated_with_a_pointer() {
        // A finding longer than the per-message cap would 422 the whole report
        // request; the annotation message is truncated and points at the comment,
        // while a short finding passes through unchanged.
        let long = "x".repeat(MAX_ANNOTATION_MESSAGE + 100);
        let big = finding(FindingKind::Optional, "src/a.rs", 1, 1, &long);
        let annotated = annotations_for(std::slice::from_ref(&big));
        assert_eq!(annotated.len(), 1);
        assert!(annotated[0].message.chars().count() <= MAX_ANNOTATION_MESSAGE + 80);
        assert!(
            annotated[0]
                .message
                .contains("(truncated; see the Bastion comment")
        );

        let small = finding(FindingKind::Optional, "src/a.rs", 2, 2, "nit");
        assert_eq!(
            annotations_for(std::slice::from_ref(&small))[0].message,
            "nit"
        );
    }

    #[test]
    fn oversized_check_summary_is_capped_with_a_pointer() {
        // A reviewer carrying an enormous finding: the per-reviewer check summary must
        // stay under GitHub's output.summary limit, with a pointer to the comment.
        let huge = "y".repeat(MAX_CHECK_SUMMARY + 5000);
        let events = recorded_run(
            &["g1"],
            vec![gate_resolved(
                "g1",
                Decision::Block,
                vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, &huge)],
            )],
            (
                Decision::Block,
                Gates {
                    total: 1,
                    passed: 0,
                    blocked: 1,
                },
            ),
        );
        let digest = digest(&events);
        let checks = check_runs(&ctx(), &digest);
        let g1 = checks.iter().find(|c| c.name == "bastion / g1").unwrap();
        assert!(g1.summary.chars().count() <= MAX_CHECK_SUMMARY + 80);
        assert!(g1.summary.contains("truncated; see the Bastion comment"));
    }

    #[test]
    fn synthetic_crash_finding_is_not_annotated() {
        // The runner's fail-closed marker has an empty path and line 0; it must be
        // rendered in prose but never sent as an annotation (GitHub rejects line 0).
        let crash = finding(
            FindingKind::Blocking,
            "",
            0,
            0,
            "reviewer failed to complete",
        );
        assert!(!is_locatable(&crash));
        assert!(annotations_for(std::slice::from_ref(&crash)).is_empty());
        assert!(finding_bullet(&crash).contains("- **blocking**: reviewer failed to complete"));
    }

    #[test]
    fn annotations_cap_at_the_limit_and_the_summary_notes_the_overflow() {
        // GitHub accepts at most MAX_ANNOTATIONS annotations per request. With more
        // locatable findings than the cap, annotations_for must stop at the cap and
        // the reviewer-check summary must say how many located findings went unpinned.
        let overflow = 5;
        let findings: Vec<Finding> = (0..MAX_ANNOTATIONS + overflow)
            .map(|i| {
                let line = u32::try_from(i + 1).unwrap();
                finding(FindingKind::Optional, "src/big.rs", line, line, "nit")
            })
            .collect();

        let annotations = annotations_for(&findings);
        assert_eq!(annotations.len(), MAX_ANNOTATIONS);

        let row = ReviewerRow {
            name: "style".into(),
            mode: Mode::Advisor,
            backend: Some(Backend::Codex),
            decision: Decision::Pass,
            summary: "many nits".into(),
            findings,
            duration_ms: 1000,
            usage: None,
            replayed: false,
        };
        let summary = reviewer_check_summary(&row, &annotations);
        assert!(summary.contains(&format!(
            "{overflow} more located finding(s) are listed above but not pinned to the diff"
        )));
    }

    #[test]
    fn reviewer_summary_tokens_line_includes_cache_only_when_nonzero() {
        let row_with = |cache_read: u64| ReviewerRow {
            name: "r".into(),
            mode: Mode::Gate,
            backend: Some(Backend::ClaudeCode),
            decision: Decision::Pass,
            summary: "ok".into(),
            findings: vec![],
            duration_ms: 1000,
            usage: Some(Usage {
                tokens_in: 1200,
                tokens_out: 80,
                cache_read,
                cost_usd: Money::from_cents(5),
            }),
            replayed: false,
        };

        // Cache hits present: the cached figure rides the token line.
        let with_cache = reviewer_check_summary(&row_with(600), &[]);
        assert!(with_cache.contains("- Tokens: 1200 in, 80 out, 600 cached ($0.05)"));

        // No cache hits: the cached segment is omitted, the in/out line stays.
        let no_cache = reviewer_check_summary(&row_with(0), &[]);
        assert!(no_cache.contains("- Tokens: 1200 in, 80 out ($0.05)"));
        assert!(!no_cache.contains("cached"));
    }

    #[test]
    fn request_builders_target_the_right_endpoints() {
        let ctx = ctx();
        assert_eq!(
            comment_list_request(&ctx).path,
            "/repos/acme/app/issues/12/comments?per_page=100"
        );
        let create = comment_create_request(&ctx, "hi");
        assert_eq!(create.method, Method::Post);
        assert_eq!(create.path, "/repos/acme/app/issues/12/comments");
        assert_eq!(create.body.unwrap()["body"], "hi");

        let update = comment_update_request(&ctx, 7, "ho");
        assert_eq!(update.method, Method::Patch);
        assert_eq!(update.path, "/repos/acme/app/issues/comments/7");

        let check = CheckRun {
            name: "bastion".into(),
            head_sha: "sha".into(),
            conclusion: Conclusion::Failure,
            title: "t".into(),
            summary: "s".into(),
            annotations: vec![],
        };
        let req = check_run_request(&ctx, &check);
        assert_eq!(req.path, "/repos/acme/app/check-runs");
        let body = req.body.unwrap();
        assert_eq!(body["conclusion"], "failure");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["head_sha"], "sha");
    }

    #[test]
    fn find_marker_comment_matches_only_bastions_own() {
        let list = serde_json::json!([
            {"id": 1, "body": "a human comment"},
            {"id": 2, "body": format!("{MARKER}\n## Bastion review")},
        ]);
        assert_eq!(find_marker_comment(&list).unwrap(), Some(2));

        let none = serde_json::json!([{"id": 1, "body": "no marker here"}]);
        assert_eq!(find_marker_comment(&none).unwrap(), None);

        // A malformed body (not the expected array) fails closed rather than
        // reporting "none found", which would post a duplicate comment.
        let malformed = serde_json::json!({"message": "Not Found"});
        assert!(find_marker_comment(&malformed).is_err());
    }

    #[tokio::test]
    async fn report_creates_a_comment_then_posts_checks() {
        // No existing comment: the list returns empty, so the report POSTs a new one.
        // The check-run responses carry the shared `github-actions` app, as the
        // default GITHUB_TOKEN would, so the report should detect that and nudge.
        let api = RecordingClient::with_responder(|req| {
            if req.method == Method::Get {
                ApiResponse {
                    status: 200,
                    body: serde_json::json!([]),
                }
            } else if req.path.ends_with("/check-runs") {
                ApiResponse {
                    status: 201,
                    body: serde_json::json!({"id": 1, "app": {"slug": "github-actions"}}),
                }
            } else {
                ApiResponse {
                    status: 201,
                    body: serde_json::json!({"id": 555}),
                }
            }
        });
        let summary = report(&api, &ctx(), &sample_events(), None)
            .await
            .expect("reports");
        assert_eq!(summary.comment, CommentAction::Created);
        assert_eq!(summary.checks, 4);

        let calls = api.calls();
        // The checks are posted first (so the report can read its posting identity
        // from a response), then the comment is upserted.
        let last_check = calls
            .iter()
            .rposition(|c| c.path.ends_with("/check-runs"))
            .expect("a check-run POST");
        let first_comment = calls
            .iter()
            .position(|c| c.path.contains("/issues/"))
            .expect("a comment request");
        assert!(
            last_check < first_comment,
            "checks should be posted before the comment: {calls:?}"
        );
        let check_calls = calls
            .iter()
            .filter(|c| c.path.ends_with("/check-runs"))
            .count();
        assert_eq!(check_calls, 4);
        // The created comment body carries the marker and the optional finding.
        let comment_post = calls
            .iter()
            .find(|c| c.method == Method::Post && c.path.ends_with("/issues/12/comments"))
            .expect("a comment POST");
        let body = comment_post.body.as_ref().unwrap()["body"]
            .as_str()
            .unwrap();
        assert!(body.contains(MARKER));
        assert!(body.contains("rename foo"));
        // Posted under the shared github-actions app, so the nudge is present.
        assert!(body.contains(SETUP_URL));
    }

    #[tokio::test]
    async fn report_omits_the_nudge_under_a_dedicated_app() {
        // The check-run responses carry a distinct app slug, as a dedicated Bastion
        // app would. The checks then form their own suite, so no nudge is needed.
        let api = RecordingClient::with_responder(|req| {
            if req.method == Method::Get {
                ApiResponse {
                    status: 200,
                    body: serde_json::json!([]),
                }
            } else if req.path.ends_with("/check-runs") {
                ApiResponse {
                    status: 201,
                    body: serde_json::json!({"id": 1, "app": {"slug": "bastion-acme"}}),
                }
            } else {
                ApiResponse {
                    status: 201,
                    body: serde_json::json!({"id": 555}),
                }
            }
        });
        report(&api, &ctx(), &sample_events(), None)
            .await
            .expect("reports");

        let comment_post = api
            .calls()
            .into_iter()
            .find(|c| c.method == Method::Post && c.path.ends_with("/issues/12/comments"))
            .expect("a comment POST");
        let body = comment_post.body.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !body.contains(SETUP_URL),
            "dedicated app should not nudge: {body}"
        );
    }

    #[test]
    fn posting_app_classifies_the_creating_app() {
        use serde_json::json;

        assert_eq!(
            PostingApp::from_check_run(&json!({"id": 1, "app": {"slug": "github-actions"}})),
            PostingApp::SharedActions
        );
        assert_eq!(
            PostingApp::from_check_run(&json!({"id": 1, "app": {"slug": "bastion-acme"}})),
            PostingApp::Dedicated
        );
        // A response missing the app, the slug, or with a non-string slug is Unknown,
        // so a malformed body leaves the nudge off rather than guessing.
        assert_eq!(
            PostingApp::from_check_run(&json!({"id": 1})),
            PostingApp::Unknown
        );
        assert_eq!(
            PostingApp::from_check_run(&json!({"app": {}})),
            PostingApp::Unknown
        );
        assert_eq!(
            PostingApp::from_check_run(&json!({"app": {"slug": 7}})),
            PostingApp::Unknown
        );

        // Only the shared identity nudges.
        assert!(PostingApp::SharedActions.should_suggest_dedicated_app());
        assert!(!PostingApp::Dedicated.should_suggest_dedicated_app());
        assert!(!PostingApp::Unknown.should_suggest_dedicated_app());
    }

    #[tokio::test]
    async fn report_updates_an_existing_comment_in_place() {
        // The list returns Bastion's own comment, so the report PATCHes it. The
        // non-GET responses carry no `app`, so this also pins the missing-slug path:
        // an unreadable identity leaves the nudge off.
        let api = RecordingClient::with_responder(|req| match req.method {
            Method::Get => ApiResponse {
                status: 200,
                body: serde_json::json!([{"id": 909, "body": format!("{MARKER} old")}]),
            },
            _ => ApiResponse {
                status: 200,
                body: serde_json::Value::Null,
            },
        });
        let summary = report(&api, &ctx(), &sample_events(), None)
            .await
            .expect("reports");
        assert_eq!(summary.comment, CommentAction::Updated(909));

        // The existing comment is updated in place with a PATCH (the checks are
        // posted first, so the PATCH is no longer at a fixed index).
        let patch = api
            .calls()
            .into_iter()
            .find(|c| c.method == Method::Patch)
            .expect("a PATCH to the existing comment");
        assert!(patch.path.ends_with("/issues/comments/909"));
        let body = patch.body.as_ref().unwrap()["body"].as_str().unwrap();
        assert!(
            !body.contains(SETUP_URL),
            "missing app.slug should not nudge: {body}"
        );
    }

    #[tokio::test]
    async fn report_fails_closed_on_a_rejected_request() {
        // GitHub rejects the first request (a check-run POST): the report errors
        // rather than pressing on.
        let api = RecordingClient::with_responder(|_| ApiResponse {
            status: 403,
            body: serde_json::json!({"message": "Resource not accessible by integration"}),
        });
        let err = report(&api, &ctx(), &sample_events(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("returned 403"));
        assert!(err.to_string().contains("Resource not accessible"));
    }

    #[test]
    fn truncate_caps_and_marks_overflow() {
        assert_eq!(truncate("short", 110), "short");
        let long = "x".repeat(200);
        let cut = truncate(&long, 110);
        assert_eq!(cut.chars().count(), 110);
        assert!(cut.ends_with("..."));
    }
}
