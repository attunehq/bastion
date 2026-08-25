//! Reading, writing, and pruning persisted runs under the [`Layout`].
//!
//! The run is always persisted as JSONL regardless of how it was displayed, so a
//! run can be replayed or inspected after the fact. These functions are the
//! local equivalent of the GitHub run-summary page: they read back what a past
//! run recorded without re-running it.

use std::time::{Duration, SystemTime};

use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::{Deserialize, Serialize};

use crate::context::PriorFinding;
use crate::event::{RunEvent, RunId};
use crate::paths::Layout;
use crate::seal::Seal;
use crate::verdict::Decision;

/// A one-line description of a persisted run, for `bastion runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The run id.
    pub run: RunId,
    /// The branch under review, if recorded.
    pub branch: Option<String>,
    /// The base branch, if recorded.
    pub base: Option<String>,
    /// The aggregate decision, if the run completed.
    pub verdict: Option<Decision>,
    /// Number of reviewers in the run's recorded plan (the triggered set,
    /// or only the selected subset on a partial run).
    pub reviewers: u32,
    /// Whether the run was narrowed to a subset of the triggered reviewers
    /// (`bastion review --reviewer`), so its verdict speaks only for those.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// Create a run's directory (and any missing parents), naming it in the error.
/// Both writers land a file under the run dir, so each ensures it exists first.
fn ensure_run_dir(layout: &Layout, id: &RunId) -> Result<()> {
    let dir = layout.run_dir(id);
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("creating run directory {}", dir.display()))
}

/// Persist a run's full event stream and update the `latest` pointer.
///
/// # Errors
///
/// Returns an error if the data directory cannot be created or written.
pub fn write_run(layout: &Layout, id: &RunId, events: &[RunEvent]) -> Result<()> {
    ensure_run_dir(layout, id)?;

    let mut body = String::new();
    for event in events {
        body.push_str(&serde_json::to_string(event).wrap_err("serializing run event")?);
        body.push('\n');
    }
    let jsonl = layout.run_jsonl(id);
    std::fs::write(&jsonl, body).wrap_err_with(|| format!("writing {}", jsonl.display()))?;

    std::fs::write(layout.latest_pointer(), id.as_str()).wrap_err("updating latest run pointer")?;
    Ok(())
}

