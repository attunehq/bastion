//! Report rendering and posting tests.

use super::*;
// Post-side internals (find_marker_comment, PostingApp) live in the post
// submodule and are exercised only here, so import them for the test build
// rather than re-globbing them into the non-test lib.
use super::post::*;
use crate::event::{ReviewerRef, RunId};
use crate::github::client::test_support::RecordingClient;
use crate::github::client::{ApiResponse, Method};

fn ctx() -> PrContext {
    PrContext {
        owner: "acme".into(),
        repo: "app".into(),
        pr: 12,
        head_sha: "deadbeef".into(),
    }
}

fn finding(kind: FindingKind, path: &str, start: u32, end: u32, detail: &str) -> Finding {
    Finding {
        kind,
        path: path.into(),
        line_start: start,
        line_end: end,
        detail: detail.into(),
    }
}

/// A representative run: one blocking gate (with a blocking and an optional
/// finding), one passing gate, one advisor with an optional finding.
fn sample_events() -> Vec<RunEvent> {
    let run = RunId("r-1".into());
    vec![
        RunEvent::RunStarted {
            partial: false,
            run: run.clone(),
            branch: "feat/cart".into(),
            base: "main".into(),
            changed: 3,
            reviewers: vec![
                ReviewerRef {
                    name: "tenant-isolation".into(),
                    mode: Mode::Gate,
                },
                ReviewerRef {
                    name: "file-responsibility".into(),
                    mode: Mode::Gate,
                },
                ReviewerRef {
                    name: "style".into(),
                    mode: Mode::Advisor,
                },
            ],
        },
        RunEvent::ReviewerStarted {
            run: run.clone(),
            reviewer: "tenant-isolation".into(),
            mode: Mode::Gate,
            backend: Backend::Codex,
        },
        RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            run: run.clone(),
            reviewer: "tenant-isolation".into(),
            verdict: Decision::Block,
            summary: "A query reads rows without scoping by tenant id.".into(),
            findings: vec![
                finding(
                    FindingKind::Blocking,
                    "src/db.ts",
                    88,
                    91,
                    "scope by tenant_id",
                ),
                finding(
                    FindingKind::Optional,
                    "src/db.ts",
                    10,
                    10,
                    "consider an index",
                ),
            ],
            usage: Some(Usage {
                tokens_in: 1820,
                tokens_out: 156,
                cache_read: 900,
                cost_usd: Money::from_cents(21),
            }),
            duration_ms: 38_000,
            has_transcript: true,
            replayed: false,
        },
        RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            run: run.clone(),
            reviewer: "file-responsibility".into(),
            verdict: Decision::Pass,
            summary: "Responsibilities look well separated.".into(),
            findings: vec![],
            usage: None,
            duration_ms: 12_000,
            has_transcript: true,
            replayed: false,
        },
        RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            run: run.clone(),
            reviewer: "style".into(),
            verdict: Decision::Pass,
            summary: "A couple of nits.".into(),
            findings: vec![finding(
                FindingKind::Optional,
                "src/x.ts",
                4,
                4,
                "rename foo",
            )],
            usage: None,
            duration_ms: 5_000,
            has_transcript: true,
            replayed: false,
        },
        RunEvent::RunCompleted {
            partial: false,
            run,
            verdict: Decision::Block,
            gates: Gates {
                total: 2,
                passed: 1,
                blocked: 1,
            },
            duration_ms: 40_000,
            tokens_in: 1820,
            tokens_out: 156,
            cache_read: 900,
            cost_usd: Money::from_cents(21),
        },
    ]
}

#[test]
fn comment_surfaces_every_finding_including_optional() {
    let body = comment_body(&digest(&sample_events()), false, None);
    // Marker for in-place upsert, and the headline.
    assert!(body.starts_with(MARKER));
    assert!(body.contains("**Blocked.** 1 of 2 gate(s) passed."));
    // The table lists all three reviewers with their verdict words.
    assert!(body.contains("| `tenant-isolation` | gate | block |"));
    assert!(body.contains("| `style` | advisor | advisory |"));
    // Both a blocking and an optional finding are rendered, with locations...
    assert!(body.contains("- **blocking** `src/db.ts:88-91`: scope by tenant_id"));
    assert!(body.contains("- **optional** `src/db.ts:10`: consider an index"));
    // ...including the advisor's optional finding, which never gates.
    assert!(body.contains("- **optional** `src/x.ts:4`: rename foo"));
    // No em dashes leaked into generated prose.
    assert!(!body.contains('\u{2014}') && !body.contains('\u{2013}'));
}

