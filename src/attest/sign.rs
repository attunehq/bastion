//! Signing and verifying an attestation bundle, and resolving which SSH key to
//! sign with.
//!
//! `bastion attest` shells out to `ssh-keygen -Y sign`/`-Y verify` rather than
//! linking an SSH library: the signer and verifier are the same tool a
//! developer's own `git commit -S` already trusts, so there is no separate
//! trust story to build.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use color_eyre::eyre::{Context, Result, bail};

use crate::git;

/// The SSH signature namespace attestations are signed and verified under
/// (`ssh-keygen -Y sign/verify -n <namespace>`). Scoping the namespace keeps a
/// bastion attestation signature from being replayable as, say, a git commit
/// signature by the same key: `ssh-keygen` binds the namespace into what it
/// signs.
pub const SIG_NAMESPACE: &str = "bastion";

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
    // Windows OpenSSH cannot reopen a NamedTempFile while its handle is open.
    // TempPath keeps deletion-on-drop ownership after closing that handle.
    let allowed_signers = allowed_signers.into_temp_path();

    let mut sig_file =
        tempfile::NamedTempFile::new().wrap_err("creating a temporary signature file")?;
    std::io::Write::write_all(&mut sig_file, signature.as_bytes())
        .wrap_err("writing the signature file")?;
    sig_file.flush().wrap_err("flushing the signature file")?;
    let sig_file = sig_file.into_temp_path();

    let output = Command::new("ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            &allowed_signers.to_string_lossy(),
            "-I",
            principal,
            "-n",
            SIG_NAMESPACE,
            "-s",
            &sig_file.to_string_lossy(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to invoke ssh-keygen; is it installed and on PATH?")
        .and_then(|mut child| {
            #[expect(
                clippy::expect_used,
                reason = "stdin is present on a Stdio::piped() spawn"
            )]
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

    #[expect(
        clippy::expect_used,
        reason = "stdin is present on a Stdio::piped() spawn"
    )]
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
pub struct SigningKey {
    /// The path `-f` is given. May be a private key file (the ordinary case)
    /// or a public key file backed by an agent (the `git config
    /// user.signingkey` literal-public-key case).
    pub key_file: std::path::PathBuf,
    /// The single-line public key text recorded in the bundle.
    pub public_key: String,
}

/// What `git config user.signingkey` names, parsed once at the boundary rather
/// than re-sniffed by string prefix wherever the value is used.
///
/// Git overloads this single config key: it is a path to a private key file in
/// the ordinary case, but by git's own SSH-signing convention it may instead be
/// a *literal public key* (`ssh-ed25519 AAAA...`, `ecdsa-sha2-nistp256
/// AAAA...`, an `sk-*` security-key variant, and so on), naming an identity an
/// agent resolves the private half of. Parsing into this enum up front, with
/// the same recognizer [`is_public_key_text`] uses for a key *file's* content,
/// means every key type git supports (not just the `ssh-`/`sk-ssh-` prefixes an
/// earlier, narrower check matched) is classified correctly; an `ecdsa-sha2-*`
/// literal previously fell through and was misread as a file path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SigningKeySource {
    /// A path to a key file (typically a private key).
    KeyFile(std::path::PathBuf),
    /// A literal public key line, resolved via an agent.
    LiteralPublicKey(String),
}

impl SigningKeySource {
    /// Classify a `git config user.signingkey` value.
    fn parse(value: String) -> Self {
        if is_public_key_text(&value) {
            SigningKeySource::LiteralPublicKey(value)
        } else {
            SigningKeySource::KeyFile(std::path::PathBuf::from(value))
        }
    }
}

