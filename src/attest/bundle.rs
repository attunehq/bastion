//! The attestation bundle: its shape, and the envelope it travels in as a git
//! note.
//!
//! A bundle is the signed record `bastion attest` produces
//! ([`crate::attest::attest`]) and CI replay consumes
//! ([`crate::attest::replay::plan`]). This module owns only the bundle's data
//! shape and its serialization to and from a note's raw text; signing and
//! verification live in [`mod@crate::attest::sign`].

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};

use crate::seal::Seal;

/// The bundle `kind` every attestation carries. A note whose `kind` differs was
/// never produced by this module, so [`Bundle::from_json`] rejects it outright
/// rather than trying to interpret a foreign shape.
const KIND: &str = "bastion-attestation";

/// The bundle schema version this binary produces and accepts. Bumped only on a
/// breaking bundle-shape change; [`Bundle::from_json`] refuses any other value
/// rather than guessing at a migration.
const SCHEMA: u32 = 1;

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
        use color_eyre::eyre::Context;
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
        use color_eyre::eyre::Context;
        serde_json::to_string(self).wrap_err("serializing attestation bundle")
    }

    /// Build a bundle carrying `KIND` and `SCHEMA`, for callers assembling one
    /// from scratch ([`crate::attest::attest`]).
    #[must_use]
    pub fn new(
        version: String,
        attested_at: String,
        public_key: String,
        seal: Seal,
        events: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Bundle {
            kind: KIND.to_string(),
            schema: SCHEMA,
            version,
            attested_at,
            public_key,
            seal,
            events,
        }
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
/// Parses by structure, not by searching for a marker anywhere in the text: an
/// envelope is exactly one JSON line (a bundle's `to_json` always emits compact,
/// single-line JSON) followed by the armored signature block. This splits at the
/// *first* newline and requires the remainder, once trimmed, to begin with
/// `SIG_BEGIN`. Searching for that marker anywhere in the text (the prior
/// approach) is unsound: a bundle's `events` carry untrusted reviewer-authored
/// finding text, and a finding whose detail happens to contain the literal
/// marker string would split a perfectly valid bundle at the wrong point.
///
/// The bundle half's trailing newline (the join character [`envelope`]
/// inserted) is trimmed so the round trip is byte-exact:
/// `split_envelope(&envelope(json, sig)) == (json, sig)`.
///
/// # Errors
///
/// Returns an error if the text carries no first-line/signature-block shape:
/// no newline at all, or a remainder that does not begin with
/// `-----BEGIN SSH SIGNATURE-----`.
pub fn split_envelope(note: &str) -> Result<(&str, &str)> {
    let (bundle_part, rest) = note
        .split_once('\n')
        .ok_or_else(|| eyre!("note carries no signature block; not a bastion attestation"))?;
    let sig_part = rest.trim_start();
    if !sig_part.starts_with(SIG_BEGIN) {
        bail!("note carries no SSH signature block; not a bastion attestation");
    }
    Ok((bundle_part, sig_part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::SealBindings;
    use std::collections::BTreeMap;

    fn sample_bundle() -> Bundle {
        Bundle::new(
            "0.1.0".to_string(),
            "2026-07-02T00:00:00Z".to_string(),
            "ssh-ed25519 AAAA test@bastion.dev".to_string(),
            crate::seal::seal(
                b"test-secret",
                "0.1.0",
                &SealBindings {
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
            ),
            BTreeMap::new(),
        )
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
        assert!(err.to_string().contains("no signature block"));
    }

    #[test]
    fn split_envelope_round_trips_when_a_finding_detail_contains_the_marker_text() {
        // A bundle's `events` carry untrusted reviewer-authored finding text. A
        // finding whose detail contains the literal `-----BEGIN SSH SIGNATURE-----`
        // string must not confuse the split: the envelope is parsed by structure
        // (first newline, then the signature block), never by searching for the
        // marker anywhere in the text.
        let bundle_json = serde_json::json!({
            "kind": "bastion-attestation",
            "schema": 1,
            "finding": "the detail says -----BEGIN SSH SIGNATURE----- right here",
        })
        .to_string();
        let signature = format!("{SIG_BEGIN}\nAAAA\n-----END SSH SIGNATURE-----\n");
        let note = envelope(&bundle_json, &signature);

        let (parsed_bundle, parsed_sig) = split_envelope(&note).expect("splits cleanly");
        assert_eq!(parsed_bundle, bundle_json);
        assert_eq!(parsed_sig, signature.trim_end());
    }
}
