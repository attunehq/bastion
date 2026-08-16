//! The run seal: tamper evidence for a persisted run.
//!
//! A verdict is a judgment about a changeset under a policy, so CI can reuse a
//! persisted run only if the run proves it reviewed the claimed changeset under
//! the claimed policy. The seal is an HMAC-SHA256, keyed by a secret embedded in
//! the binary at build time, over a canonical digest of the committed HEAD tree,
//! the merge-base tree, the `base..HEAD` patch-id, the effective config hash, the
//! seam and dirty flags, and the resolved reviewer events. See
//! `docs/developer-guide/attestation.md` (the run seal) for the full design; this
//! module implements the seal.
//!
//! A dirty run (uncommitted or untracked changes present when it ran) is sealed
//! with `dirty: true`, but attestation refuses to attest it: a green review over
//! content that never landed in a commit says nothing about the committed tree
//! the seal otherwise binds.
//!
//! A keyed MAC rather than an asymmetric signature is deliberate: the sealer and
//! the verifier are the same binary on both ends (the local `bastion` that sealed
//! the run, and the `bastion` in CI that later verifies it), so a keypair would
//! ship both halves in the same artifact and add nothing a shared secret does not
//! already provide.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// The concrete HMAC type this module seals and verifies with.
type HmacSha256 = Hmac<Sha256>;

/// The git- and config-derived values a run's seal binds, gathered by the caller
/// (`bastion review` locally; `bastion attest` and CI replay reuse the same shape)
/// and threaded into [`crate::runner::ExecContext`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealBindings {
    /// The git tree hash of HEAD.
    pub head_tree: String,
    /// The git tree hash of the merge base.
    pub base_tree: String,
    /// The `git patch-id --stable` of the diff `base..HEAD`.
    pub patch_id: String,
    /// The effective repo-registry hash ([`crate::config::Config::effective_hash`]).
    pub config_hash: String,
    /// Names of the repository reviewers eligible to be sealed. A resolved event
    /// for a reviewer outside this set (a user-level-only reviewer, say) is
    /// excluded from the seal: attestation never covers a personal reviewer.
    pub repo_reviewers: std::collections::BTreeSet<String>,
}

/// The inputs a seal binds, in their canonical serialization order.
///
/// Field order here *is* the canonical form: both the sealer and the verifier
/// are the same binary, so there is no cross-implementation encoding to agree on,
/// and `serde_json`'s field-order-preserving struct serialization is enough to
/// make the digest reproducible.
#[derive(Serialize)]
struct SealInput<'a> {
    version: &'a str,
    head_tree: &'a str,
    base_tree: &'a str,
    patch_id: &'a str,
    config_hash: &'a str,
    seams: bool,
    /// Whether the working tree carried uncommitted or untracked changes when the
    /// run reviewed it. Placed next to `seams`: both are process-state flags the
    /// seal binds so a run under non-ordinary conditions is recorded and later
    /// refused by `bastion attest`.
    dirty: bool,
    /// The sealed `reviewer.resolved` events, sorted by reviewer name so the
    /// digest does not depend on completion order.
    events: &'a [serde_json::Value],
}

/// A persisted, tamper-evident record of what a run reviewed and concluded.
///
/// Stored alongside a run (`store::write_seal`/`read_seal`) so a later `bastion
/// attest` or CI replay can re-derive the same `SealInput` and check `mac`
/// against it. Every field here is also a `SealInput` field except `mac`
/// itself and `reviewers` (the sealed reviewer *names*, kept for a human-readable
/// record; the events they resolved are re-read from the run store to recompute
/// the digest, not carried in the seal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seal {
    /// The `bastion` version that produced this seal.
    pub version: String,
    /// The git tree hash of HEAD at seal time.
    pub head_tree: String,
    /// The git tree hash of the merge base at seal time.
    pub base_tree: String,
    /// The `git patch-id --stable` of the diff `base..HEAD`.
    pub patch_id: String,
    /// The effective repo-registry hash at seal time.
    pub config_hash: String,
    /// Whether any test seam (a `BASTION_*_BIN` backend override, or the
    /// container-engine override) was active during the run. `bastion attest`
    /// refuses to attest a sealed run with this set: a run against a stubbed
    /// reviewer exercised the binary, but not a real review.
    pub seams: bool,
    /// Whether the working tree carried uncommitted or untracked changes,
    /// sampled both before reviewers ran and again at seal time (the run is
    /// dirty if either sample was, since a reviewer can dirty the tree mid-run).
    /// `bastion review` reviews the working tree, but the rest of this seal
    /// binds only HEAD's *committed* tree and the `base..HEAD` patch-id, so a
    /// dirty run's reviewers may have judged content those bindings never name.
    /// `bastion attest` refuses to attest a sealed run with this set, with a
    /// plain reason: commit the final content, re-run `bastion review`, and
    /// attest that run instead.
    pub dirty: bool,
    /// The sorted names of the repository reviewers this seal covers.
    pub reviewers: Vec<String>,
    /// The lowercase-hex HMAC-SHA256 over the `SealInput` this seal was built
    /// from.
    pub mac: String,
}