/// Read a run's full event stream.
///
/// # Errors
///
/// Returns an error if the run does not exist or its `run.jsonl` is malformed.
pub fn read_run(layout: &Layout, id: &RunId) -> Result<Vec<RunEvent>> {
    let jsonl = layout.run_jsonl(id);
    let text = std::fs::read_to_string(&jsonl)
        .wrap_err_with(|| format!("no such run '{id}' (expected {})", jsonl.display()))?;
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str(line)
            .wrap_err_with(|| format!("{}:{}: malformed run event", jsonl.display(), i + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Persist a run's seal.
///
/// Sealing is best-effort at the call site (see [`crate::runner::execute_with`]):
/// a run with no [`crate::seal::SealBindings`], or one whose git derivation
/// failed, simply never calls this, and the run persists unsealed. This
/// function itself always writes when called.
///
/// # Errors
///
/// Returns an error if the run directory cannot be created or the seal cannot
/// be written.
pub fn write_seal(layout: &Layout, id: &RunId, seal: &Seal) -> Result<()> {
    ensure_run_dir(layout, id)?;
    let path = layout.seal(id);
    let body = serde_json::to_string_pretty(seal).wrap_err("serializing seal")?;
    std::fs::write(&path, body).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Read a run's seal, if it has one.
///
/// Returns `Ok(None)` when the run predates sealing, or was left unsealed
/// because sealing failed at persist time: an absent seal means "this run
/// cannot be attested," not an error.
///
/// # Errors
///
/// Returns an error if the seal file exists but cannot be read or is malformed
/// JSON.
pub fn read_seal(layout: &Layout, id: &RunId) -> Result<Option<Seal>> {
    let path = layout.seal(id);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let seal = serde_json::from_str(&text)
                .wrap_err_with(|| format!("{}: malformed seal", path.display()))?;
            Ok(Some(seal))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err_with(|| format!("reading {}", path.display())),
    }
}

/// Resolve an optional run id to a concrete one, defaulting to the latest run.
///
/// # Errors
///
/// Returns an error if no id is given and there is no recorded latest run, or if
/// the named run does not exist.
pub fn resolve_run(layout: &Layout, id: Option<&str>) -> Result<RunId> {
    let run = match id {
        Some(explicit) => RunId(explicit.to_string()),
        None => {
            let pointer = layout.latest_pointer();
            let latest = std::fs::read_to_string(&pointer)
                .map_err(|_| eyre!("no runs recorded yet; run `bastion review` first"))?;
            RunId(latest.trim().to_string())
        }
    };
    if !layout.run_dir(&run).is_dir() {
        bail!("no such run '{run}'");
    }
    Ok(run)
}

/// List recorded runs, most recent first (by directory modification time).
///
/// # Errors
///
/// Returns an error if the runs directory cannot be read. A missing runs
/// directory is treated as an empty list.
pub fn list_runs(layout: &Layout) -> Result<Vec<RunSummary>> {
    Ok(collect_runs(layout)?
        .into_iter()
        .map(|(id, _)| summarize(layout, &id))
        .collect())
}

/// Prior runs recorded on `branch`, newest first (the same order
/// [`collect_runs`] already uses).
///
/// A review resolves this once and shares it: prior-findings recall
/// ([`findings_from_events`]) wants only the newest run, while carry planning
/// ([`crate::carry::plan`]) walks the list per reviewer so a later partial or
/// unsealed run cannot hide an earlier eligible pass for a reviewer it did not
/// resolve. Empty when the branch has no prior run (or the run history cannot
/// be read).
///
/// This does not exclude any run id. A review assembles its context and plans
/// carry *before* the runner persists the current run, so the current run is not
/// yet in the store. That includes a previous invocation at the same `HEAD` (a
/// local rerun on a dirty working tree reuses the same run id and overwrites it
/// only at the end, so consulting it first is correct).
#[must_use]
pub fn runs_on_branch(layout: &Layout, branch: &str) -> Vec<(RunSummary, Vec<RunEvent>)> {
    let Ok(runs) = collect_runs(layout) else {
        return Vec::new();
    };
    let mut matched = Vec::new();
    for (id, _) in runs {
        let events = read_run(layout, &id).unwrap_or_default();
        let summary = summarize_events(&id, &events);
        if summary.branch.as_deref() == Some(branch) {
            matched.push((summary, events));
        }
    }
    matched
}

/// The most recent persisted run recorded on `branch`, with its full event
/// stream and one-line [`RunSummary`], or `None` when the branch has no prior
/// run (or the run history cannot be read).
///
/// This is the first entry of [`runs_on_branch`]. Call that when a review needs
/// the whole newest-first list; call this when only the newest run is required
/// (prior-findings recall, tests).
#[must_use]
pub fn latest_run_on_branch(layout: &Layout, branch: &str) -> Option<(RunSummary, Vec<RunEvent>)> {
    runs_on_branch(layout, branch).into_iter().next()
}

/// Prune persisted runs, keeping the `keep` most recent and/or removing any
/// older than `older_than`. Returns the ids that were removed.
///
/// # Errors
///
/// Returns an error if a run directory cannot be removed.
pub fn prune(
    layout: &Layout,
    keep: Option<usize>,
    older_than: Option<Duration>,
) -> Result<Vec<RunId>> {
    let runs = collect_runs(layout)?;

    let now = SystemTime::now();
    let mut removed = Vec::new();
    for (index, (id, modified)) in runs.iter().enumerate() {
        let beyond_keep = keep.is_some_and(|k| index >= k);
        let too_old = older_than.is_some_and(|max_age| {
            now.duration_since(*modified)
                .map(|age| age > max_age)
                .unwrap_or(false)
        });
        if beyond_keep || too_old {
            let dir = layout.run_dir(id);
            std::fs::remove_dir_all(&dir)
                .wrap_err_with(|| format!("removing run {}", dir.display()))?;
            removed.push(id.clone());
        }
    }
    Ok(removed)
}

/// Recall the substantive findings a prior run's reviewers raised, one
/// [`PriorFinding`] per recorded finding keyed by its reviewer, so a re-review
/// can be reminded of what it already said.
///
/// The synthetic fail-closed crash finding (an empty path) is skipped: "the
/// reviewer failed to complete" is not a substantive prior finding to
/// re-evaluate. The caller passes the events of the branch's latest run (the
/// first entry of [`runs_on_branch`]); an absent prior run recalls nothing, so
/// recall never fails a review.
#[must_use]
pub fn findings_from_events(events: &[RunEvent]) -> Vec<PriorFinding> {
    let mut findings = Vec::new();
    for event in events {
        if let RunEvent::ReviewerResolved {
            reviewer,
            findings: resolved,
            ..
        } = event
        {
            for finding in resolved {
                if finding.path.is_empty() {
                    continue;
                }
                findings.push(PriorFinding::from_finding(reviewer, finding));
            }
        }
    }
    findings
}

/// Gather `(RunId, modified-time)` for every run directory, most recent first
/// (ties broken by descending id for a stable order). Both callers want this same
/// ordering, so the sort lives here once.
fn collect_runs(layout: &Layout) -> Result<Vec<(RunId, SystemTime)>> {
    let runs_dir = layout.runs_dir();
    let entries = match std::fs::read_dir(&runs_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).wrap_err_with(|| format!("reading {}", runs_dir.display())),
    };

    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry.wrap_err("reading runs directory entry")?;
        let meta = entry.metadata().wrap_err("reading run metadata")?;
        if !meta.is_dir() {
            continue; // skips the `latest` pointer file
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        runs.push((RunId(name), modified));
    }
    runs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    Ok(runs)
}

/// Build a [`RunSummary`] from a run's recorded events, reading them from disk.
///
/// A run whose `run.jsonl` is missing or malformed degrades to a summary with
/// only its id, rather than failing the whole listing.
fn summarize(layout: &Layout, id: &RunId) -> RunSummary {
    summarize_events(id, &read_run(layout, id).unwrap_or_default())
}

/// Fold a run's events into its one-line summary. Split from [`summarize`] so a
/// caller that already holds the events (see [`latest_run_on_branch`]) does not
/// re-read the file just to summarize what it already parsed.
fn summarize_events(id: &RunId, events: &[RunEvent]) -> RunSummary {
    let mut summary = RunSummary {
        run: id.clone(),
        branch: None,
        base: None,
        verdict: None,
        reviewers: 0,
        partial: false,
    };
    for event in events {
        match event {
            RunEvent::RunStarted {
                branch,
                base,
                reviewers,
                partial,
                ..
            } => {
                summary.branch = Some(branch.clone());
                summary.base = Some(base.clone());
                summary.reviewers = u32::try_from(reviewers.len()).unwrap_or(u32::MAX);
                summary.partial = *partial;
            }
            RunEvent::RunCompleted { verdict, .. } => summary.verdict = Some(*verdict),
            _ => {}
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Gates, ReviewerRef};
    use crate::reviewer::Mode;
    use crate::verdict::Money;

    fn sample_events(id: &str) -> Vec<RunEvent> {
        vec![
            RunEvent::RunStarted {
                partial: false,
                run: RunId(id.into()),
                branch: "feat/x".into(),
                base: "main".into(),
                changed: 3,
                reviewers: vec![ReviewerRef {
                    name: "r1".into(),
                    mode: Mode::Gate,
                }],
            },
            RunEvent::RunCompleted {
                partial: false,
                run: RunId(id.into()),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 1,
                    passed: 1,
                    blocked: 0,
                    skipped: 0,
                },
                duration_ms: 100,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(5),
            },
        ]
    }

    fn sample_seal() -> Seal {
        crate::seal::seal(
            b"test-secret",
            "0.1.0",
            &crate::seal::SealBindings {
                head_tree: "head".into(),
                base_tree: "base".into(),
                patch_id: "patch".into(),
                config_hash: "hash".into(),
                repo_reviewers: ["r1".to_string()].into_iter().collect(),
            },
            false,
            false,
            vec!["r1".into()],
            &[],
        )
    }

    #[test]
    fn write_seal_then_read_seal_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        let id = RunId("r-sealed".into());

        assert_eq!(read_seal(&layout, &id).unwrap(), None);

        let seal = sample_seal();
        write_seal(&layout, &id, &seal).unwrap();
        assert_eq!(read_seal(&layout, &id).unwrap(), Some(seal));
    }

    #[test]
    fn read_seal_is_none_for_a_run_that_was_never_sealed() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        let id = RunId("r-unsealed".into());
        write_run(&layout, &id, &sample_events("r-unsealed")).unwrap();
        assert_eq!(read_seal(&layout, &id).unwrap(), None);
    }

    #[test]
    fn writes_reads_and_summarizes_a_run() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        let id = RunId("r-0001".into());

        write_run(&layout, &id, &sample_events("r-0001")).unwrap();

        let events = read_run(&layout, &id).unwrap();
        assert_eq!(events.len(), 2);

        let resolved = resolve_run(&layout, None).unwrap();
        assert_eq!(resolved, id);

        let summaries = list_runs(&layout).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(summaries[0].verdict, Some(Decision::Pass));
        assert_eq!(summaries[0].reviewers, 1);
    }

    #[test]
    fn a_partial_run_summarizes_as_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        let id = RunId("r-part".into());
        let mut events = sample_events("r-part");
        if let RunEvent::RunStarted { partial, .. } = &mut events[0] {
            *partial = true;
        }
        write_run(&layout, &id, &events).unwrap();
        let summaries = list_runs(&layout).unwrap();
        assert!(summaries[0].partial);
    }

    #[test]
    fn prune_keeps_the_most_recent_n() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        for id in ["r-0001", "r-0002", "r-0003"] {
            write_run(&layout, &RunId(id.into()), &sample_events(id)).unwrap();
        }
        let removed = prune(&layout, Some(2), None).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(list_runs(&layout).unwrap().len(), 2);
    }

    #[test]
    fn prune_older_than_zero_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        write_run(&layout, &RunId("r-0001".into()), &sample_events("r-0001")).unwrap();
        let removed = prune(&layout, None, Some(Duration::from_secs(0))).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(list_runs(&layout).unwrap().is_empty());
    }

    #[test]
    fn resolve_run_errors_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        assert!(resolve_run(&layout, None).is_err());
    }

    /// A run on `branch` that resolved `reviewer` with the given findings.
    fn run_with_findings(
        id: &str,
        branch: &str,
        reviewer: &str,
        findings: Vec<crate::verdict::Finding>,
    ) -> Vec<RunEvent> {
        vec![
            RunEvent::RunStarted {
                partial: false,
                run: RunId(id.into()),
                branch: branch.into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![ReviewerRef {
                    name: reviewer.into(),
                    mode: Mode::Gate,
                }],
            },
            RunEvent::ReviewerResolved {
                carried: false,
                scope_digest: None,
                trigger: None,
                run: RunId(id.into()),
                reviewer: reviewer.into(),
                verdict: Decision::Block,
                summary: "s".into(),
                findings,
                usage: None,
                duration_ms: 1,
                has_transcript: false,
                replayed: false,
            },
            RunEvent::RunCompleted {
                partial: false,
                run: RunId(id.into()),
                verdict: Decision::Block,
                gates: Gates {
                    total: 1,
                    passed: 0,
                    blocked: 1,
                    skipped: 0,
                },
                duration_ms: 1,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ]
    }

    /// Resolve `branch`'s latest run and extract its recalled findings, exactly
    /// as `review` composes [`latest_run_on_branch`] and [`findings_from_events`]
    /// to fill the review context. Absent prior run recalls nothing.
    fn recalled_findings(layout: &Layout, branch: &str) -> Vec<PriorFinding> {
        latest_run_on_branch(layout, branch)
            .map(|(_, events)| findings_from_events(&events))
            .unwrap_or_default()
    }

    #[test]
    fn prior_findings_recalls_the_latest_run_on_the_branch_and_skips_synthetic() {
        use crate::verdict::{Finding, FindingKind};
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());

        let real = Finding {
            kind: FindingKind::Blocking,
            path: "src/p.rs".into(),
            line_start: 10,
            line_end: 12,
            detail: "O(n^2) append".into(),
        };
        // The synthetic fail-closed crash finding (empty path) must not be recalled.
        let synthetic = Finding {
            kind: FindingKind::Blocking,
            path: String::new(),
            line_start: 0,
            line_end: 0,
            detail: "reviewer failed to complete".into(),
        };
        write_run(
            &layout,
            &RunId("r-old".into()),
            &run_with_findings("r-old", "feat/x", "perf", vec![real, synthetic]),
        )
        .unwrap();

        // A run on a *different* branch must not be recalled for `feat/x`.
        write_run(
            &layout,
            &RunId("r-other".into()),
            &run_with_findings(
                "r-other",
                "feat/y",
                "perf",
                vec![Finding {
                    kind: FindingKind::Blocking,
                    path: "src/q.rs".into(),
                    line_start: 1,
                    line_end: 1,
                    detail: "unrelated".into(),
                }],
            ),
        )
        .unwrap();

        let recalled = recalled_findings(&layout, "feat/x");
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].reviewer, "perf");
        assert_eq!(recalled[0].detail, "O(n^2) append");
        assert_eq!(recalled[0].path, "src/p.rs");

        // The first review of a branch (no prior run) recalls nothing.
        assert!(recalled_findings(&layout, "brand-new").is_empty());
    }

    #[test]
    fn prior_findings_recalls_the_newest_of_several_runs_on_the_branch() {
        // Several runs on the same branch: recall must return the newest one's findings,
        // not stale findings from an earlier run. `list_runs` orders by modified time and
        // breaks ties by descending run id, so the later-written, larger-id run wins
        // either way.
        use crate::verdict::{Finding, FindingKind};
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());

        let finding = |detail: &str| Finding {
            kind: FindingKind::Blocking,
            path: "src/p.rs".into(),
            line_start: 1,
            line_end: 1,
            detail: detail.into(),
        };
        write_run(
            &layout,
            &RunId("r-1".into()),
            &run_with_findings("r-1", "feat/x", "perf", vec![finding("old finding")]),
        )
        .unwrap();
        write_run(
            &layout,
            &RunId("r-2".into()),
            &run_with_findings("r-2", "feat/x", "perf", vec![finding("new finding")]),
        )
        .unwrap();

        let recalled = recalled_findings(&layout, "feat/x");
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].detail, "new finding");
    }

    #[test]
    fn prior_findings_recalls_a_same_id_run_from_an_earlier_invocation() {
        // A local rerun on a dirty working tree reuses the same run id (keyed by HEAD).
        // Recall happens before the current run is persisted, so the previous
        // invocation's run (same id) is still on disk and must be recalled, not skipped:
        // this is what lets the local edit-and-rerun loop see its own prior findings.
        use crate::verdict::{Finding, FindingKind};
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());

        write_run(
            &layout,
            &RunId("r-samehead".into()),
            &run_with_findings(
                "r-samehead",
                "feat/x",
                "perf",
                vec![Finding {
                    kind: FindingKind::Blocking,
                    path: "src/p.rs".into(),
                    line_start: 1,
                    line_end: 1,
                    detail: "still slow".into(),
                }],
            ),
        )
        .unwrap();

        // The about-to-be-written run shares the id, yet recall (which runs first) finds
        // the earlier invocation's findings.
        let recalled = recalled_findings(&layout, "feat/x");
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].detail, "still slow");
    }

    #[test]
    fn latest_run_on_branch_pairs_the_newest_matching_runs_summary_with_its_events() {
        use crate::verdict::Finding;
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());

        let finding = |detail: &str| Finding {
            kind: crate::verdict::FindingKind::Blocking,
            path: "src/p.rs".into(),
            line_start: 1,
            line_end: 1,
            detail: detail.into(),
        };
        // Two runs on the branch (newest wins) plus one on another branch (ignored).
        write_run(
            &layout,
            &RunId("r-1".into()),
            &run_with_findings("r-1", "feat/x", "perf", vec![finding("old")]),
        )
        .unwrap();
        write_run(
            &layout,
            &RunId("r-2".into()),
            &run_with_findings("r-2", "feat/x", "perf", vec![finding("new")]),
        )
        .unwrap();
        write_run(
            &layout,
            &RunId("r-other".into()),
            &run_with_findings("r-other", "feat/y", "perf", vec![finding("elsewhere")]),
        )
        .unwrap();

        // The summary and the events are the *same* run, resolved once: the newest
        // run recorded on the branch, most-recent-first tie broken by descending id.
        let (summary, events) = latest_run_on_branch(&layout, "feat/x").expect("a prior run");
        assert_eq!(summary.run, RunId("r-2".into()));
        assert_eq!(summary.branch.as_deref(), Some("feat/x"));
        assert_eq!(
            findings_from_events(&events),
            recalled_findings(&layout, "feat/x")
        );
        assert_eq!(findings_from_events(&events)[0].detail, "new");

        // A branch with no run resolves to nothing.
        assert!(latest_run_on_branch(&layout, "brand-new").is_none());
    }

    #[test]
    fn runs_on_branch_lists_matching_runs_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        write_run(&layout, &RunId("r-1".into()), &sample_events("r-1")).unwrap();
        write_run(&layout, &RunId("r-2".into()), &sample_events("r-2")).unwrap();
        write_run(
            &layout,
            &RunId("r-other".into()),
            &run_with_findings(
                "r-other",
                "feat/y",
                "perf",
                vec![crate::verdict::Finding {
                    kind: crate::verdict::FindingKind::Blocking,
                    path: "src/p.rs".into(),
                    line_start: 1,
                    line_end: 1,
                    detail: "elsewhere".into(),
                }],
            ),
        )
        .unwrap();

        let runs = runs_on_branch(&layout, "feat/x");
        let ids: Vec<&str> = runs.iter().map(|(s, _)| s.run.as_str()).collect();
        assert_eq!(ids, ["r-2", "r-1"], "newest first, other branches omitted");
        assert_eq!(
            latest_run_on_branch(&layout, "feat/x")
                .expect("a prior run")
                .0
                .run
                .as_str(),
            "r-2"
        );
    }
}
