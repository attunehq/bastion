//! Folding replayed and carried verdicts into resolved rows.

use super::*;

/// Reconstruct a [`Resolved`] row for a replayed reviewer from its attested
/// `reviewer.resolved` event.
///
/// `replay.event` arrives already parsed and boundary-checked:
/// [`crate::attest::replay::plan`] is the sole producer of a [`ReplayedReviewer`]
/// and only ever hands this function a [`RunEvent::ReviewerResolved`] whose
/// `reviewer` field matches `replay.reviewer.name` (its own key-to-event binding
/// check). There is nothing left here to parse or revalidate at that boundary; a
/// non-`ReviewerResolved` variant reaching this function would be a defect in
/// the planner, not attacker-shaped input, so the fallback arm exists only to
/// keep this total rather than to police untrusted data a second time.
///
/// What *does* stay reviewer-shaped input, and so is checked here rather than
/// trusted: whether the claimed verdict is internally consistent. Fresh
/// execution never reaches [`resolve`] with a `pass` that also carries a
/// blocking finding, because [`backend::extract_verdict`] and the Claude Code
/// backend's own extraction both reject that shape before it ever becomes a
/// [`ReviewOutcome`] (see [`Verdict::is_consistent`]). A signed bundle is
/// attacker-shaped input just like an agent's raw output, so a replayed event
/// gets the identical check: reconstruct the [`Verdict`] the event claims and
/// require it to be consistent before trusting its decision.
pub(super) fn resolve_replayed(replay: &ReplayedReviewer) -> Resolved {
    let is_gate = replay.reviewer.mode == Mode::Gate;
    match &replay.event {
        RunEvent::ReviewerResolved {
            verdict,
            summary,
            findings,
            usage,
            duration_ms,
            ..
        } => {
            // Normalize an advisor to its non-blocking shape first: an attested
            // row (produced by any release, including one predating the universal
            // invariant) may carry a blocking finding, which becomes optional here.
            let (decision, findings) = clamp_advisor(is_gate, *verdict, findings.clone());
            let claimed = Verdict {
                decision,
                summary: summary.clone(),
                findings: findings.clone(),
            };
            // Consistency is now a universal invariant: a pass must carry no
            // blocking finding. A gate that violates it fails closed; a normalized
            // advisor always satisfies it, so this never discards an advisor's
            // advice (that advice now rides as optional findings).
            if !claimed.is_consistent() {
                return fail(
                    &replay.reviewer,
                    is_gate,
                    "the attested reviewer.resolved event was internally inconsistent \
                     (a pass carrying a blocking finding, or a block with none)",
                    Duration::from_millis(*duration_ms),
                );
            }
            Resolved {
                reviewer: replay.reviewer.clone(),
                decision,
                summary: summary.clone(),
                findings,
                usage: *usage,
                // There is no local transcript in the CI store: the bundle carries
                // only the resolved event, never the transcript file.
                transcript: None,
                duration: Duration::from_millis(*duration_ms),
                counts_as_gate: is_gate,
                replayed: true,
                carried: false,
                // A replayed event's digest (if any) describes the attester's
                // working tree; do not re-stamp it here, so a replayed verdict
                // is never itself carried from.
                scope_digest: None,
            }
        }
        _ => fail(
            &replay.reviewer,
            is_gate,
            "the attested reviewer.resolved event was not a reviewer.resolved event \
             (a defect in the attestation planner, not the bundle)",
            Duration::ZERO,
        ),
    }
}

/// Reconstruct a [`Resolved`] row for a carried reviewer from the branch's
/// previous run ([`crate::carry`]).
///
/// The prior event was produced by this same pipeline and, for a repository
/// reviewer, re-verified against the prior run's seal by [`crate::carry::plan`].
/// A user-level reviewer's store carries no seal, so the same
/// internal-consistency check [`resolve_replayed`] applies guards a hand-edited
/// event from smuggling in an inconsistent verdict; anything inconsistent fails
/// closed per the reviewer's mode. Usage is dropped (no tokens were spent this
/// run) and the duration reads zero; the scope digest is re-stamped from the
/// prior event so the carry chain stays unbroken on the next run.
pub(super) fn resolve_carried(carry: &crate::carry::Carried) -> Resolved {
    let is_gate = carry.reviewer.mode == Mode::Gate;
    match &carry.event {
        RunEvent::ReviewerResolved {
            verdict,
            summary,
            findings,
            scope_digest,
            ..
        } => {
            // As in [`resolve_replayed`], normalize an advisor to its non-blocking
            // shape before the consistency check, so a carried advisor's blocking
            // finding becomes optional rather than tripping the universal invariant.
            let (decision, findings) = clamp_advisor(is_gate, *verdict, findings.clone());
            let claimed = Verdict {
                decision,
                summary: summary.clone(),
                findings: findings.clone(),
            };
            // Consistency is a universal invariant: a gate that violates it fails
            // closed, so a hand-edited (user-level, unsealed) store cannot smuggle
            // in a pass that carries a blocking finding; a normalized advisor
            // always satisfies it.
            if !claimed.is_consistent() {
                return fail(
                    &carry.reviewer,
                    is_gate,
                    "the prior run's reviewer.resolved event was internally inconsistent \
                     (a pass carrying a blocking finding, or a block with none)",
                    Duration::ZERO,
                );
            }
            Resolved {
                reviewer: carry.reviewer.clone(),
                decision,
                summary: summary.clone(),
                findings,
                usage: None,
                transcript: None,
                duration: Duration::ZERO,
                counts_as_gate: is_gate,
                replayed: false,
                carried: true,
                scope_digest: scope_digest.clone(),
            }
        }
        _ => fail(
            &carry.reviewer,
            is_gate,
            "the carried event was not a reviewer.resolved event \
             (a defect in the carry planner, not the store)",
            Duration::ZERO,
        ),
    }
}
