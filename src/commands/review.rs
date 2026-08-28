//! The `bastion review` gate: routing, attestation replay, carry, execution.

use super::skills::warn_on_stale_skills;
use crate::config::Config;
use crate::context::PriorFinding;
use crate::context::ReviewContext;
use crate::event::ReviewerRef;
use crate::event::RunEvent;
use crate::event::RunId;
use crate::git;
use crate::paths::Layout;
use crate::render;
use crate::render::Format;
use crate::routing::Router;
use crate::runner;
use crate::runner::ExecContext;
use crate::seal::SealBindings;
use crate::store;
use crate::verdict::Decision;
use crate::verdict::Money;
use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use color_eyre::eyre::bail;
use color_eyre::eyre::eyre;
use std::io;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;

/// What a `bastion review` invocation asked for, beyond where to run it.
///
/// Parsed at the CLI boundary and handed to [`review`] whole, so the handler's
/// signature does not grow a parameter per flag.
#[derive(Debug, Default)]
pub struct ReviewOptions {
    /// Base branch the changeset is computed against.
    pub base: String,
    /// Output format.
    pub format: Format,
    /// The pull request the review runs against, when any (`--repo`/`--pr`).
    pub github: Option<GithubSource>,
    /// Restrict the run to these triggered reviewers (`--reviewer`, repeatable).
    /// Empty means the full triggered set. A non-empty selection that excludes
    /// a triggered reviewer makes the run *partial*: it persists and renders as
    /// partial and is never sealed, so a filtered green cannot be presented (or
    /// attested) as a full green.
    pub only: Vec<String>,
    /// Disable carrying prior passes forward (`--fresh`): every triggered
    /// reviewer executes even when its trigger-scoped diff is unchanged since
    /// its newest resolution on the branch ([`crate::carry`]).
    pub fresh: bool,
    /// Extra registry files merged into the repository layer (`--include`,
    /// repeatable), as if the repository registry's `include:` array listed
    /// them. Part of the effective repository config, so they move the config
    /// hash that carry and attestation bind.
    pub includes: Vec<std::path::PathBuf>,
    /// Merge the user-level registry when a repository registry exists. Without
    /// this opt-in, personal reviewers are a fallback for repositories without a
    /// registry.
    pub should_merge_user_reviewers: bool,
}

