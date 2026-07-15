//! The reviewer schema: the declarative definition of a single-concern reviewer.
//!
//! A reviewer is a bundle of *prompt + trigger + mode + backend + capabilities +
//! (optional) environment*: its execution profile. Reviewers are declarative and
//! static so they stay reviewable and produce a stable trigger set; see
//! `docs/developer-guide/design.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Whether a reviewer gates the merge or only advises.
///
/// A `Gate` decides the merge: all gates must pass. An `Advisor` always
/// functionally passes and contributes findings without blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Mode {
    /// Blocks the merge unless it passes.
    Gate,
    /// Comments but never blocks.
    Advisor,
}

impl Mode {
    /// The lowercase wire form (`"gate"` / `"advisor"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Gate => "gate",
            Mode::Advisor => "advisor",
        }
    }
}

/// The agent harness a reviewer runs on.
///
/// `Any` lets Bastion choose; the named variants pin a specific harness, e.g.
/// because a subscription's terms require it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Backend {
    /// Bastion picks an available backend.
    #[default]
    Any,
    /// Anthropic's Claude Code CLI.
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
    /// The Pi harness.
    Pi,
}

impl Backend {
    /// The wire form of the backend name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Any => "any",
            Backend::ClaudeCode => "claude-code",
            Backend::Codex => "codex",
            Backend::Pi => "pi",
        }
    }
}

/// A reviewer's opt-out of attestation replay.
///
/// Absent (the default) means the reviewer is replayable: a CI run may honor a
/// verified local attestation instead of executing it fresh
/// (`docs/developer-guide/attestation.md`). `Never` means CI must execute this
/// reviewer itself on every run regardless of any attestation, for a gate a team
/// wants continuously re-verified (a security-sensitive check, say, where the
/// team wants CI's own execution environment in the loop every time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AttestationPolicy {
    /// This reviewer is never replayed from an attestation; CI always executes
    /// it fresh.
    Never,
}

impl AttestationPolicy {
    /// The lowercase wire form (`"never"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AttestationPolicy::Never => "never",
        }
    }
}

/// A backend-specific model identifier, forwarded verbatim to the backend's model
/// selector (`--model` for Claude Code, `-m`/`--model` for Codex).
///
/// Kept opaque on purpose: a model id means something only to the backend it
/// names (an alias like `opus`, a full id like `gpt-5`, or Pi's provider-bearing
/// `provider/id` form like `openai-codex/gpt-5.5`), so Bastion neither parses nor
/// validates it beyond requiring a pinned backend (the registry rejects a model
/// under `backend: any`). Parse, don't validate: the registry boundary produces
/// this newtype once, and the rest of the code passes it through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    /// Borrow the underlying identifier for handing to a backend CLI.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A reasoning-effort level, forwarded verbatim to the backend's effort control
/// (`--effort` for Claude Code, `model_reasoning_effort` for Codex, `--thinking`
/// for Pi).
///
/// Kept opaque, like [`ModelId`]: Bastion does not parse or remap the value, so a
/// reviewer can use whatever vocabulary its backend accepts. Claude Code takes
/// `low`/`medium`/`high`/`xhigh`/`max`; Codex takes `minimal`/`low`/`medium`/`high`;
/// Pi takes `off`/`minimal`/`low`/`medium`/`high`/`xhigh`. The shared
/// `low`/`medium`/`high` levels are portable across all three; the backend-specific
/// ones are not. Absent, the house default [`DEFAULT_EFFORT`] applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Effort(String);

impl Effort {
    /// Borrow the underlying level for handing to a backend CLI.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Bastion's house default reasoning effort, applied when a reviewer (and the
/// registry default) set none. `high` is accepted by every effort-aware backend
/// (Claude Code, Codex, and Pi).
pub const DEFAULT_EFFORT: &str = "high";

/// Capabilities a reviewer opts into. Least privilege is the default: an empty
/// block grants nothing beyond the checkout and the model provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Capabilities {
    /// General outbound network beyond the model provider. In a container this is the
    /// only egress tier Bastion can provision today, so `true` is required to run there
    /// (it grants general egress); the default `false` fails closed in a container,
    /// because provider-only scoped egress (an allowlisting proxy) is unbuilt.
    #[serde(default)]
    pub network: bool,
    /// MCP servers to load into the agent's context and permit it to call.
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Skills to load into the agent's context.
    #[serde(default)]
    pub skills: Vec<String>,
}

impl Capabilities {
    /// Whether this is the default least-privilege profile (no opt-ins).
    #[must_use]
    pub fn is_least_privilege(&self) -> bool {
        !self.network && self.mcp.is_empty() && self.skills.is_empty()
    }
}