/// The sealing secret embedded into this binary at build time.
///
/// Each release embeds a secret generated by the release workflow and shared by
/// every platform binary of that release, so a bundle sealed by one platform's
/// binary verifies under any other platform's binary of the same release. A
/// locally compiled binary embeds a random per-build secret instead (cached
/// across incremental rebuilds in `build.rs`), so a dev build can seal runs only
/// for itself.
///
/// The embedded string is already the key material: it is hex text at rest (so
/// `--version`-adjacent tooling and `strings` on the binary see a printable
/// value, never raw bytes that could look like binary garbage or, worse, embed
/// a stray null byte), but HMAC keys are just bytes, and there is no reason to
/// decode the hex back to 16 raw bytes before keying: doing so would only
/// *halve* the effective key material for no benefit. Keying on the raw ASCII
/// bytes of the embedded string keeps the full 64-plus characters of entropy
/// and needs no fallible decode step here.
#[must_use]
pub fn embedded_secret() -> &'static [u8] {
    env!("BASTION_SEAL_SECRET").as_bytes()
}

/// Whether any of the test seams were active in the current process
/// environment: the five backend program overrides
/// ([`crate::backend::claude_code::PROGRAM_ENV`],
/// [`crate::backend::codex::PROGRAM_ENV`], [`crate::backend::pi::PROGRAM_ENV`],
/// [`crate::backend::grok::PROGRAM_ENV`], [`crate::backend::muse::PROGRAM_ENV`])
/// or the container-engine override
/// ([`crate::backend::container::ENGINE_ENV`]).
///
/// A run that used any of these exercised the binary for real, but not a real
/// review: `bastion attest` refuses to attest such a run. This only records
/// whether a seam was active on the sealer's box; it says nothing about the
/// reviewer process beyond that environment fact.
#[must_use]
pub fn seams_active() -> bool {
    seams_active_from(|name| std::env::var_os(name).is_some())
}

/// [`seams_active`] with an injectable environment lookup, so tests can assert
/// the seam set without mutating the real process environment (which is process-
/// global and unsafe to touch from parallel tests).
#[must_use]
pub fn seams_active_from(lookup: impl Fn(&str) -> bool) -> bool {
    [
        crate::backend::claude_code::PROGRAM_ENV,
        crate::backend::codex::PROGRAM_ENV,
        crate::backend::pi::PROGRAM_ENV,
        crate::backend::grok::PROGRAM_ENV,
        crate::backend::muse::PROGRAM_ENV,
        crate::backend::container::ENGINE_ENV,
    ]
    .into_iter()
    .any(lookup)
}

/// Build a [`SealInput`] from a run's bindings and its sealed `reviewer.resolved`
/// events (already filtered to the repo reviewers and sorted by name).
fn seal_input<'a>(
    version: &'a str,
    bindings: &'a SealBindings,
    seams: bool,
    dirty: bool,
    events: &'a [serde_json::Value],
) -> SealInput<'a> {
    SealInput {
        version,
        head_tree: &bindings.head_tree,
        base_tree: &bindings.base_tree,
        patch_id: &bindings.patch_id,
        config_hash: &bindings.config_hash,
        seams,
        dirty,
        events,
    }
}