#[test]
fn status_line_carries_time_tokens_cache_and_cost_in_order() {
    // The sample run's only reviewer with usage reported 1820 in / 156 out / 900
    // cached, so the aggregate counter mirrors the local one: time, then tokens
    // (with the cache-read figure), then cost.
    let line = status_line(&digest(&sample_events()));
    assert!(line.contains("3 reviewer(s) ran, 40s, 1820 in / 156 out / 900 cached tokens, $0.21."));
}

#[test]
fn status_line_omits_tokens_when_none_were_reported() {
    // A zero-reviewer run reports no usage; the counter drops the token segment
    // rather than printing "0 in / 0 out tokens".
    let events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r".into()),
            branch: "b".into(),
            base: "main".into(),
            changed: 0,
            reviewers: vec![],
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    let line = status_line(&digest(&events));
    assert!(!line.contains("tokens"), "no token segment: {line}");
}

#[test]
fn comment_handles_zero_reviewers() {
    let events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r".into()),
            branch: "b".into(),
            base: "main".into(),
            changed: 0,
            reviewers: vec![],
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    let body = comment_body(&digest(&events), false, None);
    assert!(body.contains("No gates were triggered."));
    assert!(body.contains("No reviewers were triggered"));
    // With the nudge off, the footer carries no dedicated-app note.
    assert!(!body.contains(SETUP_URL));
}

#[test]
fn comment_footer_nudges_to_a_dedicated_app_when_asked() {
    // The nudge rides the footer in both the populated and the zero-reviewer
    // shapes, so a passing trivial run still surfaces it.
    let populated = comment_body(&digest(&sample_events()), true, None);
    assert!(populated.contains(SETUP_URL));
    assert!(populated.contains("shared GitHub Actions app"));

    let empty_events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r".into()),
            branch: "b".into(),
            base: "main".into(),
            changed: 0,
            reviewers: vec![],
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    assert!(comment_body(&digest(&empty_events), true, None).contains(SETUP_URL));
    // No Unicode dashes slipped into the nudge prose.
    assert!(!populated.contains('\u{2014}') && !populated.contains('\u{2013}'));
}

#[test]
fn comment_folds_in_a_skills_warning_when_given_one() {
    // The advisory rides just under the headline, above the reviewer table, and
    // only when supplied: a `None` warning leaves the comment untouched.
    let digest = digest(&sample_events());
    let warning = "> [!WARNING]\n> skills are stale; run `bastion skills install`.\n";

    let with = comment_body(&digest, false, Some(warning));
    assert!(with.contains("> [!WARNING]"));
    assert!(with.contains("skills are stale"));
    // It sits after the status headline but before the reviewer table.
    let warn_at = with.find("[!WARNING]").unwrap();
    let table_at = with.find("| Reviewer |").unwrap();
    let headline_at = with.find("reviewer(s) ran").unwrap();
    assert!(headline_at < warn_at && warn_at < table_at);

    let without = comment_body(&digest, false, None);
    assert!(!without.contains("[!WARNING]"));
}

#[test]
fn comment_folds_in_a_skills_warning_on_a_zero_reviewer_run() {
    // The zero-reviewer comment returns early through its own branch; the warning
    // is inserted before that branch, so it must still ride a no-reviewer run.
    let events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r".into()),
            branch: "b".into(),
            base: "main".into(),
            changed: 0,
            reviewers: vec![],
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    let warning = "> [!WARNING]\n> skills are stale.\n";
    let body = comment_body(&digest(&events), false, Some(warning));
    assert!(body.contains("> [!WARNING]"));
    // It lands after the headline but before the zero-reviewer sentence.
    let warn_at = body.find("[!WARNING]").unwrap();
    let none_at = body.find("No reviewers were triggered").unwrap();
    assert!(warn_at < none_at, "warning must precede the empty-run note");
}

/// [`sample_events`] with `tenant-isolation`'s resolved row marked
/// `replayed: true` and a matching `run.attested` audit event appended.
fn sample_events_with_replay() -> Vec<RunEvent> {
    let mut events = sample_events();
    for event in &mut events {
        if let RunEvent::ReviewerResolved {
            reviewer, replayed, ..
        } = event
            && reviewer == "tenant-isolation"
        {
            *replayed = true;
        }
    }
    events.push(RunEvent::AttestationReplayed {
        run: RunId("r-1".into()),
        reviewers: vec!["tenant-isolation".into()],
        public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZQ== ada@example.com"
            .into(),
        attested_at: "2026-07-01T12:00:00Z".into(),
    });
    events
}

