//! `bastion attest`: sign a sealed run as a git-note attestation on HEAD, and CI's
//! verification and replay of one.
//!
//! A sealed run (see [`crate::seal`]) is tamper-evident but unsigned: it proves
//! the run store was not edited after the fact, not that the author stands
//! behind it. Attesting turns that seal into a bundle CI can trust: the author's
//! SSH signature over the bundle, binding it to the repository state at signing
//! time. See `docs/developer-guide/attestation.md` ("The attestation bundle",
//! "The run seal", "Storage: a git note", "Signing", "The `bastion attest` flow",
//! "Verification and replay in CI") for the full design.
//!
//! This module is split by concern:
//! - [`bundle`]: the [`bundle::Bundle`] shape and its serialization to and from a
//!   note's raw text ([`bundle::envelope`]/[`bundle::split_envelope`]).
//! - [`mod@sign`]: signing and verifying with `ssh-keygen -Y`, and resolving
//!   which key to sign with.
//! - [`replay`]: CI's side, verifying a note and planning which routed reviewers
//!   replay from it ([`replay::plan`]).
//! - This file: the local `bastion attest` flow ([`attest`]) that ties the other
//!   three together, plus their public re-exports so `crate::attest::Foo` keeps
//!   resolving for every existing caller.

pub mod bundle;
pub mod replay;
pub mod sign;

pub use bundle::{Bundle, envelope, split_envelope};
pub use replay::{
    AttestationOutcome, CiBindings, ReplayPlan, derive_ci_bindings, note_for_review, plan,
};
pub use sign::{SIG_NAMESPACE, sign, verify_signature};

use std::collections::BTreeMap;
use std::path::Path;

use color_eyre::eyre::{Context, Result, bail, eyre};

use crate::config::Config;
use crate::event::RunEvent;
use crate::git;
use crate::paths::Layout;
use crate::store;

use self::sign::resolve_signing_key;

