//! Command handlers.
//!
//! Each function implements one CLI subcommand. The read-back commands
//! (`transcript`, `show`, `runs`, `clean`) are fully functional over saved runs;
//! `review` does real config discovery, git-based change detection, and routing,
//! then hands off to the [`crate::runner`] to execute the matched reviewers. The
//! runner owns event emission and persistence; this handler renders the stream and
//! reports the aggregate decision so the CLI can set the exit status.
//! `codeowners` is pure generation.

use std::io::{self, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;

use color_eyre::eyre::{Context, Result, bail, eyre};

use crate::config::Config;
use crate::context::ReviewContext;
use crate::event::{ReviewerRef, RunEvent, RunId};
use crate::git;
use crate::paths::Layout;
use crate::render::{self, Format};
use crate::reviewer::{Mode, ModelId};
use crate::routing::Router;
use crate::runner::{self, ExecContext};
use crate::seal::SealBindings;
use crate::skills;
use crate::store;
use crate::verdict::{Decision, Money};

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
    /// the branch's previous run ([`crate::carry`]).
    pub fresh: bool,
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
/// `user_dir` is the user-level config directory whose reviewers are merged with
/// the repository's (`None` to skip it). This is how a personal reviewer runs
/// locally even when the repository does not adopt Bastion in CI; an identical
/// reviewer in both files is deduplicated and a same-name collision keeps both with
/// the repo side scoped (see [`Config::discover_merged`]).
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
    } = options;
    let base = base.as_str();
    let repo_root = git::repo_root(cwd)?;
    warn_on_stale_skills(&repo_root);
    let branch = git::current_branch(&repo_root)?;
    let (_sources, repo_attestation, config) =
        Config::discover_merged_attested(&repo_root, user_dir)?;
    let changed = git::changed_files(&repo_root, base)?;

    let router = Router::compile(&config.reviewers)?;
    let triggered = router.matched(&changed);
    let matched = select_reviewers(&triggered, &config.reviewers, &only)?;
    // Partial means coverage was actually reduced: selecting every triggered
    // reviewer by name is still a full run.
    let partial = matched.len() < triggered.len();
    let run = local_run_id(&repo_root);
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

    // Assemble the review context: the author's stated intent (the PR body, or this
    // branch's commit messages locally), this branch's prior findings recalled from the
    // run store, and the surrounding discussion (GitHub only). Empty when nothing
    // applies, which leaves every reviewer's prompt exactly as it was.
    let mut context = ReviewContext {
        intent: git::commit_messages(&repo_root, base),
        comments: Vec::new(),
        prior_findings: store::prior_findings(layout, &branch),
    };
    let mut gathered_github: Option<crate::github::context::GatheredContext> = None;
    if let Some(source) = github.as_ref() {
        match gather_github_context(source).await {
            Ok(gathered) => {
                // A PR body is a better statement of intent than the commit messages,
                // so it wins when present; the discussion is GitHub-only.
                if gathered.intent.is_some() {
                    context.intent = gathered.intent.clone();
                }
                context.comments = gathered.comments.clone();
                gathered_github = Some(gathered);
            }
            Err(err) => {
                eprintln!("bastion review: continuing without GitHub context ({err:#})");
            }
        }
    }

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

    let (replayed, attestation, attestation_fallback, matched): (
        std::collections::BTreeMap<String, runner::ReplayedReviewer>,
        Option<runner::AttestationAudit>,
        Option<RunEvent>,
        Vec<&crate::reviewer::Reviewer>,
    ) = match attested {
        Some(crate::attest::AttestationOutcome::Replay(plan)) => {
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
                (replayed, None, None, fresh)
            } else {
                let callout = format!(
                    "bastion review: {} reviewer(s) replayed from a signed local attestation (key {}, attested {}): {}",
                    replayed.len(),
                    bundle_public_key,
                    bundle_attested_at,
                    replayed.keys().cloned().collect::<Vec<_>>().join(", "),
                );
                // Fallible on purpose: a closed stderr must not panic the gate
                // out of an otherwise valid review.
                let _ = writeln!(io::stderr(), "{callout}");
                (
                    replayed,
                    Some(runner::AttestationAudit {
                        public_key: bundle_public_key,
                        attested_at: bundle_attested_at,
                    }),
                    None,
                    fresh,
                )
            }
        }
        Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
            // Fallible on purpose: the fallback must still render and the
            // fresh reviewers must still run if stderr is closed.
            let _ = writeln!(
                io::stderr(),
                "bastion review: attestation not honored: {reason}"
            );
            let fallback = RunEvent::AttestationFallback {
                run: run.clone(),
                reason,
            };
            render::write_event(&mut out, format, &fallback)?;
            (
                std::collections::BTreeMap::new(),
                None,
                Some(fallback),
                matched,
            )
        }
        None => (std::collections::BTreeMap::new(), None, None, matched),
    };

    // Trigger-scoped digests for everything about to run, stamped onto each
    // resolved event so the *next* run can decide whether to carry it. Best
    // effort: a digest that fails to compute only leaves that reviewer
    // executing fresh and uncarryable, never fails the review.
    let mut scope_digests: std::collections::BTreeMap<String, String> = Default::default();
    let mut digest_probe: Option<runner::DigestProbe> = None;
    match git::merge_base(&repo_root, base) {
        Ok(merge_base) => {
            for reviewer in &matched {
                match crate::carry::scope_digest(&repo_root, base, &merge_base, reviewer, &changed)
                {
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
            // The runner re-derives every stamped digest after the reviewers
            // finish, so a tree that changed mid-run cannot leave a stale
            // digest behind for a later run to carry from.
            digest_probe = Some(runner::DigestProbe {
                base: base.to_string(),
                merge_base,
            });
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not resolve a merge base; no scope digests this run");
        }
    }

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
    let carried = if !fresh && only.is_empty() {
        let candidates: Vec<(&crate::reviewer::Reviewer, String)> = matched
            .iter()
            .filter_map(|r| {
                scope_digests
                    .get(&r.name)
                    .map(|digest| (*r, digest.clone()))
            })
            .collect();
        crate::carry::plan(
            layout,
            &branch,
            &candidates,
            &repo_attestation.reviewers,
            crate::seal::embedded_secret(),
        )
    } else {
        Default::default()
    };
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
        changed: changed_count,
        reviewers: reviewer_refs,
        context,
        seal,
        dirty,
        replayed,
        attestation,
        partial,
        carried,
        scope_digests,
        digest_probe,
        attestation_fallback,
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
    // most commits is simply "no note", which is a fallback in its own right and
    // has nothing to do with whether bindings re-derive cleanly. Deriving
    // bindings first would record a `could not re-derive CI bindings` fallback
    // even when there was never a note to replay against, burying the real
    // "no attestation note found" reason under irrelevant noise.
    let head_sha = gathered.and_then(|g| g.head_sha.as_deref());
    let note = match crate::attest::note_for_review(repo_root, "HEAD", head_sha) {
        Ok(Some(note)) => note,
        Ok(None) => {
            return Some(crate::attest::AttestationOutcome::Fallback {
                reason: "no attestation note found on HEAD".to_string(),
            });
        }
        Err(err) => {
            return Some(crate::attest::AttestationOutcome::Fallback {
                reason: format!("could not look up the attestation note: {err:#}"),
            });
        }
    };

    let ci = match crate::attest::derive_ci_bindings(repo_root, base, &repo_attestation.config_hash)
    {
        Ok(ci) => ci,
        Err(err) => {
            return Some(crate::attest::AttestationOutcome::Fallback {
                reason: format!("could not re-derive CI bindings: {err:#}"),
            });
        }
    };

    let Some(author) = gathered.and_then(|g| g.author_login.as_deref()) else {
        return Some(crate::attest::AttestationOutcome::Fallback {
            reason: "could not determine the pull request author to verify the attestation signature against".to_string(),
        });
    };

    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            return Some(crate::attest::AttestationOutcome::Fallback {
                reason: format!("could not build a GitHub client to fetch signing keys: {err:#}"),
            });
        }
    };
    let keys = match crate::github::signing::ssh_signing_keys(&client, author).await {
        Ok(keys) => keys,
        Err(err) => {
            return Some(crate::attest::AttestationOutcome::Fallback {
                reason: format!("could not fetch {author}'s registered SSH signing keys: {err:#}"),
            });
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

/// `bastion validate`: parse the reviewer registry and report any problems.
///
/// Loads the registry (the explicit `file`, or the merged set discovered by
/// walking up from `cwd` and layering in the user-level config dir) through the
/// same [`Config`] path `bastion review` uses, so it surfaces exactly the errors a
/// real review would hit at load time: malformed YAML, an unknown field, a
/// duplicate reviewer name, or a model pinned under `backend: any`. On success it
/// prints a one-line summary and the parsed reviewers and returns `Ok`; on any
/// problem it returns the error, which `color_eyre` renders before the process
/// exits non-zero, so the command doubles as a CI lint and a cheap local check that
/// never spends a model call.
///
/// `user_dir` is the user-level config directory layered into discovery (`None` to
/// skip it). An explicit `file` is validated on its own, with no layering, since it
/// is a deliberate single-file check.
///
/// # Errors
///
/// Returns an error if no registry is found, the file cannot be read, or it fails
/// to parse or validate.
pub fn validate(cwd: &Path, file: Option<&Path>, user_dir: Option<&Path>) -> Result<()> {
    let (label, config) = match file {
        Some(file) => (file.display().to_string(), Config::load(file)?),
        None => {
            // Resolve from the repo root when we are inside one (so the command
            // works from any subdirectory, like `review`), falling back to `cwd`
            // when git cannot tell us, which keeps a not-yet-initialized repo
            // working. `discover_merged_located` warns on the deprecated location,
            // gives the clear "no registry found" error, and hands back the sources
            // it loaded, so the summary reports exactly the files that were merged.
            let root = git::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let (sources, config) = Config::discover_merged_located(&root, user_dir)?;
            (describe_sources(&sources), config)
        }
    };

    let gates = config
        .reviewers
        .iter()
        .filter(|r| r.mode == Mode::Gate)
        .count();
    let advisors = config.reviewers.len() - gates;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{label} is valid: {} reviewer(s), {gates} gate(s), {advisors} advisor(s).",
        config.reviewers.len(),
    )?;
    for reviewer in &config.reviewers {
        let model = reviewer.model.as_ref().map_or("default", ModelId::as_str);
        writeln!(
            out,
            "  - {} ({}, backend: {}, model: {model})",
            reviewer.name,
            reviewer.mode.as_str(),
            reviewer.backend.as_str(),
        )?;
    }
    Ok(())
}

/// Describe the registry [`Sources`] that fed a merged config, for the `validate`
/// summary line. A single source reads as its own path (so the common case matches
/// the pre-merge wording); both sources name each file so it is clear what was
/// merged. At least one is always present, since discovery errors otherwise.
fn describe_sources(sources: &crate::config::Sources) -> String {
    match (&sources.repo, &sources.user) {
        (Some(repo), Some(user)) => format!(
            "the merged registry (repo: {}, user: {})",
            repo.path.display(),
            user.display()
        ),
        (Some(repo), None) => repo.path.display().to_string(),
        (None, Some(user)) => user.display().to_string(),
        (None, None) => unreachable!("discover_merged_located errors when both sources are absent"),
    }
}

/// `bastion transcript [<run>] <reviewer>`: print a saved session transcript.
///
/// # Errors
///
/// Returns an error if the run or transcript does not exist.
pub fn transcript(layout: &Layout, run: Option<&str>, reviewer: &str) -> Result<()> {
    let run = store::resolve_run(layout, run)?;
    let path = layout.transcript(&run, reviewer);
    let text = std::fs::read_to_string(&path).wrap_err_with(|| {
        format!(
            "no saved transcript for reviewer '{reviewer}' in run '{run}' ({})",
            path.display()
        )
    })?;
    io::stdout()
        .write_all(text.as_bytes())
        .wrap_err("writing transcript")?;
    Ok(())
}

/// `bastion show [<run>]`: re-emit a past run's verdicts and findings.
///
/// # Errors
///
/// Returns an error if the run does not exist or cannot be read.
pub fn show(layout: &Layout, run: Option<&str>, format: Format) -> Result<()> {
    let run = store::resolve_run(layout, run)?;
    let events = store::read_run(layout, &run)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for event in &events {
        if matches!(
            event,
            RunEvent::ReviewerResolved { .. } | RunEvent::RunCompleted { .. }
        ) {
            render::write_event(&mut out, format, event)?;
        }
    }
    Ok(())
}

/// `bastion runs`: list recorded runs.
///
/// # Errors
///
/// Returns an error if the runs directory cannot be read.
pub fn runs(layout: &Layout, format: Format) -> Result<()> {
    let runs = store::list_runs(layout)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render::write_runs(&mut out, format, &runs).wrap_err("rendering runs")?;
    Ok(())
}

/// `bastion clean`: prune saved runs.
///
/// # Errors
///
/// Returns an error if a run cannot be removed.
pub fn clean(layout: &Layout, keep: Option<usize>, older_than: Option<Duration>) -> Result<()> {
    let keep = if keep.is_none() && older_than.is_none() {
        Some(default_keep())
    } else {
        keep
    };
    let removed = store::prune(layout, keep, older_than)?;
    println!("removed {} run(s)", removed.len());
    for id in &removed {
        println!("  {id}");
    }
    Ok(())
}

/// `bastion github codeowners`: print a CODEOWNERS block for the reviewer-policy paths.
///
/// Covers the reviewer registry, the Bastion workflow, and the CODEOWNERS file
/// itself, so any PR touching review policy requires a human review.
///
/// # Errors
///
/// Returns an error if writing to stdout fails.
pub fn codeowners(owners: &[String]) -> Result<()> {
    io::stdout()
        .write_all(crate::github::codeowners::block(owners).as_bytes())
        .wrap_err("writing CODEOWNERS block")?;
    Ok(())
}

/// `bastion attest`: sign the latest sealed run (or `run`, when given) as an
/// attestation note on HEAD.
///
/// Resolves the repository root from the current directory and hands off to
/// [`crate::attest::attest`] with the build's embedded sealing secret; the real
/// verification and signing logic lives there so it stays testable without a
/// CLI harness.
///
/// # Errors
///
/// Returns an error under any of the refusals documented on
/// [`crate::attest::attest`] (an unsealed run, a seam-tainted seal, a
/// tampered run store, repository drift since the run, or no resolvable
/// signing key), or if writing to stdout fails.
pub fn attest(layout: &Layout, run: Option<&str>, key: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
    let root = git::repo_root(&cwd)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    crate::attest::attest(
        &root,
        layout,
        run,
        key,
        crate::seal::embedded_secret(),
        &mut out,
    )
}

/// `bastion github report`: post a finished run's results to its pull request.
///
/// Reads the persisted run (the latest, or `run` when given), builds the GitHub
/// client from the Actions environment (`GITHUB_TOKEN`, `GITHUB_API_URL`), and
/// upserts the sticky PR comment plus a check run per reviewer and the aggregate
/// `bastion` check. The run is already persisted by `bastion review`, so this is a
/// pure read-and-post step that can run after the gate has decided.
///
/// `slug` is the `owner/name` repository, `pr` the pull request number, and `sha`
/// the head commit the check runs attach to (all supplied by the workflow from the
/// pull-request event).
///
/// # Errors
///
/// Returns an error if the run cannot be read, the client cannot be built (e.g. a
/// missing token), or a GitHub request fails. A missing run is reported as a
/// non-fatal notice (so a report step running after an infrastructure failure does
/// not pile a second, confusing error on top of the real one).
pub async fn github_report(
    layout: &Layout,
    cwd: &Path,
    slug: &str,
    pr: u64,
    sha: &str,
    run: Option<&str>,
) -> Result<()> {
    let ctx = crate::github::PrContext::new(slug, pr, sha)?;

    let run = match store::resolve_run(layout, run) {
        Ok(run) => run,
        Err(err) => {
            // No run to report: surface it as a notice and stop, rather than failing
            // the step on top of whatever already went wrong upstream.
            eprintln!("bastion github report: nothing to report ({err:#})");
            return Ok(());
        }
    };
    let events = store::read_run(layout, &run)?;

    // Fold a skills-freshness advisory into the comment when the checked-out repo's
    // bundled skills are missing or stale, mirroring the stderr notice the local
    // review prints. Best effort, so a check error never fails the report step.
    let skills_warning = stale_skills_warning(cwd);

    let client = crate::github::client::RestClient::from_env()?;
    let summary =
        crate::github::report::report(&client, &ctx, &events, skills_warning.as_ref()).await?;

    writeln!(
        io::stdout(),
        "Reported run {run} to {slug}#{pr}: {summary}."
    )
    .wrap_err("writing report summary")?;
    Ok(())
}

/// `bastion skills install`: write the bundled agent skills into the repository.
///
/// Resolves the repository root from `cwd`, writes each bundled skill into every
/// target directory (the defaults, or the `--dir` overrides), and prints what it
/// did. Existing files that differ are left untouched unless `force` is set, so a
/// local edit is never clobbered silently.
///
/// # Errors
///
/// Returns an error if a skill directory cannot be created or a file cannot be
/// read or written, or if writing the summary to stdout fails.
pub fn skills_install(cwd: &Path, dirs: &[PathBuf], force: bool) -> Result<()> {
    let root = skills_root(cwd);
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::install(&root, &targets, force)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut skipped = 0usize;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Installed::Created => "created",
            skills::Installed::Updated => "updated",
            skills::Installed::Unchanged => "unchanged",
            skills::Installed::Skipped => {
                skipped += 1;
                "skipped (exists)"
            }
        };
        writeln!(
            out,
            "  {label}: {}",
            skills::display_relative(&root, &outcome.path)
        )?;
    }
    if skipped > 0 {
        writeln!(
            out,
            "\n{skipped} file(s) already existed and were left as-is; re-run with --force to overwrite."
        )?;
    } else {
        writeln!(
            out,
            "\nCommit these files so your agents discover them on checkout."
        )?;
    }
    Ok(())
}

