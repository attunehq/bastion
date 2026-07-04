//! Rendering the sticky PR comment body.

use super::*;

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
pub(super) fn comment_body(
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
        out.push_str(&attestation_fallback_callout(reason));
        out.push('\n');
    }

    // A carried reviewer never dispatched a backend this run: its prior pass was
    // reused because its trigger-scoped diff was unchanged. Flag it at comment
    // level so the sticky comment matches the per-reviewer check runs and the
    // local CLI, both of which already mark a carried reviewer.
    let carried: Vec<&str> = digest
        .rows
        .iter()
        .filter(|row| row.carried)
        .map(|row| row.name.as_str())
        .collect();
    if !carried.is_empty() {
        out.push_str(&carried_callout(&carried));
        out.push('\n');
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
pub(super) fn status_line(digest: &RunDigest) -> String {
    let reviewers = digest.rows.len();
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

    let headline = match AggregateOutcome::classify(digest) {
        AggregateOutcome::BlockedInconsistent => {
            "**Blocked.** A gate verdict is internally inconsistent (a pass carrying a \
             blocking finding); failing closed."
                .to_string()
        }
        AggregateOutcome::PassedNoGates => "**Passed.** No gates were triggered.".to_string(),
        AggregateOutcome::Passed { total, .. } => {
            format!("**Passed.** All {total} gate(s) passed.")
        }
        AggregateOutcome::Blocked { passed, total } => {
            format!("**Blocked.** {passed} of {total} gate(s) passed.")
        }
        AggregateOutcome::Incomplete => "**Incomplete.** The run did not finish.".to_string(),
    };
    // A filtered run's verdict speaks only for the reviewers that ran.
    let partial = if digest.partial {
        " **Partial run:** only an explicitly selected subset of the triggered reviewers ran."
    } else {
        ""
    };
    format!("{headline}{partial} {reviewers} reviewer(s) ran{timing}{tokens}{cost}.")
}

/// A truncated rendering of an SSH public key line for display: the key type
/// and a short prefix of the base64 material, plus the comment if the key
/// carries one. Keeps the callout from dumping the full base64 blob inline.
pub(super) fn truncate_key(public_key: &str) -> String {
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
pub(super) fn reviewer_table(digest: &RunDigest) -> String {
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
pub(super) fn finding_bullet(finding: &Finding) -> String {
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
pub(super) fn location(path: &str, start: u32, end: u32) -> String {
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

/// Neutralize Markdown table-breaking characters in a free-text cell: pipes would
/// start a new column and newlines would end the row.
pub(super) fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// The trailing note. Always credits Bastion and points at the run artifact; when
/// the report is posting under the shared Actions identity, it also nudges toward a
/// dedicated app so the checks group on their own instead of under a sibling workflow.
pub(super) fn footer(suggest_dedicated_app: bool) -> String {
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
