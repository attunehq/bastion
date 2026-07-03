//! `bastion attest`: sign a sealed run as a git-note attestation on HEAD.
//!
//! A sealed run (see [`crate::seal`]) is tamper-evident but unsigned: it proves
//! the run store was not edited after the fact, not that the author stands
//! behind it. Attesting turns that seal into a bundle CI can trust: the author's
//! SSH signature over the bundle, binding it to the repository state at
//! signing time. See `docs/developer-guide/attestation.md` ("The attestation
//! bundle", "The run seal", "Storage: a git note", "Signing", "The `bastion
//! attest` flow") for the full design; this module implements it.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::event::RunEvent;
use crate::git;
use crate::paths::Layout;
use crate::seal::Seal;
use crate::store;

/// The bundle `kind` every attestation carries. A note whose `kind` differs was
/// never produced by this module, so [`Bundle::from_json`] rejects it outright
/// rather than trying to interpret a foreign shape.
const KIND: &str = "bastion-attestation";

/// The bundle schema version this binary produces and accepts. Bumped only on a
/// breaking bundle-shape change; [`Bundle::from_json`] refuses any other value
/// rather than guessing at a migration.
const SCHEMA: u32 = 1;

/// The SSH signature namespace attestations are signed and verified under
/// (`ssh-keygen -Y sign/verify -n <namespace>`). Scoping the namespace keeps a
/// bastion attestation signature from being replayable as, say, a git commit
/// signature by the same key: `ssh-keygen` binds the namespace into what it
/// signs.
pub const SIG_NAMESPACE: &str = "bastion";

/// A signed record of what a local run reviewed and concluded.
///
/// Field order here *is* the canonical serialization: this module is both the
/// sole producer and the sole consumer of a bundle, so the signature is simply
/// computed over the exact `serde_json` bytes this struct serializes to, with
/// no separate canonicalization step to keep in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// Always `KIND`. Present so a note this module did not produce is
    /// rejected by [`Bundle::from_json`] rather than partially parsed.
    pub kind: String,
    /// Always `SCHEMA`. A future breaking change to this shape bumps the
    /// constant; an old binary then refuses a new bundle instead of
    /// misreading it.
    pub schema: u32,
    /// The `bastion` version that produced this bundle, as plain text (not
    /// part of the seal). Carried so a verification failure caused by a
    /// version mismatch reports as "attested by vX, CI runs vY" rather than a
    /// bare signature or seal failure.
    pub version: String,
    /// When the bundle was signed, RFC3339 at seconds precision.
    pub attested_at: String,
    /// The signer's SSH public key line (type, base64, and an optional
    /// comment), exactly as `ssh-keygen -y` or the key file itself renders
    /// it. CI resolves the PR author's registered signing keys independently
    /// and checks that this key is among them; the bundle only records which
    /// key was used.
    pub public_key: String,
    /// The run seal this bundle attests to. Re-verified against the run store
    /// (never trusted from the note alone) both here, before signing, and
    /// again by CI, with each side's own embedded secret.
    pub seal: Seal,
    /// Each sealed reviewer's `reviewer.resolved` event, keyed by reviewer
    /// name, so a replaying CI run has the full verdict and findings without
    /// re-reading the author's local run store (which it does not have
    /// access to).
    pub events: BTreeMap<String, serde_json::Value>,
}

impl Bundle {
    /// Parse and validate a bundle from its JSON text.
    ///
    /// This is the boundary between "text found in a git note" and "a bundle
    /// this binary understands": a wrong `kind` or `schema` is rejected here,
    /// by name, rather than surfacing as a downstream field-access panic or a
    /// mystifying signature-verification failure.
    ///
    /// # Errors
    ///
    /// Returns an error if `json` is not valid JSON for this shape, or if its
    /// `kind` or `schema` do not match what this binary produces.
    pub fn from_json(json: &str) -> Result<Self> {
        let bundle: Bundle =
            serde_json::from_str(json).wrap_err("attestation bundle is not valid JSON")?;
        if bundle.kind != KIND {
            bail!(
                "not a bastion attestation bundle: expected kind '{KIND}', found '{}'",
                bundle.kind
            );
        }
        if bundle.schema != SCHEMA {
            bail!(
                "unsupported attestation bundle schema {} (this binary produces schema {SCHEMA})",
                bundle.schema
            );
        }
        Ok(bundle)
    }

    /// Serialize to compact JSON, in field-declaration order.
    ///
    /// # Errors
    ///
    /// Returns an error only if a carried [`serde_json::Value`] is somehow not
    /// serializable, which cannot happen for a value that was itself parsed
    /// from JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).wrap_err("serializing attestation bundle")
    }
}