/// `bastion review`: route and run the triggered reviewers, gating the result.
///
/// Computes the changed files against `options.base`, selects matching
/// reviewers, emits a `run.started` plan, and hands off to the runner to execute
/// them concurrently. With zero matches the run is a trivial pass (mirroring the
/// always-present `bastion` check in CI). Returns the aggregate [`Decision`] so
/// the caller can map `block` to a non-zero exit status.
///
/// A run also plans *carry* ([`crate::carry`]): a reviewer whose prior run on this
/// branch passed, and whose trigger-scoped diff digest is unchanged, folds that
/// pass forward instead of executing, so a fix-and-re-review loop re-runs only the
/// reviewers whose scoped content actually changed. This holds on both surfaces. A
/// CI run carries from its own branch's previous CI run the same way a local run
/// does: the run seal (verified under this binary's embedded secret) plus the
/// content-binding digest already prove the carried verdict is a real review of
/// exactly this content, which is all carry's soundness needs. Carry and
/// attestation replay are complementary rather than alternatives: replay reuses the
/// *author's* signed local run (crossing from their machine into CI), carry reuses
/// CI's *own* prior run. `--fresh` disables carry; an explicit `--reviewer`
/// selection also executes its reviewers fresh (asking for a reviewer by name means
/// asking for it to run).
///
/// The runner owns event emission for the per-reviewer and completion events and
/// persists the full run; this handler renders the `run.started` event and the
/// events the runner streams back.
///
/// `cwd` is the directory to resolve the repository and config from: the process
/// working directory in normal use, but explicit so the handler is testable.
///
/// `user_dir` is the user-level config directory (`None` to skip it). Personal
/// reviewers are used as a fallback when the repository has no registry, or are
/// merged when `options.should_merge_user_reviewers` is set. An identical reviewer
/// in both files is deduplicated and a same-name collision keeps both with the repo
/// side scoped (see [`Config::discover_merged`]).
///
/// `options.github` carries the `owner/name` slug and PR number when the review runs
/// against a pull request, so the reviewers get its description and discussion as
/// context. It is best effort: a failure to reach GitHub is logged and the review
/// proceeds on the local context (commit messages and prior findings) alone.
///
/// # Errors
///
/// Returns an error if the repository, config, git queries, or persistence fail,
/// or if `options.only` names a reviewer that is not in the triggered set.
/// A blocked review is *not* an error: it returns `Ok(Decision::Block)`.
pub async fn review(
    layout: &Layout,
    cwd: &Path,
    options: ReviewOptions,
    user_dir: Option<&Path>,
) -> Result<Decision> {
    let ReviewOptions {
        base,
        format,
        github,
        only,
        fresh,
        includes,
        should_merge_user_reviewers,
    } = options;
    let base = base.as_str();
    let repo_root = git::repo_root(cwd)?;
    warn_on_stale_skills(&repo_root);
    let branch = git::current_branch(&repo_root)?;
    let (_sources, repo_attestation, config) = Config::discover_merged_attested(
        &repo_root,
        user_dir,
        &includes,
        should_merge_user_reviewers,
    )?;
    // The changeset is defined against the merge base with `base`, never the
    // base branch's tip: a tip that has moved on since this branch forked
    // would put the base's own changes under review and route reviewers at
    // them. A base with no common ancestor cannot define a changeset at all,
    // so that fails the review rather than guessing.
    let merge_base = git::merge_base(&repo_root, base)
        .wrap_err_with(|| format!("resolving the merge base of HEAD and '{base}'"))?;
    if let Some(warning) = stale_base_warning(&repo_root, base, &merge_base) {
        // Fallible on purpose: a closed stderr must not panic the gate out of
        // an otherwise valid review.
        let _ = writeln!(io::stderr(), "{warning}");
    }
    let changed = git::changed_files(&repo_root, &merge_base)?;

    let router = Router::compile(&config.reviewers)?;
    let triggered = router.matched(&changed);
    let force = !only.is_empty();
    let matched = select_reviewers(&triggered, &config.reviewers, &only)?;
    // Partial means coverage was actually reduced: selecting every triggered
    // reviewer by name is still a full run.
    let partial = matched.len() < triggered.len();
    let run = local_run_id(&repo_root, partial);
    let reviewer_refs: Vec<ReviewerRef> = matched
        .iter()
        .map(|r| ReviewerRef {
            name: r.name.clone(),
            mode: r.mode,
        })
        .collect();
    let changed_count = u32::try_from(changed.len()).unwrap_or(u32::MAX);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let started = RunEvent::RunStarted {
        partial,
        run: run.clone(),
        branch: branch.clone(),
        base: base.to_string(),
        changed: changed_count,
        reviewers: reviewer_refs.clone(),
    };
    render::write_event(&mut out, format, &started)?;

    if matched.is_empty() {
        // No reviewer triggered: a trivial, honest pass. Persist it so the run is
        // inspectable afterward, exactly like an executed run.
        let completed = RunEvent::RunCompleted {
            partial: false,
            run: run.clone(),
            verdict: Decision::Pass,
            gates: crate::event::Gates {
                total: 0,
                passed: 0,
                blocked: 0,
                skipped: 0,
            },
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cost_usd: Money::from_cents(0),
        };
        render::write_event(&mut out, format, &completed)?;
        store::write_run(layout, &run, &[started, completed])?;
        return Ok(Decision::Pass);
    }

    // Resolve the branch's prior runs once (newest first) and share them: the
    // prior-findings recall here wants only the newest run, while carry planning
    // further down walks the list per reviewer. Each would otherwise rescan and
    // reparse the whole run history independently ([`store::runs_on_branch`]).
    let prior_runs = store::runs_on_branch(layout, &branch);
    let prior_findings = prior_runs
        .first()
        .map(|(_, events)| store::findings_from_events(events))
        .unwrap_or_default();

    let (context, gathered_github) =
        assemble_context(&repo_root, base, github.as_ref(), prior_findings).await;

    let seal = derive_seal_bindings(&repo_root, base, &repo_attestation);
    // A dirty working tree is a first-class seal input, not merely a caveat: a
    // green review over uncommitted content must not be attestable as a verdict
    // on the committed tree the rest of the seal binds. Sealing still proceeds
    // (the seal is the honest record); it is `bastion attest` that refuses.
    let dirty = git::is_dirty(&repo_root).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "could not determine whether the working tree is dirty; treating the run as dirty out of caution");
        true
    });

    // Attestation verify-and-replay is the CI surface only: a purely local
    // review (no GithubSource) never attempts it. When the repository has
    // opted in, look up the note, verify it against the author's registered
    // signing keys, and replay whatever checks out; every failure is a
    // fallback to full fresh execution, never a silent skip. A dirty checkout
    // never replays: `changed_files` includes uncommitted and untracked files,
    // so this run's reviewers see content no attestation's committed bindings
    // name, and a replayed verdict would vouch for a changeset it never saw.
    let attested = match (&github, repo_attestation.attestations_enabled) {
        (Some(_), true) if dirty => Some(crate::attest::AttestationOutcome::Fallback {
            reason: "the CI working tree has uncommitted or untracked files, which the \
                     reviewers see but no attestation binds; executing every reviewer fresh"
                .to_string(),
        }),
        (Some(_), true) => {
            plan_attestation_replay(
                &repo_root,
                base,
                &repo_attestation,
                gathered_github.as_ref(),
                &matched,
            )
            .await
        }
        _ => None,
    };

    let AttestationResolution {
        replayed,
        attestation,
        fallback: attestation_fallback,
        fresh: matched,
    } = resolve_attestation(attested, matched, &run);
    // Render the fallback (an offered attestation that was refused) to the live
    // stream before any reviewer resolves, so the plan reads the reason up front.
    // It is also carried into `ExecContext` so persistence keeps it.
    if let Some(fallback) = &attestation_fallback {
        render::write_event(&mut out, format, fallback)?;
    }

    // Trigger-scoped digests for everything about to run, stamped onto each
    // resolved event so the *next* run can decide whether to carry it. The runner
    // re-derives every stamped digest after the reviewers finish (through the
    // probe), so a tree that changed mid-run cannot leave a stale digest behind
    // for a later run to carry from.
    let scope_digests = plan_scope_digests(&repo_root, &merge_base, &matched, &changed);
    let digest_probe = Some(runner::DigestProbe {
        merge_base: merge_base.clone(),
    });

    // Carry prior passes forward ([`crate::carry`]): both locally and in CI, a
    // reviewer whose prior run on this branch passed and whose trigger-scoped
    // digest is unchanged folds that pass forward instead of executing. A
    // repository reviewer carries only from a prior run whose seal verifies (under
    // this binary's embedded secret) and records no test seam, so a restored CI run
    // store cannot smuggle in a fabricated pass: the seal proves the prior run was a
    // real review by this release, and the digest binds the content that verdict
    // judged. A reviewer already replayed from an attestation above is not a carry
    // candidate (it is no longer in `matched`). Carry runs only for the full
    // triggered set (an explicit `--reviewer` selection asks for those reviewers to
    // run) and only unless `--fresh` opted out.
    let carried = plan_carry(
        layout,
        &prior_runs,
        &matched,
        &scope_digests,
        &repo_attestation.reviewers,
        fresh,
        &only,
    );
    let matched: Vec<&crate::reviewer::Reviewer> = matched
        .into_iter()
        .filter(|r| !carried.contains_key(&r.name))
        .collect();
    if !carried.is_empty() {
        let names: Vec<&str> = carried.keys().map(String::as_str).collect();
        // Fallible on purpose: a closed stderr must not panic the gate out of
        // an otherwise valid review.
        let _ = writeln!(
            io::stderr(),
            "bastion review: {} reviewer(s) carried forward from this branch's previous run \
             (trigger-scoped diff unchanged): {}; pass --fresh to re-run everything",
            carried.len(),
            names.join(", "),
        );
    }

    let ctx = ExecContext {
        run,
        repo_root,
        branch,
        base: base.to_string(),
        merge_base,
        changed: changed_count,
        reviewers: reviewer_refs,
        context,
        seal,
        dirty,
        replayed,
        attestation,
        partial,
        force,
        carried,
        scope_digests,
        digest_probe,
        attestation_fallback,
        limits: config.limits,
        akari: crate::akari::resolve(user_dir),
    };

    // The runner streams the per-reviewer and completion events; render each as it
    // lands. Rendering failures must not be swallowed, so capture the first.
    let mut render_err: Option<io::Error> = None;
    let aggregate = {
        let out = &mut out;
        runner::execute(&matched, &ctx, layout, &mut |event| {
            if render_err.is_none()
                && let Err(err) = render::write_event(out, format, event)
            {
                render_err = Some(err);
            }
        })
        .await?
    };
    if let Some(err) = render_err {
        return Err(err).wrap_err("rendering run events");
    }

    Ok(aggregate)
}