/// Sign the latest sealed run (or `run`, when given) as an attestation note on
/// HEAD.
///
/// Implements `docs/developer-guide/attestation.md` ("The `bastion attest`
/// flow"): loads and verifies the run's seal, re-derives the repository state
/// and refuses on any drift, resolves the signing key, builds and signs the
/// bundle, and writes it to `refs/notes/bastion` on HEAD. `includes` is the
/// `--include` set of extra registry files, which must match what the run was
/// reviewed with for the re-derived config hash to agree. `secret` is the
/// sealing secret to verify against (`seal::embedded_secret()` in production;
/// injected here so tests can seal and attest under a fixed test secret
/// without depending on the build-time embedded one).
///
/// # Errors
///
/// Returns an error, each one naming exactly what was wrong, when: the run has
/// no seal; the seal recorded that a test seam was active or that the working
/// tree was dirty at review time; the run store no longer matches its own
/// seal; the repository has moved on since the run (HEAD's tree, the merge
/// base's tree, the patch id, or the effective config hash no longer match); no
/// signing key can be resolved; or signing or writing the note fails.
pub fn attest(
    root: &Path,
    layout: &Layout,
    run: Option<&str>,
    key: Option<&Path>,
    includes: &[std::path::PathBuf],
    secret: &[u8],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let run_id = store::resolve_run(layout, run)?;
    let events = store::read_run(layout, &run_id)?;

    // Checked before the seal: a partial run is never sealed, but "re-run
    // `bastion review` with this binary" would be misleading advice when the
    // actual problem is that the run covered only a hand-picked subset of the
    // triggered reviewers.
    if events.iter().any(|event| {
        matches!(
            event,
            RunEvent::RunStarted { partial: true, .. }
                | RunEvent::RunCompleted { partial: true, .. }
        )
    }) {
        bail!(
            "run '{run_id}' was partial (`bastion review --reviewer` ran a subset of the \
             triggered reviewers); its verdict speaks only for those reviewers and cannot be \
             attested. Run a full `bastion review` and attest that run instead"
        );
    }

    let seal = store::read_seal(layout, &run_id)?
        .ok_or_else(|| eyre!("run '{run_id}' was not sealed; re-run `bastion review` with this binary before attesting"))?;

    if seal.seams {
        bail!(
            "run '{run_id}' used a test seam (a backend or container-engine override); it exercised the binary, but is not a real review, and cannot be attested"
        );
    }
    if seal.dirty {
        bail!(
            "run '{run_id}' reviewed a dirty working tree (uncommitted or untracked changes); commit the final content, re-run `bastion review`, and attest that run"
        );
    }

    let sealed_reviewer_names: std::collections::BTreeSet<&str> =
        seal.reviewers.iter().map(String::as_str).collect();
    let mut sealed_events: Vec<(&str, &RunEvent)> = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ReviewerResolved { reviewer, .. }
            | RunEvent::ReviewerSkipped { reviewer, .. }
                if sealed_reviewer_names.contains(reviewer.as_str()) =>
            {
                Some((reviewer.as_str(), event))
            }
            _ => None,
        })
        .collect();
    sealed_events.sort_by_key(|(name, _)| *name);

    let event_values: Vec<serde_json::Value> = sealed_events
        .iter()
        .map(|(_, event)| serde_json::to_value(event))
        .collect::<std::result::Result<_, _>>()
        .wrap_err("serializing sealed reviewer events")?;

    if !crate::seal::verify(secret, &seal, &event_values) {
        bail!(
            "run '{run_id}' does not match its own seal: the run store was edited after the run finished, or it was sealed by a different build of bastion"
        );
    }

    let head_tree = git::tree_hash(root, "HEAD").wrap_err("resolving HEAD's tree")?;
    drift_check(
        "HEAD has changed since this run",
        "tree",
        "HEAD is now",
        &seal.head_tree,
        &head_tree,
    )?;

    let base = base_ref(&events).ok_or_else(|| {
        eyre!("run '{run_id}' has no recorded base ref; cannot re-derive its merge base")
    })?;
    let merge_base_commit = git::merge_base(root, &base)
        .wrap_err("resolving the merge base against the run's recorded base ref")?;
    let base_tree =
        git::tree_hash(root, &merge_base_commit).wrap_err("resolving the merge base's tree")?;
    drift_check(
        "the merge base has moved since this run",
        "base tree",
        "it is now",
        &seal.base_tree,
        &base_tree,
    )?;

    let patch_id =
        git::patch_id(root, &merge_base_commit).wrap_err("recomputing the diff's patch id")?;
    drift_check(
        "the diff has changed since this run",
        "patch id",
        "it is now",
        &seal.patch_id,
        &patch_id,
    )?;

    // `includes` must be the same `--include` set the run was reviewed with:
    // extra includes are part of the effective repository config, so the hash
    // only matches when this command re-derives it from the same files.
    let (_, repo_attestation, _) = Config::discover_merged_attested(root, None, includes, false)
        .wrap_err("re-deriving the effective repository reviewer config")?;
    drift_check(
        "the reviewer registry has changed since this run",
        "config hash",
        "it is now",
        &seal.config_hash,
        &repo_attestation.config_hash,
    )?;

    let temp_pubkey_file =
        tempfile::NamedTempFile::new().wrap_err("creating a temporary key file")?;
    let signing_key = resolve_signing_key(root, key, &temp_pubkey_file)?;

    let attested_at = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let bundle_events: BTreeMap<String, serde_json::Value> = sealed_events
        .iter()
        .map(|(name, event)| {
            serde_json::to_value(event)
                .map(|value| ((*name).to_string(), value))
                .wrap_err("serializing a sealed reviewer event")
        })
        .collect::<Result<_>>()?;
    let bundle = Bundle::new(
        crate::version::VERSION.to_string(),
        attested_at,
        signing_key.public_key.clone(),
        seal.clone(),
        bundle_events,
    );

    let bundle_json = bundle.to_json()?;
    let signature = sign(&signing_key.key_file, bundle_json.as_bytes())?;
    let note = envelope(&bundle_json, &signature);
    git::note_add(root, git::NOTES_REF, "HEAD", &note).wrap_err("writing the attestation note")?;

    writeln!(
        out,
        "Attested run '{run_id}' on HEAD ({} reviewer(s): {})",
        seal.reviewers.len(),
        seal.reviewers.join(", ")
    )
    .wrap_err("writing attest summary")?;
    writeln!(out, "Signed with {}", signing_key.public_key).wrap_err("writing attest summary")?;
    writeln!(
        out,
        "Push the note with: git push origin {}",
        git::NOTES_REF
    )
    .wrap_err("writing attest summary")?;

    Ok(())
}

