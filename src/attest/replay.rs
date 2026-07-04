//! Verification and replay in CI: turning a signed note found on HEAD into a
//! decision about which routed reviewers replay from it, per
//! `docs/developer-guide/attestation.md` ("Verification and replay in CI").
//!
//! [`plan`] is the single boundary between "an attestation note as raw git-note
//! text" and "a plan the runner can execute": every signature, seal, and binding
//! check happens here, and by the time a [`ReplayPlan`] exists its `replay` map
//! holds already-validated, already-typed [`RunEvent`]s. The runner
//! (`crate::runner::resolve_replayed`) consumes that type directly and never
//! re-parses or re-checks the underlying JSON.

use std::collections::BTreeMap;
use std::path::Path;

use color_eyre::eyre::{Context, Result};

use crate::event::RunEvent;
use crate::git;

use super::bundle::{Bundle, split_envelope};
use super::sign::verify_signature;

/// A CI run's decision to replay one or more reviewers from a verified
/// attestation, and to execute the rest fresh.
///
/// Built by [`plan`] once every binding, signature, and seal check has passed.
#[derive(Debug, Clone)]
pub struct ReplayPlan {
    /// The verified bundle this plan replays from.
    pub bundle: Bundle,
    /// The parsed, checked `reviewer.resolved` event for each reviewer that
    /// will be replayed, keyed by reviewer name. A subset of `bundle.events`:
    /// only the names that are both routed by CI's own diff and not opted out
    /// via [`AttestationPolicy::Never`](crate::reviewer::AttestationPolicy::Never).
    ///
    /// Typed as [`RunEvent`], not [`serde_json::Value`]: `plan` already parsed
    /// each event and confirmed it is a `reviewer.resolved` event bound to its
    /// own map key (see the key-to-event binding check below), so a consumer
    /// (`crate::runner::resolve_replayed`) receives the already-validated type
    /// and has no unchecked JSON left to reparse.
    pub replay: BTreeMap<String, RunEvent>,
    /// Names of routed reviewers that must execute fresh even though the
    /// bundle verified: a reviewer CI routed that the bundle does not cover, or
    /// one that opted out of replay. Coverage mismatch degrades, it does not
    /// invalidate the plan.
    pub executed_fresh: Vec<String>,
}

/// The outcome of attempting to verify and plan a replay in CI.
#[derive(Debug, Clone)]
pub enum AttestationOutcome {
    /// The note verified and at least the binding checks passed; `plan` says which
    /// reviewers replay and which still execute. Boxed: [`ReplayPlan`] carries a
    /// full [`Bundle`] (including every replayed reviewer's event), which would
    /// otherwise make every [`AttestationOutcome`] pay for the largest variant's
    /// size even on the common `Fallback` path.
    Replay(Box<ReplayPlan>),
    /// An attestation was present but not honored; every routed reviewer executes
    /// fresh. `reason` names exactly what failed, in plain English, so the report
    /// can tell the author rather than leaving them guessing. This is the surfaced
    /// case: a note was offered and rejected, which the author should see.
    Fallback {
        /// Why the attestation was not honored.
        reason: String,
    },
    /// No attestation was offered at all: HEAD carried no note. Every routed
    /// reviewer executes fresh, exactly as when attestation is disabled. Distinct
    /// from [`Fallback`](AttestationOutcome::Fallback) because there is nothing to
    /// tell the author: a missing note is the ordinary case for most commits, not
    /// a rejection, so no event is recorded and no report line is drawn. Only an
    /// attestation that was *offered and refused* warrants surfacing.
    NotAttested,
}

/// The re-derived repository state CI's own checkout produces, to compare
/// against a bundle's recorded [`crate::seal::Seal`] bindings.
///
/// Named separately from [`crate::seal::SealBindings`] because it is missing the
/// [`crate::seal::SealBindings::repo_reviewers`] field: coverage is compared by
/// the routed-reviewer set, not folded into the binding check, so a reviewer
/// added or removed since the local run degrades to fresh execution for that
/// reviewer rather than invalidating the whole bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiBindings {
    /// The git tree hash of CI's checked-out HEAD.
    pub head_tree: String,
    /// The git tree hash of CI's own re-derived merge base.
    pub base_tree: String,
    /// The `git patch-id --stable` of CI's own `merge_base..HEAD` diff.
    pub patch_id: String,
    /// The effective repository-only config hash CI computed from its own
    /// checkout.
    pub config_hash: String,
}

