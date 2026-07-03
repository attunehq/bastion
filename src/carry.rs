//! Incremental re-review: carrying a prior pass forward when nothing a
//! reviewer's verdict was scoped to has changed.
//!
//! The review loop's dominant cost is re-running reviewers that already passed:
//! after fixing one reviewer's findings, the next `bastion review` re-executes
//! the whole triggered set even though most of it judged content the fix never
//! touched. This module keys each reviewer's verdict to a *scope digest*: a hash
//! of the reviewer's own effective definition plus the diff of exactly the
//! changed files its trigger matched. On the next run of the same branch, a
//! reviewer whose digest is unchanged and whose prior verdict was a pass is
//! *carried* instead of executed; a reviewer whose scoped content changed (the
//! one that blocked, plus anything the fix touched) runs fresh.
//!
//! The soundness boundary is the trigger, deliberately: Bastion's design routes
//! a reviewer by its trigger globs, so those globs are already the declaration
//! of what the reviewer's concern depends on. A reviewer whose judgment spans
//! files outside its trigger should widen its trigger; that is the same
//! contract routing itself enforces.
//!
//! Carry has two hard limits:
//!
//! - **Local only.** In CI the run store arrives as a restored artifact, and a
//!   seal is only tamper *evidence* (the HMAC secret ships inside the public
//!   binary), not proof of origin. The CI-side skip mechanism is attestation
//!   replay (`docs/developer-guide/attestation.md`), which demands the author's
//!   SSH signature; carry never applies to a run with a GitHub source, so it can
//!   never bypass that. The caller (`commands::review`) enforces this.
//! - **A repository reviewer only carries from a sealed, verified run.** The
//!   carried verdict flows into the new run's seal and from there into anything
//!   the author later attests, so every link in that chain must have been
//!   binary-verified: an unsealed prior run, a seal that does not verify, or a
//!   seal recording an active test seam disqualifies carry for every repository
//!   reviewer. A user-level reviewer (never sealed, never gating anyone else's
//!   PR) carries on the digest alone.
//!
//! Blocks are never carried: a blocked reviewer whose scoped diff is unchanged
//! still re-runs, because the surrounding context (intent, discussion, prior
//! findings) may have changed the author's answer to it, and re-confirming a
//! block is exactly the loop's next question. A reviewer with `attestation:
//! never` is never carried either: that policy asks for fresh execution every
//! time, and carry honors it on both surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use color_eyre::eyre::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::event::RunEvent;
use crate::git;
use crate::paths::Layout;
use crate::reviewer::{AttestationPolicy, Reviewer};
use crate::store;
use crate::verdict::Decision;

/// One reviewer's verdict carried forward from the branch's previous run,
/// ready for the runner to fold in without executing a backend.
#[derive(Debug, Clone)]
pub struct Carried {
    /// The reviewer's current definition (the digest already proved it is
    /// byte-identical to the one that produced the prior verdict).
    pub reviewer: Reviewer,
    /// The prior run's `reviewer.resolved` event for this reviewer, exactly as
    /// persisted (and, for a repository reviewer, exactly as sealed).
    pub event: RunEvent,
}