#[test]
fn comment_opens_with_a_replay_callout_naming_reviewers_key_and_timestamp() {
    let digest = digest(&sample_events_with_replay());
    let body = comment_body(&digest, false, None);
    assert!(body.contains("[!NOTE]"));
    assert!(body.contains("`tenant-isolation`"));
    assert!(body.contains("2026-07-01T12:00:00Z"));
    // The full base64 blob is not dumped verbatim; only a truncated form.
    assert!(!body.contains("AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZQ=="));
    assert!(body.contains("ssh-ed25519"));
    assert!(body.contains("ada@example.com"));

    // The callout sits after the headline, before the reviewer table.
    let headline_at = body.find("reviewer(s) ran").unwrap();
    let note_at = body.find("[!NOTE]").unwrap();
    let table_at = body.find("| Reviewer |").unwrap();
    assert!(headline_at < note_at && note_at < table_at);
}

#[test]
fn comment_has_no_callout_when_nothing_replayed() {
    let digest = digest(&sample_events());
    let body = comment_body(&digest, false, None);
    assert!(!body.contains("[!NOTE]"));
}

#[test]
fn comment_warns_on_a_refused_attestation() {
    // A `run.attestation-fallback` event is only recorded when an attestation
    // was offered and rejected (a missing note produces `NotAttested`, no
    // event), so the report surfaces it prominently: a `[!WARNING]` block, not
    // an easily missed aside.
    let mut events = sample_events();
    events.push(RunEvent::AttestationFallback {
        run: RunId("r-1".into()),
        reason:
            "the attestation signature does not verify against grace's registered SSH signing keys"
                .into(),
    });
    let body = comment_body(&digest(&events), false, None);
    assert!(body.contains("> [!WARNING]"));
    assert!(
        body.contains("Attestation was not honored: the attestation signature does not verify")
    );
    assert!(!body.contains("[!NOTE]"), "a fallback is not a replay");
}

#[test]
fn replayed_reviewer_check_summary_states_it_was_replayed() {
    let digest = digest(&sample_events_with_replay());
    let checks = check_runs(&ctx(), &digest);
    let replayed_check = checks
        .iter()
        .find(|c| c.name == "bastion / tenant-isolation")
        .unwrap();
    assert!(
        replayed_check
            .summary
            .contains("Replayed from an attested local run")
    );

    // A non-replayed reviewer's summary carries no such note.
    let fresh_check = checks
        .iter()
        .find(|c| c.name == "bastion / file-responsibility")
        .unwrap();
    assert!(!fresh_check.summary.contains("Replayed"));
}

#[test]
fn carried_reviewer_check_summary_states_it_was_carried() {
    let mut events = sample_events();
    for event in &mut events {
        if let RunEvent::ReviewerResolved {
            reviewer, carried, ..
        } = event
            && reviewer == "file-responsibility"
        {
            *carried = true;
        }
    }
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    let carried_check = checks
        .iter()
        .find(|c| c.name == "bastion / file-responsibility")
        .unwrap();
    assert!(
        carried_check
            .summary
            .contains("Carried forward from the branch's previous run"),
        "got: {}",
        carried_check.summary
    );
    let fresh_check = checks
        .iter()
        .find(|c| c.name == "bastion / tenant-isolation")
        .unwrap();
    assert!(!fresh_check.summary.contains("Carried"));
}

/// Flip one reviewer to carried, the way a re-run with an unchanged
/// trigger-scoped diff records it.
fn sample_events_with_carry() -> Vec<RunEvent> {
    let mut events = sample_events();
    for event in &mut events {
        if let RunEvent::ReviewerResolved {
            reviewer, carried, ..
        } = event
            && reviewer == "file-responsibility"
        {
            *carried = true;
        }
    }
    events
}

#[test]
fn comment_opens_with_a_carry_callout_naming_the_carried_reviewers() {
    let body = comment_body(&digest(&sample_events_with_carry()), false, None);
    assert!(body.contains("[!NOTE]"));
    assert!(body.contains("carried forward from the branch's previous run"));
    assert!(body.contains("`file-responsibility`"));
    // A fresh reviewer is not named in the carry callout: the phrase and the
    // fresh reviewer's name never share a line.
    let callout_line = body
        .lines()
        .find(|line| line.contains("carried forward from the branch's previous run"))
        .unwrap();
    assert!(!callout_line.contains("`tenant-isolation`"));

    // The callout sits after the headline, before the reviewer table, the
    // same slot the replay callout uses.
    let headline_at = body.find("reviewer(s) ran").unwrap();
    let note_at = body.find("[!NOTE]").unwrap();
    let table_at = body.find("| Reviewer |").unwrap();
    assert!(headline_at < note_at && note_at < table_at);
}

