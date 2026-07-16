//! Persistence and the read-back surface.
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;

use std::time::Duration;

use bastion::event::RunEvent;
use bastion::store::{self, RunSummary};
use bastion::verdict::{Decision, FindingKind, Verdict};

/// What a blocking run persists round-trips faithfully: the on-disk `run.jsonl` is
/// the full ordered event stream, `verdict.json`/`meta.json` carry the structured
/// result, and `show` replays the same blocking finding the live run emitted.
#[test]
fn a_blocking_run_persists_and_replays_faithfully() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "persisted",
        "claude-code",
        "gate",
    )
    .behavior("block")
    .env("FAKE_SUMMARY", "blocked for a real reason")]));
    let run = repo.review(fake);
    assert_eq!(run.code, Some(1));

    let layout = repo.layout();
    let runs = store::list_runs(&layout).unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = &runs[0].run;

    // run.jsonl is the full event stream in order. With a single reviewer the
    // exact sequence is pinned, so a reordering (e.g. resolved before started)
    // would be caught, not just a missing-event regression.
    let persisted = store::read_run(&layout, run_id).unwrap();
    let sequence: Vec<&str> = persisted.iter().map(event_kind).collect();
    assert_eq!(
        sequence,
        [
            "run.started",
            "reviewer.started",
            "reviewer.finished",
            "reviewer.resolved",
            "run.completed"
        ],
        "persisted run.jsonl is not the expected ordered stream"
    );

    // verdict.json carries the structured verdict.
    let verdict_json = std::fs::read_to_string(layout.verdict(run_id, "persisted")).unwrap();
    let verdict: Verdict = serde_json::from_str(&verdict_json).unwrap();
    assert_eq!(verdict.decision, Decision::Block);
    assert!(
        verdict
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::Blocking)
    );

    // meta.json carries the reviewer's backend/mode/trigger (ReviewerMeta is
    // private, so assert via the JSON shape).
    let meta_json = std::fs::read_to_string(layout.meta(run_id, "persisted")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta["backend"], "claude-code");
    assert_eq!(meta["mode"], "gate");
    assert_eq!(meta["trigger"][0], "src/**/*.rs");

    // `show <run>` replays the persisted finding, proving read-back equals the run.
    let show = repo.run(fake, &["show", run_id.as_str(), "--format", "jsonl"], &[]);
    assert!(show.status.success());
    let replay = parse_events(
        &String::from_utf8_lossy(&show.stdout),
        &String::from_utf8_lossy(&show.stderr),
    );
    let replayed_finding = replay.iter().any(|e| match e {
        RunEvent::ReviewerResolved { findings, .. } => findings
            .iter()
            .any(|f| f.detail.contains("simulated blocking finding")),
        _ => false,
    });
    assert!(replayed_finding, "show did not replay the blocking finding");
}

/// The read-back surface works over a real persisted run, including the explicit
/// run-id forms of `transcript` and `show`, and the deterministic `clean --keep 0`.
#[test]
fn the_read_back_commands_work_over_a_real_run() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[Reviewer::new("readback", "codex", "gate")
        .behavior("pass")
        .env("FAKE_SUMMARY", "a memorable summary")]));
    let review = repo.review(fake);
    assert!(review.exited_zero(), "stderr:\n{}", review.stderr);

    let run_id = repo.latest_run_id();

    // `runs --format jsonl` lists exactly that run.
    let runs_out = repo.run(fake, &["runs", "--format", "jsonl"], &[]);
    assert!(runs_out.status.success());
    let summaries: Vec<RunSummary> = String::from_utf8_lossy(&runs_out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("runs line is a RunSummary"))
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].run, run_id);
    assert_eq!(summaries[0].verdict, Some(Decision::Pass));

    // `show <run> --format jsonl` re-emits the resolved verdict and the completion.
    let show_out = repo.run(fake, &["show", run_id.as_str(), "--format", "jsonl"], &[]);
    assert!(show_out.status.success());
    let show_events = parse_events(
        &String::from_utf8_lossy(&show_out.stdout),
        &String::from_utf8_lossy(&show_out.stderr),
    );
    let resolved_ok = show_events.iter().any(|e| {
        matches!(
            e,
            RunEvent::ReviewerResolved { reviewer, summary, .. }
                if reviewer == "readback" && summary == "a memorable summary"
        )
    });
    assert!(
        resolved_ok,
        "show did not re-emit the readback reviewer with its summary"
    );
    assert!(
        show_events
            .iter()
            .any(|e| matches!(e, RunEvent::RunCompleted { .. }))
    );

    // `transcript <run> <reviewer>` (explicit two-positional form) prints the saved
    // session, which carries this run's specific summary.
    let transcript_out = repo.run(fake, &["transcript", run_id.as_str(), "readback"], &[]);
    assert!(transcript_out.status.success());
    let transcript = String::from_utf8_lossy(&transcript_out.stdout);
    assert!(
        transcript.contains("a memorable summary"),
        "transcript was {transcript:?}"
    );

    // `clean --keep 0` deterministically prunes every run.
    let clean_out = repo.run(fake, &["clean", "--keep", "0"], &[]);
    assert!(clean_out.status.success());
    assert!(store::list_runs(&repo.layout()).unwrap().is_empty());
}

