//! Per-run spend caps: the safety net that bounds how many agent subprocesses a
//! single `bastion review` may launch.
//!
//! Bastion fans a changeset out to several reviewers, each of which shells out to
//! an agent CLI (Codex, Claude Code, Pi). Nothing in the base design bounds the
//! *total* cost of that fan-out: a reviewer that fails to start is retried, a
//! transient spawn or auth failure can turn into a respawn loop, and a run that is
//! quietly broken keeps launching agents until something external notices. One
//! real incident launched 561 Codex sessions in a day (about $272), including a
//! burst of 500 in 18 minutes where three quarters died at zero tokens and kept
//! respawning. Nothing said "we have launched hundreds of agents, stop."
//!
//! [`SpawnLimits`] is that stop. It is enforced by the spawn governor
//! ([`crate::backend::governor`]) at the one seam every agent launch passes
//! through, so the count includes reprompts and launches that die immediately, not
//! just the ones that produce a verdict. The defaults are conservative: they leave
//! a healthy full run untouched while turning a respawn storm into a loud, fast
//! abort.

use serde::Deserialize;

/// The caps one review run's agent fan-out must stay within.
///
/// Set on the root registry file under a `limits:` block; any field left out
/// takes its conservative default. The governor treats these as hard ceilings for
/// a single `bastion review`: reaching one aborts the run with a clear error
/// rather than continuing to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct SpawnLimits {
    /// The most agent subprocesses allowed to run at once, bounding the fan-out
    /// width (and so the peak concurrent spend). A run with more matched reviewers
    /// than this still runs them all; the extras queue until a slot frees. Clamped
    /// to at least 1 by the governor, since a width of zero could never launch a
    /// reviewer.
    pub max_concurrent: u32,
    /// The most agent launches a single review run may attempt in total. Every
    /// launch counts, including a reprompt and a launch that dies immediately at
    /// zero tokens, so a respawn storm trips this even though none of its spawns
    /// did any real work. A healthy full run spends a fraction of the default;
    /// reaching the cap means something is wrong, so the run aborts.
    pub max_total_spawns: u32,
    /// How many agent launches may fail to produce any output *in a row* before
    /// the breaker trips and aborts the run. This is the dead-spawn signature of
    /// the incident: a broken or unauthenticated agent CLI (an exit-127, an auth
    /// failure) that launches, dies at zero tokens, and is retried. A single
    /// productive launch resets the count, so an occasional transient failure
    /// never trips it; a sustained run of them trips it quickly.
    pub max_consecutive_failures: u32,
}

impl Default for SpawnLimits {
    fn default() -> Self {
        // A standard run fans out five gate reviewers plus an advisor, each of which
        // launches once (twice if it reprompts for a malformed verdict), so a healthy
        // full run is on the order of a dozen launches. These defaults sit well above
        // that headroom while still catching a storm: the consecutive-failure breaker
        // does the fast work (four dead launches in a row and the run aborts), and the
        // total cap backstops a slower leak that never trips the consecutive one.
        Self {
            max_concurrent: 8,
            max_total_spawns: 60,
            max_consecutive_failures: 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative_and_positive() {
        let limits = SpawnLimits::default();
        assert!(limits.max_concurrent >= 1);
        assert!(limits.max_total_spawns >= limits.max_concurrent);
        assert!(limits.max_consecutive_failures >= 1);
    }

    #[test]
    fn missing_fields_fall_back_to_the_conservative_defaults() {
        // An empty `limits:` block, or one that sets only a single field, must leave
        // the rest at their defaults rather than zeroing them (which would disable
        // the safety net or deadlock the fan-out).
        let only_total: SpawnLimits = serde_yaml_ng::from_str("max_total_spawns: 5").unwrap();
        assert_eq!(only_total.max_total_spawns, 5);
        assert_eq!(
            only_total.max_concurrent,
            SpawnLimits::default().max_concurrent
        );
        assert_eq!(
            only_total.max_consecutive_failures,
            SpawnLimits::default().max_consecutive_failures
        );

        let empty: SpawnLimits = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(empty, SpawnLimits::default());
    }

    #[test]
    fn an_unknown_limit_key_is_rejected() {
        // A typo in a cap name must fail loudly rather than silently leaving the
        // intended cap at its default.
        assert!(serde_yaml_ng::from_str::<SpawnLimits>("max_spawns: 5").is_err());
    }
}