/// The canonical input a scope digest hashes, serialized once here so the
/// digest can never disagree with itself across call sites. Field order in the
/// struct definition is the canonical form, same as [`crate::seal`]'s digest.
#[derive(Serialize)]
struct DigestInput<'a> {
    /// The reviewer's full effective definition (defaults already applied by
    /// config loading), so any edit to the reviewer (prompt, backend, model,
    /// trigger, capabilities) invalidates its carried verdicts.
    reviewer: &'a Reviewer,
    /// The merge-base commit the diff below is taken against, so a digest can
    /// never survive the base branch moving under the changeset: a new merge
    /// base means a new comparison point, even where the textual diff happens
    /// to coincide.
    merge_base: &'a str,
    /// The changed files the reviewer's trigger matched, sorted.
    files: &'a [&'a str],
    /// `git diff <merge-base> -- <files>` over the working tree: the tracked
    /// slice of the changeset this reviewer's verdict judged.
    diff: &'a str,
    /// The same scoped diff taken against the base branch's *current* tip
    /// (`git diff <base> -- <files>`), which is the diff the reviewer's prompt
    /// actually instructs it to review. The two coincide until the base
    /// advances; if the base then moves on a file this trigger covers, the
    /// merge-base diff can stay textually identical while what the reviewer
    /// would see changes, and this field is what invalidates the carry.
    base_diff: &'a str,
    /// The commit messages on `merge_base..HEAD` that touched the matched
    /// files: the trigger-scoped slice of the stated intent the reviewer's
    /// prompt carries. Rewording a commit that touched this reviewer's files
    /// invalidates its carried verdict; rewording one that did not leaves it
    /// carryable. (The prompt's other context, a reviewer's own prior
    /// findings, is deliberately excluded: it changes on every loop iteration
    /// by construction, so keying the digest to it would disable carry
    /// entirely, and a pass is not made unsound by the findings that preceded
    /// it.)
    intent: &'a str,
    /// Untracked matched files, which `git diff` cannot see, as
    /// `(path, descriptor)` pairs sorted by path, where the descriptor is
    /// [`untracked_descriptor`]'s encoding of kind, mode, and content.
    untracked: &'a [(String, String)],
}

/// The digest's encoding of one untracked file: its kind and git-relevant
/// mode, not just its bytes, so a chmod or a retargeted symlink invalidates a
/// carried verdict the way it would change a git changeset.
///
/// - A symlink hashes its *target path* (`symlink:<sha256 of target>`), never
///   the content behind it, mirroring how git records symlinks.
/// - A regular file hashes its content, tagged with the executable bit on
///   Unix (`file:x:<sha256>` vs `file:-:<sha256>`), the one mode bit git
///   tracks. Windows has no such bit, so files there always encode as `-`.
fn untracked_descriptor(repo_root: &Path, path: &str) -> std::io::Result<String> {
    let full = repo_root.join(path);
    let meta = std::fs::symlink_metadata(&full)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&full)?;
        let target_bytes = target.to_string_lossy();
        return Ok(format!(
            "symlink:{}",
            hex::encode(Sha256::digest(target_bytes.as_bytes()))
        ));
    }
    let bytes = std::fs::read(&full)?;
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(format!(
        "file:{}:{}",
        if executable { "x" } else { "-" },
        hex::encode(Sha256::digest(&bytes))
    ))
}

/// Compute the scope digest for one triggered reviewer: a lowercase-hex
/// SHA-256 over the reviewer's effective definition, the merge-base commit,
/// the diffs of the changed files its trigger matched (working tree against
/// both `merge_base` and `base`'s current tip; untracked matched files encoded
/// by kind, mode, and content), and the `merge_base..HEAD` commit messages
/// that touched those files.
///
/// `changed` is the full changed-file set the run routed on
/// ([`git::changed_files`]); the trigger scoping happens here so the digest and
/// routing can never disagree about which files a reviewer's globs cover.
///
/// # Errors
///
/// Returns an error if a trigger glob fails to compile (the router would have
/// rejected it first in practice), a git query fails, or an untracked matched
/// file cannot be read.
pub fn scope_digest(
    repo_root: &Path,
    base: &str,
    merge_base: &str,
    reviewer: &Reviewer,
    changed: &[String],
) -> Result<String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &reviewer.trigger {
        builder.add(Glob::new(pattern).wrap_err_with(|| {
            format!(
                "reviewer '{}' has an invalid trigger glob: {pattern}",
                reviewer.name
            )
        })?);
    }
    let globs = builder
        .build()
        .wrap_err_with(|| format!("building trigger matcher for reviewer '{}'", reviewer.name))?;

    let mut files: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| globs.is_match(path))
        .collect();
    files.sort_unstable();

    let diff = git::scoped_diff(repo_root, merge_base, &files).wrap_err_with(|| {
        format!(
            "diffing the files reviewer '{}' is scoped to",
            reviewer.name
        )
    })?;

    let base_diff = git::scoped_diff(repo_root, base, &files).wrap_err_with(|| {
        format!(
            "diffing the files reviewer '{}' is scoped to against the base tip",
            reviewer.name
        )
    })?;

    let intent =
        git::scoped_commit_messages(repo_root, merge_base, &files).wrap_err_with(|| {
            format!(
                "gathering the scoped commit messages reviewer '{}' is keyed to",
                reviewer.name
            )
        })?;

    let untracked_set: BTreeSet<String> = git::untracked_files(repo_root)
        .wrap_err("listing untracked files for the scope digest")?
        .into_iter()
        .collect();
    let mut untracked: Vec<(String, String)> = Vec::new();
    for path in &files {
        if untracked_set.contains(*path) {
            let descriptor = untracked_descriptor(repo_root, path)
                .wrap_err_with(|| format!("reading untracked file {path} for the scope digest"))?;
            untracked.push(((*path).to_string(), descriptor));
        }
    }

    let input = DigestInput {
        reviewer,
        merge_base,
        files: &files,
        diff: &diff,
        base_diff: &base_diff,
        intent: &intent,
        untracked: &untracked,
    };
    let bytes =
        serde_json::to_vec(&input).wrap_err("serializing the scope digest's canonical input")?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Decide which of `candidates` carry their verdict forward from the most