/// How a reviewer's execution environment is provisioned. Absent means the
/// reviewer runs native/in-process on the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RunnerSpec {
    /// A Dockerfile to build the environment from. Takes precedence over `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    /// A pre-built image to run, as an alternative to `dockerfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// Where a reviewer's prompt text comes from, as written in the registry file.
///
/// The registry accepts either the prompt written inline (the common case) or a
/// `{file: <path>}` mapping naming a file (markdown, typically) whose whole
/// content is the prompt, resolved relative to the registry file that declares
/// the reviewer. This type exists only at the parse boundary: loading resolves
/// it into the plain `String` the rest of the system sees
/// ([`crate::config::Config::load`]), so a [`Reviewer<String>`] always carries
/// the actual prompt text and everything downstream (hashing, sealing, the
/// backends) is oblivious to where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    untagged,
    expecting = "an inline prompt string or a `{file: <path>}` mapping"
)]
pub enum PromptSource {
    /// The prompt text written directly in the registry file.
    Inline(String),
    /// A reference to a file whose content is the prompt.
    File(PromptFile),
}

/// The `{file: <path>}` form of a prompt: a reference to a file whose whole
/// content is the review instruction.
///
/// A separate struct (rather than an inline enum variant) so
/// `deny_unknown_fields` applies: inside an untagged enum a struct variant would
/// otherwise silently swallow a typoed sibling key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptFile {
    /// Path to the prompt file, resolved relative to the registry file that
    /// declares the reviewer (absolute paths are used as-is).
    pub file: PathBuf,
}

/// How Bastion decides whether a reviewer belongs in a changeset's plan.
///
/// The sequence form is the existing path-only trigger. The tagged `agent`
/// form first applies its optional path prefilter, then asks a small agent to
/// decide whether the full reviewer is relevant to the actual changeset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Trigger {
    /// Run whenever any changed path matches one of these globs.
    Paths(Vec<String>),
    /// Use an agent decision after an optional path prefilter.
    Agent(AgentTrigger),
}

impl Trigger {
    /// The cheap path prefilter, when this trigger has one.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        match self {
            Self::Paths(paths) => paths,
            Self::Agent(agent) => &agent.paths,
        }
    }

    /// The agent profile when semantic routing is enabled.
    #[must_use]
    pub fn agent(&self) -> Option<&AgentTrigger> {
        match self {
            Self::Paths(_) => None,
            Self::Agent(agent) => Some(agent),
        }
    }
}

impl From<Vec<String>> for Trigger {
    fn from(paths: Vec<String>) -> Self {
        Self::Paths(paths)
    }
}

/// The execution profile for one semantic routing decision.
///
/// This is deliberately smaller than a reviewer profile. A trigger can select
/// a backend, model, effort, and timeout, but it cannot opt into capabilities,
/// environment variables, or a container. Its job is only to inspect the
/// changeset and decide whether the full reviewer applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTrigger {
    /// Tagged-union discriminator. Deserialization only accepts `agent`.
    pub kind: AgentTriggerKind,
    /// The routing instruction. Bastion adds the response contract and the
    /// requirement to inspect the actual changeset.
    pub prompt: String,
    /// The harness used for the routing decision.
    #[serde(default)]
    pub backend: Backend,
    /// The backend-specific model identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// The backend-specific reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// A wall-clock timeout for the routing decision.
    #[serde(
        default,
        with = "humantime_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    /// Optional cheap prefilter. The agent runs only when one of these globs
    /// matches; an empty list considers every non-empty changeset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// The only supported tagged trigger kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTriggerKind {
    /// A model decides whether the reviewer applies.
    Agent,
}

/// A single reviewer definition, as parsed from the registry file.
///
/// Trigger globs are kept as raw strings here; they are compiled into a matcher
/// by [`crate::routing`] (parse-don't-validate: the compiled form is a distinct
/// type produced once, at the boundary).
///
/// The `P` parameter is the prompt representation: [`PromptSource`] straight out
/// of the YAML (inline text or a file reference), `String` once loading has
/// resolved it. Everything past the config boundary uses the `String` default,
/// so an unresolved prompt cannot leak downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewer<P = String> {
    /// Unique reviewer name; also the check-run name in CI.
    pub name: String,
    /// The path-only or agent-assisted rule that triggers this reviewer.
    pub trigger: Trigger,
    /// Whether this reviewer gates or advises.
    pub mode: Mode,
    /// The harness to run on.
    #[serde(default)]
    pub backend: Backend,
    /// The model the backend should use. A model id is backend-specific, so this
    /// requires a pinned `backend`: the registry rejects a model under
    /// `backend: any`. Absent means the backend's built-in default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// The reasoning-effort level, mapped onto each backend's native control.
    /// Absent means the house default ([`Effort::default`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Per-reviewer wall-clock timeout.
    #[serde(
        default,
        with = "humantime_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    /// Container/runner provisioning; native when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<RunnerSpec>,
    /// Environment variables injected into the reviewer's environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Capability opt-ins.
    #[serde(default, skip_serializing_if = "Capabilities::is_least_privilege")]
    pub capabilities: Capabilities,
    /// Variables interpolated into the prompt before handing off to the agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    /// Opts out of attestation replay when set to [`AttestationPolicy::Never`].
    /// Absent means replayable: CI may honor a verified local attestation
    /// instead of executing this reviewer fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AttestationPolicy>,
    /// The review instruction handed to the agent: [`PromptSource`] as parsed,
    /// the resolved text once loaded.
    pub prompt: P,
}

