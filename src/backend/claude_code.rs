//! The Claude Code backend.
//!
//! Translates a reviewer's execution profile into a headless `claude` CLI
//! invocation (`claude -p <prompt> --output-format json --json-schema <schema>`),
//! runs it through the injectable [`CommandRunner`] seam, and parses the final
//! structured output into a [`Verdict`]. Usage (tokens/cost) is captured when the
//! CLI reports it, and the raw session JSON is kept as the transcript.
//!
//! Per `docs/developer-guide/design.md`, when the agent fails to produce output matching the
//! verdict schema, Bastion re-runs the same session once (explaining the schema
//! again and asking only for the structured output) before giving up. A backend
//! error is the runner's signal to fail a gate closed.

use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::Deserialize;

use crate::reviewer;
use crate::verdict::{Money, Usage, Verdict};

use super::command::{CommandOutput, CommandRunner, CommandSpec, resolve_program};
use super::{Backend, ReviewOutcome, ReviewRequest};

/// Environment variable that overrides the `claude` program path (tests point this
/// at a fake executable; deployments can pin a specific binary).
pub const PROGRAM_ENV: &str = "BASTION_CLAUDE_BIN";

/// The default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "claude";

/// Bastion's house default model for this backend, used when a reviewer (and the
/// registry default) pin none. Opus 4.8 at the default [`Effort`](reviewer::Effort)
/// is the out-of-the-box experience; the explicit id keeps a review reproducible
/// rather than following whatever `claude` would otherwise pick.
const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// The JSON Schema Bastion asks `claude` to constrain its final output to. It is
/// the wire form of [`Verdict`]: `verdict`, `summary`, and `findings`.
const VERDICT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["verdict", "summary"],
  "properties": {
    "verdict": { "type": "string", "enum": ["pass", "block"] },
    "summary": { "type": "string" },
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "path", "line_start", "line_end", "detail"],
        "properties": {
          "kind": { "type": "string", "enum": ["blocking", "optional"] },
          "path": { "type": "string" },
          "line_start": { "type": "integer", "minimum": 0 },
          "line_end": { "type": "integer", "minimum": 0 },
          "detail": { "type": "string" }
        }
      }
    }
  }
}"#;

/// The Claude Code agent backend.
///
/// Generic over the [`CommandRunner`] so production wires a real subprocess while
/// tests drive a fake executable through the identical path.
#[derive(Debug, Clone)]
pub struct ClaudeCodeBackend<R> {
    runner: R,
    program: std::ffi::OsString,
}

impl<R: CommandRunner> ClaudeCodeBackend<R> {
    /// Build a backend over `runner`, resolving the `claude` program from
    /// [`PROGRAM_ENV`] (falling back to [`DEFAULT_PROGRAM`] on `PATH`).
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            program: resolve_program(DEFAULT_PROGRAM, PROGRAM_ENV),
        }
    }

    /// Build a backend over `runner` with an explicit program path, bypassing the
    /// environment lookup. Used by tests that construct a fake binary path.
    #[must_use]
    pub fn with_program(runner: R, program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            runner,
            program: program.into(),
        }
    }

    /// Borrow the resolved program path (the `claude` binary or a test fake).
    #[must_use]
    pub fn program(&self) -> &std::ffi::OsStr {
        &self.program
    }

    /// Assemble the base CLI invocation shared by the first turn and the reprompt.
    fn base_spec(&self, request: &ReviewRequest<'_>) -> CommandSpec {
        let reviewer = request.reviewer;
        let mut spec = CommandSpec::new(self.program.clone(), request.repo_root);
        spec.arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(VERDICT_SCHEMA)
            // Reviewers run unattended over a trusted checkout (see the threat
            // model in docs/developer-guide/design.md); skip interactive permission prompts so the
            // headless run does not wedge.
            .arg("--permission-mode")
            .arg("bypassPermissions");

        // Pin the model and reasoning effort. A reviewer (or the registry default)
        // may set either; absent, Bastion's house default for this backend applies
        // (Opus 4.8 at high effort) rather than deferring to the CLI's own config,
        // so a review is reproducible across machines.
        spec.arg("--model").arg(
            reviewer
                .model
                .as_ref()
                .map_or(DEFAULT_MODEL, reviewer::ModelId::as_str),
        );
        spec.arg("--effort").arg(
            reviewer
                .effort
                .as_ref()
                .map_or(reviewer::DEFAULT_EFFORT, reviewer::Effort::as_str),
        );

        // The `mcp` and `skills` capability opt-ins are not provisioned in this
        // build, and a native `network: true` fails closed: `dispatch` rejects such a
        // reviewer up front when it resolves the `ExecutionPlan`
        // (`ExecutionPlan::resolve`), so nothing capability-specific reaches this
        // `claude` invocation. A container `runner` *is* provisioned, but it is
        // applied at the container boundary (the `ContainerRunner` decorator wraps
        // this same spec into a `docker run`); the backend builds the identical spec
        // either way. Only the reviewer's declared environment is injected, into the
        // child process so the agent (and any tools it runs) sees it.
        for (key, value) in &reviewer.env {
            spec.env.insert(key.clone(), value.clone());
        }
        spec
    }

    /// Run one turn and parse the `claude` JSON envelope.
    async fn run_turn(&self, spec: &CommandSpec) -> Result<Envelope> {
        let output = self.runner.run(spec).await?;
        parse_envelope(&output)
    }
}

