//! Sealing an eligible run for later attestation.

use super::*;

/// Seal the run when `ctx` carries [`SealBindings`], persisting the result
/// alongside `run.jsonl`.
///
/// Sealing failure is deliberately non-fatal: an unsealed run is simply a run
/// nobody can attest later, not a failed review. Every failure path here (no
/// bindings, zero repo-reviewer events, a persistence error) logs at most a
/// `tracing::warn!` and returns without touching `aggregate`.
pub(super) fn seal_run(layout: &Layout, ctx: &ExecContext, stream: &[RunEvent]) {
    // A partial run is never sealed: its aggregate speaks only for the selected
    // reviewers, and a seal would let `bastion attest` present a filtered green
    // as a verdict on the full triggered set.
    if ctx.partial {
        return;
    }
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

    // The pre-run sample (`ctx.dirty`) alone is not enough: a reviewer can take
    // long enough that the tree turns dirty mid-run (an uncommitted fix written
    // while reviewers are still executing), which the pre-run sample cannot see.
    // Re-sample at seal time and OR the two, so a tree dirty at either point
    // seals dirty. The pre-run sample still matters on its own: a file could be
    // dirtied and then removed again before seal time, which the seal-time
    // sample alone would miss.
    let dirty_at_seal_time = crate::git::is_dirty(&ctx.repo_root).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "could not re-sample working tree cleanliness at seal time; treating the run as dirty out of caution");
        true
    });

    let seal = crate::seal::seal(
        crate::seal::embedded_secret(),
        crate::version::VERSION,
        bindings,
        crate::seal::seams_active(),
        ctx.dirty || dirty_at_seal_time,
        reviewers,
        &events,
    );

    if let Err(err) = crate::store::write_seal(layout, &ctx.run, &seal) {
        tracing::warn!(error = %err, "failed to persist run seal; run will be unsealed");
    }
}