/// The stderr warning for a `--base` that lags its remote-tracking ref, or
/// `None` when the check does not apply or the base is in sync.
///
/// A local `main` that lags `origin/main` resolves a merge base older than the
/// branch's true fork point, so the changeset silently includes the upstream
/// commits between the stale tip and the fork point, and the reviewers review
/// (and can flag) work that is not this branch's own. That is the same
/// misreview class the merge-base contract stops on the base-tip side, entered
/// from the stale-local side instead. The check is cheap: when
/// merge-base(HEAD, base) and merge-base(HEAD, upstream-of-base) disagree, the
/// two refs put different changesets under review, and the upstream one is the
/// fork point CI will re-derive.
///
/// Advisory only: the review proceeds unchanged, nothing fetches, and there is
/// no override flag. An explicit commit, tag, or remote ref as the base, a
/// branch with no upstream, or any git failure here skips the check silently.
fn stale_base_warning(repo_root: &Path, base: &str, merge_base: &str) -> Option<String> {
    let upstream = git::upstream_of_local_branch(repo_root, base)?;
    let upstream_merge_base = git::merge_base(repo_root, &upstream).ok()?;
    if upstream_merge_base == merge_base {
        return None;
    }
    Some(format!(
        "bastion review: '{base}' and its upstream '{upstream}' give HEAD different merge \
         bases, so this changeset may include upstream commits that are not this branch's \
         work; review against '--base {upstream}', or fetch and bring '{base}' up to date \
         first"
    ))
}

/// Narrow the triggered set to an explicit `--reviewer` selection.
///
/// With no selection the full triggered set passes through untouched.
/// Otherwise every requested name must be a *triggered* reviewer: a name that
/// is not in the registry at all, or one whose trigger did not match this
/// changeset, is a usage error, not a silent no-op, because a re-run loop that
/// typos a name must not quietly run nothing and report green. The result
/// keeps registry order and deduplicates repeated names.
///
/// # Errors
///
/// Returns an error naming every unknown and every untriggered requested name,
/// plus the names that *did* trigger, so the fix is one glance away.
fn select_reviewers<'a>(
    triggered: &[&'a crate::reviewer::Reviewer],
    all: &[crate::reviewer::Reviewer],
    only: &[String],
) -> Result<Vec<&'a crate::reviewer::Reviewer>> {
    if only.is_empty() {
        return Ok(triggered.to_vec());
    }

    let requested: std::collections::BTreeSet<&str> = only.iter().map(String::as_str).collect();
    let mut unknown = Vec::new();
    let mut untriggered = Vec::new();
    for name in &requested {
        if triggered.iter().any(|r| r.name == *name) {
            continue;
        }
        if all.iter().any(|r| r.name == *name) {
            untriggered.push(*name);
        } else {
            unknown.push(*name);
        }
    }
    if !unknown.is_empty() || !untriggered.is_empty() {
        let mut problems = Vec::new();
        if !unknown.is_empty() {
            problems.push(format!(
                "not in the reviewer registry: {}",
                unknown.join(", ")
            ));
        }
        if !untriggered.is_empty() {
            problems.push(format!(
                "in the registry but not triggered by this changeset: {}",
                untriggered.join(", ")
            ));
        }
        let triggered_names: Vec<&str> = triggered.iter().map(|r| r.name.as_str()).collect();
        bail!(
            "--reviewer named reviewers that cannot run ({}); triggered reviewers: {}",
            problems.join("; "),
            if triggered_names.is_empty() {
                "none".to_string()
            } else {
                triggered_names.join(", ")
            },
        );
    }

    Ok(triggered
        .iter()
        .copied()
        .filter(|r| requested.contains(r.name.as_str()))
        .collect())
}

/// Assemble the review context every reviewer sees beyond the diff: the author's
/// stated intent (the PR body when reviewing one, otherwise this branch's commit
/// messages), this branch's `prior_findings` (already recalled from the run store
/// by the caller), and the surrounding discussion (GitHub only).
///
/// GitHub gathering is best effort: a failure to reach it is logged and the
/// review proceeds on the local context alone. The returned
/// [`GatheredContext`](crate::github::context::GatheredContext), when present, is
/// also what attestation replay reads the PR author and head SHA from. Empty
/// context leaves every reviewer's prompt exactly as it was.
async fn assemble_context(
    repo_root: &Path,
    base: &str,
    github: Option<&GithubSource>,
    prior_findings: Vec<PriorFinding>,
) -> (
    ReviewContext,
    Option<crate::github::context::GatheredContext>,
) {
    let mut context = ReviewContext {
        intent: git::commit_messages(repo_root, base),
        comments: Vec::new(),
        prior_findings,
    };
    let Some(source) = github else {
        return (context, None);
    };
    match gather_github_context(source).await {
        Ok(gathered) => {
            // A PR body is a better statement of intent than the commit messages,
            // so it wins when present; the discussion is GitHub-only.
            if gathered.intent.is_some() {
                context.intent = gathered.intent.clone();
            }
            context.comments = gathered.comments.clone();
            (context, Some(gathered))
        }
        Err(err) => {
            eprintln!("bastion review: continuing without GitHub context ({err:#})");
            (context, None)
        }
    }
}