impl<R: CommandRunner> Backend for ClaudeCodeBackend<R> {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::ClaudeCode
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        let prompt = build_prompt(request);

        // First turn: the full review prompt with the schema instruction.
        let mut spec = self.base_spec(request);
        spec.arg("-p").arg(&prompt);
        let first = self.run_turn(&spec).await?;

        let transcript = first.raw.clone();
        match first.verdict() {
            Some(verdict) => Ok(ReviewOutcome {
                verdict,
                usage: first.usage(),
                transcript: Some(transcript),
            }),
            None => {
                // Malformed/missing structured output. Per design.md, re-run the
                // same session once asking only for the structured output, then
                // fail if it is still wrong.
                let session = first.session_id.clone().ok_or_else(|| {
                    eyre!(
                        "claude produced no structured verdict and no session id to resume \
                         (reviewer '{}')",
                        request.reviewer.name
                    )
                })?;

                let mut reprompt = self.base_spec(request);
                reprompt
                    .arg("--resume")
                    .arg(&session)
                    .arg("-p")
                    .arg(REPROMPT);
                let second = self.run_turn(&reprompt).await?;

                let mut transcript = transcript;
                transcript.push('\n');
                transcript.push_str(&second.raw);

                match second.verdict() {
                    Some(verdict) => Ok(ReviewOutcome {
                        verdict,
                        // The reprompt resumes the same session, so its reported
                        // total is cumulative; combine without double-counting.
                        usage: combine_session_usage(first.usage(), second.usage()),
                        transcript: Some(transcript),
                    }),
                    None => bail!(
                        "claude did not produce a valid verdict for reviewer '{}' even after \
                         re-prompting for the structured output",
                        request.reviewer.name
                    ),
                }
            }
        }
    }
}

/// The reprompt sent on the resumed session when the first turn's output did not
/// conform to the verdict schema.
const REPROMPT: &str = "Your previous response did not include a valid structured verdict. \
     Do not perform any further review work. Reply with ONLY the structured output for the \
     review you already performed, conforming exactly to the requested JSON schema: a top-level \
     `verdict` of \"pass\" or \"block\", a `summary` string, and an optional `findings` array. \
     A `block` must include at least one finding with kind \"blocking\".";

/// The structured-output instruction closing every `claude` review prompt, pinning
/// the reviewer's judgment to the backend's native JSON verdict schema.
const JSON_SCHEMA_INSTRUCTION: &str = "\
When you have finished reviewing, return your judgment as structured output \
conforming to the requested JSON schema: a top-level `verdict` of \"pass\" or \"block\", \
a human-friendly `summary`, and a `findings` array locating specific comments. Mark a \
finding `blocking` if it is a reason to block, or `optional` if it is a non-blocking \
suggestion. If you block, include a blocking finding for each issue you are blocking on.";

/// Build the prompt handed to `claude`: the [shared review body](super::review_prompt)
/// closed with this backend's native-JSON-schema instruction.
fn build_prompt(request: &ReviewRequest<'_>) -> String {
    super::review_prompt(request, JSON_SCHEMA_INSTRUCTION)
}

