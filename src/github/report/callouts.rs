//! The attestation and carried-verdict comment callouts.

use super::*;

/// Join reviewer names as comma-separated inline code (`` `a`, `b` ``), the form
/// both callouts list their reviewers in.
fn quoted_names(names: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    names
        .into_iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The subject-and-verb a callout opens with: `"1 reviewer was"` for a single
/// reviewer, `"reviewers were"` otherwise.
fn reviewer_was(count: usize) -> &'static str {
    if count == 1 {
        "1 reviewer was"
    } else {
        "reviewers were"
    }
}

/// The prominent `[!NOTE]` callout opening the comment when one or more
/// reviewers replayed from a signed local attestation instead of executing
/// fresh in CI (`docs/developer-guide/attestation.md`, "Verification and
/// replay in CI"). Uses the same GitHub alert mechanism as the skills-drift
/// `[!WARNING]` block so both advisories read consistently.
pub(super) fn attestation_callout(attested: &AttestedSummary) -> String {
    let names = quoted_names(&attested.reviewers);
    format!(
        "> [!NOTE]\n\
         > {} replayed from a signed local attestation rather than executed fresh: {names}.\n\
         > Attested by {} at {}.\n",
        reviewer_was(attested.reviewers.len()),
        truncate_key(&attested.public_key),
        attested.attested_at,
    )
}

/// A `[!NOTE]` callout naming the reviewers whose verdict was carried forward
/// from the branch's previous run (trigger-scoped diff unchanged) rather than
/// executed fresh this run ([`crate::carry`]). Mirrors [`attestation_callout`]
/// so the sticky comment flags carry the way it flags an attestation replay,
/// and stays consistent with the per-reviewer check-run line and the local
/// CLI's `carried` marker. Carry is not attestation and carries no signature:
/// the note states only that the verdict was reused, not that anyone signed it.
pub(super) fn carried_callout(reviewers: &[&str]) -> String {
    let names = quoted_names(reviewers.iter().copied());
    format!(
        "> [!NOTE]\n\
         > {} carried forward from the branch's previous run (trigger-scoped diff \
         unchanged) rather than executed fresh: {names}.\n",
        reviewer_was(reviewers.len()),
    )
}

/// The `[!WARNING]` callout drawn when an attestation was offered on HEAD but
/// not honored, so CI resolved every reviewer the ordinary way (carry-or-execute)
/// instead of replaying. Only a rejected attestation reaches here: a commit that
/// simply carries no note produces `NotAttested` upstream
/// (`src/attest/replay.rs`), records no `run.attestation-fallback` event, and so
/// draws nothing. Uses GitHub's `> [!WARNING]` alert, matching the skills-drift
/// block, so a refused attestation is prominent rather than an easily missed
/// italic aside.
pub(super) fn attestation_fallback_callout(reason: &str) -> String {
    format!("> [!WARNING]\n> Attestation was not honored: {reason}\n")
}