/// What resolving a run's attestation plan settled: which reviewers replay, the
/// audit trail, an optional fallback event, and the reviewers still to execute.
struct AttestationResolution<'a> {
    /// Reviewers replaying a verified attestation instead of executing, keyed by
    /// name. Empty for any local review and for any CI run without a verified
    /// bundle.
    replayed: std::collections::BTreeMap<String, runner::ReplayedReviewer>,
    /// The attestation audit trail, present only when something replayed.
    attestation: Option<runner::AttestationAudit>,
    /// The `run.attestation-fallback` event to render and persist, present only
    /// when an offered attestation was refused. A genuinely absent note is not a
    /// refusal and produces no event.
    fallback: Option<RunEvent>,
    /// The reviewers that still execute fresh this run: everything not replayed.
    fresh: Vec<&'a crate::reviewer::Reviewer>,
}

/// Resolve a computed [`AttestationOutcome`](crate::attest::AttestationOutcome)
/// into the run's replay set, audit trail, optional fallback event, and the
/// reviewers still to execute fresh.
///
/// Renders no `run.jsonl` event itself: the caller renders the returned
/// `fallback` so the ordering (before any reviewer resolves) stays visible at the
/// call site. The stderr callouts it does write are advisory and deliberately
/// fallible, since a closed stderr must never panic the gate out of an otherwise
/// valid review. A genuinely absent note (`NotAttested`) or an attestation never
/// attempted (`None`) runs every reviewer fresh with no event and no stderr line,
/// so an un-attested PR is never nagged.
fn resolve_attestation<'a>(
    attested: Option<crate::attest::AttestationOutcome>,
    matched: Vec<&'a crate::reviewer::Reviewer>,
    run: &RunId,
) -> AttestationResolution<'a> {
    let plan = match attested {
        Some(crate::attest::AttestationOutcome::Replay(plan)) => plan,
        Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
            let _ = writeln!(
                io::stderr(),
                "bastion review: attestation not honored: {reason}"
            );
            return AttestationResolution {
                replayed: std::collections::BTreeMap::new(),
                attestation: None,
                fallback: Some(RunEvent::AttestationFallback {
                    run: run.clone(),
                    reason,
                }),
                fresh: matched,
            };
        }
        Some(crate::attest::AttestationOutcome::NotAttested) | None => {
            return AttestationResolution {
                replayed: std::collections::BTreeMap::new(),
                attestation: None,
                fallback: None,
                fresh: matched,
            };
        }
    };

    let bundle_public_key = plan.bundle.public_key.clone();
    let bundle_attested_at = plan.bundle.attested_at.clone();
    let mut replayed = std::collections::BTreeMap::new();
    for reviewer in &matched {
        if let Some(event) = plan.replay.get(&reviewer.name) {
            replayed.insert(
                reviewer.name.clone(),
                runner::ReplayedReviewer {
                    reviewer: (*reviewer).clone(),
                    event: event.clone(),
                },
            );
        }
    }
    let fresh: Vec<&crate::reviewer::Reviewer> = matched
        .iter()
        .copied()
        .filter(|r| !replayed.contains_key(&r.name))
        .collect();
    if replayed.is_empty() {
        return AttestationResolution {
            replayed,
            attestation: None,
            fallback: None,
            fresh,
        };
    }

    let callout = format!(
        "bastion review: {} reviewer(s) replayed from a signed local attestation (key {}, attested {}): {}",
        replayed.len(),
        bundle_public_key,
        bundle_attested_at,
        replayed.keys().cloned().collect::<Vec<_>>().join(", "),
    );
    // Fallible on purpose: a closed stderr must not panic the gate.
    let _ = writeln!(io::stderr(), "{callout}");
    AttestationResolution {
        replayed,
        attestation: Some(runner::AttestationAudit {
            public_key: bundle_public_key,
            attested_at: bundle_attested_at,
        }),
        fallback: None,
        fresh,
    }
}

/// Compute the trigger-scoped digest for each reviewer about to run.
///
/// Best effort: a reviewer whose digest fails to compute is left out of the map
/// (it executes fresh and cannot be carried from). Never fails the review.
fn plan_scope_digests(
    repo_root: &Path,
    merge_base: &str,
    matched: &[&crate::reviewer::Reviewer],
    changed: &[String],
) -> std::collections::BTreeMap<String, String> {
    let mut scope_digests = std::collections::BTreeMap::new();
    for reviewer in matched {
        match crate::carry::scope_digest(repo_root, merge_base, reviewer, changed) {
            Ok(digest) => {
                scope_digests.insert(reviewer.name.clone(), digest);
            }
            Err(err) => tracing::warn!(
                reviewer = %reviewer.name,
                error = %err,
                "could not compute a scope digest; the reviewer executes fresh and cannot be carried from"
            ),
        }
    }
    scope_digests
}