/// Bail if a value re-derived at attest time has drifted from what the run's seal
/// recorded, with the uniform "re-run `bastion review` before attesting" guidance.
///
/// `lead` names what moved, `subject` names the sealed value, and `now` introduces
/// the current value ("HEAD is now" for the head tree, "it is now" for the rest);
/// the interleaved derivations that produce `actual` stay at the call site.
fn drift_check(lead: &str, subject: &str, now: &str, sealed: &str, actual: &str) -> Result<()> {
    if actual != sealed {
        bail!(
            "{lead}: the reviewed {subject} was {sealed}, {now} {actual}; re-run `bastion review` before attesting"
        );
    }
    Ok(())
}

/// The base ref a run diffed against, from its `RunStarted` event.
fn base_ref(events: &[RunEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        RunEvent::RunStarted { base, .. } => Some(base.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Gates, ReviewerRef, RunId};
    use crate::reviewer::Mode;
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
        // The `-c` isolation above only covers commands issued through this
        // helper. The code under test (`attest` writing its note) runs plain
        // `git` in the same repo and needs an identity from config on a host
        // that has none (CI), so persist one repo-locally at init.
        if args.first() == Some(&"init") {
            git(cwd, &["config", "user.email", "grace@bastion.dev"]);
            git(cwd, &["config", "user.name", "Grace Hopper"]);
        }
    }

    /// Whether `tool` is runnable at all, for detect-and-skip on machines
    /// without it (mirroring the house style for real-tool tests).
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

    /// Generate a throwaway ed25519 keypair at `<dir>/id`, returning
    /// `(private_key_path, public_key_line)`.
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

        let config_hash = Config::from_yaml(registry_yaml, Path::new("."))
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
                trigger: None,
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
                trigger: None,
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
                    skipped: 0,
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

    #[test]
    fn attest_happy_path_writes_a_verifiable_note() {
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
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
            &[],
            fixture.secret,
            &mut out,
        )
        .expect("attest succeeds");

        let note = git::note_show(&fixture.repo, git::NOTES_REF, "HEAD")
            .unwrap()
            .expect("a note was written");
        let (bundle_json, signature) = split_envelope(&note).expect("splits cleanly");
        let bundle = Bundle::from_json(bundle_json).expect("bundle parses");

        let stored_seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(bundle.seal, stored_seal);
        assert_eq!(bundle.events.len(), 2);
        assert!(bundle.events.contains_key("r1"));
        assert!(bundle.events.contains_key("r2"));

        let verified = verify_signature(
            bundle_json.as_bytes(),
            signature,
            "test-principal",
            &[pubkey],
        )
        .expect("verification runs");
        assert!(verified, "the note's own signature must verify");

        let summary = String::from_utf8(out).unwrap();
        assert!(summary.contains("r-test"));
        assert!(summary.contains("git push origin refs/notes/bastion"));
    }

    #[test]
    fn attest_refuses_a_run_with_no_seal() {
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

        let layout = Layout::with_root(tmp.path().join("data"));
        let run_id = RunId("r-unsealed".into());
        store::write_run(
            &layout,
            &run_id,
            &[RunEvent::RunStarted {
                partial: false,
                run: run_id.clone(),
                branch: "feature".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            }],
        )
        .unwrap();

        let mut out = Vec::new();
        let err = attest(&repo, &layout, None, None, &[], b"secret", &mut out).unwrap_err();
        assert!(err.to_string().contains("was not sealed"));
    }

    #[test]
    fn attest_refuses_a_partial_run() {
        // A filtered run (`bastion review --reviewer`) is never sealed, but the
        // refusal must say *why* attesting it is wrong, not suggest re-running
        // "with this binary" as the unsealed-run message does.
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

        let layout = Layout::with_root(tmp.path().join("data"));
        let run_id = RunId("r-partial".into());
        store::write_run(
            &layout,
            &run_id,
            &[RunEvent::RunStarted {
                run: run_id.clone(),
                branch: "feature".into(),
                base: "main".into(),
                changed: 1,
                reviewers: vec![ReviewerRef {
                    name: "r1".into(),
                    mode: Mode::Gate,
                }],
                partial: true,
            }],
        )
        .unwrap();

        let mut out = Vec::new();
        let err = attest(&repo, &layout, None, None, &[], b"secret", &mut out).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("was partial"), "got: {message}");
        assert!(message.contains("cannot be attested"), "got: {message}");
    }

    #[test]
    fn attest_refuses_a_seal_with_seams_active() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let fixture = build_fixture();
        // Flip the persisted seal's `seams` flag to simulate a run that used a
        // test-backend override; re-sign it under the same secret so only
        // `seams` differs, isolating the refusal being tested.
        let mut seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        let sealed_events: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        seal = crate::seal::seal(
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
        store::write_seal(&fixture.layout, &fixture.run_id, &seal).unwrap();

        let mut out = Vec::new();
        let err = attest(
            &fixture.repo,
            &fixture.layout,
            None,
            None,
            &[],
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("test seam"));
    }

    #[test]
    fn attest_refuses_a_seal_with_dirty_true() {
        // Mirrors `attest_refuses_a_seal_with_seams_active`: a dirty run's
        // reviewers may have judged content the seal's committed bindings never
        // name, so `bastion attest` must refuse it with a plain, actionable
        // reason, exactly like the seams refusal.
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let fixture = build_fixture();
        let mut seal = store::read_seal(&fixture.layout, &fixture.run_id)
            .unwrap()
            .unwrap();
        let events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        let sealed_events: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        seal = crate::seal::seal(
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
        store::write_seal(&fixture.layout, &fixture.run_id, &seal).unwrap();

        let mut out = Vec::new();
        let err = attest(
            &fixture.repo,
            &fixture.layout,
            None,
            None,
            &[],
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("dirty working tree"),
            "got: {err:#}"
        );
    }

    #[test]
    fn attest_refuses_when_the_run_store_was_edited_after_sealing() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let fixture = build_fixture();
        let mut events = store::read_run(&fixture.layout, &fixture.run_id).unwrap();
        for event in &mut events {
            if let RunEvent::ReviewerResolved { summary, .. } = event {
                *summary = "a perturbed summary that never happened".to_string();
            }
        }
        store::write_run(&fixture.layout, &fixture.run_id, &events).unwrap();

        let mut out = Vec::new();
        let err = attest(
            &fixture.repo,
            &fixture.layout,
            None,
            None,
            &[],
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match its own seal"));
    }

    #[test]
    fn attest_refuses_after_a_new_commit_moves_head() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let fixture = build_fixture();
        std::fs::write(fixture.repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(
            &fixture.repo,
            &["commit", "-am", "one more change after sealing"],
        );

        let mut out = Vec::new();
        let err = attest(
            &fixture.repo,
            &fixture.layout,
            None,
            None,
            &[],
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("HEAD has changed"));
    }
}