/// `bastion skills check`: verify the installed skills match this binary's
/// embedded source.
///
/// Prints one line per skill file and returns whether every one is up to date.
/// Returns `Ok(false)` when any file is missing or has drifted (a hand edit, or a
/// stale install left behind after the skill source changed), so the caller can
/// exit non-zero: a CI step can run this to fail when the checked-in skills fall
/// out of sync with the source.
///
/// # Errors
///
/// Returns an error if a skill file exists but cannot be read, or if writing the
/// summary to stdout fails.
pub fn skills_check(cwd: &Path, dirs: &[PathBuf]) -> Result<bool> {
    let root = skills_root(cwd);
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::check(&root, &targets)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut current = true;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Checked::UpToDate => "up to date",
            skills::Checked::Drifted => {
                current = false;
                "drifted"
            }
            skills::Checked::Missing => {
                current = false;
                "missing"
            }
        };
        writeln!(
            out,
            "  {label}: {}",
            skills::display_relative(&root, &outcome.path)
        )?;
    }
    if !current {
        writeln!(
            out,
            "\nChecked-in skills are out of sync; run `bastion skills install` to refresh them."
        )?;
    }
    Ok(current)
}

/// `bastion skills list`: show the skills bundled into this binary.
///
/// # Errors
///
/// Returns an error if writing to stdout fails.
pub fn skills_list() -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "Skills bundled in bastion {}:",
        crate::version::VERSION
    )?;
    for skill in skills::BUNDLED {
        writeln!(out, "  {} - {}", skill.slug, skill.summary)?;
    }
    writeln!(
        out,
        "\nInstall them with `bastion skills install` (default targets: {}).",
        skills::DEFAULT_DIRS.join(", ")
    )?;
    Ok(())
}