/// recent prior run of `branch`, keyed by reviewer name.
///
/// Each candidate pairs a triggered reviewer with its current scope digest. A
/// candidate carries when all of the following hold; anything short of that
/// executes fresh, silently (carry is an optimization, never an error):
///
/// - the prior run resolved this reviewer to a **pass** whose recorded
///   `scope_digest` equals the current one (a carried prior pass qualifies too:
///   digest equality is over content, so the chain stays anchored);
/// - the reviewer does not set `attestation: never`;
/// - when the reviewer is in `repo_reviewers`, the prior run has a seal that
///   verifies under `secret` over the prior run's own sealed events, covers
///   this reviewer, and records no active test seam. A dirty prior seal is
///   acceptable: the digest binds the actual working-tree content the reviewer
///   judged, so digest equality carries its own proof that the content is the
///   same one now under review.
///
/// A user-level reviewer (absent from `repo_reviewers`) skips the seal
/// requirement: its verdict is never sealed and never gates anyone else's PR.
#[must_use]
pub fn plan(
    layout: &Layout,
    branch: &str,
    candidates: &[(&Reviewer, String)],
    repo_reviewers: &BTreeSet<String>,
    secret: &[u8],
) -> BTreeMap<String, Carried> {
    let mut carried = BTreeMap::new();
    if candidates.is_empty() {
        return carried;
    }

    let Some((prior_run, events)) = prior_run_on_branch(layout, branch) else {
        return carried;
    };

    // Verify the prior seal once, lazily reusable for every repo candidate.
    // `None` means "no verified seal": every repo reviewer then executes fresh.
    let sealed_names = verified_seal_reviewers(layout, &prior_run, &events, secret);

    for (reviewer, digest) in candidates {
        if reviewer.attestation == Some(AttestationPolicy::Never) {
            continue;
        }
        let Some(event) = resolved_pass_with_digest(&events, &reviewer.name, digest) else {
            continue;
        };
        if repo_reviewers.contains(&reviewer.name)
            && !sealed_names
                .as_ref()
                .is_some_and(|names| names.contains(reviewer.name.as_str()))
        {
            continue;
        }
        carried.insert(
            reviewer.name.clone(),
            Carried {
                reviewer: (*reviewer).clone(),
                event: event.clone(),
            },
        );
    }
    carried
}

/// The most recent persisted run on `branch`, with its events. Mirrors
/// [`store::prior_findings`]'s recall: the latest run on the branch, including
/// a same-id run from an earlier invocation at the same HEAD.
fn prior_run_on_branch(
    layout: &Layout,
    branch: &str,
) -> Option<(crate::event::RunId, Vec<RunEvent>)> {
    let runs = store::list_runs(layout).ok()?;
    let prior = runs
        .into_iter()
        .find(|run| run.branch.as_deref() == Some(branch))?;
    let events = store::read_run(layout, &prior.run).ok()?;
    Some((prior.run, events))
}