/// The parsed `claude --output-format json` result envelope plus the raw text.
#[derive(Debug)]
struct Envelope {
    raw: String,
    result: ResultJson,
    session_id: Option<String>,
}

impl Envelope {
    /// Extract a structured [`Verdict`], preferring the CLI's validated
    /// `structured_output` and falling back to parsing the `result` text as JSON.
    /// Returns `None` if neither yields a schema-conforming, internally consistent
    /// verdict.
    ///
    /// This is only reached for a turn that already succeeded (execution errors are
    /// caught in [`parse_envelope`]), so `None` unambiguously means "no conforming
    /// verdict" and is the condition that triggers the single reprompt.
    fn verdict(&self) -> Option<Verdict> {
        let verdict = self
            .result
            .structured_output
            .as_ref()
            .and_then(|value| serde_json::from_value::<Verdict>(value.clone()).ok())
            .or_else(|| {
                self.result
                    .result
                    .as_deref()
                    .and_then(parse_verdict_from_text)
            })?;

        // A reviewer that blocks must explain itself with a blocking finding;
        // an inconsistent verdict is treated as malformed so we reprompt.
        verdict.is_consistent().then_some(verdict)
    }

    /// Token and cost accounting, when the CLI reported it.
    fn usage(&self) -> Option<Usage> {
        let usage = self.result.usage.as_ref()?;
        let tokens_in = usage.input_tokens.unwrap_or(0);
        let tokens_out = usage.output_tokens.unwrap_or(0);
        let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
        let cost = self
            .result
            .total_cost_usd
            .map(super::money_from_dollars)
            .unwrap_or_default();
        // Report usage only if at least one signal is present; an all-zero block
        // with no cost is indistinguishable from "not reported".
        if tokens_in == 0 && tokens_out == 0 && cache_read == 0 && cost.cents() == 0 {
            return None;
        }
        Some(Usage {
            tokens_in,
            tokens_out,
            cache_read,
            cost_usd: cost,
        })
    }
}

/// The subset of `claude`'s `--output-format json` envelope Bastion consumes.
///
/// The CLI emits a single JSON object (the final `result` message). Unknown
/// fields are ignored so CLI additions do not break parsing.
#[derive(Debug, Deserialize)]
struct ResultJson {
    /// The final assistant text. Used as a fallback verdict source when
    /// `structured_output` is absent.
    #[serde(default)]
    result: Option<String>,
    /// The schema-validated structured output, when `--json-schema` is honored.
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
    /// The session id, used to resume for a reprompt.
    #[serde(default)]
    session_id: Option<String>,
    /// Token accounting.
    #[serde(default)]
    usage: Option<UsageJson>,
    /// Total session cost in dollars.
    #[serde(default)]
    total_cost_usd: Option<f64>,
    /// Whether the CLI reports the turn itself errored.
    #[serde(default)]
    is_error: Option<bool>,
}

