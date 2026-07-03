//! Writing per-reviewer artifacts and the run event log.

use super::*;

/// Persist one reviewer's saved artifacts: transcript, raw verdict, and metadata.
pub(super) fn persist_reviewer(layout: &Layout, run: &RunId, item: &Resolved) -> Result<()> {
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
pub(super) fn persist_run(
    layout: &Layout,
    run: &RunId,
    ctx: &ExecContext,
    tail: &[RunEvent],
) -> Result<()> {
    // The store writes `run.jsonl` and updates `latest`. We reconstruct the
    // opening event here so a replayed run is complete; `changed` is recorded by
    // the caller's emitted event, which is the canonical one shown on screen.
    let started = RunEvent::RunStarted {
        partial: ctx.partial,
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