/// The armored signature block's opening line, as `ssh-keygen -Y sign` emits it.
const SIG_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";

/// Compose a note's text from a bundle's JSON and its armored signature.
///
/// The note is the bundle JSON, a newline, then the signature block verbatim.
/// `signature` is expected already armored (as `ssh-keygen -Y sign` emits it,
/// trailing newline included or not); this only controls the join between the
/// two halves, so [`split_envelope`] can invert it exactly.
#[must_use]
pub fn envelope(bundle_json: &str, signature: &str) -> String {
    format!("{bundle_json}\n{}", signature.trim_end())
}

/// Split a note's text back into its bundle JSON and armored signature.
///
/// Splits at `SIG_BEGIN` (the signature block's own opening line) rather than
/// at a fixed line count, since the bundle JSON is always a single line
/// (compact `serde_json` output) but is not guaranteed to stay that way
/// forever. The bundle half's trailing newline (the join character
/// [`envelope`] inserted) is trimmed so the round trip is byte-exact:
/// `split_envelope(&envelope(json, sig)) == (json, sig)`.
///
/// # Errors
///
/// Returns an error if the text carries no `-----BEGIN SSH SIGNATURE-----`
/// line, meaning it is not an envelope this module produced.
pub fn split_envelope(note: &str) -> Result<(&str, &str)> {
    let idx = note
        .find(SIG_BEGIN)
        .ok_or_else(|| eyre!("note carries no SSH signature block; not a bastion attestation"))?;
    let bundle_part = note[..idx].strip_suffix('\n').unwrap_or(&note[..idx]);
    let sig_part = &note[idx..];
    Ok((bundle_part, sig_part))
}

/// Sign `data` with the SSH key at `key_file`, returning the armored signature.
///
/// Shells out to `ssh-keygen -Y sign -f <key_file> -n bastion`, which reads the
/// data to sign from stdin (no file operand) and writes the armored signature
/// to stdout. `key_file` may be a private key file or, when the configured
/// signing key is a literal public key, a public key file backed by an agent
/// holding the matching private key.
///
/// # Errors
///
/// Returns an error if `ssh-keygen` cannot be run, or exits non-zero (a
/// missing key, an agent that refuses to sign, or a presence prompt the user
/// declined).
pub fn sign(key_file: &Path, data: &[u8]) -> Result<String> {
    run_ssh_keygen_with_stdin(
        &[
            "-Y",
            "sign",
            "-f",
            &key_file.to_string_lossy(),
            "-n",
            SIG_NAMESPACE,
        ],
        data,
    )
}