/// Plan which prior passes carry forward this run ([`crate::carry`]).
///
/// `prior_runs` is the branch's prior runs newest first, resolved once by the
/// caller ([`store::runs_on_branch`]) and shared with the prior-findings recall
/// so the history is scanned only once per review. An empty slice is a branch
/// with no prior run.
///
/// Carry runs only for the full triggered set and only when the author did not
/// opt out: `--fresh` disables it, and an explicit `--reviewer` selection asks
/// for those reviewers to run, so a non-empty `only` disables it too. A reviewer
/// with no computed scope digest is not a carry candidate.
fn plan_carry(
    layout: &Layout,
    prior_runs: &[(store::RunSummary, Vec<RunEvent>)],
    matched: &[&crate::reviewer::Reviewer],
    scope_digests: &std::collections::BTreeMap<String, String>,
    repo_reviewers: &std::collections::BTreeSet<String>,
    fresh: bool,
    only: &[String],
) -> std::collections::BTreeMap<String, crate::carry::Carried> {
    if fresh || !only.is_empty() {
        return Default::default();
    }
    let candidates: Vec<(&crate::reviewer::Reviewer, String)> = matched
        .iter()
        .filter_map(|r| {
            scope_digests
                .get(&r.name)
                .map(|digest| (*r, digest.clone()))
        })
        .collect();
    let prior: Vec<(&RunId, &[RunEvent])> = prior_runs
        .iter()
        .map(|(summary, events)| (&summary.run, events.as_slice()))
        .collect();
    crate::carry::plan(
        layout,
        &prior,
        &candidates,
        repo_reviewers,
        crate::seal::embedded_secret(),
    )
}

/// Attempt to verify and plan an attestation replay for a CI run.
///
/// Best effort in the same sense [`gather_github_context`] is: any failure to
/// gather the inputs a verification needs (the author's login, their
/// registered signing keys, or a note to verify) degrades to `None`, which the
/// caller treats as "no attestation available" and falls through to the
/// planner's own fallback reporting only when a note *was* found but failed to
/// verify. A genuinely absent note (the ordinary case for most commits) is
/// reported as a fallback too, since attestations are enabled for this repo
/// and the author is simply expected to know why none applied.
///
/// `routed` is the reviewers CI's own diff matched, the same set `review`
/// already computed.
async fn plan_attestation_replay(
    repo_root: &Path,
    base: &str,
    repo_attestation: &crate::config::RepoAttestation,
    gathered: Option<&crate::github::context::GatheredContext>,
    routed: &[&crate::reviewer::Reviewer],
) -> Option<crate::attest::AttestationOutcome> {
    plan_attestation_replay_with(
        repo_root,
        base,
        repo_attestation,
        gathered,
        routed,
        crate::github::client::RestClient::from_env,
    )
    .await
}

/// [`plan_attestation_replay`] with the GitHub client construction injected as
/// `build_client`, so a test can exercise "the client could not be built" and
/// "the key fetch failed" without mutating the real process environment
/// (`GITHUB_API_URL`/`GITHUB_TOKEN` are process-global and racy to touch from
/// parallel tests) or standing up a network fake for the happy path this
/// function does not otherwise need.
async fn plan_attestation_replay_with<C, B>(
    repo_root: &Path,
    base: &str,
    repo_attestation: &crate::config::RepoAttestation,
    gathered: Option<&crate::github::context::GatheredContext>,
    routed: &[&crate::reviewer::Reviewer],
    build_client: B,
) -> Option<crate::attest::AttestationOutcome>
where
    C: crate::github::client::GitHubApi,
    B: FnOnce() -> Result<C>,
{
    // Look up the note before deriving CI's own bindings: the ordinary case for
    // most commits is simply "no note", which is not a rejection and has nothing
    // to do with whether bindings re-derive cleanly. Deriving bindings first
    // would record a `could not re-derive CI bindings` fallback even when there
    // was never a note to replay against, surfacing a warning for what is really
    // the unremarkable "this author did not attest" case. A missing note yields
    // `NotAttested` (silent: the reviewers go through ordinary carry-or-execute,
    // just without a replay); only a note that was offered and refused becomes a
    // surfaced `Fallback`.
    let head_sha = gathered.and_then(|g| g.head_sha.as_deref());
    let note = match crate::attest::note_for_review(repo_root, "HEAD", head_sha) {
        Ok(Some(note)) => note,
        Ok(None) => return Some(crate::attest::AttestationOutcome::NotAttested),
        Err(err) => {
            return Some(fallback_outcome(format!(
                "could not look up the attestation note: {err:#}"
            )));
        }
    };

    let ci = match crate::attest::derive_ci_bindings(repo_root, base, &repo_attestation.config_hash)
    {
        Ok(ci) => ci,
        Err(err) => {
            return Some(fallback_outcome(format!(
                "could not re-derive CI bindings: {err:#}"
            )));
        }
    };

    let Some(author) = gathered.and_then(|g| g.author_login.as_deref()) else {
        return Some(fallback_outcome(
            "could not determine the pull request author to verify the attestation signature against"
                .to_string(),
        ));
    };

    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            return Some(fallback_outcome(format!(
                "could not build a GitHub client to fetch signing keys: {err:#}"
            )));
        }
    };
    let keys = match crate::github::signing::ssh_signing_keys(&client, author).await {
        Ok(keys) => keys,
        Err(err) => {
            return Some(fallback_outcome(format!(
                "could not fetch {author}'s registered SSH signing keys: {err:#}"
            )));
        }
    };

    let routed_map: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
        routed.iter().map(|r| (r.name.as_str(), *r)).collect();

    Some(crate::attest::plan(
        &note,
        author,
        &keys,
        &ci,
        &routed_map,
        crate::seal::embedded_secret(),
    ))
}

/// Wrap `reason` as a surfaced [`Fallback`](crate::attest::AttestationOutcome::Fallback):
/// the shape every early return in [`plan_attestation_replay_with`] takes when a replay
/// precondition (the note, CI bindings, the author, the client, the keys) is unmet.
fn fallback_outcome(reason: String) -> crate::attest::AttestationOutcome {
    crate::attest::AttestationOutcome::Fallback { reason }
}

/// The pull request a review is running against, so its description and discussion can
/// be gathered as reviewer context. Present only when `bastion review` is given a PR.
///
/// The `owner/name` slug is parsed into its parts when the source is built, so a
/// malformed repository is rejected at the boundary rather than re-checked later.
#[derive(Debug, Clone)]
pub struct GithubSource {
    /// The repository owner (the part before the `/`).
    owner: String,
    /// The repository name (the part after the `/`).
    name: String,
    /// The pull request number.
    pr: NonZeroU64,
}