/// The token-usage shape inside the CLI envelope.
#[derive(Debug, Deserialize)]
struct UsageJson {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Cache-read input tokens (prompt-cache hits), reported separately from
    /// `input_tokens` by the Anthropic usage block. Cache-creation tokens are not
    /// consumed; only the read figure is surfaced.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// Parse the `claude` JSON envelope from a finished process.
///
/// An execution failure (non-zero/signal exit, empty output, unparseable JSON, or
/// the CLI's own `is_error: true`) is reported as `Err` and must *not* be
/// re-prompted: it is the runner's signal to fail a gate closed. Only an otherwise
/// successful turn whose output simply lacks a conforming verdict is eligible for
/// the single reprompt, which is why that distinction lives here rather than being
/// collapsed into "no verdict".
fn parse_envelope(output: &CommandOutput) -> Result<Envelope> {
    let exit = || {
        output
            .code
            .map_or_else(|| "signal".to_string(), |c| c.to_string())
    };

    let raw = output.stdout.trim();
    if raw.is_empty() {
        bail!(
            "claude produced no output (exit {}): {}",
            exit(),
            output.stderr.trim()
        );
    }
    let result: ResultJson = serde_json::from_str(raw).wrap_err_with(|| {
        format!(
            "claude output was not valid JSON (exit {}): {}",
            exit(),
            truncate(raw, 400)
        )
    })?;

    // A non-zero (or signal) exit is an execution failure even when stdout happens
    // to carry parseable JSON: trusting a `pass` verdict from a process that exited
    // in error would be a fail-open hole. The CLI's own `is_error: true` is the
    // same signal expressed in-band.
    if !output.success() {
        bail!(
            "claude exited unsuccessfully (exit {}): {}",
            exit(),
            truncate(&output.stderr, 400)
        );
    }
    if result.is_error.unwrap_or(false) {
        bail!(
            "claude reported an execution error (is_error=true, exit {})",
            exit()
        );
    }

    let session_id = result.session_id.clone();
    Ok(Envelope {
        raw: output.stdout.clone(),
        result,
        session_id,
    })
}

/// Parse a [`Verdict`] from a free-form `result` string, tolerating a fenced or
/// prose-wrapped JSON object by extracting the outermost `{...}`.
fn parse_verdict_from_text(text: &str) -> Option<Verdict> {
    let trimmed = text.trim();
    if let Ok(verdict) = serde_json::from_str::<Verdict>(trimmed) {
        return Some(verdict);
    }
    // Fall back to the first balanced-looking object: from the first `{` to the
    // last `}`. This rescues output wrapped in a code fence or a sentence.
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Verdict>(&trimmed[start..=end]).ok()
}

/// Combine the usage reported by the first turn and the resumed reprompt turn.
///
/// `claude`'s `total_cost_usd` is the *cumulative* cost of the whole session, and
/// the reprompt resumes the same session, so the second turn's total already
/// includes the first. Summing them would double-count; instead we take the later
/// (larger) cumulative figure. Token counts are treated the same way (cumulative
/// session totals), guarding against a turn that under-reports by keeping the max.
/// Returns `None` only when neither turn reported usage.
fn combine_session_usage(first: Option<Usage>, second: Option<Usage>) -> Option<Usage> {
    match (first, second) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(Usage {
            tokens_in: a.tokens_in.max(b.tokens_in),
            tokens_out: a.tokens_out.max(b.tokens_out),
            cache_read: a.cache_read.max(b.cache_read),
            cost_usd: Money::from_cents(a.cost_usd.cents().max(b.cost_usd.cents())),
        }),
    }
}