/// The repository root to install skills into: the git toplevel containing `cwd`,
/// or `cwd` itself when it is not inside a repo, so first-time setup still works.
fn skills_root(cwd: &Path) -> PathBuf {
    git::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// The skills-freshness advisory for the repository containing `cwd`, or `None`
/// when every bundled skill is present and current.
///
/// Both review surfaces call this to warn when an agent may be working against
/// stale guidance. It is deliberately best effort, so a check error (an unreadable
/// skill file) maps to `None` rather than surfacing; a skills advisory must never
/// fail a review or a report. The default skills directories are checked, the same
/// ones `bastion skills install` writes.
fn stale_skills_warning(cwd: &Path) -> Option<skills::DriftWarning> {
    skills::assess(&skills_root(cwd), &skills::default_dirs())
        .ok()
        .flatten()
}

/// The skills-freshness advisory a local `bastion review` should surface, or `None`
/// when it should stay silent.
///
/// This gates [`stale_skills_warning`] on the repository having *adopted* Bastion: a
/// repository-level registry is present ([`crate::config::locate_kind`] resolves one).
/// A purely local review that merged in only the author's user-level reviewers has no
/// repo registry, and nudging that author to install skills into a project that has not
/// configured Bastion would be misdirected. Only the local surface is gated this way;
/// CI always has a repo registry, so the warning [`github_report`] folds into the
/// sticky comment is unaffected.
fn local_skills_warning(repo_root: &Path) -> Option<skills::DriftWarning> {
    // No repo registry (or an unreadable candidate): stay silent. The skills nudge is
    // meaningful only once the project itself has adopted Bastion, and a failed
    // presence check must never be the thing this advisory surfaces.
    if !matches!(crate::config::locate_kind(repo_root), Ok(Some(_))) {
        return None;
    }
    stale_skills_warning(repo_root)
}

/// Print the skills-freshness advisory to stderr, where the agent driving
/// `bastion review` sees it alongside the run. Silent when the skills are current or
/// the repository has not adopted Bastion (see [`local_skills_warning`]).
///
/// stderr keeps it out of the `--format jsonl` event stream on stdout (so a parsing
/// agent's input stays clean) while still landing somewhere both a human and an
/// agent read, matching how the GitHub-context notice is surfaced.
fn warn_on_stale_skills(repo_root: &Path) {
    if let Some(warning) = local_skills_warning(repo_root) {
        // Fail open on the write itself. This advisory runs before any reviewer, so a
        // failed stderr write (a broken pipe, say) must not abort an otherwise-passing
        // review the way `eprintln!` would by panicking; swallow the result instead.
        let _ = writeln!(io::stderr(), "bastion review: {}", warning.plain());
    }
}

/// The requested skill directories, falling back to the documented defaults when
/// none were passed.
fn resolve_skill_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    if dirs.is_empty() {
        skills::default_dirs()
    } else {
        dirs.to_vec()
    }
}