impl GithubSource {
    /// Parse an `owner/name` slug and pull request number into a source.
    ///
    /// # Errors
    ///
    /// Returns an error if `repo` is not a single `owner/name` pair with both parts
    /// non-empty.
    pub fn new(repo: &str, pr: NonZeroU64) -> Result<Self> {
        let (owner, name) = repo
            .split_once('/')
            .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| eyre!("expected an 'owner/name' repository, got '{repo}'"))?;
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
            pr,
        })
    }
}

/// Gather a pull request's intent and discussion over a real GitHub client.
///
/// Builds the REST client from the environment (the same token the report step uses) and
/// delegates to the GitHub context producer. Surfaced as an error the caller logs and
/// recovers from, never one that fails the review.
async fn gather_github_context(
    source: &GithubSource,
) -> Result<crate::github::context::GatheredContext> {
    let client = crate::github::client::RestClient::from_env()?;
    crate::github::context::gather(&client, &source.owner, &source.name, source.pr.get()).await
}

/// Build a run id from the short HEAD sha, falling back to a fixed local
/// marker when git cannot supply one.
///
/// Full runs at one commit reuse the same id and overwrite the previous full
/// run, folding carried verdicts into the newest record. A partial
/// `--reviewer` run uses a distinct `-partial` suffix so it cannot overwrite
/// that full run; otherwise the finishing full run would have no earlier
/// sealed pass to carry from.
fn local_run_id(repo_root: &Path, partial: bool) -> RunId {
    let base = match git::short_head(repo_root) {
        Some(sha) => format!("r-{sha}"),
        None => "r-local".to_string(),
    };
    if partial {
        RunId(format!("{base}-partial"))
    } else {
        RunId(base)
    }
}

/// Derive the [`crate::seal::SealBindings`] a local review should seal its run
/// with, or `None` when any git derivation fails.
///
/// Sealing is opportunistic: a detached-HEAD edge case, a base that shares no
/// history with HEAD, or any other git failure here must never fail the review
/// itself, only leave the run unattestable. Each failure is logged at `warn`
/// with enough context to diagnose, then this returns `None` and the runner
/// proceeds unsealed.
fn derive_seal_bindings(
    repo_root: &Path,
    base: &str,
    repo_attestation: &crate::config::RepoAttestation,
) -> Option<SealBindings> {
    let merge_base_commit = ok_or_warn(
        git::merge_base(repo_root, base),
        "could not derive a merge base; run will be unsealed",
    )?;
    let head_tree = ok_or_warn(
        git::tree_hash(repo_root, "HEAD"),
        "could not resolve HEAD's tree; run will be unsealed",
    )?;
    let base_tree = ok_or_warn(
        git::tree_hash(repo_root, &merge_base_commit),
        "could not resolve the merge base's tree; run will be unsealed",
    )?;
    let patch_id = ok_or_warn(
        git::patch_id(repo_root, &merge_base_commit),
        "could not compute a patch id; run will be unsealed",
    )?;

    Some(SealBindings {
        head_tree,
        base_tree,
        patch_id,
        config_hash: repo_attestation.config_hash.clone(),
        repo_reviewers: repo_attestation.reviewers.clone(),
    })
}