/// Re-derive the [`CiBindings`] CI's own checkout produces, for comparison
/// against a bundle's recorded seal.
///
/// # Errors
///
/// Returns an error if the merge base, either tree, or the patch id cannot be
/// resolved from `root`.
pub fn derive_ci_bindings(root: &Path, base: &str, config_hash: &str) -> Result<CiBindings> {
    let merge_base_commit =
        git::merge_base(root, base).wrap_err("resolving CI's own merge base")?;
    let head_tree = git::tree_hash(root, "HEAD").wrap_err("resolving CI's HEAD tree")?;
    let base_tree =
        git::tree_hash(root, &merge_base_commit).wrap_err("resolving CI's merge-base tree")?;
    let patch_id =
        git::patch_id(root, &merge_base_commit).wrap_err("recomputing CI's own diff patch id")?;
    Ok(CiBindings {
        head_tree,
        base_tree,
        patch_id,
        config_hash: config_hash.to_string(),
    })
}

/// Verify a note found on `rev` and, on success, plan which routed reviewers
/// replay.
///
/// `note` is the raw text `git::note_show` returned (bundle JSON plus armored
/// signature; see [`split_envelope`]). `author` and `keys` are the PR author's
/// GitHub login and their registered SSH signing keys (fetched independently so
/// this function stays testable without the network). `ci` is CI's own
/// re-derived bindings ([`derive_ci_bindings`]). `routed` is the reviewers CI's
/// own diff matched, name to definition, so the per-reviewer replay/fresh split
/// can consult each one's
/// [`AttestationPolicy`](crate::reviewer::AttestationPolicy). `secret` is the
/// sealing secret to verify the bundle's seal against
/// (`seal::embedded_secret()` in production).
///
/// Every failure path returns `AttestationOutcome::Fallback` with a reason
/// naming exactly what did not check out, per
/// `docs/developer-guide/attestation.md`'s fail-closed list: an unparseable
/// note, a signature that does not verify against the author's registered
/// keys, a seal that does not verify (worded as a version mismatch when
/// `bundle.version` differs from this binary's), a seal with test seams or a
/// dirty working tree recorded, or a binding mismatch (named: head tree, base
/// tree, patch id, or config hash).
#[must_use]
pub fn plan(
    note: &str,
    author: &str,
    keys: &[String],
    ci: &CiBindings,
    routed: &std::collections::BTreeMap<&str, &crate::reviewer::Reviewer>,
    secret: &[u8],
) -> AttestationOutcome {
    let (bundle_json, signature) = match split_envelope(note) {
        Ok(parts) => parts,
        Err(err) => return fallback(format!("the attestation note is unreadable: {err:#}")),
    };

    let bundle = match Bundle::from_json(bundle_json) {
        Ok(bundle) => bundle,
        Err(err) => return fallback(format!("the attestation bundle is unreadable: {err:#}")),
    };

    let verified = match verify_signature(bundle_json.as_bytes(), signature, author, keys) {
        Ok(verified) => verified,
        Err(err) => {
            return fallback(format!(
                "the attestation signature could not be checked: {err:#}"
            ));
        }
    };
    if !verified {
        return fallback(format!(
            "the attestation signature does not verify against {author}'s registered SSH signing keys"
        ));
    }

    if bundle.seal.seams {
        return fallback(
            "the attested run used a test seam (a backend or container-engine override) and cannot be replayed"
                .to_string(),
        );
    }
    if bundle.seal.dirty {
        return fallback(
            "the attested run reviewed a dirty working tree (uncommitted or untracked changes) and cannot be replayed"
                .to_string(),
        );
    }

    let mut events_sorted: Vec<(&String, &serde_json::Value)> = bundle.events.iter().collect();
    events_sorted.sort_by_key(|(name, _)| (*name).clone());
    let event_values: Vec<serde_json::Value> =
        events_sorted.into_iter().map(|(_, v)| v.clone()).collect();
    if !crate::seal::verify(secret, &bundle.seal, &event_values) {
        if bundle.version != crate::version::VERSION {
            return fallback(format!(
                "attested by v{}, this CI runs v{}; the seal does not verify across releases",
                bundle.version.trim_start_matches('v'),
                crate::version::VERSION.trim_start_matches('v'),
            ));
        }
        return fallback(
            "the attestation's seal does not verify: the bundle was tampered with, or the run store it was built from was edited after the fact"
                .to_string(),
        );
    }

    // Each content binding ties the attested seal to CI's own view of the same
    // surface; the first mismatch means it moved between the local review and CI, so
    // the run cannot be replayed as sealed. Formatted lazily, so only the failing
    // binding builds its message.
    let bindings = [
        (
            &bundle.seal.head_tree,
            &ci.head_tree,
            "head tree does not match CI's checkout",
            "",
        ),
        (
            &bundle.seal.base_tree,
            &ci.base_tree,
            "base tree does not match CI's merge base",
            "; the base may have moved since the local review",
        ),
        (
            &bundle.seal.patch_id,
            &ci.patch_id,
            "patch id does not match CI's diff",
            "",
        ),
        (
            &bundle.seal.config_hash,
            &ci.config_hash,
            "reviewer config does not match CI's",
            "; the registry has changed since the local review",
        ),
    ];
    for (attested, actual, what, note) in bindings {
        if attested != actual {
            return fallback(format!(
                "the attested {what} (attested {attested}, CI has {actual}){note}"
            ));
        }
    }

    // Every binding matched: decide, per routed reviewer, whether it replays.
    // A routed reviewer covered by the bundle and not opted out replays;
    // everything else (uncovered, or `attestation: never`) executes fresh.
    // Coverage mismatch degrades rather than invalidating the whole plan.
    //
    // The seal MAC covers `bundle.events`' *values* (sorted, see above) but never
    // its map keys, so a signed-but-malformed bundle could file reviewer A's
    // sealed event under reviewer B's key and so skip executing B entirely. Bind
    // key to value here: require the event under `name` to actually be that
    // reviewer's own `reviewer.resolved` event before trusting it to replay. This
    // is also where the event is parsed into its typed [`RunEvent`] form once and
    // for all: `ReplayPlan::replay` carries that type onward, so nothing
    // downstream re-parses or re-validates this JSON.
    let mut replay = BTreeMap::new();
    let mut executed_fresh = Vec::new();
    for (name, reviewer) in routed {
        let never_replay = matches!(
            reviewer.attestation,
            Some(crate::reviewer::AttestationPolicy::Never)
        );
        // A `never` reviewer, or one the bundle does not cover, executes fresh
        // rather than replaying; only a covered, replay-eligible reviewer is parsed.
        let Some(event) = bundle.events.get(*name).filter(|_| !never_replay) else {
            executed_fresh.push((*name).to_string());
            continue;
        };
        match serde_json::from_value::<RunEvent>(event.clone()) {
            Ok(ref parsed @ RunEvent::ReviewerResolved { ref reviewer, .. })
                if reviewer == name =>
            {
                replay.insert((*name).to_string(), parsed.clone());
            }
            _ => {
                return fallback(format!(
                    "the attestation bundle carries a malformed or mismatched event under \
                     reviewer '{name}' (its key does not match the event's own reviewer \
                     field, or the event is not a reviewer.resolved event)"
                ));
            }
        }
    }

    AttestationOutcome::Replay(Box::new(ReplayPlan {
        bundle,
        replay,
        executed_fresh,
    }))
}