#[test]
fn comment_has_no_carry_callout_when_nothing_carried() {
    let body = comment_body(&digest(&sample_events()), false, None);
    assert!(!body.contains("carried forward from the branch's previous run"));
}

#[test]
fn a_partial_run_is_named_in_the_comment_headline() {
    let mut events = sample_events();
    for event in &mut events {
        if let RunEvent::RunCompleted { partial, .. } = event {
            *partial = true;
        }
    }
    let body = comment_body(&digest(&events), false, None);
    assert!(
        body.contains("**Partial run:**"),
        "a filtered verdict must not read as a full one, got: {body}"
    );

    // The ordinary full run carries no such note.
    let full = comment_body(&digest(&sample_events()), false, None);
    assert!(!full.contains("Partial run"));
}

#[test]
fn truncate_key_shortens_the_material_and_keeps_type_and_comment() {
    let long = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGVudGlyZWx5RmFrZWtleW1hdGVyaWFsaGVyZQ== ada@example.com";
    let short = truncate_key(long);
    assert!(short.starts_with("ssh-ed25519 AAAAC3NzaC1l"));
    assert!(short.contains("..."));
    assert!(short.contains("(ada@example.com)"));
    assert!(short.len() < long.len());
}

#[test]
fn comment_cell_escaping_keeps_the_table_intact() {
    // A summary with a pipe and a newline must not break the row.
    let cell = escape_cell("a | b\nc");
    assert_eq!(cell, "a \\| b c");
}

#[test]
fn check_runs_map_gate_and_advisor_conclusions() {
    let checks = check_runs(&ctx(), &digest(&sample_events()));
    // Per reviewer plus the aggregate.
    assert_eq!(checks.len(), 4);

    let by_name = |name: &str| checks.iter().find(|c| c.name == name).unwrap().clone();

    let blocked = by_name("bastion / tenant-isolation");
    assert_eq!(blocked.conclusion, Conclusion::Failure);
    assert!(blocked.title.starts_with("Blocked:"));
    // Its blocking finding becomes a failure annotation; the optional one a warning.
    assert_eq!(blocked.annotations.len(), 2);
    assert_eq!(blocked.annotations[0].level, "failure");
    assert_eq!(blocked.annotations[1].level, "warning");
    assert_eq!(blocked.head_sha, "deadbeef");
    // The per-reviewer summary lists its token usage, including the cache-read
    // figure when nonzero.
    assert!(
        blocked
            .summary
            .contains("- Tokens: 1820 in, 156 out, 900 cached ($0.21)")
    );

    // The advisor, even with a finding, concludes success and never gates.
    let advisor = by_name("bastion / style");
    assert_eq!(advisor.conclusion, Conclusion::Success);
    assert!(advisor.title.starts_with("Advisory:"));

    // The aggregate reflects the blocked run and carries no annotations.
    let aggregate = by_name("bastion");
    assert_eq!(aggregate.conclusion, Conclusion::Failure);
    assert!(aggregate.annotations.is_empty());
    assert!(aggregate.title.contains("1/2"));
}

#[test]
fn aggregate_reads_incomplete_run_as_failure() {
    // A stream with no run.completed has no recorded verdict, so the aggregate
    // cannot read as a pass: an incomplete run concludes failure.
    let events = vec![RunEvent::RunStarted {
        partial: false,
        run: RunId("r".into()),
        branch: "b".into(),
        base: "main".into(),
        changed: 1,
        reviewers: vec![],
    }];
    let checks = check_runs(&ctx(), &digest(&events));
    let aggregate = checks.iter().find(|c| c.name == "bastion").unwrap();
    assert_eq!(aggregate.conclusion, Conclusion::Failure);
    assert_eq!(aggregate.title, "Incomplete run");
}

/// Build a run: it starts `started_gates` gates, resolves `rows` of them, and
/// records `completed` (the aggregate verdict and tally).
fn recorded_run(
    started_gates: &[&str],
    rows: Vec<RunEvent>,
    completed: (Decision, Gates),
) -> Vec<RunEvent> {
    let run = RunId("r-rec".into());
    let mut events = vec![RunEvent::RunStarted {
        partial: false,
        run: run.clone(),
        branch: "feat".into(),
        base: "main".into(),
        changed: 1,
        reviewers: started_gates
            .iter()
            .map(|name| ReviewerRef {
                name: (*name).into(),
                mode: Mode::Gate,
            })
            .collect(),
    }];
    events.extend(rows);
    events.push(RunEvent::RunCompleted {
        partial: false,
        run,
        verdict: completed.0,
        gates: completed.1,
        duration_ms: 1000,
        tokens_in: 0,
        tokens_out: 0,
        cache_read: 0,
        cost_usd: Money::from_cents(0),
    });
    events
}

