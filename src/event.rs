//! The run event stream.
//!
//! A run is a sequence of typed events emitted as each thing happens. The same
//! events are streamed to stdout as JSONL (`docs/developer-guide/local-surface.md`) and persisted to the
//! run's `run.jsonl`; the GitHub surfaces (`docs/developer-guide/github-adapter.md`) mirror them one to
//! one. Verbose detail (transcripts) is deliberately kept *off* the stream and
//! saved to disk instead, hence `has_transcript` on each terminal reviewer event
//! rather than the transcript itself.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::reviewer::{Backend, Mode};
use crate::verdict::{Decision, Finding, Money, Usage};

/// A run identifier, e.g. `r-0f3a`. Doubles as the run's directory name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl RunId {
    /// Borrow the underlying id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name + mode pair announced for each reviewer in a run's opening event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerRef {
    /// The reviewer's name.
    pub name: String,
    /// Whether it gates or advises.
    pub mode: Mode,
}

/// The gate tally carried by [`RunEvent::RunCompleted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gates {
    /// Total number of gates in the run's plan (the full triggered set, or
    /// only the selected subset on a partial run).
    pub total: u32,
    /// Gates that passed.
    pub passed: u32,
    /// Gates that blocked (or failed closed).
    pub blocked: u32,
    /// Gates whose agent trigger decided the reviewer did not apply.
    #[serde(default)]
    pub skipped: u32,
}

/// The semantic decision an agent trigger made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerDecision {
    /// Execute the full reviewer.
    Run,
    /// Omit the full reviewer from this changeset.
    Skip,
}

/// The recorded result of an agent trigger call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerResolution {
    /// The backend that made the routing decision.
    pub backend: crate::reviewer::Backend,
    /// Whether the full reviewer ran.
    pub decision: TriggerDecision,
    /// The agent's concise explanation, or the fail-closed reason that forced a run.
    pub reason: String,
    /// Token and cost accounting for the trigger call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Wall-clock duration of the trigger call.
    pub duration_ms: u64,
}