impl<P> Reviewer<P> {
    /// Whether the reviewer runs in a container rather than native.
    #[must_use]
    pub fn is_containerized(&self) -> bool {
        self.runner.is_some()
    }

    /// Convert the prompt representation, carrying every other field across
    /// unchanged. The destructure is exhaustive, so adding a reviewer field
    /// cannot silently drop it here.
    ///
    /// # Errors
    ///
    /// Returns whatever `f` returns; the conversion itself cannot fail.
    pub fn map_prompt<Q, E>(self, f: impl FnOnce(P) -> Result<Q, E>) -> Result<Reviewer<Q>, E> {
        let Reviewer {
            name,
            trigger,
            mode,
            backend,
            model,
            effort,
            timeout,
            runner,
            env,
            capabilities,
            inputs,
            attestation,
            prompt,
        } = self;
        let prompt = f(prompt)?;
        Ok(Reviewer {
            name,
            trigger,
            mode,
            backend,
            model,
            effort,
            timeout,
            runner,
            env,
            capabilities,
            inputs,
            attestation,
            prompt,
        })
    }
}

/// Serde helper for an optional [`Duration`] written in human form (`15m`, `90s`).
mod humantime_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(duration) => {
                serializer.serialize_str(&humantime::format_duration(*duration).to_string())
            }
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw {
            Some(text) => humantime::parse_duration(&text)
                .map(Some)
                .map_err(D::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_reviewer() {
        let yaml = r"
name: file-responsibility
trigger: [src/**/*.ts]
mode: gate
prompt: Check single responsibility.
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert_eq!(reviewer.name, "file-responsibility");
        assert_eq!(reviewer.trigger.paths(), ["src/**/*.ts"]);
        assert_eq!(reviewer.mode, Mode::Gate);
        assert_eq!(reviewer.backend, Backend::Any);
        assert!(reviewer.timeout.is_none());
        assert!(!reviewer.is_containerized());
        assert!(reviewer.capabilities.is_least_privilege());
    }

    #[test]
    fn parses_an_agent_trigger_with_an_optional_path_prefilter() {
        let yaml = r"
name: single-responsibility
trigger:
  kind: agent
  prompt: Run only when the change can create a new responsibility boundary.
  backend: codex
  model: gpt-5.6-luna
  effort: high
  timeout: 45s
  paths: [src/**/*.rs]
mode: gate
prompt: Check single responsibility.
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        let agent = reviewer.trigger.agent().expect("agent trigger");
        assert_eq!(agent.kind, AgentTriggerKind::Agent);
        assert_eq!(agent.backend, Backend::Codex);
        assert_eq!(
            agent.model.as_ref().map(ModelId::as_str),
            Some("gpt-5.6-luna")
        );
        assert_eq!(agent.effort.as_ref().map(Effort::as_str), Some("high"));
        assert_eq!(agent.timeout, Some(Duration::from_secs(45)));
        assert_eq!(agent.paths, ["src/**/*.rs"]);
    }

    #[test]
    fn parses_a_containerized_reviewer_with_capabilities() {
        let yaml = r"
name: e2e-checkout-flow
trigger: [src/**]
mode: gate
backend: claude-code
timeout: 15m
runner:
  dockerfile: ./.bastion/e2e.Dockerfile
  image: ghcr.io/acme/e2e:latest
env:
  PREVIEW_URL: http://localhost:3000
capabilities:
  network: true
  mcp: [playwright]
  skills: [checkout-flow, browser]
inputs:
  preview_url: http://localhost:3000
prompt: Run the e2e checkout flow.
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert_eq!(reviewer.backend, Backend::ClaudeCode);
        assert_eq!(reviewer.timeout, Some(Duration::from_secs(15 * 60)));
        assert!(reviewer.is_containerized());
        assert!(reviewer.capabilities.network);
        assert_eq!(reviewer.capabilities.mcp, ["playwright"]);
        assert_eq!(
            reviewer.env.get("PREVIEW_URL").map(String::as_str),
            Some("http://localhost:3000")
        );
        assert!(!reviewer.capabilities.is_least_privilege());
    }

    #[test]
    fn parses_a_reviewer_with_model_and_effort() {
        let yaml = r"
name: pinned
trigger: [src/**/*.rs]
mode: gate
backend: codex
model: gpt-5
effort: medium
prompt: Check it.
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert_eq!(reviewer.model.as_ref().map(ModelId::as_str), Some("gpt-5"));
        assert_eq!(reviewer.effort.as_ref().map(Effort::as_str), Some("medium"));
    }

    #[test]
    fn model_and_effort_are_absent_by_default() {
        let yaml = r"
name: bare
trigger: [src/**]
mode: gate
prompt: p
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert!(reviewer.model.is_none());
        assert!(reviewer.effort.is_none());
    }

    #[test]
    fn the_house_default_effort_is_high() {
        // The fallback a reviewer runs at when it (and the registry) pin no effort.
        assert_eq!(DEFAULT_EFFORT, "high");
    }

    #[test]
    fn effort_is_an_opaque_passthrough_value() {
        // Like a model id, an effort level is forwarded verbatim: a backend-specific
        // value (Claude's `xhigh`) parses and round-trips unchanged, no enum to fence
        // it in.
        let effort: Effort = serde_yaml_ng::from_str("xhigh").unwrap();
        assert_eq!(effort.as_str(), "xhigh");
        assert_eq!(serde_yaml_ng::to_string(&effort).unwrap().trim(), "xhigh");
    }

    #[test]
    fn mode_and_backend_round_trip_through_their_wire_form() {
        assert_eq!(
            serde_yaml_ng::from_str::<Mode>("advisor").unwrap(),
            Mode::Advisor
        );
        assert_eq!(Mode::Gate.as_str(), "gate");
        assert_eq!(
            serde_yaml_ng::from_str::<Backend>("claude-code").unwrap(),
            Backend::ClaudeCode
        );
        assert_eq!(Backend::Pi.as_str(), "pi");
        assert_eq!(Backend::default(), Backend::Any);
    }

    #[test]
    fn a_prompt_parses_as_inline_text_or_a_file_reference() {
        let inline: PromptSource = serde_yaml_ng::from_str("Check it.").unwrap();
        assert_eq!(inline, PromptSource::Inline("Check it.".to_string()));

        let file: PromptSource = serde_yaml_ng::from_str("{file: reviewers/check.md}").unwrap();
        assert_eq!(
            file,
            PromptSource::File(PromptFile {
                file: PathBuf::from("reviewers/check.md")
            })
        );
    }

    #[test]
    fn a_prompt_mapping_with_a_stray_key_is_rejected() {
        // `deny_unknown_fields` on the file form: a typoed sibling key must not
        // be silently swallowed by the untagged match.
        let err = serde_yaml_ng::from_str::<PromptSource>("{file: p.md, fil: q.md}").unwrap_err();
        assert!(
            err.to_string()
                .contains("an inline prompt string or a `{file: <path>}` mapping"),
            "the error should state the accepted forms, got: {err}"
        );
    }

    #[test]
    fn a_reviewer_parses_with_a_prompt_file_reference() {
        let yaml = r"
name: from-file
trigger: [src/**]
mode: gate
prompt:
  file: reviewers/from-file.md
";
        let reviewer: Reviewer<PromptSource> = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            reviewer.prompt,
            PromptSource::File(PromptFile {
                file: PathBuf::from("reviewers/from-file.md")
            })
        );
    }

    #[test]
    fn map_prompt_carries_every_other_field_across() {
        let yaml = r"
name: carried
trigger: [src/**]
mode: gate
backend: codex
model: gpt-5
effort: low
prompt: original
";
        let raw: Reviewer<PromptSource> = serde_yaml_ng::from_str(yaml).unwrap();
        let mapped: Reviewer = raw
            .map_prompt(|_| Ok::<_, std::convert::Infallible>("resolved".to_string()))
            .unwrap();
        assert_eq!(mapped.name, "carried");
        assert_eq!(mapped.model.as_ref().map(ModelId::as_str), Some("gpt-5"));
        assert_eq!(mapped.effort.as_ref().map(Effort::as_str), Some("low"));
        assert_eq!(mapped.prompt, "resolved");
    }

    #[test]
    fn attestation_policy_is_absent_by_default() {
        let yaml = r"
name: bare
trigger: [src/**]
mode: gate
prompt: p
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert!(reviewer.attestation.is_none());
    }

    #[test]
    fn parses_an_attestation_never_opt_out() {
        let yaml = r"
name: always-fresh
trigger: [src/**]
mode: gate
attestation: never
prompt: p
";
        let reviewer: Reviewer = serde_yaml_ng::from_str(yaml).expect("valid reviewer");
        assert_eq!(reviewer.attestation, Some(AttestationPolicy::Never));
        assert_eq!(
            reviewer.attestation.map(AttestationPolicy::as_str),
            Some("never")
        );
    }
}
