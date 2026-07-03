//! Turning run events and summaries into output.
//!
//! Two audiences, two formats. [`Format::Human`] renders readable progress for a
//! person watching; [`Format::Jsonl`] emits one JSON object per line for an agent
//! to parse as it arrives. The JSONL form is exactly the persisted event shape,
//! so on-screen output and `run.jsonl` never disagree.

use std::io::{self, Write};

use crate::event::RunEvent;
use crate::store::RunSummary;
use crate::verdict::{Decision, Finding, FindingKind};

/// The output format for streamed and replayed run data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Format {
    /// Human-readable progress.
    #[default]
    Human,
    /// One JSON object per line.
    Jsonl,
}

/// Write a single run event in the chosen format.
///
/// # Errors
///
/// Returns an error if writing to `out` fails or the event cannot be serialized.
pub fn write_event<W: Write>(out: &mut W, format: Format, event: &RunEvent) -> io::Result<()> {
    match format {
        Format::Jsonl => writeln!(
            out,
            "{}",
            serde_json::to_string(event).map_err(io::Error::other)?
        ),
        Format::Human => write_event_human(out, event),
    }
}

fn write_event_human<W: Write>(out: &mut W, event: &RunEvent) -> io::Result<()> {
    match event {
        RunEvent::RunStarted {
            run,
            branch,
            base,
            changed,
            reviewers,
            partial,
        } => writeln!(
            out,
            "run {run}: {branch} vs {base}, {changed} file(s) changed, {} reviewer(s) {}",
            reviewers.len(),
            if *partial {
                "selected (partial run)"
            } else {
                "triggered"
            },
        ),
        RunEvent::ReviewerStarted {
            reviewer,
            mode,
            backend,
            ..
        } => {
            writeln!(
                out,
                "  .. {reviewer} ({}, {})",
                mode.as_str(),
                backend.as_str()
            )
        }
        RunEvent::ReviewerResolved {
            reviewer,
            verdict,
            summary,
            findings,
            duration_ms,
            replayed,
            carried,
            ..
        } => {
            writeln!(
                out,
                "  {} {reviewer}: {summary} ({}s{}{})",
                marker(*verdict),
                duration_ms / 1000,
                if *replayed { ", replayed" } else { "" },
                if *carried { ", carried" } else { "" },
            )?;
            for finding in findings {
                write_finding(out, finding)?;
            }
            Ok(())
        }
        RunEvent::AttestationReplayed {
            reviewers,
            public_key,
            attested_at,
            ..
        } => writeln!(
            out,
            "  attested: {} reviewer(s) replayed from a signed local run (key {public_key}, attested {attested_at})",
            reviewers.len()
        ),
        RunEvent::AttestationFallback { reason, .. } => {
            writeln!(out, "  attestation not honored: {reason}")
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
        } => writeln!(
            out,
            "{} run complete{}: {}/{} gates passed ({}s{}, {cost_usd})",
            marker(*verdict),
            // A filtered green speaks only for the reviewers that ran; say so on
            // the one line a person actually reads.
            if *partial {
                " (partial: only the selected reviewers ran)"
            } else {
                ""
            },
            gates.passed,
            gates.total,
            duration_ms / 1000,
            token_counter(*tokens_in, *tokens_out, *cache_read),
        ),
    }
}

fn write_finding<W: Write>(out: &mut W, finding: &Finding) -> io::Result<()> {
    let tag = match finding.kind {
        FindingKind::Blocking => "blocking",
        FindingKind::Optional => "optional",
    };
    writeln!(
        out,
        "      [{tag}] {}:{}-{}: {}",
        finding.path, finding.line_start, finding.line_end, finding.detail
    )
}

/// Write a list of run summaries in the chosen format.
///
/// # Errors
///
/// Returns an error if writing to `out` fails.
pub fn write_runs<W: Write>(out: &mut W, format: Format, runs: &[RunSummary]) -> io::Result<()> {
    match format {
        Format::Jsonl => {
            for run in runs {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string(run).map_err(io::Error::other)?
                )?;
            }
            Ok(())
        }
        Format::Human => {
            if runs.is_empty() {
                return writeln!(out, "no runs recorded");
            }
            for run in runs {
                let verdict = run.verdict.map_or("(incomplete)", Decision::as_str);
                let branch = run.branch.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "{}  {verdict:<5}  {branch}  {} reviewer(s){}",
                    run.run,
                    run.reviewers,
                    if run.partial { "  (partial)" } else { "" },
                )?;
            }
            Ok(())
        }
    }
}

/// The token segment of a run's counter, e.g. `, 4200 in / 270 out / 3100 cached
/// tokens`. The leading separator lets it slot between the elapsed time and the
/// cost. Empty when no tokens were reported (a mock run, a zero-reviewer trivial
/// pass, or a run persisted before tokens were tracked) so the counter stays clean.
/// Shares [`crate::verdict::format_token_counter`] with the GitHub status line so
/// the two surfaces never drift.
fn token_counter(tokens_in: u64, tokens_out: u64, cache_read: u64) -> String {
    crate::verdict::format_token_counter(tokens_in, tokens_out, cache_read)
        .map(|segment| format!(", {segment}"))
        .unwrap_or_default()
}