/// Unwrap `result`, or log `msg` at `warn` and yield `None` so the caller's `?`
/// short-circuits. The uniform shape behind [`derive_seal_bindings`]'s opportunistic
/// git derivations: any failure leaves the run unsealed rather than failing it.
fn ok_or_warn<T, E: std::fmt::Display>(result: std::result::Result<T, E>, msg: &str) -> Option<T> {
    result
        .inspect_err(|err| tracing::warn!(error = %err, "{msg}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::Mode;

    #[test]
    fn a_partial_run_id_does_not_reuse_the_full_run_id_at_the_same_head() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("README"), "x\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "base"]);

        let full = local_run_id(dir, false);
        let partial = local_run_id(dir, true);
        assert_ne!(full, partial, "a partial must not overwrite the full run");
        assert!(
            partial.as_str().ends_with("-partial"),
            "got {}",
            partial.as_str()
        );
        assert!(
            partial.as_str().starts_with(full.as_str()),
            "partial id should be the full id plus a suffix; full={} partial={}",
            full.as_str(),
            partial.as_str()
        );
    }

    #[test]
    fn github_source_parses_a_slug_and_rejects_malformed_ones() {
        let pr = NonZeroU64::new(7).unwrap();
        let ok = GithubSource::new("acme/app", pr).expect("a well-formed slug parses");
        assert_eq!(ok.owner, "acme");
        assert_eq!(ok.name, "app");
        assert_eq!(ok.pr, pr);

        // No slash, an empty half, or an extra path segment are all rejected at the
        // boundary rather than reaching the GitHub client as a bad request.
        for bad in ["acme", "acme/", "/app", "acme/app/extra", "", "/"] {
            assert!(
                GithubSource::new(bad, pr).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    /// Run `git` with deterministic identity/config in `dir`.
    fn git(dir: &Path, args: &[&str]) {
        let isolate = [
            "-c",
            "user.email=t@bastion.dev",
            "-c",
            "user.name=T",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ];
        let status = std::process::Command::new("git")
            .args(isolate)
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
        // The `-c` isolation only covers commands issued through this helper.
        // Production code under test runs plain `git` in the same repo and
        // needs an identity from config on a host that has none (CI), so
        // persist one repo-locally at init.
        if args.first() == Some(&"init") {
            git(dir, &["config", "user.email", "grace@bastion.dev"]);
            git(dir, &["config", "user.name", "Grace Hopper"]);
        }
    }

    #[tokio::test]
    async fn review_with_no_matching_reviewers_is_a_persisted_pass() {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = repo.path();

        // A registry whose reviewers only trigger on docs, committed so it is not
        // itself a pending change.
        std::fs::write(
            dir.join(".bastion.yaml"),
            "reviewers:\n  - name: docs-only\n    trigger: [docs/**]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        git(dir, &["init"]);
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "base"]);

        // Change a source file that no reviewer triggers on.
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();

        let layout = Layout::with_root(data.path().to_path_buf());
        let options = ReviewOptions {
            base: "main".into(),
            format: Format::Jsonl,
            ..Default::default()
        };
        let decision = review(&layout, dir, options, None)
            .await
            .expect("zero-match review passes");
        assert_eq!(decision, Decision::Pass);

        // The pass was persisted and is now inspectable.
        let runs = store::list_runs(&layout).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].verdict, Some(Decision::Pass));
        assert_eq!(runs[0].reviewers, 0);
    }

    fn named_reviewer(name: &str) -> crate::reviewer::Reviewer {
        crate::reviewer::Reviewer {
            name: name.into(),
            trigger: vec!["**".into()].into(),
            mode: Mode::Gate,
            backend: crate::reviewer::Backend::ClaudeCode,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Default::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "p".into(),
        }
    }

    #[test]
    fn select_reviewers_passes_the_full_set_through_without_a_selection() {
        let all = vec![named_reviewer("a"), named_reviewer("b")];
        let triggered: Vec<&crate::reviewer::Reviewer> = all.iter().collect();
        let selected = select_reviewers(&triggered, &all, &[]).unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_reviewers_narrows_deduplicates_and_keeps_registry_order() {
        let all = vec![
            named_reviewer("a"),
            named_reviewer("b"),
            named_reviewer("c"),
        ];
        let triggered: Vec<&crate::reviewer::Reviewer> = all.iter().collect();
        let selected = select_reviewers(
            &triggered,
            &all,
            &["c".to_string(), "a".to_string(), "c".to_string()],
        )
        .unwrap();
        let names: Vec<&str> = selected.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "c"], "registry order, duplicates collapsed");
    }

    #[test]
    fn select_reviewers_rejects_unknown_and_untriggered_names() {
        let all = vec![named_reviewer("a"), named_reviewer("b")];
        // Only `a` triggered; `b` exists but did not match the changeset.
        let triggered: Vec<&crate::reviewer::Reviewer> = all[..1].iter().collect();
        let err = select_reviewers(
            &triggered,
            &all,
            &["b".to_string(), "typo".to_string(), "a".to_string()],
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("not in the reviewer registry: typo"),
            "got: {message}"
        );
        assert!(
            message.contains("not triggered by this changeset: b"),
            "got: {message}"
        );
        assert!(
            message.contains("triggered reviewers: a"),
            "the fix should be one glance away, got: {message}"
        );
    }

    /// A clone whose local `main` lags the `origin/main` it tracks, with HEAD
    /// on a feature branch forked from the fetched `origin/main`: the exact
    /// shape where `--base main` resolves a merge base older than the true
    /// fork point. Returns the temp root and the clone's path; the origin
    /// repository lives under the same root at `origin/`.
    fn clone_with_lagging_main() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init"]);
        std::fs::write(origin.join("a.txt"), "one\n").unwrap();
        git(&origin, &["add", "a.txt"]);
        git(&origin, &["commit", "-m", "base"]);

        // A file-path clone sets up `main` tracking `origin/main` with no
        // network involved.
        git(root, &["clone", "origin", "clone"]);
        let clone = root.join("clone");

        // Advance the origin's main behind the clone's back, then fetch: the
        // clone's origin/main moves, its local main does not.
        std::fs::write(origin.join("a.txt"), "one\nupstream\n").unwrap();
        git(&origin, &["commit", "-am", "upstream change"]);
        git(&clone, &["fetch", "origin"]);

        // Fork the feature branch from the *fetched* tip, the way a branch cut
        // from an up-to-date checkout (or rebased onto origin/main) sits.
        git(&clone, &["checkout", "-b", "feat", "origin/main"]);
        std::fs::write(clone.join("b.txt"), "feature\n").unwrap();
        git(&clone, &["add", "b.txt"]);
        git(&clone, &["commit", "-m", "feature work"]);

        (tmp, clone)
    }

    #[test]
    fn stale_base_warning_fires_when_the_local_base_lags_its_upstream() {
        let (_tmp, clone) = clone_with_lagging_main();

        let merge_base = git::merge_base(&clone, "main").unwrap();
        let warning =
            stale_base_warning(&clone, "main", &merge_base).expect("a lagging local base warns");
        assert!(warning.contains("'main'"), "got: {warning}");
        assert!(warning.contains("'origin/main'"), "got: {warning}");
        assert!(
            warning.contains("--base origin/main"),
            "the fix should be one glance away, got: {warning}"
        );
    }

    #[test]
    fn stale_base_warning_is_silent_when_the_local_base_matches_its_upstream() {
        let (_tmp, clone) = clone_with_lagging_main();
        // Bring the local main up to date; the two merge bases now agree.
        git(&clone, &["branch", "-f", "main", "origin/main"]);

        let merge_base = git::merge_base(&clone, "main").unwrap();
        assert_eq!(stale_base_warning(&clone, "main", &merge_base), None);
    }

    #[test]
    fn stale_base_warning_skips_a_base_without_an_upstream() {
        let tmp = repo_with_a_note_and_resolvable_base();
        let dir = tmp.path();

        let merge_base = git::merge_base(dir, "base").unwrap();
        assert_eq!(stale_base_warning(dir, "base", &merge_base), None);
    }

    #[test]
    fn stale_base_warning_skips_commit_and_remote_ref_bases() {
        let (_tmp, clone) = clone_with_lagging_main();

        // An explicit commit as the base: not a local branch, no upstream to
        // compare against, even though the repository is in the lagging shape.
        let commit = git::merge_base(&clone, "main").unwrap();
        assert_eq!(stale_base_warning(&clone, &commit, &commit), None);

        // The remote-tracking ref itself as the base is already the right
        // fork point; there is nothing to warn toward.
        let merge_base = git::merge_base(&clone, "origin/main").unwrap();
        assert_eq!(stale_base_warning(&clone, "origin/main", &merge_base), None);
    }

    /// A minimal repository whose HEAD carries a note under `NOTES_REF`, for
    /// `plan_attestation_replay` tests. The note's own content never matters for
    /// these tests: every scenario below fails (or is meant to fail) before the
    /// note's bundle is ever parsed.
    fn repo_with_a_note() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);
        git::note_add(dir, git::NOTES_REF, "HEAD", "not-a-real-bundle").unwrap();
        tmp
    }

    /// Like [`repo_with_a_note`], but with a resolvable `base` branch too, so a
    /// test can get past the note lookup *and* the CI-bindings derivation to
    /// reach the author/client/key-fetch checks further down
    /// `plan_attestation_replay_with`.
    fn repo_with_a_note_and_resolvable_base() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);
        git(dir, &["branch", "base"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(dir, &["commit", "-am", "feature work"]);
        git::note_add(dir, git::NOTES_REF, "HEAD", "not-a-real-bundle").unwrap();
        tmp
    }

    fn attestation_enabled() -> crate::config::RepoAttestation {
        crate::config::RepoAttestation {
            config_hash: "config-hash".into(),
            reviewers: std::collections::BTreeSet::new(),
            attestations_enabled: true,
        }
    }

    #[tokio::test]
    async fn plan_attestation_replay_is_not_attested_when_note_absent() {
        // The ordinary case: no note at all. This must resolve to `NotAttested`
        // (silent, not replayed), not a surfaced fallback, and it must win over an
        // unrelated bindings failure rather than getting shadowed by it: the
        // missing-note check runs before `derive_ci_bindings`, so even an
        // unresolvable base cannot turn "this author did not attest" into a
        // "could not re-derive CI bindings" warning.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        let outcome =
            plan_attestation_replay(dir, "nonexistent-base", &attestation_enabled(), None, &[])
                .await;
        assert!(
            matches!(
                outcome,
                Some(crate::attest::AttestationOutcome::NotAttested)
            ),
            "a missing note must resolve to NotAttested even when the base is also \
             unresolvable, got: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn plan_attestation_replay_falls_back_when_ci_bindings_cannot_be_derived() {
        // A note is present, but `base` does not resolve, so `derive_ci_bindings`
        // fails. This must still record a persisted fallback (not `None`), and the
        // reason must name the bindings failure, not the (irrelevant, since a note
        // exists) "no attestation note found" reason.
        let tmp = repo_with_a_note();
        let dir = tmp.path();

        let outcome = plan_attestation_replay(
            dir,
            "this-base-does-not-exist",
            &attestation_enabled(),
            None,
            &[],
        )
        .await;
        match outcome {
            Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
                assert!(
                    reason.contains("could not re-derive CI bindings"),
                    "got: {reason}"
                );
            }
            other => panic!("expected a fallback, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_attestation_replay_falls_back_when_the_pr_author_is_missing() {
        // A note is present and the bindings re-derive cleanly, but `gathered`
        // carries no author login (a deleted account, or GitHub simply omitting
        // it). `plan_attestation_replay` takes `gathered` directly, so this
        // branch is testable with no network at all: an author-less
        // `GatheredContext` reaches the check directly.
        let tmp = repo_with_a_note_and_resolvable_base();
        let dir = tmp.path();
        let gathered = crate::github::context::GatheredContext {
            author_login: None,
            ..Default::default()
        };

        let outcome =
            plan_attestation_replay(dir, "base", &attestation_enabled(), Some(&gathered), &[])
                .await;
        match outcome {
            Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
                assert!(reason.contains("pull request author"), "got: {reason}");
            }
            other => panic!("expected a fallback, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_attestation_replay_falls_back_when_the_github_client_cannot_be_built() {
        // Every check ahead of the client construction passes (a note exists, the
        // base resolves, an author is present), so the injected `build_client`
        // closure's own failure is what is under test here. This is
        // `plan_attestation_replay_with`'s whole reason for existing: exercising
        // this branch without it would mean mutating the real
        // `GITHUB_API_URL`/`GITHUB_TOKEN` environment, which is process-global
        // and racy across this suite's parallel tests.
        let tmp = repo_with_a_note_and_resolvable_base();
        let dir = tmp.path();
        let gathered = crate::github::context::GatheredContext {
            author_login: Some("grace".to_string()),
            ..Default::default()
        };

        let outcome = plan_attestation_replay_with(
            dir,
            "base",
            &attestation_enabled(),
            Some(&gathered),
            &[],
            || -> Result<crate::github::client::test_support::RecordingClient> {
                Err(eyre!("simulated client construction failure"))
            },
        )
        .await;
        match outcome {
            Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
                assert!(
                    reason.contains("could not build a GitHub client"),
                    "got: {reason}"
                );
                assert!(reason.contains("simulated client construction failure"));
            }
            other => panic!("expected a fallback, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_attestation_replay_falls_back_when_the_signing_key_fetch_fails() {
        // The client itself builds fine, but the signing-key request comes back
        // non-2xx. `RecordingClient` (the same double `github::report`'s own
        // tests use) stands in for the network here, so this exercises
        // `ssh_signing_keys`'s failure path with no real GitHub involved.
        let tmp = repo_with_a_note_and_resolvable_base();
        let dir = tmp.path();
        let gathered = crate::github::context::GatheredContext {
            author_login: Some("grace".to_string()),
            ..Default::default()
        };

        let outcome = plan_attestation_replay_with(
            dir,
            "base",
            &attestation_enabled(),
            Some(&gathered),
            &[],
            || {
                Ok(
                    crate::github::client::test_support::RecordingClient::with_responder(|_req| {
                        crate::github::client::ApiResponse {
                            status: 404,
                            body: serde_json::json!({"message": "Not Found"}),
                        }
                    }),
                )
            },
        )
        .await;
        match outcome {
            Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
                assert!(reason.contains("could not fetch"), "got: {reason}");
                assert!(reason.contains("signing keys"));
            }
            other => panic!("expected a fallback, got: {other:?}"),
        }
    }
}