/// Build a [`AttestationOutcome::Fallback`] from a reason.
fn fallback(reason: String) -> AttestationOutcome {
    AttestationOutcome::Fallback { reason }
}

/// Look up the attestation note for a review, trying `rev` first and falling
/// back to `fallback_rev` when `rev` carries none.
///
/// CI's checkout can be a merge commit while the attestation note hangs off the
/// PR's own head commit (the commit the author actually attested), so a caller
/// with both available (typically `HEAD` and the PR's head SHA) tries the more
/// specific one first and falls back rather than treating an absent note on the
/// merge commit as decisive.
///
/// # Errors
///
/// Returns an error if a lookup fails for a reason other than the note being
/// absent (see [`git::note_show`]).
pub fn note_for_review(
    root: &Path,
    rev: &str,
    fallback_rev: Option<&str>,
) -> Result<Option<String>> {
    if let Some(note) = git::note_show(root, git::NOTES_REF, rev)? {
        return Ok(Some(note));
    }
    match fallback_rev {
        Some(fallback) if fallback != rev => git::note_show(root, git::NOTES_REF, fallback),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attest::attest;
    use crate::attest::bundle::{Bundle as BundleT, envelope};
    use crate::attest::sign::sign;
    use crate::event::{Gates, ReviewerRef, RunId};
    use crate::paths::Layout;
    use crate::reviewer::Mode;
    use crate::store;
    use crate::verdict::{Decision, Money};
    use std::process::{Command, Stdio};

    /// git config flags that make a temp repo deterministic regardless of the
    /// developer's global git configuration, mirroring `git.rs`'s test fixture.
    const ISOLATE: &[&str] = &[
        "-c",
        "user.email=test@bastion.dev",
        "-c",
        "user.name=Bastion Test",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=main",
    ];

    fn git(cwd: &Path, args: &[&str]) {
        let full: Vec<&str> = ISOLATE
            .iter()
            .copied()
            .chain(args.iter().copied())
            .collect();
        let output = Command::new("git")
            .args(&full)
            .current_dir(cwd)
            .output()
            .expect("git is installed");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args.first() == Some(&"init") {
            git(cwd, &["config", "user.email", "grace@bastion.dev"]);
            git(cwd, &["config", "user.name", "Grace Hopper"]);
        }
    }

    fn tool_available(tool: &str) -> bool {
        Command::new(tool)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    fn ssh_keygen_available() -> bool {
        tool_available("ssh-keygen")
    }

    fn git_available() -> bool {
        tool_available("git")
    }

    fn generate_keypair(dir: &Path) -> (std::path::PathBuf, String) {
        let key_path = dir.join("id");
        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                &key_path.to_string_lossy(),
                "-C",
                "test@bastion.dev",
            ])
            .output()
            .expect("ssh-keygen is installed");
        assert!(
            output.status.success(),
            "ssh-keygen keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pub_text =
            std::fs::read_to_string(key_path.with_extension("pub")).expect("public key written");
        (key_path, pub_text.lines().next().unwrap_or("").to_string())
    }

    /// A throwaway repo with one base commit (branched as `base`) and one head
    /// commit on top, plus a private data-dir [`Layout`] with a plausible sealed
    /// run fabricated in it, ready for [`attest`].
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: std::path::PathBuf,
        layout: Layout,
        run_id: RunId,
        secret: &'static [u8],
    }

    fn build_fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["branch", "base"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git(&repo, &["commit", "-am", "feature work"]);

        let layout = Layout::with_root(tmp.path().join("data"));
        let run_id = RunId("r-test".into());

        // A real `.bastion.yaml` on disk: `attest` re-derives the config hash by
        // discovering it from the repository root, the same way `bastion review`
        // did when the run was sealed, so the fixture needs the file present, not
        // just an in-memory `Config`.
        let registry_yaml = "reviewers:\n  - name: r1\n    trigger: [\"**\"]\n    mode: gate\n    prompt: p\n  - name: r2\n    trigger: [\"**\"]\n    mode: gate\n    prompt: p\n";
        std::fs::write(repo.join(".bastion.yaml"), registry_yaml).unwrap();
        git(&repo, &["add", ".bastion.yaml"]);
        git(&repo, &["commit", "-m", "add registry"]);

        let merge_base_commit = git::merge_base(&repo, "base").unwrap();
        let head_tree = git::tree_hash(&repo, "HEAD").unwrap();
        let base_tree = git::tree_hash(&repo, &merge_base_commit).unwrap();
        let patch_id = git::patch_id(&repo, &merge_base_commit).unwrap();

        let config_hash = crate::config::Config::from_yaml(registry_yaml)
            .unwrap()
            .effective_hash();

        let resolved_events = vec![
            RunEvent::RunStarted {
                partial: false,
                run: run_id.clone(),
                branch: "feature".into(),
                base: "base".into(),
                changed: 1,
                reviewers: vec![
                    ReviewerRef {
                        name: "r1".into(),
                        mode: Mode::Gate,
                    },
                    ReviewerRef {
                        name: "r2".into(),
                        mode: Mode::Gate,
                    },
                ],
            },
            RunEvent::ReviewerResolved {
                carried: false,
                scope_digest: None,
                run: run_id.clone(),
                reviewer: "r1".into(),
                verdict: Decision::Pass,
                summary: "looks fine".into(),
                findings: vec![],
                usage: None,
                duration_ms: 10,
                has_transcript: false,
                replayed: false,
            },
            RunEvent::ReviewerResolved {
                carried: false,
                scope_digest: None,
                run: run_id.clone(),
                reviewer: "r2".into(),
                verdict: Decision::Pass,
                summary: "also fine".into(),
                findings: vec![],
                usage: None,
                duration_ms: 12,
                has_transcript: false,
                replayed: false,
            },
            RunEvent::RunCompleted {
                partial: false,
                run: run_id.clone(),
                verdict: Decision::Pass,
                gates: Gates {
                    total: 2,
                    passed: 2,
                    blocked: 0,
                },
                duration_ms: 22,
                tokens_in: 0,
                tokens_out: 0,
                cache_read: 0,
                cost_usd: Money::from_cents(0),
            },
        ];
        store::write_run(&layout, &run_id, &resolved_events).unwrap();

        let secret: &'static [u8] = b"fixture-test-secret";
        let sealed_events: Vec<serde_json::Value> = resolved_events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let seal = crate::seal::seal(
            secret,
            "0.1.0",
            &crate::seal::SealBindings {
                head_tree,
                base_tree,
                patch_id,
                config_hash,
                repo_reviewers: ["r1".to_string(), "r2".to_string()].into_iter().collect(),
            },
            false,
            false,
            vec!["r1".into(), "r2".into()],
            &sealed_events,
        );
        store::write_seal(&layout, &run_id, &seal).unwrap();

        Fixture {
            _tmp: tmp,
            repo,
            layout,
            run_id,
            secret,
        }
    }

    /// A minimal gate reviewer definition for planner tests, with an optional
    /// [`crate::reviewer::AttestationPolicy`].
    fn reviewer_def(
        name: &str,
        attestation: Option<crate::reviewer::AttestationPolicy>,
    ) -> crate::reviewer::Reviewer {
        crate::reviewer::Reviewer {
            name: name.into(),
            trigger: vec!["**".into()],
            mode: crate::reviewer::Mode::Gate,
            backend: crate::reviewer::Backend::default(),
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Default::default(),
            inputs: Default::default(),
            attestation,
            prompt: "p".into(),
        }
    }

    /// A fully attested [`build_fixture`] repo: attest it with a fresh keypair
    /// and return everything a planner test needs (the note, the author
    /// principal, the matching keys, and the re-derivable CI bindings).
    struct AttestedFixture {
        fixture: Fixture,
        note: String,
        author: &'static str,
        keys: Vec<String>,
        ci: CiBindings,
    }

    fn build_attested_fixture() -> AttestedFixture {
        let fixture = build_fixture();
        let keys_dir = fixture._tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);

        let mut out = Vec::new();
        attest(
            &fixture.repo,
            &fixture.layout,
            None,
            Some(&key_path),
            fixture.secret,
            &mut out,
        )
        .expect("attest succeeds");

        let note = git::note_show(&fixture.repo, git::NOTES_REF, "HEAD")
            .unwrap()
            .expect("a note was written");

        let seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let ci = CiBindings {
            head_tree: seal.head_tree.clone(),
            base_tree: seal.base_tree.clone(),
            patch_id: seal.patch_id.clone(),
            config_hash: seal.config_hash.clone(),
        };

        AttestedFixture {
            fixture,
            note,
            author: "author@example.com",
            keys: vec![pubkey],
            ci,
        }
    }

    #[test]
    fn plan_replays_routed_reviewers_covered_by_the_bundle() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let r2 = reviewer_def("r2", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1), ("r2", &r2)].into_iter().collect();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        let plan = match outcome {
            AttestationOutcome::Replay(plan) => plan,
            AttestationOutcome::Fallback { reason } => {
                panic!("expected a replay, got a fallback: {reason}")
            }
            AttestationOutcome::NotAttested => panic!("expected a replay, got NotAttested"),
        };
        assert_eq!(plan.replay.len(), 2);
        assert!(plan.replay.contains_key("r1"));
        assert!(plan.replay.contains_key("r2"));
        assert!(plan.executed_fresh.is_empty());
        // The replayed events are already typed `RunEvent`s, not raw JSON.
        assert!(matches!(
            plan.replay.get("r1"),
            Some(RunEvent::ReviewerResolved { .. })
        ));
    }

    #[test]
    fn plan_excludes_an_attestation_never_reviewer_even_when_covered() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        // r2 is covered by the bundle (build_fixture seals both r1 and r2) but opts
        // out of replay.
        let r2 = reviewer_def("r2", Some(crate::reviewer::AttestationPolicy::Never));
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1), ("r2", &r2)].into_iter().collect();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        let plan = match outcome {
            AttestationOutcome::Replay(plan) => plan,
            AttestationOutcome::Fallback { reason } => {
                panic!("expected a replay, got a fallback: {reason}")
            }
            AttestationOutcome::NotAttested => panic!("expected a replay, got NotAttested"),
        };
        assert_eq!(plan.replay.keys().collect::<Vec<_>>(), ["r1"]);
        assert_eq!(plan.executed_fresh, vec!["r2".to_string()]);
    }

    #[test]
    fn plan_executes_a_routed_reviewer_the_bundle_does_not_cover() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        // r3 is routed by CI's diff but was never in the sealed bundle at all.
        let r3 = reviewer_def("r3", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1), ("r3", &r3)].into_iter().collect();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        let plan = match outcome {
            AttestationOutcome::Replay(plan) => plan,
            AttestationOutcome::Fallback { reason } => {
                panic!("expected a replay, got a fallback: {reason}")
            }
            AttestationOutcome::NotAttested => panic!("expected a replay, got NotAttested"),
        };
        assert_eq!(plan.replay.keys().collect::<Vec<_>>(), ["r1"]);
        assert_eq!(plan.executed_fresh, vec!["r3".to_string()]);
    }

    #[test]
    fn plan_falls_back_on_a_bundle_with_a_permuted_event_key() {
        // The seal MAC covers `bundle.events`' *values* (sorted by map key) but
        // never checks that a value's own `reviewer` field matches the key it is
        // filed under. Anyone holding the `bastion` binary can compute a valid
        // seal (the embedded secret is tamper evidence, not a secret, per
        // `docs/developer-guide/attestation.md`), so a signer who legitimately
        // controls their own attestation could file reviewer r1's event under
        // r2's key, sign the result with their own valid key, and still produce
        // a seal that verifies against the (relabeled) event set. Without a
        // key-to-event binding check, CI would then skip executing r2 and trust
        // r1's verdict in its place. `plan` must reject this outright.
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let (bundle_json, _signature) = split_envelope(&att.note).expect("splits cleanly");
        let mut bundle = BundleT::from_json(bundle_json).expect("bundle parses");

        // Swap which key each event is filed under; each event's own embedded
        // `reviewer` field still names its original reviewer.
        let r1_event = bundle.events.remove("r1").expect("r1 was sealed");
        let r2_event = bundle.events.remove("r2").expect("r2 was sealed");
        bundle.events.insert("r1".to_string(), r2_event.clone());
        bundle.events.insert("r2".to_string(), r1_event.clone());

        // Recompute a valid seal over the permuted (but still sorted-by-key)
        // event values: this is exactly what a legitimate signer's own
        // `bastion` binary could do, since the sealing secret ships embedded in
        // every binary.
        let mut sorted: Vec<(&String, &serde_json::Value)> = bundle.events.iter().collect();
        sorted.sort_by_key(|(name, _)| (*name).clone());
        let event_values: Vec<serde_json::Value> =
            sorted.into_iter().map(|(_, v)| v.clone()).collect();
        bundle.seal = crate::seal::seal(
            att.fixture.secret,
            &bundle.seal.version,
            &crate::seal::SealBindings {
                head_tree: bundle.seal.head_tree.clone(),
                base_tree: bundle.seal.base_tree.clone(),
                patch_id: bundle.seal.patch_id.clone(),
                config_hash: bundle.seal.config_hash.clone(),
                repo_reviewers: bundle.seal.reviewers.iter().cloned().collect(),
            },
            bundle.seal.seams,
            bundle.seal.dirty,
            bundle.seal.reviewers.clone(),
            &event_values,
        );

        // Re-sign the permuted bundle with the same key material the fixture
        // already generated, so the signature itself verifies cleanly and the
        // only thing under test is the key-to-event binding.
        let keys_dir = att.fixture._tmp.path().join("keys-permuted");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);
        let tampered_json = bundle.to_json().unwrap();
        let signature = sign(&key_path, tampered_json.as_bytes()).unwrap();
        let tampered_note = envelope(&tampered_json, &signature);

        let r1 = reviewer_def("r1", None);
        let r2 = reviewer_def("r2", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1), ("r2", &r2)].into_iter().collect();

        let outcome = plan(
            &tampered_note,
            "test-principal",
            &[pubkey],
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(
                    reason.contains("malformed") || reason.contains("mismatched"),
                    "expected a reason naming the malformed bundle, got: {reason}"
                );
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("a permuted-key bundle must fall back, not replay")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_missing_note() {
        let ci = CiBindings {
            head_tree: "h".into(),
            base_tree: "b".into(),
            patch_id: "p".into(),
            config_hash: "c".into(),
        };
        let routed = std::collections::BTreeMap::new();
        let outcome = plan("", "author", &[], &ci, &routed, b"secret");
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("unreadable"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_garbage_note() {
        let ci = CiBindings {
            head_tree: "h".into(),
            base_tree: "b".into(),
            patch_id: "p".into(),
            config_hash: "c".into(),
        };
        let routed = std::collections::BTreeMap::new();
        let outcome = plan(
            "not a real note, just some text",
            "author",
            &[],
            &ci,
            &routed,
            b"secret",
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("unreadable"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_when_the_signer_key_is_not_registered() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        // A key that never signed this bundle: the author's "registered keys"
        // do not include the real signer.
        let other_dir = att.fixture._tmp.path().join("other-key");
        std::fs::create_dir_all(&other_dir).unwrap();
        let (_key, other_pubkey) = generate_keypair(&other_dir);

        let outcome = plan(
            &att.note,
            att.author,
            &[other_pubkey],
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("does not verify"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_tampered_bundle() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        // Corrupt a byte in the bundle JSON half of the note, ahead of the
        // signature block, so the signature no longer covers what it signed.
        let (bundle_json, sig) = split_envelope(&att.note).unwrap();
        let tampered_json = bundle_json.replacen("\"r1\"", "\"r9\"", 1);
        let tampered_note = envelope(&tampered_json, sig);

        let outcome = plan(
            &tampered_note,
            att.author,
            &att.keys,
            &att.ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("does not verify"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_seal_mac_mismatch_from_a_different_secret() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        // A different secret than the one the bundle was sealed with, at the
        // same version this binary produces: the MAC does not verify, worded as
        // a tampered/edited run rather than a version mismatch.
        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &att.ci,
            &routed,
            b"a-completely-different-secret",
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(
                    reason.contains("does not verify") && reason.contains("tampered"),
                    "expected a tampered-run wording (same version), got: {reason}"
                );
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_words_a_seal_mismatch_as_a_version_mismatch_when_versions_differ() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let fixture = build_fixture();
        let seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        let sealed_events: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();

        // Hand-build a bundle whose `version` deliberately differs from this
        // binary's `crate::version::VERSION`, so a MAC mismatch has a genuine
        // version discrepancy to attribute the failure to. The seal itself keeps
        // the fixture's secret, so the mismatch is real, not simulated.
        let keys_dir = fixture._tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);
        let bundle = BundleT::new(
            "0.0.1-a-much-older-release".to_string(),
            "2026-07-02T00:00:00Z".to_string(),
            pubkey.clone(),
            seal.clone(),
            sealed_events
                .iter()
                .zip(seal.reviewers.iter())
                .map(|(event, name)| (name.clone(), event.clone()))
                .collect(),
        );
        let bundle_json = bundle.to_json().unwrap();
        let signature = sign(&key_path, bundle_json.as_bytes()).unwrap();
        let note = envelope(&bundle_json, &signature);

        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();
        let ci = CiBindings {
            head_tree: seal.head_tree.clone(),
            base_tree: seal.base_tree.clone(),
            patch_id: seal.patch_id.clone(),
            config_hash: seal.config_hash.clone(),
        };

        // Verify with a *different* secret than the fixture sealed with, so the
        // MAC genuinely fails (a same-secret, cross-version bundle would still
        // verify, since the seal's digest does not include the bundle's plain
        // `version` field at all).
        let outcome = plan(
            &note,
            "author@example.com",
            &[pubkey],
            &ci,
            &routed,
            b"a-completely-different-secret",
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(
                    reason.contains("attested by v0.0.1-a-much-older-release"),
                    "expected version-mismatch wording, got: {reason}"
                );
                assert!(reason.contains(&format!(
                    "this CI runs v{}",
                    crate::version::VERSION.trim_start_matches('v')
                )));
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_seams_true_bundle() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let fixture = build_fixture();
        // Flip `seams` on the persisted seal and re-sign it, like the existing
        // `attest_refuses_a_seal_with_seams_active` fixture perturbation, so the
        // bundle this test attests carries seams: true.
        let seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        let sealed_events: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let seamed_seal = crate::seal::seal(
            fixture.secret,
            &seal.version,
            &crate::seal::SealBindings {
                head_tree: seal.head_tree.clone(),
                base_tree: seal.base_tree.clone(),
                patch_id: seal.patch_id.clone(),
                config_hash: seal.config_hash.clone(),
                repo_reviewers: seal.reviewers.iter().cloned().collect(),
            },
            true,
            false,
            seal.reviewers.clone(),
            &sealed_events,
        );
        store::write_seal(&fixture.layout, &fixture.run_id, &seamed_seal).unwrap();

        // `attest` itself refuses a seams-active run, so build the bundle and
        // note by hand rather than going through it, mirroring what a
        // maliciously-crafted note would look like.
        let keys_dir = fixture._tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);
        let bundle = BundleT::new(
            crate::version::VERSION.to_string(),
            "2026-07-02T00:00:00Z".to_string(),
            pubkey.clone(),
            seamed_seal.clone(),
            sealed_events
                .iter()
                .zip(seal.reviewers.iter())
                .map(|(event, name)| (name.clone(), event.clone()))
                .collect(),
        );
        let bundle_json = bundle.to_json().unwrap();
        let signature = sign(&key_path, bundle_json.as_bytes()).unwrap();
        let note = envelope(&bundle_json, &signature);

        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();
        let ci = CiBindings {
            head_tree: seamed_seal.head_tree.clone(),
            base_tree: seamed_seal.base_tree.clone(),
            patch_id: seamed_seal.patch_id.clone(),
            config_hash: seamed_seal.config_hash.clone(),
        };

        let outcome = plan(
            &note,
            "author@example.com",
            &[pubkey],
            &ci,
            &routed,
            fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("test seam"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_dirty_true_bundle() {
        // Mirrors `plan_falls_back_on_a_seams_true_bundle`: a bundle whose seal
        // carries `dirty: true` must never replay in CI, even if every other
        // check would otherwise pass. Defense in depth: such a bundle can only
        // exist if `bastion attest`'s own dirty refusal was bypassed.
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let fixture = build_fixture();
        let seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        let sealed_events: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let dirty_seal = crate::seal::seal(
            fixture.secret,
            &seal.version,
            &crate::seal::SealBindings {
                head_tree: seal.head_tree.clone(),
                base_tree: seal.base_tree.clone(),
                patch_id: seal.patch_id.clone(),
                config_hash: seal.config_hash.clone(),
                repo_reviewers: seal.reviewers.iter().cloned().collect(),
            },
            false,
            true,
            seal.reviewers.clone(),
            &sealed_events,
        );
        store::write_seal(&fixture.layout, &fixture.run_id, &dirty_seal).unwrap();

        let keys_dir = fixture._tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);
        let bundle = BundleT::new(
            crate::version::VERSION.to_string(),
            "2026-07-02T00:00:00Z".to_string(),
            pubkey.clone(),
            dirty_seal.clone(),
            sealed_events
                .iter()
                .zip(seal.reviewers.iter())
                .map(|(event, name)| (name.clone(), event.clone()))
                .collect(),
        );
        let bundle_json = bundle.to_json().unwrap();
        let signature = sign(&key_path, bundle_json.as_bytes()).unwrap();
        let note = envelope(&bundle_json, &signature);

        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();
        let ci = CiBindings {
            head_tree: dirty_seal.head_tree.clone(),
            base_tree: dirty_seal.base_tree.clone(),
            patch_id: dirty_seal.patch_id.clone(),
            config_hash: dirty_seal.config_hash.clone(),
        };

        let outcome = plan(
            &note,
            "author@example.com",
            &[pubkey],
            &ci,
            &routed,
            fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("dirty"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_head_tree_binding_mismatch() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        let mut drifted_ci = att.ci.clone();
        drifted_ci.head_tree = "a-different-tree-entirely".to_string();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &drifted_ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("head tree"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_base_tree_binding_mismatch() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        let mut drifted_ci = att.ci.clone();
        drifted_ci.base_tree = "a-different-base-entirely".to_string();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &drifted_ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("base"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_patch_id_binding_mismatch() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        let mut drifted_ci = att.ci.clone();
        drifted_ci.patch_id = "a-different-patch-id".to_string();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &drifted_ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("patch id"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn plan_falls_back_on_a_config_hash_binding_mismatch() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let att = build_attested_fixture();
        let r1 = reviewer_def("r1", None);
        let routed: std::collections::BTreeMap<&str, &crate::reviewer::Reviewer> =
            [("r1", &r1)].into_iter().collect();

        let mut drifted_ci = att.ci.clone();
        drifted_ci.config_hash = "a-different-config-hash".to_string();

        let outcome = plan(
            &att.note,
            att.author,
            &att.keys,
            &drifted_ci,
            &routed,
            att.fixture.secret,
        );
        match outcome {
            AttestationOutcome::Fallback { reason } => {
                assert!(reason.contains("config"), "got: {reason}");
            }
            AttestationOutcome::Replay(_) | AttestationOutcome::NotAttested => {
                panic!("expected a fallback")
            }
        }
    }

    #[test]
    fn note_for_review_falls_back_to_the_pr_head_sha() {
        // Distinct revisions exercise the fallback branch: a note is written on
        // the *first* commit, HEAD is a *second* commit that carries none, and
        // the first commit's SHA is passed as `fallback_rev`. If the fixture
        // wrote the note on whatever HEAD resolves to (the prior version of this
        // test), the primary lookup on "HEAD" would always hit and the fallback
        // path would never actually run.
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "first commit"]);
        let first_sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // A second commit advances HEAD past the commit that carries the note.
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git(&repo, &["commit", "-am", "second commit"]);

        // No note anywhere yet: both the primary ("HEAD") and fallback (the
        // first commit) lookups miss.
        assert_eq!(
            note_for_review(&repo, "HEAD", Some(&first_sha)).unwrap(),
            None
        );

        // Write the note on the *first* commit only; HEAD (the second commit)
        // still carries none, so the primary "HEAD" lookup misses and the
        // fallback lookup on `first_sha` is what actually finds it.
        git::note_add(&repo, git::NOTES_REF, &first_sha, "bundle-v1").unwrap();
        assert_eq!(
            note_for_review(&repo, "HEAD", Some(&first_sha)).unwrap(),
            Some("bundle-v1".to_string())
        );
    }

    #[test]
    fn note_for_review_prefers_the_primary_rev_when_both_carry_notes() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "base"]);

        git::note_add(&repo, git::NOTES_REF, "HEAD", "primary-note").unwrap();
        assert_eq!(
            note_for_review(&repo, "HEAD", Some("HEAD~0")).unwrap(),
            Some("primary-note".to_string())
        );
    }
}