/// One event in a run's life cycle.
///
/// Serialized with a `"type"` discriminator using the dotted names from the
/// design (`run.started`, `reviewer.started`, the terminal reviewer events,
/// and `run.completed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum RunEvent {
    /// The run's plan: the locally-rendered equivalent of a PR's pending
    /// checks appearing. Each listed reviewer executes, semantically skips,
    /// replays from a verified attestation, or carries an unchanged prior pass.
    #[serde(rename = "run.started")]
    RunStarted {
        /// The run id.
        run: RunId,
        /// The branch under review.
        branch: String,
        /// The base branch the changeset is computed against.
        base: String,
        /// Number of changed files.
        changed: u32,
        /// The reviewer candidates in the plan.
        reviewers: Vec<ReviewerRef>,
        /// Whether this run was deliberately narrowed to a subset of the
        /// triggered reviewers (`bastion review --reviewer`). A partial run's
        /// aggregate verdict speaks only for the reviewers that ran, so the
        /// flag is load-bearing: it keeps a filtered green from being read (or
        /// attested) as a full green. Additive: runs persisted before this
        /// field existed were never filtered, so `false` is the correct
        /// default.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        partial: bool,
    },
    /// A reviewer began resolving (its spinner). Usually that is backend
    /// execution; for a replayed or carried reviewer the event still appears,
    /// so the plan reads the same, but no backend dispatches.
    #[serde(rename = "reviewer.started")]
    ReviewerStarted {
        /// The run id.
        run: RunId,
        /// The reviewer name.
        reviewer: String,
        /// Its mode.
        mode: Mode,
        /// The backend it runs on when it executes (nominal for a replayed or
        /// carried reviewer).
        backend: Backend,
    },
    /// A reviewer reached its conclusion, carrying the verdict and findings but
    /// not the transcript (see [`ReviewerResolved::has_transcript`]).
    #[serde(rename = "reviewer.resolved")]
    ReviewerResolved {
        /// The run id.
        run: RunId,
        /// The reviewer name.
        reviewer: String,
        /// The gate decision.
        verdict: Decision,
        /// A human-friendly summary.
        summary: String,
        /// Located findings explaining the decision.
        #[serde(default)]
        findings: Vec<Finding>,
        /// Token and cost accounting, when the backend reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
        /// Whether a transcript was saved to disk for this reviewer.
        has_transcript: bool,
        /// Whether this verdict was replayed from a signed local attestation
        /// rather than executed fresh (`docs/developer-guide/attestation.md`).
        /// Additive: a run persisted before this field existed deserializes with
        /// `false`, which is the correct reading (nothing could be replayed
        /// before the feature existed).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replayed: bool,
        /// Whether this verdict was carried forward from the branch's previous
        /// run because the reviewer's trigger-scoped diff was unchanged, rather
        /// than executed fresh this run (`docs/developer-guide/local-surface.md`,
        /// "Incremental re-review"). Additive: `false` for runs persisted before
        /// the field existed, which is the correct reading.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        carried: bool,
        /// A digest of everything this reviewer's verdict was scoped to: its own
        /// effective definition plus the path-matched diff for a path trigger or
        /// the full changeset for an agent trigger. A later run whose digest is
        /// identical may carry this verdict forward instead of re-executing the reviewer. `None` for runs
        /// persisted before the field existed and for replayed events, which
        /// simply makes them ineligible to carry from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_digest: Option<String>,
        /// The agent trigger decision that preceded this full review, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<TriggerResolution>,
    },
    /// An agent trigger decided that the full reviewer did not apply.
    #[serde(rename = "reviewer.skipped")]
    ReviewerSkipped {
        /// The run id.
        run: RunId,
        /// The reviewer name.
        reviewer: String,
        /// Its mode.
        mode: Mode,
        /// The recorded semantic routing decision.
        trigger: TriggerResolution,
        /// Whether a trigger transcript was saved locally.
        has_transcript: bool,
        /// Whether this outcome was replayed from an attestation.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replayed: bool,
    },
    /// A CI run that replayed one or more reviewers from a signed local
    /// attestation instead of executing them, recorded once per run as the
    /// audit trail for what was replayed, by which key, and when. The runner
    /// emits it when a verified attestation replays.
    #[serde(rename = "run.attested")]
    AttestationReplayed {
        /// The run id.
        run: RunId,
        /// The names of the reviewers replayed from the attestation.
        reviewers: Vec<String>,
        /// The SSH public key (as registered with the forge) that signed the
        /// attestation.
        public_key: String,
        /// When the attestation was signed, as recorded in the bundle.
        attested_at: String,
    },
    /// Emitted when attestations are enabled for a CI run and an attestation was
    /// *offered but refused*: the reviewers executed fresh, and this records why,
    /// so the report can tell the author rather than leaving them guessing
    /// (`docs/developer-guide/attestation.md`, "Verification and replay in CI").
    /// A commit that simply carries no note is not a refusal and records no such
    /// event (it resolves to `AttestationOutcome::NotAttested`); only a note that
    /// failed a check reaches here, which is why the report surfaces it in a
    /// `[!WARNING]` block.
    #[serde(rename = "run.attestation-fallback")]
    AttestationFallback {
        /// The run id.
        run: RunId,
        /// A plain-English reason naming the cause (an unverifiable signature, a
        /// seal mismatch, a stale binding, and so on).
        reason: String,
    },
    /// The aggregate outcome: the local equivalent of the `bastion` check.
    #[serde(rename = "run.completed")]
    RunCompleted {
        /// The run id.
        run: RunId,
        /// The aggregate gate decision.
        verdict: Decision,
        /// The gate tally.
        gates: Gates,
        /// Total wall-clock duration in milliseconds.
        duration_ms: u64,
        /// Total input tokens across reviewers. Defaults to 0 for runs persisted
        /// before this field existed, and for runs whose backends report no usage.
        #[serde(default)]
        tokens_in: u64,
        /// Total output tokens across reviewers. Defaults to 0 for runs persisted
        /// before this field existed, and for runs whose backends report no usage.
        #[serde(default)]
        tokens_out: u64,
        /// Total cache-read input tokens across reviewers. Defaults to 0 for runs
        /// persisted before this field existed, and for runs whose backends report
        /// no cache usage.
        #[serde(default)]
        cache_read: u64,
        /// Total cost across reviewers.
        cost_usd: Money,
        /// Whether this run was narrowed to a subset of the triggered reviewers
        /// (see [`RunEvent::RunStarted`]'s `partial`). Repeated on the closing
        /// event so a consumer reading only the verdict line still sees that the
        /// green (or red) is partial.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        partial: bool,
    },
}

