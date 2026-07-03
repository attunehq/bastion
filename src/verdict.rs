//! The structured judgment a reviewer returns.
//!
//! Every reviewer emits a [`Verdict`] via its backend's structured-output mode.
//! The top-level [`Decision`] is the authoritative gate outcome; [`Finding`]s
//! explain it. See the verdict schema in `docs/developer-guide/design.md`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A monetary amount, stored exactly as integer cents but carried on the wire as
/// decimal dollars to match the documented event schema (`"cost_usd": 0.21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Money {
    cents: u64,
}

impl Money {
    /// Construct from an exact number of cents.
    #[must_use]
    pub const fn from_cents(cents: u64) -> Self {
        Self { cents }
    }

    /// The amount in whole cents.
    #[must_use]
    pub const fn cents(self) -> u64 {
        self.cents
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}.{:02}", self.cents / 100, self.cents % 100)
    }
}

impl Serialize for Money {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[expect(
            clippy::cast_precision_loss,
            reason = "cent counts are far below f64's exact range"
        )]
        serializer.serialize_f64(self.cents as f64 / 100.0)
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let dollars = f64::deserialize(deserializer)?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rounded, non-negative cents"
        )]
        let cents = (dollars * 100.0).round().max(0.0) as u64;
        Ok(Self { cents })
    }
}

/// The authoritative gate outcome of a reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Decision {
    /// The reviewer is satisfied; it does not block the merge.
    Pass,
    /// The reviewer blocks the merge.
    Block,
}

impl Decision {
    /// Whether this decision blocks the merge.
    #[must_use]
    pub fn is_block(self) -> bool {
        matches!(self, Decision::Block)
    }

    /// The lowercase wire form (`"pass"` / `"block"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Pass => "pass",
            Decision::Block => "block",
        }
    }
}

/// Whether a finding holds up the merge or is merely a suggestion.
///
/// A finding's kind affects how a comment is surfaced, not the gate outcome;
/// only the top-level [`Decision`] decides that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FindingKind {
    /// A reason the reviewer blocked.
    Blocking,
    /// A non-blocking suggestion.
    Optional,
}

/// A specific, located comment from a reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Whether the finding is blocking or optional.
    pub kind: FindingKind,
    /// Repository-relative path the finding refers to.
    pub path: String,
    /// First line of the referenced range (1-based).
    pub line_start: u32,
    /// Last line of the referenced range (1-based, inclusive).
    pub line_end: u32,
    /// The human-readable comment.
    pub detail: String,
}

/// Token and cost accounting for a single reviewer run, when the backend
/// reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens consumed.
    pub tokens_in: u64,
    /// Output tokens produced.
    pub tokens_out: u64,
    /// Input tokens served from the provider's prompt cache (cache-read tokens),
    /// when the backend reports them. Reported as the backend gives it: how it
    /// relates to `tokens_in` (subset or separate) varies by provider, so treat it
    /// as an independent figure. Defaults to 0 for a backend or run that reports no
    /// cache usage.
    #[serde(default)]
    pub cache_read: u64,
    /// Session cost.
    pub cost_usd: Money,
}

/// Format a token-counter segment like `1820 in / 156 out / 900 cached tokens`,
/// returning `None` when nothing was reported. The cache-read figure is appended
/// only when nonzero. Shared by the local counter ([`crate::render`]) and the
/// GitHub status line ([`crate::github`]) so the two surfaces never drift.
#[must_use]
pub fn format_token_counter(tokens_in: u64, tokens_out: u64, cache_read: u64) -> Option<String> {
    if tokens_in == 0 && tokens_out == 0 && cache_read == 0 {
        return None;
    }
    let cached = if cache_read > 0 {
        format!(" / {cache_read} cached")
    } else {
        String::new()
    };
    Some(format!("{tokens_in} in / {tokens_out} out{cached} tokens"))
}

/// A reviewer's complete structured judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// The authoritative gate decision.
    #[serde(rename = "verdict")]
    pub decision: Decision,
    /// A human-friendly summary of the review.
    pub summary: String,
    /// Specific located comments explaining the decision.
    #[serde(default)]
    pub findings: Vec<Finding>,
}

