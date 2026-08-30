//! Durable agent-conversation references for incremental review.
//!
//! Carry avoids an agent launch when a prior pass still describes the current
//! changeset. When a reviewer must execute again, this module lets it continue
//! the newest compatible backend conversation instead of starting over. A
//! reference is only a hint: session files and provider-side state are often
//! absent in CI, so every backend falls back to a fresh conversation when resume
//! fails.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use crate::backend::concrete_backend;
use crate::reviewer::{Backend, Reviewer};
use crate::verdict::Usage;

/// A non-empty backend conversation identifier parsed from agent output.
///
/// Backend CLIs call this value a session id or thread id. Bastion treats those
/// spellings as one concept because every backend performs the same operation:
/// submit another turn to an existing conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationId(String);

impl ConversationId {
    /// Parse a backend-reported identifier, returning `None` for an empty value.
    #[must_use]
    pub fn parse(raw: impl Into<String>) -> Option<Self> {
        let raw = raw.into();
        let raw = raw.trim();
        (!raw.is_empty()).then(|| Self(raw.to_string()))
    }

    /// Borrow the backend-native identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ConversationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConversationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).ok_or_else(|| de::Error::custom("conversation id must not be empty"))
    }
}

/// The durable information needed to continue one reviewer's agent conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRef {
    backend: Backend,
    id: ConversationId,
    reviewer_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_dir: Option<PathBuf>,
    /// The backend's cumulative session usage at the end of this run, when its
    /// protocol reports cumulative rather than per-turn totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cumulative_usage: Option<Usage>,
}

impl ConversationRef {
    /// Build a reference from a backend-reported id.
    ///
    /// Returns `None` when the backend omitted the id or reported an empty one;
    /// the review verdict remains valid, but there is no conversation to resume.
    #[must_use]
    pub fn from_backend(
        backend: Backend,
        id: impl Into<String>,
        reviewer: &Reviewer,
        session_dir: Option<&Path>,
        cumulative_usage: Option<Usage>,
    ) -> Option<Self> {
        Some(Self {
            backend: concrete_backend(backend),
            id: ConversationId::parse(id)?,
            reviewer_hash: reviewer_hash(reviewer),
            session_dir: session_dir.map(Path::to_path_buf),
            cumulative_usage,
        })
    }

    /// Whether this reference was produced by the current reviewer's effective
    /// backend and profile.
    #[must_use]
    pub fn is_compatible(&self, reviewer: &Reviewer) -> bool {
        self.backend == concrete_backend(reviewer.backend)
            && self.reviewer_hash == reviewer_hash(reviewer)
    }

    /// Whether the reference's explicitly isolated session storage is present.
    ///
    /// A reference without an isolated directory may live in the backend's
    /// default store or provider-side state, so availability can only be learned
    /// by attempting resume.
    #[must_use]
    pub fn storage_is_available(&self) -> bool {
        self.session_dir.as_ref().is_none_or(|dir| dir.is_dir())
    }

    /// Borrow the backend-native conversation id.
    #[must_use]
    pub fn id(&self) -> &ConversationId {
        &self.id
    }

    /// Borrow the isolated session directory, when the conversation uses one.
    #[must_use]
    pub fn session_dir(&self) -> Option<&Path> {
        self.session_dir.as_deref()
    }

    /// Cumulative usage reported at the end of the prior conversation turn.
    #[must_use]
    pub fn cumulative_usage(&self) -> Option<Usage> {
        self.cumulative_usage
    }
}

/// Hash the effective reviewer definition that determines conversation
/// compatibility. A change to any reviewer field starts a fresh conversation.
fn reviewer_hash(reviewer: &Reviewer) -> String {
    #[expect(
        clippy::expect_used,
        reason = "a fully loaded reviewer has no fallible serialization path"
    )]
    let bytes = serde_json::to_vec(reviewer).expect("loaded reviewers serialize");
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::reviewer::{Capabilities, Mode, Trigger};

    fn reviewer() -> Reviewer {
        Reviewer {
            name: "conversation-continuity".into(),
            trigger: Trigger::Paths(vec!["src/**".into()]),
            mode: Mode::Gate,
            backend: Backend::Codex,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: BTreeMap::new(),
            capabilities: Capabilities::default(),
            inputs: BTreeMap::new(),
            attestation: None,
            prompt: "Review the current changeset.".into(),
        }
    }

    #[test]
    fn conversation_ids_parse_once_at_the_storage_boundary() {
        assert!(ConversationId::parse("").is_none());
        assert!(ConversationId::parse("   ").is_none());
        assert_eq!(
            ConversationId::parse("  thread-42  ").map(|id| id.as_str().to_string()),
            Some("thread-42".into())
        );

        let malformed = r#"{"backend":"codex","id":"","reviewer_hash":"hash"}"#;
        assert!(serde_json::from_str::<ConversationRef>(malformed).is_err());
    }

    #[test]
    fn compatibility_binds_the_backend_and_effective_reviewer_profile() {
        let reviewer = reviewer();
        let reference =
            ConversationRef::from_backend(Backend::Codex, "thread-42", &reviewer, None, None)
                .expect("non-empty session id");
        assert!(reference.is_compatible(&reviewer));

        let mut changed = reviewer.clone();
        changed.prompt = "Review a different concern.".into();
        assert!(!reference.is_compatible(&changed));

        changed = reviewer;
        changed.backend = Backend::ClaudeCode;
        assert!(!reference.is_compatible(&changed));
    }

    #[test]
    fn an_explicit_session_directory_must_still_exist() {
        let reviewer = reviewer();
        let tmp = tempfile::tempdir().unwrap();
        let present = ConversationRef::from_backend(
            Backend::Codex,
            "thread-present",
            &reviewer,
            Some(tmp.path()),
            None,
        )
        .unwrap();
        assert!(present.storage_is_available());

        let missing = ConversationRef::from_backend(
            Backend::Codex,
            "thread-missing",
            &reviewer,
            Some(&tmp.path().join("missing")),
            None,
        )
        .unwrap();
        assert!(!missing.storage_is_available());
    }
}
