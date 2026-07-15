//! Folding replayed and carried verdicts into resolved rows.

use super::*;

/// The provenance of a verdict being folded back in without re-execution:
/// replayed from a signed attestation, or carried from the branch's previous
/// run. The reconstruction is identical on both paths; these are the only
/// fields that differ, gathered here so [`resolve_reused`] can be one
/// consistency-guarded reconstruction rather than two that drift.
#[derive(Clone, Copy)]
enum Reuse {
    /// Replayed from a verified attestation bundle (the CI surface only).
    Replayed,
    /// Carried forward from the branch's previous run ([`crate::carry`]).
    Carried,
}

impl Reuse {
    /// The `(replayed, carried)` flags this provenance stamps on the row.
    fn flags(self) -> (bool, bool) {
        match self {
            Reuse::Replayed => (true, false),
            Reuse::Carried => (false, true),
        }
    }

    /// The row's usage. A replayed verdict keeps the tokens the attested run
    /// reported; a carried one spent nothing this run, so it reports none.
    fn usage(self, attested: Option<Usage>) -> Option<Usage> {
        match self {
            Reuse::Replayed => attested,
            Reuse::Carried => None,
        }
    }

    /// The row's duration. A replayed verdict reports the attested duration; a
    /// carried one reads zero, since no work happened this run.
    fn duration(self, attested_ms: u64) -> Duration {
        match self {
            Reuse::Replayed => Duration::from_millis(attested_ms),
            Reuse::Carried => Duration::ZERO,
        }
    }

    /// The scope digest to stamp on the row. A carried verdict re-stamps the
    /// prior event's digest so the carry chain stays unbroken on the next run; a
    /// replayed verdict drops it, because an attested digest describes the
    /// attester's working tree, not this run's, and must never itself be carried
    /// from.
    fn scope_digest(self, attested: &Option<String>) -> Option<String> {
        match self {
            Reuse::Replayed => None,
            Reuse::Carried => attested.clone(),
        }
    }

    /// The reason recorded when the reused event is not a `reviewer.resolved`
    /// event at all. This is a defect in the producing planner, not
    /// attacker-shaped input, so the message names the planner: the fallback arm
    /// exists only to keep the reconstruction total.
    fn malformed_reason(self) -> &'static str {
        match self {
            Reuse::Replayed => {
                "the attested reviewer.resolved event was not a reviewer.resolved event \
                 (a defect in the attestation planner, not the bundle)"
            }
            Reuse::Carried => {
                "the carried event was not a reviewer.resolved event \
                 (a defect in the carry planner, not the store)"
            }
        }
    }

    /// The reason recorded when the reused verdict is internally inconsistent (a
    /// pass carrying a blocking finding, or a block with none). Reviewer-shaped
    /// input on both paths, so it fails closed rather than being trusted.
    fn inconsistent_reason(self) -> &'static str {
        match self {
            Reuse::Replayed => {
                "the attested reviewer.resolved event was internally inconsistent \
                 (a pass carrying a blocking finding, or a block with none)"
            }
            Reuse::Carried => {
                "the prior run's reviewer.resolved event was internally inconsistent \
                 (a pass carrying a blocking finding, or a block with none)"
            }
        }
    }
}