/// Multiple runs in one data directory: the `latest` pointer advances, `runs`
/// lists newest-first, and `clean --keep 1` prunes exactly the older run.
#[test]
fn multiple_runs_track_latest_and_prune_oldest() {
    let Some(fake) = tooling() else { return };

    let repo = TestRepo::new(&registry(&[
        Reviewer::new("r", "codex", "gate").behavior("pass")
    ]));

    // First run, against the dirty working tree.
    let first = repo.review(fake);
    assert!(first.exited_zero(), "stderr:\n{}", first.stderr);
    let first_id = repo.latest_run_id();

    // Advance HEAD (so the run id changes) and introduce a genuinely new change
    // so the second run actually routes its reviewer rather than being a
    // zero-match pass. Sleep first so the second run's directory mtime is strictly
    // later than the first's, making the newest-first ordering unambiguous even on
    // coarse (1s-resolution) filesystems.
    repo.commit_all("advance");
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(repo.path().join("src/run2.rs"), "pub fn run2() {}\n").unwrap();
    let second = repo.review(fake);
    assert!(second.exited_zero(), "stderr:\n{}", second.stderr);
    assert_eq!(
        second.completed().1.total,
        1,
        "the second run should have routed its gate, not been a zero-match pass"
    );

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs.len(), 2, "two distinct runs should be recorded");
    let newest_id = runs[0].run.clone();
    assert_ne!(
        newest_id, first_id,
        "the newest run should not be the first"
    );

    // `show` with no id resolves to the latest (newest) run.
    let show = repo.run(fake, &["show", "--format", "jsonl"], &[]);
    let show_events = parse_events(
        &String::from_utf8_lossy(&show.stdout),
        &String::from_utf8_lossy(&show.stderr),
    );
    assert!(show_events.iter().any(|e| e.run_id() == &newest_id));

    // `clean --keep 1` removes exactly the older run.
    let clean = repo.run(fake, &["clean", "--keep", "1"], &[]);
    assert!(clean.status.success());
    let clean_stdout = String::from_utf8_lossy(&clean.stdout);
    assert!(
        clean_stdout.contains("removed 1 run(s)"),
        "clean said: {clean_stdout}"
    );
    assert!(
        clean_stdout.contains(first_id.as_str()),
        "clean should name the older run"
    );
    let remaining = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].run, newest_id);
}

/// A changeset that triggers no reviewer is an honest, persisted pass.
#[test]
fn a_changeset_that_triggers_no_reviewer_is_a_clean_pass() {
    let Some(fake) = tooling() else { return };

    // This reviewer only triggers on docs; the dirty tree is all under src/.
    let repo = TestRepo::new(
        "reviewers:\n  - name: docs-only\n    trigger: [docs/**]\n    mode: gate\n    backend: codex\n    prompt: docs review\n",
    );
    let run = repo.review(fake);

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let (decision, gates, _cost) = run.completed();
    assert_eq!(decision, Decision::Pass);
    assert_eq!(gates.total, 0);
    assert_eq!(run.resolved_count(), 0);

    let runs = store::list_runs(&repo.layout()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].reviewers, 0);
}