impl RunEvent {
    /// The run id this event belongs to.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        match self {
            RunEvent::RunStarted { run, .. }
            | RunEvent::ReviewerStarted { run, .. }
            | RunEvent::ReviewerResolved { run, .. }
            | RunEvent::ReviewerSkipped { run, .. }
            | RunEvent::AttestationReplayed { run, .. }
            | RunEvent::AttestationFallback { run, .. }
            | RunEvent::RunCompleted { run, .. } => run,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::FindingKind;

    #[test]
    fn resolved_event_matches_the_documented_jsonl_shape() {
        let event = RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            trigger: None,
            run: RunId("r-0f3a".into()),
            reviewer: "tenant-isolation".into(),
            verdict: Decision::Block,
            summary: "A new query path reads rows without scoping by tenant id.".into(),
            findings: vec![Finding {
                kind: FindingKind::Blocking,
                path: "src/server/db.ts".into(),
                line_start: 88,
                line_end: 91,
                detail: "scope this query by tenant_id".into(),
            }],
            usage: Some(Usage {
                tokens_in: 18204,
                tokens_out: 1560,
                cache_read: 12000,
                cost_usd: Money::from_cents(21),
            }),
            duration_ms: 38120,
            has_transcript: true,
            replayed: false,
        };

        let line = serde_json::to_string(&event).expect("serializes");
        assert!(line.contains(r#""type":"reviewer.resolved""#));
        assert!(line.contains(r#""verdict":"block""#));
        assert!(line.contains(r#""cost_usd":0.21"#));
        // `replayed: false` is the common case and is omitted from the wire form.
        assert!(!line.contains("replayed"));

        let parsed: RunEvent = serde_json::from_str(&line).expect("round-trips");
        assert_eq!(parsed, event);
        assert_eq!(parsed.run_id().as_str(), "r-0f3a");
    }

    #[test]
    fn a_legacy_resolved_event_with_no_replayed_field_defaults_to_false() {
        // A run persisted before `replayed` existed must still load, and the
        // absence of a signed attestation to replay from is the correct reading:
        // it was executed fresh.
        let line = r#"{"type":"reviewer.resolved","run":"r-old","reviewer":"r","verdict":"pass","summary":"s","duration_ms":1,"has_transcript":false}"#;
        let parsed: RunEvent = serde_json::from_str(line).expect("legacy event loads");
        match parsed {
            RunEvent::ReviewerResolved { replayed, .. } => assert!(!replayed),
            other => panic!("expected reviewer.resolved, got {other:?}"),
        }
    }

    #[test]
    fn a_replayed_resolved_event_serializes_the_flag() {
        let event = RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            trigger: None,
            run: RunId("r-1".into()),
            reviewer: "tenant-isolation".into(),
            verdict: Decision::Pass,
            summary: "replayed from an attested local run".into(),
            findings: vec![],
            usage: None,
            duration_ms: 0,
            has_transcript: false,
            replayed: true,
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""replayed":true"#));
        assert_eq!(serde_json::from_str::<RunEvent>(&line).unwrap(), event);
    }

    #[test]
    fn a_carried_resolved_event_serializes_the_flag_and_digest() {
        let event = RunEvent::ReviewerResolved {
            run: RunId("r-1".into()),
            reviewer: "tenant-isolation".into(),
            verdict: Decision::Pass,
            summary: "carried forward from the branch's previous run".into(),
            findings: vec![],
            usage: None,
            duration_ms: 0,
            has_transcript: false,
            replayed: false,
            carried: true,
            scope_digest: Some("abc123".into()),
            trigger: None,
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""carried":true"#));
        assert!(line.contains(r#""scope_digest":"abc123""#));
        assert_eq!(serde_json::from_str::<RunEvent>(&line).unwrap(), event);
    }

    #[test]
    fn a_legacy_resolved_event_defaults_carried_false_and_no_digest() {
        // A run persisted before incremental re-review existed must load with
        // `carried: false` and no digest, which correctly makes it ineligible
        // to carry from while still readable.
        let line = r#"{"type":"reviewer.resolved","run":"r-old","reviewer":"r","verdict":"pass","summary":"s","duration_ms":1,"has_transcript":false}"#;
        match serde_json::from_str::<RunEvent>(line).expect("legacy event loads") {
            RunEvent::ReviewerResolved {
                carried,
                scope_digest,
                ..
            } => {
                assert!(!carried);
                assert_eq!(scope_digest, None);
            }
            other => panic!("expected reviewer.resolved, got {other:?}"),
        }
    }

    #[test]
    fn partial_flags_round_trip_and_default_to_false_on_legacy_events() {
        let started = RunEvent::RunStarted {
            run: RunId("r-1".into()),
            branch: "feat".into(),
            base: "main".into(),
            changed: 2,
            reviewers: vec![],
            partial: true,
        };
        let line = serde_json::to_string(&started).unwrap();
        assert!(line.contains(r#""partial":true"#));
        assert_eq!(serde_json::from_str::<RunEvent>(&line).unwrap(), started);

        // The unfiltered common case stays off the wire entirely.
        let full = RunEvent::RunStarted {
            run: RunId("r-1".into()),
            branch: "feat".into(),
            base: "main".into(),
            changed: 2,
            reviewers: vec![],
            partial: false,
        };
        assert!(!serde_json::to_string(&full).unwrap().contains("partial"));

        // Legacy events (no field) load as full runs.
        let legacy = r#"{"type":"run.completed","run":"r-old","verdict":"pass","gates":{"total":1,"passed":1,"blocked":0},"duration_ms":1,"cost_usd":0.0}"#;
        match serde_json::from_str::<RunEvent>(legacy).expect("legacy run.completed loads") {
            RunEvent::RunCompleted { partial, .. } => assert!(!partial),
            other => panic!("expected run.completed, got {other:?}"),
        }
    }

    #[test]
    fn attestation_replayed_round_trips() {
        let event = RunEvent::AttestationReplayed {
            run: RunId("r-1".into()),
            reviewers: vec!["tenant-isolation".into(), "file-responsibility".into()],
            public_key: "ssh-ed25519 AAAA...".into(),
            attested_at: "2026-07-01T12:00:00Z".into(),
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""type":"run.attested""#));
        let parsed: RunEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, event);
        assert_eq!(parsed.run_id().as_str(), "r-1");
    }

    #[test]
    fn attestation_fallback_round_trips() {
        let event = RunEvent::AttestationFallback {
            run: RunId("r-1".into()),
            reason: "the attestation's seal does not verify".into(),
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""type":"run.attestation-fallback""#));
        let parsed: RunEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, event);
        assert_eq!(parsed.run_id().as_str(), "r-1");
    }

    #[test]
    fn run_started_round_trips() {
        let event = RunEvent::RunStarted {
            partial: false,
            run: RunId("r-0f3a".into()),
            branch: "feat/cart".into(),
            base: "main".into(),
            changed: 12,
            reviewers: vec![ReviewerRef {
                name: "file-responsibility".into(),
                mode: Mode::Gate,
            }],
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""type":"run.started""#));
        assert_eq!(serde_json::from_str::<RunEvent>(&line).unwrap(), event);
    }

    #[test]
    fn run_completed_without_token_fields_defaults_them_to_zero() {
        // A run.completed persisted before tokens_in/tokens_out/cache_read existed
        // must still load (a `bastion show` over an old run.jsonl), defaulting the
        // missing usage totals to 0 rather than failing to deserialize.
        let line = r#"{"type":"run.completed","run":"r-old","verdict":"pass","gates":{"total":1,"passed":1,"blocked":0},"duration_ms":1000,"cost_usd":0.0}"#;
        let parsed: RunEvent = serde_json::from_str(line).expect("legacy run.completed loads");
        match parsed {
            RunEvent::RunCompleted {
                tokens_in,
                tokens_out,
                cache_read,
                ..
            } => {
                assert_eq!(tokens_in, 0);
                assert_eq!(tokens_out, 0);
                assert_eq!(cache_read, 0);
            }
            other => panic!("expected run.completed, got {other:?}"),
        }
    }
}