/// Resolve the signing key `bastion attest` should use, following
/// `docs/developer-guide/attestation.md` ("Signing"):
///
/// 1. `--key <path>` (`explicit_key`), if given, always wins.
/// 2. Otherwise `git config user.signingkey`, parsed by
///    `SigningKeySource::parse`: a literal public key (git's own convention
///    for an SSH-signing identity resolved via an agent) is written to a
///    temporary file and used as the `-f` argument, so `ssh-keygen` resolves
///    the matching private half from the agent; a key-file path is used
///    directly.
/// 3. Neither present: refuse with actionable guidance.
///
/// The returned [`SigningKey::public_key`] is read from the resolved private
/// key's `.pub` sibling when one exists, derived with `ssh-keygen -y`
/// otherwise, or used directly when the resolved key was already a literal
/// public key.
///
/// # Errors
///
/// Returns an error when no key is configured (no `--key` and no
/// `user.signingkey`), when a literal configured key cannot be written to the
/// temporary file, or when the public key line can be neither read from the
/// `.pub` sibling nor derived with `ssh-keygen -y`.
pub fn resolve_signing_key(
    repo_root: &Path,
    explicit_key: Option<&Path>,
    temp_pubkey_file: &tempfile::NamedTempFile,
) -> Result<SigningKey> {
    let key_file = match explicit_key {
        Some(path) => path.to_path_buf(),
        None => {
            let configured = git::run_git_config_signingkey(repo_root);
            match configured.map(SigningKeySource::parse) {
                Some(SigningKeySource::LiteralPublicKey(value)) => {
                    std::fs::write(temp_pubkey_file.path(), format!("{value}\n"))
                        .wrap_err("writing the configured signing key to a temporary file")?;
                    temp_pubkey_file.path().to_path_buf()
                }
                Some(SigningKeySource::KeyFile(path)) => path,
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

    // A private key's public sibling is its full name plus `.pub`. For a key whose
    // name carries a dotted suffix (`k.pem` -> `k.pem.pub`), `with_extension` does
    // that. For the conventional extensionless name (`id_ed25519`), `with_extension`
    // would replace rather than append, so build that form by hand.
    let pub_sibling = match key_file.extension() {
        Some(ext) => key_file.with_extension(format!("{}.pub", ext.to_string_lossy())),
        None => {
            let mut name = key_file.as_os_str().to_os_string();
            name.push(".pub");
            std::path::PathBuf::from(name)
        }
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
/// itself a public key rather than a private key or a file path.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Generate a throwaway ecdsa keypair at `<dir>/id`, returning
    /// `(private_key_path, public_key_line)`. Used to exercise the
    /// `ecdsa-sha2-*` literal-public-key recognition path, distinct from the
    /// `ssh-`/`sk-ssh-` prefixes an earlier, narrower check matched.
    fn generate_ecdsa_keypair(dir: &Path) -> (std::path::PathBuf, String) {
        let key_path = dir.join("id-ecdsa");
        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ecdsa",
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
            "ssh-keygen ecdsa keygen failed: {}",
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
    fn resolve_signing_key_recognizes_a_literal_ecdsa_public_key() {
        // `ecdsa-sha2-*` is a distinct prefix family from `ssh-`/`sk-ssh-`. A
        // narrower check that only recognized those two prefixes would fall
        // through here and misread this literal public key as a file path,
        // then fail trying to read a file that does not exist.
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
        let (_key_path, pubkey) = generate_ecdsa_keypair(&keys_dir);
        assert!(
            pubkey.starts_with("ecdsa-sha2-"),
            "sanity: generated key should be ecdsa, got {pubkey}"
        );
        set_signingkey(&repo, &pubkey);

        let temp_pubkey_file = tempfile::NamedTempFile::new().unwrap();
        let resolved = resolve_signing_key(&repo, None, &temp_pubkey_file)
            .expect("a literal ecdsa public key resolves without needing the private half");
        assert_eq!(resolved.key_file, temp_pubkey_file.path());
        assert_eq!(resolved.public_key, pubkey);
    }

    #[test]
    fn verify_signature_round_trips_with_ephemeral_files() {
        if !ssh_keygen_available() {
            eprintln!("skipping: ssh-keygen not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (key_path, pubkey) = generate_keypair(tmp.path());

        let data = b"the bundle bytes";
        let sig = sign(&key_path, data).expect("signing succeeds");
        assert!(sig.contains("-----BEGIN SSH SIGNATURE-----"));

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
}