fn gate_resolved(name: &str, verdict: Decision, findings: Vec<Finding>) -> RunEvent {
    RunEvent::ReviewerResolved {
        carried: false,
        scope_digest: None,
        run: RunId("r-rec".into()),
        reviewer: name.into(),
        verdict,
        summary: format!("{name} summary"),
        findings,
        usage: None,
        duration_ms: 1000,
        has_transcript: true,
        replayed: false,
    }
}

#[test]
fn clean_pass_with_gates_concludes_success() {
    // A recorded pass: the aggregate and the per-reviewer gate both go green.
    let events = recorded_run(
        &["g1"],
        vec![gate_resolved("g1", Decision::Pass, vec![])],
        (
            Decision::Pass,
            Gates {
                total: 1,
                passed: 1,
                blocked: 0,
            },
        ),
    );
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion")
            .unwrap()
            .conclusion,
        Conclusion::Success
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion / g1")
            .unwrap()
            .conclusion,
        Conclusion::Success
    );
}

#[test]
fn recorded_block_concludes_failure() {
    // A recorded block: the aggregate fails and the blocking gate's check fails.
    let events = recorded_run(
        &["g1"],
        vec![gate_resolved(
            "g1",
            Decision::Block,
            vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
        )],
        (
            Decision::Block,
            Gates {
                total: 1,
                passed: 0,
                blocked: 1,
            },
        ),
    );
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion")
            .unwrap()
            .conclusion,
        Conclusion::Failure
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion / g1")
            .unwrap()
            .conclusion,
        Conclusion::Failure
    );
    // The comment headline reflects the recorded block.
    assert!(comment_body(&digest, false, None).contains("**Blocked.** 0 of 1 gate(s) passed."));
}

#[test]
fn self_contradictory_gate_pass_fails_closed() {
    // A gate recorded as `pass` that carries a blocking finding contradicts itself
    // (the backends reject this upstream, but the report fails closed at the
    // boundary regardless). Even though run.completed recorded a pass, the gate's
    // own check and the aggregate both fail rather than publishing a green check.
    let events = recorded_run(
        &["g1"],
        vec![gate_resolved(
            "g1",
            Decision::Pass,
            vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
        )],
        (
            Decision::Pass,
            Gates {
                total: 1,
                passed: 1,
                blocked: 0,
            },
        ),
    );
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion / g1")
            .unwrap()
            .conclusion,
        Conclusion::Failure
    );
    let aggregate = checks.iter().find(|c| c.name == "bastion").unwrap();
    assert_eq!(aggregate.conclusion, Conclusion::Failure);
    assert!(aggregate.title.contains("internally inconsistent"));
    // The comment headline fails closed rather than claiming a pass.
    assert!(comment_body(&digest, false, None).contains("internally inconsistent"));
}

#[test]
fn trivial_pass_with_a_plan_but_no_reviewers_concludes_success() {
    // The legitimate zero-reviewer run: the plan was announced (one run.started
    // with no reviewers) and recorded a clean pass. The aggregate stays green.
    let events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r-rec".into()),
            branch: "feat".into(),
            base: "main".into(),
            changed: 1,
            reviewers: vec![],
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r-rec".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 1000,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    let digest = digest(&events);
    assert_eq!(
        check_runs(&ctx(), &digest)
            .iter()
            .find(|c| c.name == "bastion")
            .unwrap()
            .conclusion,
        Conclusion::Success
    );
}

#[test]
fn advisor_with_a_blocking_finding_does_not_block() {
    // A defensive/legacy shape: the runner now normalizes an advisor to a pass with
    // only optional findings, but a row from an older release (or a hand-edited store)
    // can still carry a blocking finding on a clamped advisor pass. The report must
    // never gate off it: the `mode == Mode::Gate` guard in `blocks()` keeps the
    // advisor check green and the recorded pass aggregate green.
    let events = vec![
        RunEvent::RunStarted {
            partial: false,
            run: RunId("r-rec".into()),
            branch: "feat".into(),
            base: "main".into(),
            changed: 1,
            reviewers: vec![ReviewerRef {
                name: "a1".into(),
                mode: Mode::Advisor,
            }],
        },
        RunEvent::ReviewerResolved {
            carried: false,
            scope_digest: None,
            run: RunId("r-rec".into()),
            reviewer: "a1".into(),
            verdict: Decision::Pass,
            summary: "x".into(),
            findings: vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, "leak")],
            usage: None,
            duration_ms: 1000,
            has_transcript: true,
            replayed: false,
        },
        RunEvent::RunCompleted {
            partial: false,
            run: RunId("r-rec".into()),
            verdict: Decision::Pass,
            gates: Gates {
                total: 0,
                passed: 0,
                blocked: 0,
            },
            duration_ms: 1000,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        },
    ];
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion / a1")
            .unwrap()
            .conclusion,
        Conclusion::Success
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c.name == "bastion")
            .unwrap()
            .conclusion,
        Conclusion::Success
    );
}

