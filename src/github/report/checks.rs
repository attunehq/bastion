//! Building the per-reviewer and aggregate check runs.

use super::*;

/// A check-run conclusion, limited to the two the adapter emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    /// A passing gate, or any advisor.
    Success,
    /// A blocking gate (or the aggregate when any gate blocked).
    Failure,
}

impl Conclusion {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
        }
    }
}

/// A check-run annotation: a located finding pinned to the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Annotation {
    pub(super) path: String,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
    pub(super) level: &'static str,
    pub(super) message: String,
}

/// A fully-resolved check run ready to POST.
#[derive(Debug, Clone)]
pub(super) struct CheckRun {
    pub(super) name: String,
    pub(super) head_sha: String,
    pub(super) conclusion: Conclusion,
    pub(super) title: String,
    pub(super) summary: String,
    pub(super) annotations: Vec<Annotation>,
}

/// Build the per-reviewer check runs plus the aggregate `bastion` check.
///
/// The aggregate is always present so it can serve as the single stable required
/// check, even when zero reviewers matched (a trivial pass).
pub(super) fn check_runs(ctx: &PrContext, digest: &RunDigest) -> Vec<CheckRun> {
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
pub(super) fn reviewer_check(ctx: &PrContext, row: &ReviewerRow) -> CheckRun {
    let (conclusion, decision_word) = match row.mode {
        _ if row.skipped => (Conclusion::Success, "Skipped"),
        Mode::Advisor => (Conclusion::Success, "Advisory"),
        Mode::Gate if row.blocks() => (Conclusion::Failure, "Blocked"),
        Mode::Gate => (Conclusion::Success, "Passed"),
    };
    let title = format!(
        "{decision_word}: {}",
        crate::text::truncate(row.summary.trim(), 110)
    );

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
pub(super) fn aggregate_check(ctx: &PrContext, digest: &RunDigest) -> CheckRun {
    let conclusion = aggregate_conclusion(digest);
    let title = match AggregateOutcome::classify(digest) {
        AggregateOutcome::BlockedInconsistent => {
            "Blocked: a gate verdict is internally inconsistent".to_string()
        }
        AggregateOutcome::PassedNoGates => "No gates triggered".to_string(),
        AggregateOutcome::Passed {
            passed,
            total,
            skipped,
        } if skipped > 0 => format!("{passed}/{total} gates passed, {skipped} skipped"),
        AggregateOutcome::Passed { passed, total, .. } => {
            format!("{passed}/{total} gates passed")
        }
        AggregateOutcome::Blocked {
            passed,
            total,
            skipped,
        } if skipped > 0 => {
            format!("Blocked: {passed}/{total} gates passed, {skipped} skipped")
        }
        AggregateOutcome::Blocked { passed, total, .. } => {
            format!("Blocked: {passed}/{total} gates passed")
        }
        AggregateOutcome::Incomplete => "Incomplete run".to_string(),
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
pub(super) fn reviewer_check_summary(row: &ReviewerRow, annotations: &[Annotation]) -> String {
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
    if row.carried {
        out.push_str(
            "- Carried forward from the branch's previous run (trigger-scoped diff unchanged) \
             rather than executed fresh.\n",
        );
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
pub(super) fn is_locatable(finding: &Finding) -> bool {
    !finding.path.is_empty() && finding.line_start >= 1
}

/// The annotation `message` for a finding, truncated to [`MAX_ANNOTATION_MESSAGE`]
/// so a single long finding cannot 422 the whole report request. When cut, it points
/// the reader to the sticky comment, which always carries the full finding text.
pub(super) fn annotation_message(detail: &str) -> String {
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
pub(super) fn cap_check_summary(summary: String) -> String {
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
pub(super) fn annotations_for(findings: &[Finding]) -> Vec<Annotation> {
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