/// How many runs to keep when `bastion clean` is given no arguments.
fn default_keep() -> usize {
    20
}

/// Build a run id for a local run from the short HEAD sha, falling back to a
/// fixed local marker when git can't supply one.
fn local_run_id(repo_root: &Path) -> RunId {
    match git::short_head(repo_root) {
        Some(sha) => RunId(format!("r-{sha}")),
        None => RunId("r-local".to_string()),
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
    let merge_base_commit = match git::merge_base(repo_root, base) {
        Ok(commit) => commit,
        Err(err) => {
            tracing::warn!(error = %err, "could not derive a merge base; run will be unsealed");
            return None;
        }
    };
    let head_tree = match git::tree_hash(repo_root, "HEAD") {
        Ok(tree) => tree,
        Err(err) => {
            tracing::warn!(error = %err, "could not resolve HEAD's tree; run will be unsealed");
            return None;
        }
    };
    let base_tree = match git::tree_hash(repo_root, &merge_base_commit) {
        Ok(tree) => tree,
        Err(err) => {
            tracing::warn!(error = %err, "could not resolve the merge base's tree; run will be unsealed");
            return None;
        }
    };
    let patch_id = match git::patch_id(repo_root, &merge_base_commit) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(error = %err, "could not compute a patch id; run will be unsealed");
            return None;
        }
    };

    Some(SealBindings {
        head_tree,
        base_tree,
        patch_id,
        config_hash: repo_attestation.config_hash.clone(),
        repo_reviewers: repo_attestation.reviewers.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_skills_warning_fails_open_on_an_unreadable_skill() {
        // A skills-freshness check must never fail a review or a report. When the
        // assessment errors (here a directory where a SKILL.md should be, so reading
        // it fails), the warning maps to `None` rather than propagating the error.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".claude/skills/using-bastion/SKILL.md")).unwrap();
        assert!(
            stale_skills_warning(root).is_none(),
            "an assessment error should swallow to no warning, not surface"
        );
    }

    #[test]
    fn local_skills_warning_is_silent_without_a_repo_registry() {
        // A purely local review against a repo that has not adopted Bastion (no
        // `.bastion.yaml`) merges in only the author's user-level reviewers. Warning
        // there would tell the author to install skills into a project that has not
        // configured Bastion, which is misdirected. Even with every skill missing, the
        // local surface stays silent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(
            local_skills_warning(root).is_none(),
            "no repo registry should suppress the local skills advisory"
        );
    }

    #[test]
    fn local_skills_warning_fires_once_the_repo_adopts_bastion() {
        // With a repository registry present, the repo has adopted Bastion, so a stale
        // (here entirely missing) skills tree is worth flagging to the driving agent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join(crate::config::REGISTRY_FILE),
            "reviewers:\n  - name: r\n    trigger: [x]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        let warning = local_skills_warning(root).expect("a repo registry enables the advisory");
        assert!(warning.plain().contains("missing or out of date"));
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

    #[test]
    fn validate_accepts_a_well_formed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: a\n    trigger: [src/**]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        validate(tmp.path(), Some(&path), None).expect("a well-formed file validates");
    }

    #[test]
    fn validate_reports_a_duplicate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: dup\n    trigger: [a]\n    mode: gate\n    prompt: p\n  - name: dup\n    trigger: [b]\n    mode: gate\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate reviewer name"),
            "error should name the duplicate, got: {err:#}"
        );
    }

    #[test]
    fn validate_reports_an_unknown_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: typo\n    trigger: [src/**]\n    mode: gate\n    bakend: codex\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown field `bakend`"),
            "validate should reject an unknown field, got: {err:#}"
        );
    }

    #[test]
    fn validate_reports_a_model_under_backend_any() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".bastion.yaml");
        std::fs::write(
            &path,
            "reviewers:\n  - name: stray\n    trigger: [src/**]\n    mode: gate\n    model: gpt-5\n    prompt: p\n",
        )
        .unwrap();
        let err = validate(tmp.path(), Some(&path), None).unwrap_err();
        assert!(format!("{err:#}").contains("backend: any"), "got: {err:#}");
    }

    #[test]
    fn validate_discovers_from_the_directory_when_no_file_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".bastion.yaml"),
            "reviewers:\n  - name: a\n    trigger: [x]\n    mode: advisor\n    prompt: p\n",
        )
        .unwrap();
        validate(tmp.path(), None, None).expect("discovered registry validates");
    }

    #[test]
    fn validate_errors_clearly_when_no_registry_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = validate(tmp.path(), None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no reviewer registry found"),
            "got: {err:#}"
        );
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
            trigger: vec!["**".into()],
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
    async fn plan_attestation_replay_falls_back_with_no_attestation_note_reason_when_absent() {
        // The ordinary case: no note at all. This must stay the "no attestation
        // note found" fallback, not get shadowed by an unrelated bindings failure.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "base"]);

        let outcome =
            plan_attestation_replay(dir, "nonexistent-base", &attestation_enabled(), None, &[])
                .await;
        match outcome {
            Some(crate::attest::AttestationOutcome::Fallback { reason }) => {
                assert!(
                    reason.contains("no attestation note found"),
                    "a missing note must report its own reason even when the base is also \
                     unresolvable, got: {reason}"
                );
            }
            other => panic!("expected a fallback, got: {other:?}"),
        }
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