#[test]
fn oversized_annotation_message_is_truncated_with_a_pointer() {
    // A finding longer than the per-message cap would 422 the whole report
    // request; the annotation message is truncated and points at the comment,
    // while a short finding passes through unchanged.
    let long = "x".repeat(MAX_ANNOTATION_MESSAGE + 100);
    let big = finding(FindingKind::Optional, "src/a.rs", 1, 1, &long);
    let annotated = annotations_for(std::slice::from_ref(&big));
    assert_eq!(annotated.len(), 1);
    assert!(annotated[0].message.chars().count() <= MAX_ANNOTATION_MESSAGE + 80);
    assert!(
        annotated[0]
            .message
            .contains("(truncated; see the Bastion comment")
    );

    let small = finding(FindingKind::Optional, "src/a.rs", 2, 2, "nit");
    assert_eq!(
        annotations_for(std::slice::from_ref(&small))[0].message,
        "nit"
    );
}

#[test]
fn oversized_check_summary_is_capped_with_a_pointer() {
    // A reviewer carrying an enormous finding: the per-reviewer check summary must
    // stay under GitHub's output.summary limit, with a pointer to the comment.
    let huge = "y".repeat(MAX_CHECK_SUMMARY + 5000);
    let events = recorded_run(
        &["g1"],
        vec![gate_resolved(
            "g1",
            Decision::Block,
            vec![finding(FindingKind::Blocking, "src/a.rs", 1, 1, &huge)],
        )],
        (
            Decision::Block,
            Gates {
                total: 1,
                passed: 0,
                blocked: 1,
            },
        ),
    );
    let digest = digest(&events);
    let checks = check_runs(&ctx(), &digest);
    let g1 = checks.iter().find(|c| c.name == "bastion / g1").unwrap();
    assert!(g1.summary.chars().count() <= MAX_CHECK_SUMMARY + 80);
    assert!(g1.summary.contains("truncated; see the Bastion comment"));
}

#[test]
fn synthetic_crash_finding_is_not_annotated() {
    // The runner's fail-closed marker has an empty path and line 0; it must be
    // rendered in prose but never sent as an annotation (GitHub rejects line 0).
    let crash = finding(
        FindingKind::Blocking,
        "",
        0,
        0,
        "reviewer failed to complete",
    );
    assert!(!is_locatable(&crash));
    assert!(annotations_for(std::slice::from_ref(&crash)).is_empty());
    assert!(finding_bullet(&crash).contains("- **blocking**: reviewer failed to complete"));
}

#[test]
fn annotations_cap_at_the_limit_and_the_summary_notes_the_overflow() {
    // GitHub accepts at most MAX_ANNOTATIONS annotations per request. With more
    // locatable findings than the cap, annotations_for must stop at the cap and
    // the reviewer-check summary must say how many located findings went unpinned.
    let overflow = 5;
    let findings: Vec<Finding> = (0..MAX_ANNOTATIONS + overflow)
        .map(|i| {
            let line = u32::try_from(i + 1).unwrap();
            finding(FindingKind::Optional, "src/big.rs", line, line, "nit")
        })
        .collect();

    let annotations = annotations_for(&findings);
    assert_eq!(annotations.len(), MAX_ANNOTATIONS);

    let row = ReviewerRow {
        carried: false,
        name: "style".into(),
        mode: Mode::Advisor,
        backend: Some(Backend::Codex),
        decision: Decision::Pass,
        summary: "many nits".into(),
        findings,
        duration_ms: 1000,
        usage: None,
        replayed: false,
    };
    let summary = reviewer_check_summary(&row, &annotations);
    assert!(summary.contains(&format!(
        "{overflow} more located finding(s) are listed above but not pinned to the diff"
    )));
}