/// Seal a run: compute the HMAC-SHA256 over the canonical digest and return the
/// persisted [`Seal`].
///
/// `reviewers` are the sorted repo-reviewer names the sealed `events` resolved
/// (already filtered and ordered by the caller); `events` are their
/// `reviewer.resolved` events serialized to [`serde_json::Value`] in that same
/// order, so the digest and the persisted `reviewers` list stay in agreement.
///
/// # Panics
///
/// Never panics on ordinary input; the digest is built from already-serializable
/// values.
#[must_use]
pub fn seal(
    secret: &[u8],
    version: &str,
    bindings: &SealBindings,
    seams: bool,
    dirty: bool,
    reviewers: Vec<String>,
    events: &[serde_json::Value],
) -> Seal {
    let input = seal_input(version, bindings, seams, dirty, events);
    let mac = mac_hex(secret, &input);
    Seal {
        version: version.to_string(),
        head_tree: bindings.head_tree.clone(),
        base_tree: bindings.base_tree.clone(),
        patch_id: bindings.patch_id.clone(),
        config_hash: bindings.config_hash.clone(),
        seams,
        dirty,
        reviewers,
        mac,
    }
}

/// Verify a persisted [`Seal`] against the `events` it claims to cover.
///
/// Recomputes the digest from the seal's own recorded fields (not from a
/// caller-supplied [`SealBindings`]: comparing those recorded fields against
/// the *current* repository state is `bastion attest`'s separate
/// re-derivation check) plus `events`, and constant-time
/// compares the MAC. Returns `false` on any mismatch, including a MAC that
/// fails to hex-decode.
#[must_use]
pub fn verify(secret: &[u8], seal: &Seal, events: &[serde_json::Value]) -> bool {
    // Build the input straight from the seal's own borrowed fields: `SealInput`
    // never carries `repo_reviewers`, so reconstructing a `SealBindings` here would
    // clone four strings and rebuild a set the digest does not depend on.
    let input = SealInput {
        version: &seal.version,
        head_tree: &seal.head_tree,
        base_tree: &seal.base_tree,
        patch_id: &seal.patch_id,
        config_hash: &seal.config_hash,
        seams: seal.seams,
        dirty: seal.dirty,
        events,
    };
    let Ok(bytes) = hex::decode(&seal.mac) else {
        return false;
    };
    let Ok(mut hmac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    hmac.update(&canonical_bytes(&input));
    hmac.verify_slice(&bytes).is_ok()
}

/// Compute the lowercase-hex HMAC-SHA256 over `input`'s canonical serialization.
fn mac_hex(secret: &[u8], input: &SealInput<'_>) -> String {
    // An HMAC key may be any length (RFC 2104 recommends hashing an over-long key
    // down first, which `Mac::new_from_slice` already does), so a
    // `new_from_slice` failure here is not something ordinary input can trigger.
    #[expect(
        clippy::expect_used,
        reason = "HMAC-SHA256 accepts a key of any length"
    )]
    let mut hmac =
        HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts a key of any length");
    hmac.update(&canonical_bytes(input));
    hex::encode(hmac.finalize().into_bytes())
}