/// The prior run's seal-covered reviewer names, when the seal exists, records
/// no test seam, and verifies over the run's own persisted resolved events.
/// `None` when any of that fails, which disqualifies carry for every
/// repository reviewer.
fn verified_seal_reviewers(
    layout: &Layout,
    run: &crate::event::RunId,
    events: &[RunEvent],
    secret: &[u8],
) -> Option<BTreeSet<String>> {
    let seal = store::read_seal(layout, run).ok().flatten()?;
    if seal.seams {
        return None;
    }
    let names: BTreeSet<&str> = seal.reviewers.iter().map(String::as_str).collect();
    let mut sealed: Vec<(&str, &RunEvent)> = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ReviewerResolved { reviewer, .. } if names.contains(reviewer.as_str()) => {
                Some((reviewer.as_str(), event))
            }
            _ => None,
        })
        .collect();
    sealed.sort_by_key(|(name, _)| *name);
    let values: Vec<serde_json::Value> = sealed
        .iter()
        .map(|(_, event)| serde_json::to_value(event))
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if !crate::seal::verify(secret, &seal, &values) {
        return None;
    }
    Some(seal.reviewers.into_iter().collect())
}

/// The prior run's `reviewer.resolved` event for `name`, when it is a pass
/// whose recorded scope digest equals `digest`.
fn resolved_pass_with_digest<'a>(
    events: &'a [RunEvent],
    name: &str,
    digest: &str,
) -> Option<&'a RunEvent> {
    events.iter().find(|event| {
        matches!(
            event,
            RunEvent::ReviewerResolved {
                reviewer,
                verdict: Decision::Pass,
                scope_digest: Some(prior),
                ..
            } if reviewer == name && prior == digest
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Gates, ReviewerRef, RunId};
    use crate::reviewer::{Capabilities, Mode};
    use crate::verdict::Money;

    fn reviewer(name: &str, triggers: &[&str]) -> Reviewer {
        Reviewer {
            name: name.into(),
            trigger: triggers.iter().map(|s| (*s).to_string()).collect(),
            mode: Mode::Gate,
            backend: crate::reviewer::Backend::ClaudeCode,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Capabilities::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "p".into(),
        }
    }

    /// Run `git` with a deterministic identity in `dir`.
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
    }

    /// A repo with a committed base (`base` branch) and one changed file on top.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("docs/guide.md"), "guide\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "base"]);
        git(dir, &["branch", "base"]);
        tmp
    }

    #[test]
    fn digest_is_stable_across_calls_and_scoped_to_the_trigger() {
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* edit */ }\n").unwrap();
        std::fs::write(dir.join("docs/guide.md"), "guide, edited\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();

        let src = reviewer("src-only", &["src/**"]);
        let first = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        let second = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_eq!(first, second, "the digest must be reproducible");

        // Editing a file *outside* the trigger leaves the digest unchanged...
        std::fs::write(dir.join("docs/guide.md"), "guide, edited again\n").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();
        let after_docs_edit = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_eq!(first, after_docs_edit);

        // ...while editing a matched file changes it.
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* different */ }\n").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();
        let after_src_edit = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_ne!(first, after_src_edit);
    }

    #[test]
    fn digest_covers_untracked_matched_files() {
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/new.rs"), "fn new_one() {}\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();

        let src = reviewer("src-only", &["src/**"]);
        let first = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();

        // The file is untracked, so `git diff` cannot see it; only the content
        // hash in the digest can. Changing its content must change the digest.
        std::fs::write(dir.join("src/new.rs"), "fn new_one() { /* edited */ }\n").unwrap();
        let second = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn digest_changes_when_the_reviewer_definition_changes() {
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* edit */ }\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();

        let original = reviewer("src-only", &["src/**"]);
        let mut reworded = original.clone();
        reworded.prompt = "an entirely different concern".into();

        let a = scope_digest(dir, "base", &merge_base, &original, &changed).unwrap();
        let b = scope_digest(dir, "base", &merge_base, &reworded, &changed).unwrap();
        assert_ne!(a, b, "editing the reviewer must invalidate its carry");
    }

    /// A prior run on `branch` with one resolved pass for `name` carrying
    /// `digest`, persisted (and optionally sealed) into `layout`.
    fn persist_prior_run(
        layout: &Layout,
        branch: &str,
        name: &str,
        digest: &str,
        seal_it: Option<&[u8]>,
        dirty_seal: bool,
    ) -> RunId {
        let run = RunId("r-prior".into());
        let resolved = RunEvent::ReviewerResolved {
            run: run.clone(),
            reviewer: name.into(),
            verdict: Decision::Pass,
            summary: "clean".into(),
            findings: vec![],
            usage: None,
            duration_ms: 5,
            has_transcript: false,
            replayed: false,
            carried: false,
            scope_digest: Some(digest.into()),
        };
        let events = vec![
            RunEvent::RunStarted {
                run: run.clone(),
                branch: branch.into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![ReviewerRef {
                    name: name.into(),
                    mode: Mode::Gate,
                }],
                partial: false,
            },
            resolved.clone(),
            RunEvent::RunCompleted {
                run: run.clone(),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 1,
                    passed: 1,
                    blocked: 0,
                },
                duration_ms: 5,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
                partial: false,
            },
        ];
        store::write_run(layout, &run, &events).unwrap();
        if let Some(secret) = seal_it {
            let values = vec![serde_json::to_value(&resolved).unwrap()];
            let seal = crate::seal::seal(
                secret,
                "0.1.0",
                &crate::seal::SealBindings {
                    head_tree: "h".into(),
                    base_tree: "b".into(),
                    patch_id: "p".into(),
                    config_hash: "c".into(),
                    repo_reviewers: [name.to_string()].into_iter().collect(),
                },
                false,
                dirty_seal,
                vec![name.to_string()],
                &values,
            );
            store::write_seal(layout, &run, &seal).unwrap();
        }
        run
    }

    const SECRET: &[u8] = b"carry-test-secret";

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(tmp.path().to_path_buf());
        (tmp, layout)
    }

    #[test]
    fn a_repo_reviewer_carries_only_from_a_verified_seal() {
        let (_tmp, layout) = layout();
        persist_prior_run(&layout, "feat", "g1", "digest-1", Some(SECRET), false);
        let g1 = reviewer("g1", &["src/**"]);
        let repo_set: BTreeSet<String> = ["g1".to_string()].into_iter().collect();

        let carried = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &repo_set,
            SECRET,
        );
        assert!(carried.contains_key("g1"), "verified seal carries");

        // The same store checked under a different secret (a different build)
        // must refuse: the chain of custody breaks at the seal.
        let refused = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &repo_set,
            b"a-different-secret",
        );
        assert!(refused.is_empty(), "an unverifiable seal must not carry");
    }

    #[test]
    fn a_dirty_but_verified_seal_still_carries_a_repo_reviewer() {
        // A dirty seal is acceptable for carry (unlike for attestation): the
        // digest binds the actual working-tree content the reviewer judged, so
        // digest equality is itself the proof the content is unchanged.
        let (_tmp, layout) = layout();
        persist_prior_run(&layout, "feat", "g1", "digest-1", Some(SECRET), true);
        let g1 = reviewer("g1", &["src/**"]);
        let repo_set: BTreeSet<String> = ["g1".to_string()].into_iter().collect();

        let carried = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &repo_set,
            SECRET,
        );
        assert!(carried.contains_key("g1"), "a dirty verified seal carries");
    }

    #[test]
    fn digest_changes_when_the_base_tip_advances_on_a_matched_file() {
        let tmp = repo();
        let dir = tmp.path();
        // An "advanced" base: the same starting point plus a commit touching
        // the matched file, standing in for `main` moving under the branch.
        git(dir, &["checkout", "-b", "advanced", "base"]);
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* upstream */ }\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "upstream change"]);
        git(dir, &["checkout", "main"]);

        std::fs::write(dir.join("src/a.rs"), "fn a() { /* local edit */ }\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        assert_eq!(
            merge_base,
            git::merge_base(dir, "advanced").unwrap(),
            "both bases share the starting point, so only the tip differs"
        );
        let changed = git::changed_files(dir, "base").unwrap();
        let src = reviewer("src-only", &["src/**"]);

        let against_base = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        let against_advanced = scope_digest(dir, "advanced", &merge_base, &src, &changed).unwrap();
        assert_ne!(
            against_base, against_advanced,
            "a base that advances on a covered file must invalidate the carry"
        );
    }

    #[test]
    fn digest_changes_when_only_the_merge_base_moves() {
        // Two comparison points with byte-identical trees produce identical
        // scoped diffs; the digest must still differ, because a new merge base
        // is a new starting point for the changeset even when the text
        // coincides.
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* edit */ }\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        // An empty commit on top of `base`: same tree, different commit id.
        let other = String::from_utf8(
            std::process::Command::new("git")
                .args(["-c", "user.email=t@bastion.dev", "-c", "user.name=T"])
                .args(["commit-tree", &format!("{merge_base}^{{tree}}")])
                .args(["-p", &merge_base, "-m", "empty"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert!(!other.is_empty(), "commit-tree must produce a commit id");
        let changed = git::changed_files(dir, "base").unwrap();
        let src = reviewer("src-only", &["src/**"]);

        let a = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        let b = scope_digest(dir, "base", &other, &src, &changed).unwrap();
        assert_ne!(a, b, "a moved merge base must invalidate the carry");
    }

    #[test]
    fn digest_changes_when_a_scoped_commit_message_is_reworded() {
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/a.rs"), "fn a() { /* edit */ }\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "src: first wording"]);
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();
        let src = reviewer("src-only", &["src/**"]);
        let first = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();

        // Rewording the commit that touched the matched files changes the
        // stated intent the reviewer's prompt carries, so the digest changes.
        git(dir, &["commit", "--amend", "-m", "src: entirely reworded"]);
        let reworded = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_ne!(first, reworded);

        // A later commit that touches only unmatched files leaves this
        // reviewer's scoped intent, and so its digest, unchanged.
        std::fs::write(dir.join("docs/guide.md"), "guide, edited\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "docs: unrelated note"]);
        let changed = git::changed_files(dir, "base").unwrap();
        let after_docs = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_eq!(reworded, after_docs);
    }

    #[cfg(unix)]
    #[test]
    fn digest_changes_when_an_untracked_files_exec_bit_flips() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = repo();
        let dir = tmp.path();
        std::fs::write(dir.join("src/run.rs"), "#!/bin/sh\n").unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();
        let src = reviewer("src-only", &["src/**"]);
        let plain = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();

        let path = dir.join("src/run.rs");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&path, perms).unwrap();
        let executable = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_ne!(plain, executable, "a chmod must invalidate the carry");
    }

    #[cfg(unix)]
    #[test]
    fn digest_hashes_an_untracked_symlinks_target_not_the_content_behind_it() {
        let tmp = repo();
        let dir = tmp.path();
        std::os::unix::fs::symlink("a.rs", dir.join("src/link.rs")).unwrap();
        let merge_base = git::merge_base(dir, "base").unwrap();
        let changed = git::changed_files(dir, "base").unwrap();
        let src = reviewer("src-only", &["src/**"]);
        let first = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();

        // Retargeting the symlink changes the digest, the same way git would
        // record a different blob for it.
        std::fs::remove_file(dir.join("src/link.rs")).unwrap();
        std::os::unix::fs::symlink("../docs/guide.md", dir.join("src/link.rs")).unwrap();
        let retargeted = scope_digest(dir, "base", &merge_base, &src, &changed).unwrap();
        assert_ne!(first, retargeted);
    }

    #[test]
    fn an_unsealed_prior_run_does_not_carry_a_repo_reviewer_but_carries_a_user_one() {
        let (_tmp, layout) = layout();
        persist_prior_run(&layout, "feat", "g1", "digest-1", None, false);
        let g1 = reviewer("g1", &["src/**"]);

        let repo_set: BTreeSet<String> = ["g1".to_string()].into_iter().collect();
        let refused = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &repo_set,
            SECRET,
        );
        assert!(refused.is_empty(), "repo reviewer needs a verified seal");

        // The same reviewer treated as user-level (outside the repo set)
        // carries on the digest alone: it is never sealed and never gates
        // anyone else's PR.
        let carried = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &BTreeSet::new(),
            SECRET,
        );
        assert!(carried.contains_key("g1"));
    }

    #[test]
    fn a_changed_digest_a_block_or_a_never_policy_executes_fresh() {
        let (_tmp, layout) = layout();
        persist_prior_run(&layout, "feat", "g1", "digest-1", Some(SECRET), false);
        let g1 = reviewer("g1", &["src/**"]);
        let repo_set: BTreeSet<String> = ["g1".to_string()].into_iter().collect();

        // Different digest: the scoped content changed, so no carry.
        assert!(
            plan(
                &layout,
                "feat",
                &[(&g1, "digest-2".to_string())],
                &repo_set,
                SECRET,
            )
            .is_empty()
        );

        // `attestation: never` opts out even on an identical digest.
        let mut fresh_always = g1.clone();
        fresh_always.attestation = Some(AttestationPolicy::Never);
        assert!(
            plan(
                &layout,
                "feat",
                &[(&fresh_always, "digest-1".to_string())],
                &repo_set,
                SECRET,
            )
            .is_empty()
        );

        // A different branch's run is never consulted.
        assert!(
            plan(
                &layout,
                "other-branch",
                &[(&g1, "digest-1".to_string())],
                &repo_set,
                SECRET,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_prior_block_is_never_carried() {
        let (_tmp, layout) = layout();
        let run = RunId("r-blocked".into());
        let resolved = RunEvent::ReviewerResolved {
            run: run.clone(),
            reviewer: "g1".into(),
            verdict: Decision::Block,
            summary: "still broken".into(),
            findings: vec![],
            usage: None,
            duration_ms: 5,
            has_transcript: false,
            replayed: false,
            carried: false,
            scope_digest: Some("digest-1".into()),
        };
        let events = vec![
            RunEvent::RunStarted {
                run: run.clone(),
                branch: "feat".into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![ReviewerRef {
                    name: "g1".into(),
                    mode: Mode::Gate,
                }],
                partial: false,
            },
            resolved,
            RunEvent::RunCompleted {
                run: run.clone(),
                verdict: Decision::Block,
                gates: Gates {
                    total: 1,
                    passed: 0,
                    blocked: 1,
                },
                duration_ms: 5,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
                partial: false,
            },
        ];
        let (_tmp2, _) = (tempfile::tempdir().unwrap(), ());
        store::write_run(&layout, &run, &events).unwrap();

        let g1 = reviewer("g1", &["src/**"]);
        let carried = plan(
            &layout,
            "feat",
            &[(&g1, "digest-1".to_string())],
            &BTreeSet::new(),
            SECRET,
        );
        assert!(
            carried.is_empty(),
            "a block re-runs even when its scoped diff is unchanged"
        );
    }

    #[test]
    fn a_seam_tainted_seal_does_not_carry() {
        let (_tmp, layout) = layout();
        let run = persist_prior_run(&layout, "feat", "g1", "digest-1", Some(SECRET), false);
        // Re-seal with `seams: true` under the same secret, so only the seam
        // flag disqualifies.
        let events = store::read_run(&layout, &run).unwrap();
        let values: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let seal = crate::seal::seal(
            SECRET,
            "0.1.0",
            &crate::seal::SealBindings {
                head_tree: "h".into(),
                base_tree: "b".into(),
                patch_id: "p".into(),
                config_hash: "c".into(),
                repo_reviewers: ["g1".to_string()].into_iter().collect(),
            },
            true,
            false,
            vec!["g1".to_string()],
            &values,
        );
        store::write_seal(&layout, &run, &seal).unwrap();

        let g1 = reviewer("g1", &["src/**"]);
        let repo_set: BTreeSet<String> = ["g1".to_string()].into_iter().collect();
        assert!(
            plan(
                &layout,
                "feat",
                &[(&g1, "digest-1".to_string())],
                &repo_set,
                SECRET,
            )
            .is_empty()
        );
    }
}