#[test]
fn reviewer_summary_tokens_line_includes_cache_only_when_nonzero() {
    let row_with = |cache_read: u64| ReviewerRow {
        carried: false,
        name: "r".into(),
        mode: Mode::Gate,
        backend: Some(Backend::ClaudeCode),
        decision: Decision::Pass,
        summary: "ok".into(),
        findings: vec![],
        duration_ms: 1000,
        usage: Some(Usage {
            tokens_in: 1200,
            tokens_out: 80,
            cache_read,
            cost_usd: Money::from_cents(5),
        }),
        replayed: false,
    };

    // Cache hits present: the cached figure rides the token line.
    let with_cache = reviewer_check_summary(&row_with(600), &[]);
    assert!(with_cache.contains("- Tokens: 1200 in, 80 out, 600 cached ($0.05)"));

    // No cache hits: the cached segment is omitted, the in/out line stays.
    let no_cache = reviewer_check_summary(&row_with(0), &[]);
    assert!(no_cache.contains("- Tokens: 1200 in, 80 out ($0.05)"));
    assert!(!no_cache.contains("cached"));
}

#[test]
fn request_builders_target_the_right_endpoints() {
    let ctx = ctx();
    assert_eq!(
        comment_list_request(&ctx).path,
        "/repos/acme/app/issues/12/comments?per_page=100"
    );
    let create = comment_create_request(&ctx, "hi");
    assert_eq!(create.method, Method::Post);
    assert_eq!(create.path, "/repos/acme/app/issues/12/comments");
    assert_eq!(create.body.unwrap()["body"], "hi");

    let update = comment_update_request(&ctx, 7, "ho");
    assert_eq!(update.method, Method::Patch);
    assert_eq!(update.path, "/repos/acme/app/issues/comments/7");

    let check = CheckRun {
        name: "bastion".into(),
        head_sha: "sha".into(),
        conclusion: Conclusion::Failure,
        title: "t".into(),
        summary: "s".into(),
        annotations: vec![],
    };
    let req = check_run_request(&ctx, &check);
    assert_eq!(req.path, "/repos/acme/app/check-runs");
    let body = req.body.unwrap();
    assert_eq!(body["conclusion"], "failure");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["head_sha"], "sha");
}

#[test]
fn find_marker_comment_matches_only_bastions_own() {
    let list = serde_json::json!([
        {"id": 1, "body": "a human comment"},
        {"id": 2, "body": format!("{MARKER}\n## Bastion review")},
    ]);
    assert_eq!(find_marker_comment(&list).unwrap(), Some(2));

    let none = serde_json::json!([{"id": 1, "body": "no marker here"}]);
    assert_eq!(find_marker_comment(&none).unwrap(), None);

    // A malformed body (not the expected array) fails closed rather than
    // reporting "none found", which would post a duplicate comment.
    let malformed = serde_json::json!({"message": "Not Found"});
    assert!(find_marker_comment(&malformed).is_err());
}

#[tokio::test]
async fn report_creates_a_comment_then_posts_checks() {
    // No existing comment: the list returns empty, so the report POSTs a new one.
    // The check-run responses carry the shared `github-actions` app, as the
    // default GITHUB_TOKEN would, so the report should detect that and nudge.
    let api = RecordingClient::with_responder(|req| {
        if req.method == Method::Get {
            ApiResponse {
                status: 200,
                body: serde_json::json!([]),
            }
        } else if req.path.ends_with("/check-runs") {
            ApiResponse {
                status: 201,
                body: serde_json::json!({"id": 1, "app": {"slug": "github-actions"}}),
            }
        } else {
            ApiResponse {
                status: 201,
                body: serde_json::json!({"id": 555}),
            }
        }
    });
    let summary = report(&api, &ctx(), &sample_events(), None)
        .await
        .expect("reports");
    assert_eq!(summary.comment, CommentAction::Created);
    assert_eq!(summary.checks, 4);

    let calls = api.calls();
    // The checks are posted first (so the report can read its posting identity
    // from a response), then the comment is upserted.
    let last_check = calls
        .iter()
        .rposition(|c| c.path.ends_with("/check-runs"))
        .expect("a check-run POST");
    let first_comment = calls
        .iter()
        .position(|c| c.path.contains("/issues/"))
        .expect("a comment request");
    assert!(
        last_check < first_comment,
        "checks should be posted before the comment: {calls:?}"
    );
    let check_calls = calls
        .iter()
        .filter(|c| c.path.ends_with("/check-runs"))
        .count();
    assert_eq!(check_calls, 4);
    // The created comment body carries the marker and the optional finding.
    let comment_post = calls
        .iter()
        .find(|c| c.method == Method::Post && c.path.ends_with("/issues/12/comments"))
        .expect("a comment POST");
    let body = comment_post.body.as_ref().unwrap()["body"]
        .as_str()
        .unwrap();
    assert!(body.contains(MARKER));
    assert!(body.contains("rename foo"));
    // Posted under the shared github-actions app, so the nudge is present.
    assert!(body.contains(SETUP_URL));
}