/// Reconstruct a [`Resolved`] row from a terminal outcome folded back in without
/// re-execution: replayed from a signed attestation, or carried from the branch's
/// prior run.
///
/// `event` arrives already parsed and boundary-checked by its producer
/// ([`crate::attest::replay::plan`] for a replay, [`crate::carry::plan`] for a
/// carry), each of which only ever hands this function a terminal reviewer event
/// whose `reviewer` field matches `reviewer.name`. Carry supplies only
/// [`RunEvent::ReviewerResolved`], while replay may also supply
/// [`RunEvent::ReviewerSkipped`].
/// There is nothing left to parse or revalidate at *that* boundary; a
/// different variant reaching here would be a planner defect, so the
/// let-else arm exists only to keep this total, not to police untrusted data a
/// second time.
///
/// What *does* stay reviewer-shaped input, and so is checked here rather than
/// trusted, is whether the claimed verdict is internally consistent. Fresh
/// execution never reaches [`resolve`] with a `pass` that also carries a blocking
/// finding, because [`backend::extract_verdict`] and the Claude Code backend's
/// own extraction both reject that shape before it becomes a [`ReviewOutcome`]
/// (see [`Verdict::is_consistent`]). A signed bundle and a prior run's store are
/// both attacker-shaped input just like an agent's raw output (a repository
/// carry re-verifies the prior run's seal in [`crate::carry::plan`], but a
/// user-level store carries no seal), so a reused event gets the identical check:
/// reconstruct the [`Verdict`] the event claims and require it to be consistent
/// before trusting its decision.
fn resolve_reused(reviewer: &Reviewer, event: &RunEvent, reuse: Reuse) -> Resolved {
    let is_gate = reviewer.mode == Mode::Gate;
    if let RunEvent::ReviewerSkipped {
        trigger, replayed, ..
    } = event
    {
        return Resolved {
            reviewer: reviewer.clone(),
            decision: Decision::Pass,
            summary: trigger.reason.clone(),
            findings: Vec::new(),
            usage: None,
            transcript: None,
            duration: Duration::from_millis(trigger.duration_ms),
            replayed: *replayed || matches!(reuse, Reuse::Replayed),
            carried: false,
            scope_digest: None,
            skipped: true,
            trigger: Some(trigger.clone()),
        };
    }
    let RunEvent::ReviewerResolved {
        verdict,
        summary,
        findings,
        usage,
        duration_ms,
        scope_digest,
        trigger,
        ..
    } = event
    else {
        return fail(reviewer, is_gate, reuse.malformed_reason(), Duration::ZERO);
    };

    // Normalize an advisor to its non-blocking shape first: an attested or prior
    // row (produced by any release, including one predating the universal
    // invariant) may carry a blocking finding, which becomes optional here.
    let (decision, findings) = clamp_advisor(is_gate, *verdict, findings.clone());
    let claimed = Verdict {
        decision,
        summary: summary.clone(),
        findings: findings.clone(),
    };
    // Consistency is a universal invariant: a pass must carry no blocking
    // finding. A gate that violates it fails closed; a normalized advisor always
    // satisfies it, so this never discards an advisor's advice (that advice now
    // rides as optional findings).
    if !claimed.is_consistent() {
        return fail(
            reviewer,
            is_gate,
            reuse.inconsistent_reason(),
            reuse.duration(*duration_ms),
        );
    }

    let (replayed, carried) = reuse.flags();
    Resolved {
        reviewer: reviewer.clone(),
        decision,
        summary: summary.clone(),
        findings,
        usage: reuse.usage(*usage),
        // There is no local transcript on either reuse path: an attestation
        // bundle carries only the resolved event, and a carry reuses the prior
        // run's row without its transcript file.
        transcript: None,
        duration: reuse.duration(*duration_ms),
        replayed,
        carried,
        scope_digest: reuse.scope_digest(scope_digest),
        skipped: false,
        trigger: trigger.clone(),
    }
}

/// Reconstruct a [`Resolved`] row for a replayed reviewer from its attested
/// `reviewer.resolved` event (`docs/developer-guide/attestation.md`). See
/// [`resolve_reused`] for the shared consistency guard.
pub(super) fn resolve_replayed(replay: &ReplayedReviewer) -> Resolved {
    resolve_reused(&replay.reviewer, &replay.event, Reuse::Replayed)
}

/// Reconstruct a [`Resolved`] row for a carried reviewer from the branch's
/// previous run ([`crate::carry`]). See [`resolve_reused`] for the shared
/// consistency guard; the scope digest is re-stamped from the prior event so the
/// carry chain stays unbroken on the next run.
pub(super) fn resolve_carried(carry: &crate::carry::Carried) -> Resolved {
    resolve_reused(&carry.reviewer, &carry.event, Reuse::Carried)
}
