//! Writing per-reviewer artifacts and the run event log.

use super::*;

/// Persist one reviewer's saved artifacts: transcript, raw verdict, and metadata.
pub(super) fn persist_reviewer(
    layout: &Layout,
    run: &RunId,
    item: &Resolved,
    akari: Option<&crate::akari::HandoffRecord>,
) -> Result<()> {
    let dir = layout.reviewer_dir(run, &item.reviewer.name);
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("creating reviewer directory {}", dir.display()))?;

    let transcript_path = layout.transcript(run, &item.reviewer.name);
    if let Some(transcript) = &item.transcript {
        std::fs::write(&transcript_path, transcript)
            .wrap_err_with(|| format!("writing {}", transcript_path.display()))?;
    } else {
        remove_optional(&transcript_path)?;
    }

    // A semantic skip is not a review verdict, so it deliberately has no
    // `verdict.json`. Remove any artifact left by an earlier invocation at the
    // same HEAD-derived run id so the directory describes this run exactly.
    let verdict_path = layout.verdict(run, &item.reviewer.name);
    if item.skipped {
        remove_optional(&verdict_path)?;
    } else {
        let verdict = Verdict {
            decision: item.decision,
            summary: item.summary.clone(),
            findings: item.findings.clone(),
        };
        std::fs::write(
            &verdict_path,
            serde_json::to_string_pretty(&verdict).wrap_err("serializing verdict")?,
        )
        .wrap_err_with(|| format!("writing {}", verdict_path.display()))?;
    }

    // Per-reviewer metadata: backend, timing, usage, matched trigger.
    let (backend, usage) = if item.skipped {
        let trigger = item.trigger.as_ref().ok_or_else(|| {
            eyre!(
                "skipped reviewer '{}' has no trigger resolution",
                item.reviewer.name
            )
        })?;
        (trigger.backend, trigger.usage)
    } else {
        (item.reviewer.backend, item.usage)
    };
    let meta = ReviewerMeta {
        backend,
        mode: item.reviewer.mode,
        duration_ms: duration_ms(item.duration),
        usage,
        trigger: item.reviewer.trigger.clone(),
        akari: akari.cloned(),
    };
    let meta_path = layout.meta(run, &item.reviewer.name);
    std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).wrap_err("serializing reviewer meta")?,
    )
    .wrap_err_with(|| format!("writing {}", meta_path.display()))?;

    Ok(())
}

/// Remove an optional reviewer artifact while treating absence as the desired
/// state. Repeated runs at one commit reuse a run directory, so optional files
/// must be deleted when the new terminal outcome does not produce them.
fn remove_optional(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).wrap_err_with(|| format!("removing {}", path.display())),
    }
}

/// Per-reviewer metadata saved alongside the transcript and verdict.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReviewerMeta {
    backend: crate::reviewer::Backend,
    mode: Mode,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    trigger: crate::reviewer::Trigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    akari: Option<crate::akari::HandoffRecord>,
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