#[tokio::test]
async fn report_omits_the_nudge_under_a_dedicated_app() {
    // The check-run responses carry a distinct app slug, as a dedicated Bastion
    // app would. The checks then form their own suite, so no nudge is needed.
    let api = RecordingClient::with_responder(|req| {
        if req.method == Method::Get {
            ApiResponse {
                status: 200,
                body: serde_json::json!([]),
            }
        } else if req.path.ends_with("/check-runs") {
            ApiResponse {
                status: 201,
                body: serde_json::json!({"id": 1, "app": {"slug": "bastion-acme"}}),
            }
        } else {
            ApiResponse {
                status: 201,
                body: serde_json::json!({"id": 555}),
            }
        }
    });
    report(&api, &ctx(), &sample_events(), None)
        .await
        .expect("reports");

    let comment_post = api
        .calls()
        .into_iter()
        .find(|c| c.method == Method::Post && c.path.ends_with("/issues/12/comments"))
        .expect("a comment POST");
    let body = comment_post.body.as_ref().unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !body.contains(SETUP_URL),
        "dedicated app should not nudge: {body}"
    );
}

#[test]
fn posting_app_classifies_the_creating_app() {
    use serde_json::json;

    assert_eq!(
        PostingApp::from_check_run(&json!({"id": 1, "app": {"slug": "github-actions"}})),
        PostingApp::SharedActions
    );
    assert_eq!(
        PostingApp::from_check_run(&json!({"id": 1, "app": {"slug": "bastion-acme"}})),
        PostingApp::Dedicated
    );
    // A response missing the app, the slug, or with a non-string slug is Unknown,
    // so a malformed body leaves the nudge off rather than guessing.
    assert_eq!(
        PostingApp::from_check_run(&json!({"id": 1})),
        PostingApp::Unknown
    );
    assert_eq!(
        PostingApp::from_check_run(&json!({"app": {}})),
        PostingApp::Unknown
    );
    assert_eq!(
        PostingApp::from_check_run(&json!({"app": {"slug": 7}})),
        PostingApp::Unknown
    );

    // Only the shared identity nudges.
    assert!(PostingApp::SharedActions.should_suggest_dedicated_app());
    assert!(!PostingApp::Dedicated.should_suggest_dedicated_app());
    assert!(!PostingApp::Unknown.should_suggest_dedicated_app());
}

#[tokio::test]
async fn report_updates_an_existing_comment_in_place() {
    // The list returns Bastion's own comment, so the report PATCHes it. The
    // non-GET responses carry no `app`, so this also pins the missing-slug path:
    // an unreadable identity leaves the nudge off.
    let api = RecordingClient::with_responder(|req| match req.method {
        Method::Get => ApiResponse {
            status: 200,
            body: serde_json::json!([{"id": 909, "body": format!("{MARKER} old")}]),
        },
        _ => ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
        },
    });
    let summary = report(&api, &ctx(), &sample_events(), None)
        .await
        .expect("reports");
    assert_eq!(summary.comment, CommentAction::Updated(909));

    // The existing comment is updated in place with a PATCH (the checks are
    // posted first, so the PATCH is no longer at a fixed index).
    let patch = api
        .calls()
        .into_iter()
        .find(|c| c.method == Method::Patch)
        .expect("a PATCH to the existing comment");
    assert!(patch.path.ends_with("/issues/comments/909"));
    let body = patch.body.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(
        !body.contains(SETUP_URL),
        "missing app.slug should not nudge: {body}"
    );
}

#[tokio::test]
async fn report_fails_closed_on_a_rejected_request() {
    // GitHub rejects the first request (a check-run POST): the report errors
    // rather than pressing on.
    let api = RecordingClient::with_responder(|_| ApiResponse {
        status: 403,
        body: serde_json::json!({"message": "Resource not accessible by integration"}),
    });
    let err = report(&api, &ctx(), &sample_events(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("returned 403"));
    assert!(err.to_string().contains("Resource not accessible"));
}

#[test]
fn truncate_caps_and_marks_overflow() {
    assert_eq!(truncate("short", 110), "short");
    let long = "x".repeat(200);
    let cut = truncate(&long, 110);
    assert_eq!(cut.chars().count(), 110);
    assert!(cut.ends_with("..."));
}