fn marker(decision: Decision) -> &'static str {
    match decision {
        Decision::Pass => "PASS ",
        Decision::Block => "BLOCK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunId;

    fn resolved() -> RunEvent {
        RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            run: RunId("r-1".into()),
            reviewer: "tenant-isolation".into(),
            verdict: Decision::Block,
            summary: "missing tenant scope".into(),
            findings: vec![Finding {
                kind: FindingKind::Blocking,
                path: "src/db.ts".into(),
                line_start: 10,
                line_end: 10,
                detail: "scope by tenant_id".into(),
            }],
            usage: None,
            duration_ms: 4200,
            has_transcript: true,
            replayed: false,
        }
    }

    #[test]
    fn human_format_is_readable() {
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &resolved()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("BLOCK tenant-isolation: missing tenant scope"));
        assert!(text.contains("[blocking] src/db.ts:10-10: scope by tenant_id"));
    }

    fn completed(tokens_in: u64, tokens_out: u64, cache_read: u64) -> RunEvent {
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r-1".into()),
            verdict: Decision::Pass,
            gates: crate::event::Gates {
                total: 2,
                passed: 2,
                blocked: 0,
            },
            duration_ms: 40_000,
            tokens_in,
            tokens_out,
            cache_read,
            cost_usd: crate::verdict::Money::from_cents(37),
        }
    }

    #[test]
    fn completed_counter_shows_time_tokens_cache_and_cost() {
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &completed(4200, 270, 3100)).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("run complete: 2/2 gates passed"));
        // Time, then tokens (with the cache-read figure), then cost, in that order.
        assert!(text.contains("(40s, 4200 in / 270 out / 3100 cached tokens, $0.37)"));
    }

    #[test]
    fn completed_counter_omits_the_cache_figure_when_zero() {
        // Tokens were reported but no cache hits: the in/out counter shows, the
        // cached segment does not.
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &completed(4200, 270, 0)).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("(40s, 4200 in / 270 out tokens, $0.37)"));
        assert!(!text.contains("cached"));
    }

    #[test]
    fn completed_counter_omits_tokens_when_none_were_reported() {
        // A run with no reported usage (a mock or zero-reviewer pass) keeps the
        // counter to time and cost rather than printing "0 in / 0 out tokens".
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &completed(0, 0, 0)).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("(40s, $0.37)"));
        assert!(!text.contains("tokens"));
    }

    #[test]
    fn jsonl_format_is_the_event_shape() {
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Jsonl, &resolved()).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains(r#""type":"reviewer.resolved""#));
        let parsed: RunEvent = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed, resolved());
    }

    #[test]
    fn a_replayed_resolved_event_carries_the_replayed_suffix() {
        // A reviewer that replayed from a signed local attestation must say so
        // inline, right next to the elapsed time, so a person watching the run
        // can tell at a glance which verdicts were freshly computed.
        let mut event = resolved();
        let RunEvent::ReviewerResolved { replayed, .. } = &mut event else {
            unreachable!()
        };
        *replayed = true;

        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &event).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("(4s, replayed)"),
            "expected the replayed suffix right after the elapsed time, got: {text}"
        );
    }

    #[test]
    fn a_fresh_resolved_event_carries_no_replayed_suffix() {
        // The common case (`replayed: false`) must not print the suffix at all,
        // not even an empty one.
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &resolved()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("(4s)"));
        assert!(!text.contains("replayed"));
    }

    #[test]
    fn a_carried_resolved_event_carries_the_carried_suffix() {
        let mut event = resolved();
        let RunEvent::ReviewerResolved { carried, .. } = &mut event else {
            unreachable!()
        };
        *carried = true;

        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &event).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("(4s, carried)"),
            "expected the carried suffix right after the elapsed time, got: {text}"
        );
    }

    #[test]
    fn a_partial_run_is_labelled_on_both_ends() {
        let started = RunEvent::RunStarted {
            run: RunId("r-1".into()),
            branch: "feat".into(),
            base: "main".into(),
            changed: 3,
            reviewers: vec![],
            partial: true,
        };
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &started).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("selected (partial run)"),
            "the opening line must say the set was narrowed, got: {text}"
        );

        let mut event = completed(0, 0, 0);
        let RunEvent::RunCompleted { partial, .. } = &mut event else {
            unreachable!()
        };
        *partial = true;
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &event).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("run complete (partial: only the selected reviewers ran)"),
            "the verdict line must not read as a full green, got: {text}"
        );
    }

    #[test]
    fn a_partial_run_summary_is_labelled_in_the_runs_listing() {
        let run = RunSummary {
            run: RunId("r-1".into()),
            branch: Some("feat".into()),
            base: Some("main".into()),
            verdict: Some(Decision::Pass),
            reviewers: 2,
            partial: true,
        };
        let mut buf = Vec::new();
        write_runs(&mut buf, Format::Human, &[run]).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("(partial)"), "got: {text}");
    }

    #[test]
    fn run_attested_line_names_the_key_count_and_timestamp() {
        let event = RunEvent::AttestationReplayed {
            run: RunId("r-1".into()),
            reviewers: vec!["r1".into(), "r2".into()],
            public_key: "ssh-ed25519 AAAA grace@bastion.dev".into(),
            attested_at: "2026-07-01T12:00:00Z".into(),
        };
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &event).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains(
                "attested: 2 reviewer(s) replayed from a signed local run \
                 (key ssh-ed25519 AAAA grace@bastion.dev, attested 2026-07-01T12:00:00Z)"
            ),
            "got: {text}"
        );
    }

    #[test]
    fn run_attestation_fallback_line_states_the_reason() {
        // A fallback event is only recorded for a refused attestation (a missing
        // note is silent), so the sample reason is a genuine rejection.
        let event = RunEvent::AttestationFallback {
            run: RunId("r-1".into()),
            reason: "the attested patch id does not match CI's diff".into(),
        };
        let mut buf = Vec::new();
        write_event(&mut buf, Format::Human, &event).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains(
                "attestation not honored: the attested patch id does not match CI's diff"
            )
        );
    }
}