/// Verify an armored `signature` over `data`, claimed by `principal`, against
/// `public_keys`.
///
/// Writes an ephemeral `allowed_signers` file (one `<principal> <key>` line per
/// candidate key) and the signature to temporary files, then runs `ssh-keygen
/// -Y verify -f <allowed_signers> -I <principal> -n bastion -s <sigfile>` with
/// `data` on stdin.
///
/// Returns `Ok(false)` for an ordinary verification failure (wrong key, wrong
/// principal, tampered data or signature): this is the expected outcome for an
/// unenrolled or malicious signer, not a tooling error. `Err` is reserved for
/// an inability to run the check at all (`ssh-keygen` missing, or the
/// temporary files could not be written).
///
/// # Errors
///
/// Returns an error if `ssh-keygen` cannot be invoked or the temporary
/// allowed-signers/signature files cannot be written.
pub fn verify_signature(
    data: &[u8],
    signature: &str,
    principal: &str,
    public_keys: &[String],
) -> Result<bool> {
    let mut allowed_signers =
        tempfile::NamedTempFile::new().wrap_err("creating a temporary allowed_signers file")?;
    for key in public_keys {
        std::io::Write::write_all(
            &mut allowed_signers,
            format!("{principal} {key}\n").as_bytes(),
        )
        .wrap_err("writing the allowed_signers file")?;
    }
    allowed_signers
        .flush()
        .wrap_err("flushing the allowed_signers file")?;

    let mut sig_file =
        tempfile::NamedTempFile::new().wrap_err("creating a temporary signature file")?;
    std::io::Write::write_all(&mut sig_file, signature.as_bytes())
        .wrap_err("writing the signature file")?;
    sig_file.flush().wrap_err("flushing the signature file")?;

    let output = Command::new("ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            &allowed_signers.path().to_string_lossy(),
            "-I",
            principal,
            "-n",
            SIG_NAMESPACE,
            "-s",
            &sig_file.path().to_string_lossy(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to invoke ssh-keygen; is it installed and on PATH?")
        .and_then(|mut child| {
            std::io::Write::write_all(
                child.stdin.as_mut().expect("stdin was requested as piped"),
                data,
            )
            .wrap_err("writing data to ssh-keygen's stdin")?;
            child
                .wait_with_output()
                .wrap_err("waiting for ssh-keygen to finish")
        })?;

    Ok(output.status.success())
}

/// Run `ssh-keygen` with `args`, piping `data` to stdin and returning trimmed
/// stdout on success.
fn run_ssh_keygen_with_stdin(args: &[&str], data: &[u8]) -> Result<String> {
    let mut child = Command::new("ssh-keygen")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to invoke ssh-keygen; is it installed and on PATH?")?;

    std::io::Write::write_all(
        child.stdin.as_mut().expect("stdin was requested as piped"),
        data,
    )
    .wrap_err("writing data to ssh-keygen's stdin")?;

    let output = child
        .wait_with_output()
        .wrap_err("waiting for ssh-keygen to finish")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ssh-keygen {} failed: {}", args.join(" "), stderr.trim());
    }
    String::from_utf8(output.stdout)
        .wrap_err("ssh-keygen produced non-UTF-8 output")
        .map(|s| s.trim().to_string())
}

/// A resolved signing key: the file `ssh-keygen -Y sign` should be pointed at,
/// and the public key line the bundle should record.
#[derive(Debug)]
struct SigningKey {
    /// The path `-f` is given. May be a private key file (the ordinary case)
    /// or a public key file backed by an agent (the `git config
    /// user.signingkey` literal-public-key case).
    key_file: std::path::PathBuf,
    /// The single-line public key text recorded in the bundle.
    public_key: String,
}

/// Resolve the signing key `bastion attest` should use, following
/// `docs/developer-guide/attestation.md` ("Signing"):
///
/// 1. `--key <path>` (`explicit_key`), if given, always wins.
/// 2. Otherwise `git config user.signingkey`. A value starting with `ssh-` or
///    `sk-ssh-` is a literal public key (git's own convention for an
///    SSH-signing identity resolved via an agent): it is written to a
///    temporary file and used as the `-f` argument, so `ssh-keygen` resolves
///    the matching private half from the agent. Any other value is treated as
///    a path to a key file.
/// 3. Neither present: refuse with actionable guidance.
///
/// The returned [`SigningKey::public_key`] is read from the resolved private
/// key's `.pub` sibling when one exists, derived with `ssh-keygen -y`
/// otherwise, or used directly when the resolved key was already a literal
/// public key.
fn resolve_signing_key(
    repo_root: &Path,
    explicit_key: Option<&Path>,
    temp_pubkey_file: &tempfile::NamedTempFile,
) -> Result<SigningKey> {
    let key_file = match explicit_key {
        Some(path) => path.to_path_buf(),
        None => {
            let configured = git::run_git_config_signingkey(repo_root);
            match configured {
                Some(value) if value.starts_with("ssh-") || value.starts_with("sk-ssh-") => {
                    std::fs::write(temp_pubkey_file.path(), format!("{value}\n"))
                        .wrap_err("writing the configured signing key to a temporary file")?;
                    temp_pubkey_file.path().to_path_buf()
                }
                Some(value) => std::path::PathBuf::from(value),
                None => bail!(
                    "no signing key configured; set `git config user.signingkey <path-or-key>` or pass `--key <path>`"
                ),
            }
        }
    };

    let public_key = public_key_line(&key_file)?;
    Ok(SigningKey {
        key_file,
        public_key,
    })
}

/// Read or derive the single-line public key text for `key_file`.
///
/// A key file whose content already starts with an SSH public-key type token
/// (`ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-...`, `sk-ssh-...`) is itself the
/// public key. Otherwise it is a private key: its `.pub` sibling is read when
/// present, and derived with `ssh-keygen -y -f <key_file>` when it is not.
fn public_key_line(key_file: &Path) -> Result<String> {
    let content = std::fs::read_to_string(key_file)
        .wrap_err_with(|| format!("reading signing key at {}", key_file.display()))?;
    if is_public_key_text(&content) {
        return Ok(single_line(&content));
    }

    let pub_sibling = key_file.with_extension(match key_file.extension() {
        Some(ext) => format!("{}.pub", ext.to_string_lossy()),
        None => "pub".to_string(),
    });
    // `with_extension` on an extensionless path (the common case: `id_ed25519`
    // has no extension by ssh-keygen convention) does not append `.pub`, so
    // build that form directly rather than relying on `with_extension`'s
    // replace-not-append semantics.
    let pub_sibling = if key_file.extension().is_none() {
        let mut name = key_file.as_os_str().to_os_string();
        name.push(".pub");
        std::path::PathBuf::from(name)
    } else {
        pub_sibling
    };

    if let Ok(pub_text) = std::fs::read_to_string(&pub_sibling) {
        return Ok(single_line(&pub_text));
    }

    // No `.pub` sibling: derive it. `ssh-keygen -y -f <private key>` prints the
    // public key to stdout.
    let output = Command::new("ssh-keygen")
        .args(["-y", "-f", &key_file.to_string_lossy()])
        .output()
        .wrap_err("failed to invoke ssh-keygen; is it installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "could not derive a public key from {}: {}",
            key_file.display(),
            stderr.trim()
        );
    }
    let derived =
        String::from_utf8(output.stdout).wrap_err("ssh-keygen produced non-UTF-8 output")?;
    Ok(single_line(&derived))
}