/// The canonical byte form a [`SealInput`] is MAC'd over: its `serde_json`
/// serialization. Field order in the struct definition is the canonical form
/// (documented on [`SealInput`]); this is the one place that serialization
/// happens, so sealing and verification can never disagree about it.
fn canonical_bytes(input: &SealInput<'_>) -> Vec<u8> {
    #[expect(
        clippy::expect_used,
        reason = "SealInput serializes: every field is already JSON-safe"
    )]
    serde_json::to_vec(input).expect("SealInput serializes: every field is already JSON-safe")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings() -> SealBindings {
        SealBindings {
            head_tree: "head-tree-abc".into(),
            base_tree: "base-tree-def".into(),
            patch_id: "patch-123".into(),
            config_hash: "config-hash-xyz".into(),
            repo_reviewers: ["r1".to_string()].into_iter().collect(),
        }
    }

    fn events() -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": "reviewer.resolved",
            "reviewer": "r1",
            "verdict": "pass",
            "summary": "looks fine",
        })]
    }

    #[test]
    fn round_trips_seal_and_verify() {
        let secret = b"test-secret";
        let s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        assert!(verify(secret, &s, &events()));
    }

    #[test]
    fn perturbing_head_tree_fails_verification() {
        let secret = b"test-secret";
        let mut s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        s.head_tree = "different-tree".into();
        assert!(!verify(secret, &s, &events()));
    }

    #[test]
    fn perturbing_base_tree_fails_verification() {
        let secret = b"test-secret";
        let mut s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        s.base_tree = "different-tree".into();
        assert!(!verify(secret, &s, &events()));
    }

    #[test]
    fn perturbing_patch_id_fails_verification() {
        let secret = b"test-secret";
        let mut s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        s.patch_id = "different-patch".into();
        assert!(!verify(secret, &s, &events()));
    }

    #[test]
    fn perturbing_config_hash_fails_verification() {
        let secret = b"test-secret";
        let mut s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        s.config_hash = "different-hash".into();
        assert!(!verify(secret, &s, &events()));
    }

    #[test]
    fn perturbing_seams_flag_fails_verification() {
        let secret = b"test-secret";
        let s = seal(
            secret,
            "0.1.0",
            &bindings(),
            true,
            false,
            vec!["r1".into()],
            &events(),
        );
        assert!(verify(secret, &s, &events()));

        let s_false = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        // Same everything else, different seams flag: different MAC entirely, and
        // cross-verifying with the wrong flag fails.
        assert_ne!(s.mac, s_false.mac);
    }

    #[test]
    fn perturbing_dirty_flag_fails_verification() {
        // A run sealed as dirty must not verify against a seal claiming a clean
        // tree: attestation's dirty refusal (`src/attest.rs`) depends on `dirty`
        // being load-bearing in the digest, not decorative metadata a verifier
        // could silently drop.
        let secret = b"test-secret";
        let s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            true,
            vec!["r1".into()],
            &events(),
        );
        assert!(verify(secret, &s, &events()));

        let mut tampered = s.clone();
        tampered.dirty = false;
        assert!(
            !verify(secret, &tampered, &events()),
            "flipping dirty after sealing must fail verification"
        );
    }

    #[test]
    fn perturbing_an_events_summary_fails_verification() {
        let secret = b"test-secret";
        let s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        let mut tampered = events();
        tampered[0]["summary"] = serde_json::Value::String("a different summary".into());
        assert!(!verify(secret, &s, &tampered));
    }

    #[test]
    fn a_different_secret_fails_verification() {
        let s = seal(
            b"secret-a",
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        assert!(!verify(b"secret-b", &s, &events()));
    }

    #[test]
    fn hex_encoding_is_stable_and_lowercase() {
        let secret = b"test-secret";
        let s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        assert_eq!(s.mac, s.mac.to_lowercase());
        assert_eq!(s.mac.len(), 64, "HMAC-SHA256 is 32 bytes, 64 hex chars");
        // Recomputing from the same inputs gives the identical MAC.
        let s2 = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        assert_eq!(s.mac, s2.mac);
    }

    #[test]
    fn seams_active_from_reflects_any_seam_env_var() {
        assert!(!seams_active_from(|_| false));
        assert!(seams_active_from(
            |name| name == crate::backend::claude_code::PROGRAM_ENV
        ));
        assert!(seams_active_from(
            |name| name == crate::backend::codex::PROGRAM_ENV
        ));
        assert!(seams_active_from(
            |name| name == crate::backend::pi::PROGRAM_ENV
        ));
        assert!(seams_active_from(
            |name| name == crate::backend::grok::PROGRAM_ENV
        ));
        assert!(seams_active_from(
            |name| name == crate::backend::muse::PROGRAM_ENV
        ));
        assert!(seams_active_from(
            |name| name == crate::backend::container::ENGINE_ENV
        ));
    }

    #[test]
    fn a_malformed_mac_fails_verification_rather_than_panicking() {
        let secret = b"test-secret";
        let mut s = seal(
            secret,
            "0.1.0",
            &bindings(),
            false,
            false,
            vec!["r1".into()],
            &events(),
        );
        s.mac = "not-hex-at-all!!".into();
        assert!(!verify(secret, &s, &events()));
    }
}