/// Truncate `s` to at most `max` bytes (on a char boundary) for error messages.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::command::{CommandSpec, SystemCommandRunner};
    use crate::event::RunId;
    use crate::reviewer::{Capabilities, Mode, Reviewer};
    use crate::verdict::{Decision, FindingKind};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A [`CommandRunner`] that returns scripted outputs in order, recording the
    /// specs it was asked to run. A real fake, not a mocking framework.
    #[derive(Default)]
    struct ScriptedRunner {
        outputs: Mutex<std::collections::VecDeque<CommandOutput>>,
        seen: Mutex<Vec<CommandSpec>>,
    }

    impl ScriptedRunner {
        fn with(outputs: Vec<CommandOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn nth_args(&self, n: usize) -> Vec<String> {
            self.seen.lock().unwrap()[n]
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        }
    }

    impl CommandRunner for ScriptedRunner {
        async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
            self.seen.lock().unwrap().push(spec.clone());
            let next = self.outputs.lock().unwrap().pop_front();
            next.ok_or_else(|| eyre!("scripted runner exhausted"))
        }
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn reviewer() -> Reviewer {
        Reviewer {
            name: "demo".into(),
            trigger: vec!["**".into()],
            mode: Mode::Gate,
            backend: reviewer::Backend::ClaudeCode,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Capabilities::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "Review it.".into(),
        }
    }

    async fn review_with(
        outputs: Vec<CommandOutput>,
        reviewer: &Reviewer,
    ) -> Result<ReviewOutcome> {
        let runner = ScriptedRunner::with(outputs);
        let backend = ClaudeCodeBackend::with_program(runner, "claude-fake");
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };
        backend.review(&request).await
    }

    fn pass_envelope() -> String {
        serde_json::json!({
            "session_id": "s-1",
            "structured_output": { "verdict": "pass", "summary": "ok", "findings": [] }
        })
        .to_string()
    }

    /// Run one review and return the recorded args of the first (and only) turn.
    async fn first_turn_args(reviewer: &Reviewer) -> Vec<String> {
        let runner = ScriptedRunner::with(vec![ok(&pass_envelope())]);
        let backend = ClaudeCodeBackend::with_program(runner, "claude-fake");
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };
        backend.review(&request).await.expect("verdict parses");
        backend.runner.nth_args(0)
    }

    #[tokio::test]
    async fn pins_the_house_default_model_and_effort_when_unset() {
        let args = first_turn_args(&reviewer()).await;
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "claude-opus-4-8");
        let e = args.iter().position(|a| a == "--effort").expect("--effort");
        assert_eq!(args[e + 1], "high");
    }

    #[tokio::test]
    async fn explicit_model_and_effort_override_the_house_default() {
        let mut rev = reviewer();
        rev.model = Some(serde_yaml_ng::from_str("claude-sonnet-4-6").unwrap());
        // A Claude-specific level: the opaque value is forwarded verbatim.
        rev.effort = Some(serde_yaml_ng::from_str("xhigh").unwrap());
        let args = first_turn_args(&rev).await;
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "claude-sonnet-4-6");
        let e = args.iter().position(|a| a == "--effort").expect("--effort");
        assert_eq!(args[e + 1], "xhigh");
    }

    #[tokio::test]
    async fn usage_without_cache_read_defaults_to_zero() {
        // A usage block that omits `cache_read_input_tokens` (no prompt-cache hits)
        // must still parse, defaulting cache_read to 0.
        let envelope = serde_json::json!({
            "result": "done",
            "session_id": "s-1",
            "total_cost_usd": 0.05,
            "usage": { "input_tokens": 500, "output_tokens": 40 },
            "structured_output": { "verdict": "pass", "summary": "ok", "findings": [] }
        })
        .to_string();
        let outcome = review_with(vec![ok(&envelope)], &reviewer())
            .await
            .expect("verdict parses");
        let usage = outcome.usage.expect("usage reported");
        assert_eq!(usage.tokens_in, 500);
        assert_eq!(usage.cache_read, 0);
    }

    #[tokio::test]
    async fn parses_structured_output_into_a_pass_verdict() {
        let envelope = serde_json::json!({
            "result": "done",
            "session_id": "s-1",
            "total_cost_usd": 0.21,
            "usage": { "input_tokens": 1200, "output_tokens": 80, "cache_read_input_tokens": 950 },
            "structured_output": {
                "verdict": "pass",
                "summary": "looks fine",
                "findings": []
            }
        })
        .to_string();

        let outcome = review_with(vec![ok(&envelope)], &reviewer())
            .await
            .expect("verdict parses");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "looks fine");
        let usage = outcome.usage.expect("usage reported");
        assert_eq!(usage.tokens_in, 1200);
        assert_eq!(usage.cache_read, 950);
        assert_eq!(usage.cost_usd, Money::from_cents(21));
        assert!(outcome.transcript.is_some());
    }

    #[tokio::test]
    async fn parses_a_blocking_verdict_with_findings() {
        let envelope = serde_json::json!({
            "session_id": "s-1",
            "structured_output": {
                "verdict": "block",
                "summary": "missing tenant scope",
                "findings": [{
                    "kind": "blocking",
                    "path": "src/db.rs",
                    "line_start": 10,
                    "line_end": 12,
                    "detail": "scope by tenant_id"
                }]
            }
        })
        .to_string();

        let outcome = review_with(vec![ok(&envelope)], &reviewer())
            .await
            .expect("verdict parses");
        assert!(outcome.verdict.decision.is_block());
        assert_eq!(outcome.verdict.findings.len(), 1);
        assert_eq!(outcome.verdict.findings[0].kind, FindingKind::Blocking);
    }

    #[tokio::test]
    async fn every_finding_in_structured_output_is_surfaced_not_just_the_first() {
        // Regression for under-reporting: all findings in the structured output
        // must reach the verdict, in order, so a single pass returns the complete
        // set rather than one finding per CI cycle.
        let envelope = serde_json::json!({
            "session_id": "s-1",
            "structured_output": {
                "verdict": "block",
                "summary": "multiple tells",
                "findings": [
                    { "kind": "blocking", "path": "README.md", "line_start": 1, "line_end": 1, "detail": "aphorism opener" },
                    { "kind": "blocking", "path": "README.md", "line_start": 5, "line_end": 6, "detail": "manufactured antithesis" },
                    { "kind": "optional", "path": "docs/g.md", "line_start": 9, "line_end": 9, "detail": "dramatic colon" }
                ]
            }
        })
        .to_string();

        let outcome = review_with(vec![ok(&envelope)], &reviewer())
            .await
            .expect("verdict parses");
        assert!(outcome.verdict.decision.is_block());
        assert_eq!(outcome.verdict.findings.len(), 3);
        let details: Vec<&str> = outcome
            .verdict
            .findings
            .iter()
            .map(|f| f.detail.as_str())
            .collect();
        assert_eq!(
            details,
            [
                "aphorism opener",
                "manufactured antithesis",
                "dramatic colon"
            ]
        );
        assert_eq!(outcome.verdict.findings[2].kind, FindingKind::Optional);
    }

    #[tokio::test]
    async fn falls_back_to_parsing_the_result_text_as_json() {
        // No structured_output; the verdict is embedded in the result text,
        // wrapped in prose to exercise the brace-extraction fallback.
        let envelope = serde_json::json!({
            "session_id": "s-1",
            "result": "Here is my verdict:\n```json\n{\"verdict\":\"pass\",\"summary\":\"ok\"}\n```"
        })
        .to_string();

        let outcome = review_with(vec![ok(&envelope)], &reviewer())
            .await
            .expect("verdict parses from text");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
    }

    #[tokio::test]
    async fn reprompts_once_then_succeeds_on_malformed_first_turn() {
        let bad = serde_json::json!({
            "session_id": "s-9",
            "result": "I think it looks good but I forgot the schema."
        })
        .to_string();
        let good = serde_json::json!({
            "session_id": "s-9",
            "structured_output": { "verdict": "pass", "summary": "ok now" }
        })
        .to_string();

        let runner = ScriptedRunner::with(vec![ok(&bad), ok(&good)]);
        let backend = ClaudeCodeBackend::with_program(runner, "claude-fake");
        let r = reviewer();
        let run = RunId("r".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };

        let outcome = backend.review(&request).await.expect("reprompt succeeds");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "ok now");
        // Two turns ran; the second resumed the session for just the output.
        assert_eq!(backend.runner.calls(), 2);
        let second = backend.runner.nth_args(1);
        assert!(second.iter().any(|a| a == "--resume"));
        assert!(second.iter().any(|a| a == "s-9"));
        // Transcript captures both turns.
        assert!(outcome.transcript.unwrap().contains("forgot the schema"));
    }

    #[tokio::test]
    async fn fails_closed_when_reprompt_also_malformed() {
        let bad = serde_json::json!({ "session_id": "s", "result": "nope" }).to_string();
        let err = review_with(vec![ok(&bad), ok(&bad)], &reviewer())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("even after re-prompting"));
    }

    #[tokio::test]
    async fn inconsistent_block_without_findings_triggers_reprompt() {
        // A block with no blocking finding is inconsistent → treated as malformed.
        let inconsistent = serde_json::json!({
            "session_id": "s",
            "structured_output": { "verdict": "block", "summary": "no reason given" }
        })
        .to_string();
        let fixed = serde_json::json!({
            "session_id": "s",
            "structured_output": {
                "verdict": "block",
                "summary": "now with reason",
                "findings": [{
                    "kind": "blocking", "path": "a.rs",
                    "line_start": 1, "line_end": 1, "detail": "fix"
                }]
            }
        })
        .to_string();

        let outcome = review_with(vec![ok(&inconsistent), ok(&fixed)], &reviewer())
            .await
            .expect("reprompt fixes consistency");
        assert!(outcome.verdict.decision.is_block());
        assert!(outcome.verdict.is_consistent());
    }

    #[tokio::test]
    async fn empty_output_is_an_execution_error() {
        let empty = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let err = review_with(vec![empty], &reviewer()).await.unwrap_err();
        assert!(err.to_string().contains("no output"));
    }

    #[tokio::test]
    async fn missing_session_id_cannot_reprompt() {
        let bad = serde_json::json!({ "result": "no session here" }).to_string();
        let err = review_with(vec![ok(&bad)], &reviewer()).await.unwrap_err();
        assert!(err.to_string().contains("no session id to resume"));
    }

    #[tokio::test]
    async fn is_error_envelope_is_a_hard_failure_not_a_reprompt() {
        // A CLI-reported execution error must fail the turn outright. It must NOT
        // be re-prompted: re-prompting an errored session could let a later `pass`
        // mask the failure (a fail-open hole). Even though a verdict is present and
        // a session id is available, exactly one turn runs and it errors.
        let errored = serde_json::json!({
            "session_id": "s-err",
            "is_error": true,
            "structured_output": { "verdict": "pass", "summary": "ignore me" }
        })
        .to_string();
        let rescue = serde_json::json!({
            "session_id": "s-err",
            "structured_output": { "verdict": "pass", "summary": "too late" }
        })
        .to_string();

        let runner = ScriptedRunner::with(vec![ok(&errored), ok(&rescue)]);
        let backend = ClaudeCodeBackend::with_program(runner, "claude-fake");
        let r = reviewer();
        let run = RunId("r".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };
        let err = backend.review(&request).await.unwrap_err();
        assert!(err.to_string().contains("execution error"));
        // Only the first turn ran; no reprompt was attempted.
        assert_eq!(backend.runner.calls(), 1);
    }

    #[tokio::test]
    async fn nonzero_exit_with_parseable_pass_is_rejected() {
        // A process that exits in error but happens to print a parseable `pass`
        // verdict must not be trusted: trusting it would be a fail-open hole.
        let body = serde_json::json!({
            "session_id": "s",
            "structured_output": { "verdict": "pass", "summary": "exited 1 but said pass" }
        })
        .to_string();
        let nonzero = CommandOutput {
            code: Some(1),
            stdout: body,
            stderr: "crashed".into(),
        };
        let err = review_with(vec![nonzero], &reviewer()).await.unwrap_err();
        assert!(err.to_string().contains("exited unsuccessfully"));
    }

    #[test]
    fn combine_session_usage_takes_the_cumulative_total_not_the_sum() {
        // total_cost_usd / token counts are cumulative session figures; the resumed
        // turn already includes the first, so combining must not double-count.
        let first = Some(Usage {
            tokens_in: 1000,
            tokens_out: 100,
            cache_read: 400,
            cost_usd: Money::from_cents(20),
        });
        let second = Some(Usage {
            tokens_in: 1500,
            tokens_out: 150,
            cache_read: 900,
            cost_usd: Money::from_cents(30),
        });
        let combined = combine_session_usage(first, second).expect("present");
        assert_eq!(combined.tokens_in, 1500);
        assert_eq!(combined.tokens_out, 150);
        assert_eq!(combined.cache_read, 900);
        assert_eq!(combined.cost_usd, Money::from_cents(30));
        // One side missing yields the other untouched.
        assert_eq!(combine_session_usage(first, None), first);
        assert_eq!(combine_session_usage(None, None), None);
    }

    #[test]
    fn build_prompt_prepends_changeset_preamble_and_appends_schema() {
        let r = reviewer();
        let run = RunId("r".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };
        let prompt = build_prompt(&request);
        // The shared changeset preamble leads, naming the base and steering the
        // agent to the working-tree diff rather than the committed-only form.
        assert!(prompt.starts_with("You are reviewing a changeset"));
        assert!(prompt.contains("base branch `main`"));
        assert!(prompt.contains("git diff main"));
        assert!(prompt.contains("Do not rely on `git diff main...HEAD`"));
        // The reviewer's own instruction and the schema instruction follow.
        assert!(prompt.contains("Review it."));
        assert!(prompt.contains("structured output"));
        // The shared exhaustive-findings instruction is present, between the
        // reviewer instruction and the closing schema instruction.
        assert!(prompt.contains("Report every issue you can identify"));
        assert!(prompt.contains("Do not stop after the"));
        let reviewer_at = prompt.find("Review it.").expect("present");
        let exhaustive_at = prompt.find("Report every issue").expect("present");
        let schema_at = prompt.find("return your judgment").expect("present");
        assert!(reviewer_at < exhaustive_at);
        assert!(exhaustive_at < schema_at);
    }

    #[tokio::test]
    async fn injects_reviewer_env_and_emits_no_mcp_flag_natively() {
        let mut r = reviewer();
        r.env
            .insert("PREVIEW_URL".into(), "http://localhost:3000".into());
        // The native `claude` invocation must never emit an MCP flag: MCP is not
        // provisioned, and a reviewer that declares it fails closed upstream when
        // `dispatch` resolves the `ExecutionPlan` (`ExecutionPlan::resolve`) rather
        // than reaching this path. This drives `review` directly, bypassing that
        // preflight, to pin that the native arg-building stays MCP-free regardless.
        r.capabilities.mcp = vec!["playwright".into()];

        let envelope = serde_json::json!({
            "session_id": "s",
            "structured_output": { "verdict": "pass", "summary": "ok" }
        })
        .to_string();
        let runner = ScriptedRunner::with(vec![ok(&envelope)]);
        let backend = ClaudeCodeBackend::with_program(runner, "claude-fake");
        let run = RunId("r".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };
        backend.review(&request).await.expect("runs");

        let args = backend.runner.nth_args(0);
        // Reviewer env is injected into the child process...
        let env = &backend.runner.seen.lock().unwrap()[0].env;
        assert_eq!(
            env.get("PREVIEW_URL").map(String::as_str),
            Some("http://localhost:3000")
        );
        // ...but no MCP flag is emitted natively.
        assert!(
            !args.iter().any(|a| a == "--mcp" || a == "--mcp-config"),
            "native path must not emit an MCP flag (got {args:?})"
        );
        assert!(!args.iter().any(|a| a == "playwright"));
    }

    #[test]
    fn id_is_claude_code() {
        let backend = ClaudeCodeBackend::with_program(ScriptedRunner::default(), "claude-fake");
        assert_eq!(backend.id(), reviewer::Backend::ClaudeCode);
    }

    /// Compile a real native fake `claude` executable that ignores its arguments
    /// and prints `envelope_json` to stdout, returning its path. Using a genuine
    /// compiled binary (rather than a shell/batch script) means the test drives the
    /// *real* [`SystemCommandRunner`] subprocess path identically on every platform
    /// (including the long, metacharacter-laden `--json-schema` argument that
    /// Windows batch files cannot accept).
    ///
    /// Returns `None` (so the caller can detect-and-skip) when no `rustc` is on
    /// `PATH`, so the suite never spuriously fails on a machine without a toolchain.
    fn build_fake_claude(dir: &Path, envelope_json: &str) -> Option<PathBuf> {
        // Embed the envelope as a Rust string literal via `{:?}` (debug formatting
        // escapes quotes and backslashes), so the compiled program prints it back
        // byte for byte.
        let src = format!("fn main() {{ print!({envelope_json:?}); }}\n");
        let src_path = dir.join("fake_claude.rs");
        std::fs::write(&src_path, src).unwrap();

        let exe_name = if cfg!(windows) {
            "claude-fake.exe"
        } else {
            "claude-fake"
        };
        let out_path = dir.join(exe_name);

        let status = std::process::Command::new("rustc")
            .arg(&src_path)
            .arg("-O")
            .arg("-o")
            .arg(&out_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() && out_path.exists() => Some(out_path),
            // No usable rustc (not installed, or failed): skip rather than fail.
            _ => None,
        }
    }

    /// The full path runs: real process spawn, real stdout capture, real parse.
    #[tokio::test]
    async fn end_to_end_through_a_real_fake_executable() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = serde_json::json!({
            "result": "done",
            "session_id": "s-e2e",
            "structured_output": {
                "verdict": "pass",
                "summary": "real subprocess ok",
                "findings": []
            }
        })
        .to_string();
        let Some(program) = build_fake_claude(dir.path(), &envelope) else {
            eprintln!("skipping end-to-end test: no usable rustc on PATH");
            return;
        };

        let backend = ClaudeCodeBackend::with_program(SystemCommandRunner, &program);
        let r = reviewer();
        let run = RunId("r-e2e".into());
        let root = dir.path().to_path_buf();
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        };

        let outcome = backend
            .review(&request)
            .await
            .expect("real fake executable produces a verdict");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "real subprocess ok");
        assert!(outcome.transcript.is_some());
    }
}