/// Whether `content` starts with an SSH public-key type token, meaning it is
/// itself a public key rather than a private key.
fn is_public_key_text(content: &str) -> bool {
    let trimmed = content.trim_start();
    [
        "ssh-ed25519",
        "ssh-rsa",
        "ssh-dss",
        "ecdsa-sha2-",
        "sk-ssh-",
        "sk-ecdsa-sha2-",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

/// Trim `text` to its first non-empty line.
fn single_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Sign the latest sealed run (or `run`, when given) as an attestation note on
/// HEAD.
///
/// Implements `docs/developer-guide/attestation.md` ("The `bastion attest`
/// flow"): loads and verifies the run's seal, re-derives the repository state
/// and refuses on any drift, resolves the signing key, builds and signs the
/// bundle, and writes it to `refs/notes/bastion` on HEAD. `secret` is the
/// sealing secret to verify against (`seal::embedded_secret()` in production;
/// injected here so tests can seal and attest under a fixed test secret
/// without depending on the build-time embedded one).
///
/// # Errors
///
/// Returns an error, each one naming exactly what was wrong, when: the run has
/// no seal; the seal recorded that a test seam was active; the run store no
/// longer matches its own seal; the repository has moved on since the run
/// (HEAD's tree, the merge base's tree, the patch id, or the effective config
/// hash no longer match); no signing key can be resolved; or signing or
/// writing the note fails.
pub fn attest(
    root: &Path,
    layout: &Layout,
    run: Option<&str>,
    key: Option<&Path>,
    secret: &[u8],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let run_id = store::resolve_run(layout, run)?;
    let seal = store::read_seal(layout, &run_id)?
        .ok_or_else(|| eyre!("run '{run_id}' was not sealed; re-run `bastion review` with this binary before attesting"))?;

    if seal.seams {
        bail!(
            "run '{run_id}' used a test seam (a backend or container-engine override); it exercised the binary, but is not a real review, and cannot be attested"
        );
    }

    let events = store::read_run(layout, &run_id)?;
    let sealed_reviewer_names: std::collections::BTreeSet<&str> =
        seal.reviewers.iter().map(String::as_str).collect();
    let mut sealed_events: Vec<(&str, &RunEvent)> = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ReviewerResolved { reviewer, .. }
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
    if head_tree != seal.head_tree {
        bail!(
            "HEAD has changed since this run: the reviewed tree was {}, HEAD is now {head_tree}; re-run `bastion review` before attesting",
            seal.head_tree
        );
    }

    let base = base_ref(&events).ok_or_else(|| {
        eyre!("run '{run_id}' has no recorded base ref; cannot re-derive its merge base")
    })?;
    let merge_base_commit = git::merge_base(root, &base)
        .wrap_err("resolving the merge base against the run's recorded base ref")?;
    let base_tree =
        git::tree_hash(root, &merge_base_commit).wrap_err("resolving the merge base's tree")?;
    if base_tree != seal.base_tree {
        bail!(
            "the merge base has moved since this run: the reviewed base tree was {}, it is now {base_tree}; re-run `bastion review` before attesting",
            seal.base_tree
        );
    }

    let patch_id =
        git::patch_id(root, &merge_base_commit).wrap_err("recomputing the diff's patch id")?;
    if patch_id != seal.patch_id {
        bail!(
            "the diff has changed since this run: the reviewed patch id was {}, it is now {patch_id}; re-run `bastion review` before attesting",
            seal.patch_id
        );
    }

    let (_, repo_attestation, _) = Config::discover_merged_attested(root, None)
        .wrap_err("re-deriving the effective repository reviewer config")?;
    if repo_attestation.config_hash != seal.config_hash {
        bail!(
            "the reviewer registry has changed since this run: the reviewed config hash was {}, it is now {}; re-run `bastion review` before attesting",
            seal.config_hash,
            repo_attestation.config_hash
        );
    }

    let temp_pubkey_file =
        tempfile::NamedTempFile::new().wrap_err("creating a temporary key file")?;
    let signing_key = resolve_signing_key(root, key, &temp_pubkey_file)?;

    let attested_at = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let bundle = Bundle {
        kind: KIND.to_string(),
        schema: SCHEMA,
        version: crate::version::VERSION.to_string(),
        attested_at,
        public_key: signing_key.public_key.clone(),
        seal: seal.clone(),
        events: sealed_events
            .iter()
            .map(|(name, event)| {
                serde_json::to_value(event)
                    .map(|value| ((*name).to_string(), value))
                    .wrap_err("serializing a sealed reviewer event")
            })
            .collect::<Result<_>>()?,
    };

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

/// The base ref a run diffed against, from its `RunStarted` event.
fn base_ref(events: &[RunEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        RunEvent::RunStarted { base, .. } => Some(base.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Verification and replay in CI
// ---------------------------------------------------------------------------

/// A CI run's decision to replay one or more reviewers from a verified
/// attestation, and to execute the rest fresh.
///
/// Built by [`plan`] once every binding, signature, and seal check has passed;
/// `docs/developer-guide/attestation.md` ("Verification and replay in CI") is the
/// governing design.
#[derive(Debug, Clone)]
pub struct ReplayPlan {
    /// The verified bundle this plan replays from.
    pub bundle: Bundle,
    /// The `reviewer.resolved` event JSON for each reviewer that will be
    /// replayed, keyed by reviewer name. A subset of `bundle.events`: only the
    /// names that are both routed by CI's own diff and not opted out via
    /// [`AttestationPolicy::Never`](crate::reviewer::AttestationPolicy::Never).
    pub replay: BTreeMap<String, serde_json::Value>,
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
    /// The attestation was not honored; every routed reviewer executes fresh.
    /// `reason` names exactly what failed, in plain English, so the report can
    /// tell the author rather than leaving them guessing.
    Fallback {
        /// Why the attestation was not honored.
        reason: String,
    },
}

/// The re-derived repository state CI's own checkout produces, to compare
/// against a bundle's recorded [`Seal`] bindings.
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
/// GitHub login and their registered SSH signing keys (section 3 of the phase
/// spec; fetched independently so this function stays testable without the
/// network). `ci` is CI's own re-derived bindings ([`derive_ci_bindings`]).
/// `routed` is the reviewers CI's own diff matched, name to definition, so the
/// per-reviewer replay/fresh split can consult each one's
/// [`AttestationPolicy`](crate::reviewer::AttestationPolicy). `secret` is the
/// sealing secret to verify the bundle's seal against
/// (`seal::embedded_secret()` in production).
///
/// Every failure path returns `AttestationOutcome::Fallback` with a reason
/// naming exactly what did not check out, per
/// `docs/developer-guide/attestation.md`'s fail-closed list: an unparseable
/// note, a signature that does not verify against the author's registered
/// keys, a seal that does not verify (worded as a version mismatch when
/// `bundle.version` differs from this binary's), a seal with test seams
/// active, or a binding mismatch (named: head tree, base tree, patch id, or
/// config hash).
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

    if bundle.seal.head_tree != ci.head_tree {
        return fallback(format!(
            "the attested head tree does not match CI's checkout (attested {}, CI has {})",
            bundle.seal.head_tree, ci.head_tree
        ));
    }
    if bundle.seal.base_tree != ci.base_tree {
        return fallback(format!(
            "the attested base tree does not match CI's merge base (attested {}, CI has {}); the base may have moved since the local review",
            bundle.seal.base_tree, ci.base_tree
        ));
    }
    if bundle.seal.patch_id != ci.patch_id {
        return fallback(format!(
            "the attested patch id does not match CI's diff (attested {}, CI has {})",
            bundle.seal.patch_id, ci.patch_id
        ));
    }
    if bundle.seal.config_hash != ci.config_hash {
        return fallback(format!(
            "the attested reviewer config does not match CI's (attested {}, CI has {}); the registry has changed since the local review",
            bundle.seal.config_hash, ci.config_hash
        ));
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
    // reviewer's own `reviewer.resolved` event before trusting it to replay.
    let mut replay = BTreeMap::new();
    let mut executed_fresh = Vec::new();
    for (name, reviewer) in routed {
        let never_replay = matches!(
            reviewer.attestation,
            Some(crate::reviewer::AttestationPolicy::Never)
        );
        match bundle.events.get(*name) {
            Some(_) if never_replay => executed_fresh.push((*name).to_string()),
            Some(event) => match serde_json::from_value::<RunEvent>(event.clone()) {
                Ok(RunEvent::ReviewerResolved {
                    reviewer: bound, ..
                }) if bound == *name => {
                    replay.insert((*name).to_string(), event.clone());
                }
                _ => {
                    return fallback(format!(
                        "the attestation bundle carries a malformed or mismatched event under \
                         reviewer '{name}' (its key does not match the event's own reviewer \
                         field, or the event is not a reviewer.resolved event)"
                    ));
                }
            },
            None => executed_fresh.push((*name).to_string()),
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
    use crate::event::{Gates, ReviewerRef, RunId};
    use crate::reviewer::Mode;
    use crate::verdict::{Decision, Money};

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
        (key_path, single_line(&pub_text))
    }

    /// Set `user.signingkey` in `dir`'s *local* git config.
    ///
    /// Deliberately local, never global or system: this test binary runs many
    /// tests concurrently, `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` (or the
    /// developer's real `~/.gitconfig`) are shared process- or host-wide
    /// state, and a git subprocess whose config file disappears or changes
    /// mid-read fails with a raw `fatal: unknown error occurred while reading
    /// the configuration files`, exactly the flake this local-only scoping
    /// avoids. Local config also always wins over global/system for the same
    /// key, so this is sufficient to make `resolve_signing_key`'s "configured"
    /// tests deterministic regardless of what the host's own global git
    /// config carries.
    fn set_signingkey(dir: &Path, value: &str) {
        git(dir, &["config", "--local", "user.signingkey", value]);
    }

    /// Set `user.signingkey` to an empty value in `dir`'s *local* git config.
    ///
    /// `git config user.signingkey` returns this empty local value rather
    /// than falling through to global/system config (git resolves a key from
    /// the most specific scope that defines it at all, even if the value
    /// found there is empty), and [`crate::git::run_git_config_signingkey`]
    /// already filters an empty value to `None`. So this deterministically
    /// simulates "no signing key configured" for [`resolve_signing_key`]
    /// regardless of what the host's own global git config carries, with no
    /// process- or host-wide state to isolate.
    fn clear_signingkey(dir: &Path) {
        git(dir, &["config", "--local", "user.signingkey", ""]);
    }

    #[test]
    fn resolve_signing_key_follows_a_configured_key_file_path() {
        // `git config user.signingkey` pointing at an ordinary private key file
        // path (not a literal `ssh-...` public key) is used directly as `-f`,
        // and its public key is read from the `.pub` sibling `generate_keypair`
        // already wrote.
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);

        let keys_dir = tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (key_path, pubkey) = generate_keypair(&keys_dir);
        set_signingkey(&repo, &key_path.to_string_lossy());

        let temp_pubkey_file = tempfile::NamedTempFile::new().unwrap();
        let resolved = resolve_signing_key(&repo, None, &temp_pubkey_file)
            .expect("a configured key file path resolves");
        assert_eq!(resolved.key_file, key_path);
        assert_eq!(resolved.public_key, pubkey);
    }

    #[test]
    fn resolve_signing_key_refuses_with_no_configured_key_and_no_explicit_key() {
        // The absent-config case: `git config user.signingkey` was never set and
        // no `--key` was passed. This must refuse with actionable guidance
        // rather than trying to sign with nothing. The local config explicitly
        // clears the key (see `clear_signingkey`'s doc comment) so the
        // assertion holds regardless of what the developer's own global git
        // config carries.
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        clear_signingkey(&repo);

        let temp_pubkey_file = tempfile::NamedTempFile::new().unwrap();
        let err = resolve_signing_key(&repo, None, &temp_pubkey_file).unwrap_err();
        assert!(
            err.to_string().contains("no signing key configured"),
            "got: {err:#}"
        );
    }

    #[test]
    fn resolve_signing_key_writes_a_literal_public_key_to_a_temp_file() {
        // `git config user.signingkey` set to a literal `ssh-...` public key
        // (git's convention for an agent-backed SSH signing identity, with no
        // private key file on disk at all) is written verbatim to the caller's
        // temp file and used as `-f`; `public_key_line` recognizes the temp
        // file's content as already being a public key and returns it as-is,
        // without ever trying to derive one from a private half that does not
        // exist.
        if !git_available() || !ssh_keygen_available() {
            eprintln!("skipping: git or ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);

        // Only the public half exists; the private key backing it is never
        // written anywhere this test can see, simulating an agent-resolved
        // identity where the private key lives outside the filesystem (a
        // hardware key, or ssh-agent).
        let keys_dir = tmp.path().join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let (_key_path, pubkey) = generate_keypair(&keys_dir);
        set_signingkey(&repo, &pubkey);

        let temp_pubkey_file = tempfile::NamedTempFile::new().unwrap();
        let resolved = resolve_signing_key(&repo, None, &temp_pubkey_file)
            .expect("a literal public key resolves without needing the private half on disk");
        assert_eq!(resolved.key_file, temp_pubkey_file.path());
        assert_eq!(resolved.public_key, pubkey);
    }

    #[test]
    fn sign_and_verify_round_trip() {
        if !ssh_keygen_available() {
            eprintln!("skipping: ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (key_path, pubkey) = generate_keypair(tmp.path());

        let data = b"the bundle bytes";
        let sig = sign(&key_path, data).expect("signing succeeds");
        assert!(sig.contains(SIG_BEGIN));

        let ok = verify_signature(data, &sig, "author@example.com", &[pubkey])
            .expect("verification runs");
        assert!(ok, "a genuine signature over the exact data must verify");
    }

    #[test]
    fn verify_signature_rejects_tampered_data() {
        if !ssh_keygen_available() {
            eprintln!("skipping: ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (key_path, pubkey) = generate_keypair(tmp.path());

        let sig = sign(&key_path, b"original data").expect("signing succeeds");
        let ok = verify_signature(b"tampered data", &sig, "author@example.com", &[pubkey])
            .expect("verification runs");
        assert!(!ok, "a signature over different data must not verify");
    }

    #[test]
    fn verify_signature_rejects_a_key_not_in_allowed_signers() {
        if !ssh_keygen_available() {
            eprintln!("skipping: ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let a_dir = tmp.path().join("a");
        std::fs::create_dir_all(&a_dir).unwrap();
        let (signing_key, _signing_pubkey) = generate_keypair(&a_dir);
        let other_dir = tmp.path().join("b");
        std::fs::create_dir_all(&other_dir).unwrap();
        let (_other_key, other_pubkey) = generate_keypair(&other_dir);

        let data = b"the bundle bytes";
        let sig = sign(&signing_key, data).expect("signing succeeds");
        // Verify against a key that never signed this data.
        let ok = verify_signature(data, &sig, "author@example.com", &[other_pubkey])
            .expect("verification runs");
        assert!(!ok, "a signature must not verify against an unrelated key");
    }

    #[test]
    fn verify_signature_rejects_a_wrong_principal() {
        // `verify_signature`'s `principal` parameter both selects which
        // `allowed_signers` entries apply *and* is checked against the
        // signature's namespace-scoped identity: this test's `public_keys` are
        // only ever recorded under "author@example.com", so asking `ssh-keygen`
        // to verify them under a different principal must fail, since no
        // allowed_signers line matches that principal at all.
        if !ssh_keygen_available() {
            eprintln!("skipping: ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (key_path, pubkey) = generate_keypair(tmp.path());

        let data = b"the bundle bytes";
        let sig = sign(&key_path, data).expect("signing succeeds");

        // Confirm the key does verify under its own registered principal first,
        // so a failure below is caused by the principal mismatch, not a broken
        // signature.
        assert!(
            verify_signature(
                data,
                &sig,
                "author@example.com",
                std::slice::from_ref(&pubkey)
            )
            .expect("verification runs")
        );

        // A caller who resolved this key under "author@example.com" (as CI
        // resolves a PR author's registered keys) must not accept a claim under
        // a different principal: there is no allowed_signers entry for
        // "someone-else@example.com" naming this key, so verification fails.
        let no_keys_for_someone_else: Vec<String> = Vec::new();
        let ok = verify_signature(
            data,
            &sig,
            "someone-else@example.com",
            &no_keys_for_someone_else,
        )
        .expect("verification runs");
        assert!(
            !ok,
            "a signature verified under an unregistered principal must fail"
        );
    }

    #[test]
    fn envelope_and_split_envelope_round_trip_byte_exact() {
        let bundle_json = r#"{"kind":"bastion-attestation","schema":1}"#;
        let signature = format!("{SIG_BEGIN}\nAAAA\n-----END SSH SIGNATURE-----\n");
        let note = envelope(bundle_json, &signature);
        let (parsed_bundle, parsed_sig) = split_envelope(&note).expect("splits cleanly");
        assert_eq!(parsed_bundle, bundle_json);
        assert_eq!(parsed_sig, signature.trim_end());

        // Re-composing from the split halves reproduces the exact same note.
        let recomposed = envelope(parsed_bundle, parsed_sig);
        assert_eq!(recomposed, note);
    }

    #[test]
    fn split_envelope_rejects_a_note_without_a_signature_block() {
        let err = split_envelope("just some text, no signature here").unwrap_err();
        assert!(err.to_string().contains("no SSH signature block"));
    }

    fn sample_bundle() -> Bundle {
        Bundle {
            kind: KIND.to_string(),
            schema: SCHEMA,
            version: "0.1.0".to_string(),
            attested_at: "2026-07-02T00:00:00Z".to_string(),
            public_key: "ssh-ed25519 AAAA test@bastion.dev".to_string(),
            seal: crate::seal::seal(
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
                vec!["r1".into()],
                &[],
            ),
            events: BTreeMap::new(),
        }
    }

    #[test]
    fn bundle_round_trips_through_json() {
        let bundle = sample_bundle();
        let json = bundle.to_json().unwrap();
        let parsed = Bundle::from_json(&json).unwrap();
        assert_eq!(bundle, parsed);
    }

    #[test]
    fn bundle_from_json_rejects_wrong_kind() {
        let mut bundle = sample_bundle();
        bundle.kind = "something-else".to_string();
        let json = bundle.to_json().unwrap();
        let err = Bundle::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("bastion-attestation"));
    }

    #[test]
    fn bundle_from_json_rejects_wrong_schema() {
        let mut bundle = sample_bundle();
        bundle.schema = 99;
        let json = bundle.to_json().unwrap();
        let err = Bundle::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("schema"));
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

        let config_hash = Config::from_yaml(registry_yaml).unwrap().effective_hash();

        let resolved_events = vec![
            RunEvent::RunStarted {
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
                run: run_id.clone(),
                branch: "feature".into(),
                base: "main".into(),
                changed: 0,
                reviewers: vec![],
            }],
        )
        .unwrap();

        let mut out = Vec::new();
        let err = attest(&repo, &layout, None, None, b"secret", &mut out).unwrap_err();
        assert!(err.to_string().contains("was not sealed"));
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
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("test seam"));
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
            fixture.secret,
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("HEAD has changed"));
    }

    // -----------------------------------------------------------------------
    // Verification and replay planning
    // -----------------------------------------------------------------------

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
        };
        assert_eq!(plan.replay.len(), 2);
        assert!(plan.replay.contains_key("r1"));
        assert!(plan.replay.contains_key("r2"));
        assert!(plan.executed_fresh.is_empty());
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
        let mut bundle = Bundle::from_json(bundle_json).expect("bundle parses");

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
            AttestationOutcome::Replay(_) => {
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
        let bundle = Bundle {
            kind: KIND.to_string(),
            schema: SCHEMA,
            version: "0.0.1-a-much-older-release".to_string(),
            attested_at: "2026-07-02T00:00:00Z".to_string(),
            public_key: pubkey.clone(),
            seal: seal.clone(),
            events: sealed_events
                .iter()
                .zip(seal.reviewers.iter())
                .map(|(event, name)| (name.clone(), event.clone()))
                .collect(),
        };
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
        let bundle = Bundle {
            kind: KIND.to_string(),
            schema: SCHEMA,
            version: crate::version::VERSION.to_string(),
            attested_at: "2026-07-02T00:00:00Z".to_string(),
            public_key: pubkey.clone(),
            seal: seamed_seal.clone(),
            events: sealed_events
                .iter()
                .zip(seal.reviewers.iter())
                .map(|(event, name)| (name.clone(), event.clone()))
                .collect(),
        };
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
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
            AttestationOutcome::Replay(_) => panic!("expected a fallback"),
        }
    }

    #[test]
    fn note_for_review_falls_back_to_the_pr_head_sha() {
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
        let head_sha = String::from_utf8(
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

        // No note anywhere yet.
        assert_eq!(
            note_for_review(&repo, "HEAD", Some(&head_sha)).unwrap(),
            None
        );

        // A note on the PR head SHA (not on the literal ref "HEAD" lookup path,
        // though here they resolve to the same commit; the point is the
        // fallback path is exercised when the primary lookup misses).
        git::note_add(&repo, git::NOTES_REF, &head_sha, "bundle-v1").unwrap();
        assert_eq!(
            note_for_review(&repo, "HEAD", Some(&head_sha)).unwrap(),
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