impl Verdict {
    /// Whether the verdict is internally consistent.
    ///
    /// A `block` must carry at least one [`FindingKind::Blocking`] finding (the
    /// reason it blocked). A `pass` must carry *no* blocking findings: a passing
    /// gate that nonetheless lists blocking reasons is self-contradictory, and
    /// since the top-level decision is authoritative, trusting it would fail open
    /// (pass a merge while flagging blockers). A `pass` may carry optional
    /// findings as non-blocking suggestions.
    ///
    /// This is a *universal* invariant, not one only gates satisfy. An advisor
    /// never blocks, so the runner clamps it to a `pass` and downgrades any
    /// blocking finding to optional at resolution ([`crate::runner`]), which means
    /// a well-formed advisor row satisfies this check like any other pass. Every
    /// resolution path can therefore apply it without special-casing mode.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        let has_blocking = self
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::Blocking);
        match self.decision {
            Decision::Pass => !has_blocking,
            Decision::Block => has_blocking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_without_cache_read_defaults_to_zero() {
        // A reviewer.resolved usage object persisted before cache_read existed must
        // still deserialize, defaulting the missing field to 0 rather than erroring.
        let old: Usage =
            serde_json::from_str(r#"{"tokens_in":100,"tokens_out":10,"cost_usd":0.21}"#)
                .expect("legacy usage without cache_read still loads");
        assert_eq!(old.tokens_in, 100);
        assert_eq!(old.tokens_out, 10);
        assert_eq!(old.cache_read, 0);
        assert_eq!(old.cost_usd, Money::from_cents(21));
    }

    #[test]
    fn token_counter_formats_in_out_and_optional_cache() {
        // All three figures present.
        assert_eq!(
            format_token_counter(4200, 270, 3100).as_deref(),
            Some("4200 in / 270 out / 3100 cached tokens"),
        );
        // No cache hits: the cached segment is dropped, the in/out counter stays.
        assert_eq!(
            format_token_counter(4200, 270, 0).as_deref(),
            Some("4200 in / 270 out tokens"),
        );
        // Nothing reported at all: no segment.
        assert_eq!(format_token_counter(0, 0, 0), None);
    }

    #[test]
    fn parses_a_blocking_verdict_from_the_design_schema() {
        let yaml = r#"
verdict: block
summary: A new query path reads rows without scoping by tenant id.
findings:
  - kind: blocking
    path: src/server/db.ts
    line_start: 88
    line_end: 91
    detail: scope this query by tenant_id
"#;
        let verdict: Verdict = serde_yaml_ng::from_str(yaml).expect("valid verdict");
        assert!(verdict.decision.is_block());
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].kind, FindingKind::Blocking);
        assert!(verdict.is_consistent());
    }

    #[test]
    fn a_pass_needs_no_findings_but_a_block_does() {
        let pass = Verdict {
            decision: Decision::Pass,
            summary: "ok".into(),
            findings: vec![],
        };
        assert!(pass.is_consistent());

        let bad_block = Verdict {
            decision: Decision::Block,
            summary: "no reason".into(),
            findings: vec![],
        };
        assert!(!bad_block.is_consistent());
    }

    #[test]
    fn a_pass_carrying_blocking_findings_is_inconsistent() {
        // A passing gate that also lists a blocking reason is self-contradictory;
        // trusting its top-level `pass` would fail open. Reject it so the caller
        // fails closed.
        let contradictory = Verdict {
            decision: Decision::Pass,
            summary: "passes but blocks?".into(),
            findings: vec![Finding {
                kind: FindingKind::Blocking,
                path: "src/x.rs".into(),
                line_start: 1,
                line_end: 1,
                detail: "this blocks".into(),
            }],
        };
        assert!(!contradictory.is_consistent());

        // A pass with only optional findings is fine.
        let pass_with_optional = Verdict {
            decision: Decision::Pass,
            summary: "ok with a nit".into(),
            findings: vec![Finding {
                kind: FindingKind::Optional,
                path: "src/x.rs".into(),
                line_start: 1,
                line_end: 1,
                detail: "nit".into(),
            }],
        };
        assert!(pass_with_optional.is_consistent());
    }

    #[test]
    fn a_normalized_advisor_verdict_is_universally_consistent() {
        // The runner clamps an advisor to a pass and downgrades its blocking
        // findings to optional (see `runner::clamp_advisor`). The result must
        // satisfy `is_consistent` with no mode-aware exemption: a pass carrying
        // only optional advice is exactly the consistent shape, so the replay and
        // carry paths can apply this check to advisors and gates alike.
        let normalized_advisor = Verdict {
            decision: Decision::Pass,
            summary: "advisory concern, downgraded to advice".into(),
            findings: vec![Finding {
                kind: FindingKind::Optional,
                path: "src/x.rs".into(),
                line_start: 1,
                line_end: 1,
                detail: "was blocking, now optional".into(),
            }],
        };
        assert!(normalized_advisor.is_consistent());
    }

    #[test]
    fn money_serializes_as_dollars_and_round_trips() {
        let money = Money::from_cents(21);
        assert_eq!(serde_json::to_string(&money).unwrap(), "0.21");
        assert_eq!(money.to_string(), "$0.21");
        let back: Money = serde_json::from_str("0.37").unwrap();
        assert_eq!(back.cents(), 37);
    }
}
