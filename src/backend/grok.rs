//! The Grok Build backend.
//!
//! Translates a reviewer's execution profile into a headless `grok` CLI
//! invocation (`grok -p <prompt> --output-format json --json-schema <schema>`),
//! runs it through the injectable [`CommandRunner`] seam, and parses the final
//! structured output into a [`Verdict`]. Usage (tokens/cost) is captured from the
//! envelope, and the raw envelope JSON is kept as the transcript.
//!
//! # The Grok Build invocation contract
//!
//! Grok Build's headless mode mirrors Claude Code's: `--json-schema` constrains the
//! final message to the verdict schema and the envelope carries the validated
//! object under `structuredOutput`, so this backend shares the JSON-schema prompt
//! trailer, reprompt text, and text fallback with the Claude Code backend
//! (`super::VERDICT_JSON_SCHEMA` and friends). Reviewers run unattended under
//! `--permission-mode bypassPermissions` over a trusted checkout, the same latitude
//! the other backends are given (see the threat model in
//! `docs/developer-guide/design.md`). A pinned `model` is forwarded as `--model`
//! and `effort` as `--reasoning-effort` (default `high`); with no model pinned the
//! CLI resolves its own default, as Codex and Pi do.
//!
//! When the first turn's output does not conform to the schema, Bastion resumes the
//! same session once (`--resume <sessionId>`) asking only for the structured
//! output, then gives up with an error that the runner fails closed on. Each `grok`
//! process reports usage for its own turns only, so the two are summed.

use color_eyre::eyre::{Context, Result, bail, eyre};
use serde::Deserialize;

use crate::reviewer;
use crate::verdict::{Money, Usage, Verdict};

use super::command::{CommandOutput, CommandRunner, CommandSpec, resolve_program};
use super::{Backend, ReviewOutcome, ReviewRequest};

/// Environment variable that overrides the `grok` program path (tests point this
/// at a fake executable; deployments can pin a specific binary).
pub const PROGRAM_ENV: &str = "BASTION_GROK_BIN";

/// The default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "grok";

/// The Grok Build agent backend.
///
/// Generic over the [`CommandRunner`] so production wires a real subprocess while
/// tests drive a fake executable through the identical path.
#[derive(Debug, Clone)]
pub struct GrokBackend<R> {
    runner: R,
    program: std::ffi::OsString,
}

impl<R: CommandRunner> GrokBackend<R> {
    /// Build a backend over `runner`, resolving the `grok` program from
    /// [`PROGRAM_ENV`] (falling back to [`DEFAULT_PROGRAM`] on `PATH`).
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self::with_program(runner, resolve_program(DEFAULT_PROGRAM, PROGRAM_ENV))
    }

    /// Build a backend over `runner` with an explicit program path, bypassing the
    /// environment lookup.
    #[must_use]
    pub fn with_program(runner: R, program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            runner,
            program: program.into(),
        }
    }

    /// Assemble the base CLI invocation shared by the first turn and the reprompt.
    fn base_spec(&self, request: &ReviewRequest<'_>) -> CommandSpec {
        let reviewer = request.reviewer;
        let mut spec = CommandSpec::new(self.program.clone(), request.repo_root);
        spec.arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(super::VERDICT_JSON_SCHEMA)
            .arg("--permission-mode")
            .arg("bypassPermissions");

        // Pin the model only when a reviewer (or the registry default) sets one;
        // otherwise Grok Build resolves its own default. The effort always applies
        // so an unpinned reviewer reasons at the house default rather than the
        // CLI's, keeping a review reproducible across machines.
        if let Some(model) = &reviewer.model {
            spec.arg("--model").arg(model.as_str());
        }
        spec.arg("--reasoning-effort").arg(
            reviewer
                .effort
                .as_ref()
                .map_or(reviewer::DEFAULT_EFFORT, reviewer::Effort::as_str),
        );

        for (key, value) in &reviewer.env {
            spec.env.insert(key.clone(), value.clone());
        }
        spec
    }

    /// Run one turn and parse the `grok` JSON envelope.
    async fn run_turn(&self, spec: &CommandSpec) -> Result<Envelope> {
        let output = self.runner.run(spec).await?;
        parse_envelope(&output)
    }
}

impl<R: CommandRunner> Backend for GrokBackend<R> {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::Grok
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        let prompt = super::review_prompt(request, super::JSON_SCHEMA_INSTRUCTION);

        let (first, resumed) = if let Some(prior) = request.conversation {
            let mut spec = self.base_spec(request);
            spec.arg("--resume")
                .arg(prior.id().as_str())
                .arg("-p")
                .arg(&prompt)
                .conversation_resume();
            match self.run_turn(&spec).await {
                Ok(first) => (first, true),
                Err(err) => {
                    super::log_resume_fallback(request, reviewer::Backend::Grok, &err);
                    let mut fresh = self.base_spec(request);
                    fresh.arg("-p").arg(&prompt);
                    (self.run_turn(&fresh).await?, false)
                }
            }
        } else {
            let mut fresh = self.base_spec(request);
            fresh.arg("-p").arg(&prompt);
            (self.run_turn(&fresh).await?, false)
        };

        if let Some(verdict) = first.verdict() {
            let prior_id = if resumed {
                request
                    .conversation
                    .map(|prior| prior.id().as_str().to_string())
            } else {
                None
            };
            let conversation_id = first.result.session_id.clone().or(prior_id);
            return Ok(ReviewOutcome {
                verdict,
                usage: first.usage(),
                transcript: Some(first.raw),
                conversation: super::conversation_ref(
                    request,
                    reviewer::Backend::Grok,
                    conversation_id.as_deref(),
                    resumed,
                    None,
                ),
            });
        }

        // Malformed/missing structured output. Per design.md, re-run the same
        // session once asking only for the structured output, then fail closed.
        let prior_id = if resumed {
            request
                .conversation
                .map(|prior| prior.id().as_str().to_string())
        } else {
            None
        };
        let session = first
            .result
            .session_id
            .clone()
            .or(prior_id)
            .ok_or_else(|| {
                eyre!(
                    "grok produced no structured verdict and no session id to resume \
                     (reviewer '{}')",
                    request.reviewer.name
                )
            })?;

        let mut reprompt = self.base_spec(request);
        reprompt
            .arg("--resume")
            .arg(&session)
            .arg("-p")
            .arg(super::JSON_REPROMPT);
        let second = self.run_turn(&reprompt).await?;

        match second.verdict() {
            Some(verdict) => {
                let conversation = super::conversation_ref(
                    request,
                    reviewer::Backend::Grok,
                    second
                        .result
                        .session_id
                        .as_deref()
                        .or(Some(session.as_str())),
                    true,
                    None,
                );
                Ok(ReviewOutcome {
                    verdict,
                    // Each process reports its own turns only, so the two are disjoint.
                    usage: sum_usage(first.usage(), second.usage()),
                    transcript: Some(super::stitch_transcript(Some(&first.raw), second.raw)),
                    conversation,
                })
            }
            None => bail!(
                "grok did not produce a valid verdict for reviewer '{}' even after \
                 re-prompting for the structured output",
                request.reviewer.name
            ),
        }
    }
}

/// Sum the usage of two disjoint `grok` processes (the review and its reprompt).
/// Returns `None` only when neither reported usage.
fn sum_usage(first: Option<Usage>, second: Option<Usage>) -> Option<Usage> {
    match (first, second) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(Usage {
            tokens_in: a.tokens_in + b.tokens_in,
            tokens_out: a.tokens_out + b.tokens_out,
            cache_read: a.cache_read + b.cache_read,
            cost_usd: Money::from_cents(a.cost_usd.cents() + b.cost_usd.cents()),
        }),
    }
}

/// The parsed `grok --output-format json` envelope plus the raw text.
#[derive(Debug)]
struct Envelope {
    raw: String,
    result: ResultJson,
}

impl Envelope {
    /// Extract a structured [`Verdict`], preferring the CLI's validated
    /// `structuredOutput` and falling back to parsing the `text` as JSON. Returns
    /// `None` if neither yields a schema-conforming, internally consistent verdict,
    /// the condition that triggers the single reprompt.
    fn verdict(&self) -> Option<Verdict> {
        let verdict = self
            .result
            .structured_output
            .as_ref()
            .and_then(|value| serde_json::from_value::<Verdict>(value.clone()).ok())
            .or_else(|| {
                self.result
                    .text
                    .as_deref()
                    .and_then(super::parse_verdict_from_text)
            })?;
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

/// The subset of `grok`'s `--output-format json` envelope Bastion consumes.
/// Unknown fields are ignored so CLI additions do not break parsing.
#[derive(Debug, Deserialize)]
struct ResultJson {
    /// The final assistant text; the fallback verdict source when
    /// `structuredOutput` is absent.
    #[serde(default)]
    text: Option<String>,
    /// The schema-validated structured output, when `--json-schema` is honored.
    #[serde(default, rename = "structuredOutput")]
    structured_output: Option<serde_json::Value>,
    /// The session id, used to resume for a reprompt.
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    usage: Option<UsageJson>,
    /// Total cost of this process in dollars.
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

/// The token-usage shape inside the envelope (Anthropic-style field names).
#[derive(Debug, Deserialize)]
struct UsageJson {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// Parse the `grok` JSON envelope from a finished process.
///
/// An execution failure (non-zero/signal exit, empty output, or unparseable JSON)
/// is `Err` and is never re-prompted: it is the runner's signal to fail a gate
/// closed. A failed `grok` run prints an in-band `{"type":"error",...}` line and
/// exits non-zero, so the exit check covers it.
fn parse_envelope(output: &CommandOutput) -> Result<Envelope> {
    let exit = || {
        output
            .code
            .map_or_else(|| "signal".to_string(), |c| c.to_string())
    };

    let raw = output.stdout.trim();
    if raw.is_empty() {
        bail!(
            "grok produced no output (exit {}): {}",
            exit(),
            output.stderr.trim()
        );
    }
    // Check the exit first: a process that died may print a non-envelope error
    // line, and the exit status is the more useful message.
    if !output.success() {
        bail!(
            "grok exited unsuccessfully (exit {}): {}",
            exit(),
            super::truncate(
                if output.stderr.trim().is_empty() {
                    raw
                } else {
                    &output.stderr
                },
                400
            )
        );
    }
    let result: ResultJson = serde_json::from_str(raw).wrap_err_with(|| {
        format!(
            "grok output was not valid JSON (exit {}): {}",
            exit(),
            super::truncate(raw, 400)
        )
    })?;
    Ok(Envelope {
        raw: output.stdout.clone(),
        result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::command::SystemCommandRunner;
    use crate::event::RunId;
    use crate::reviewer::{Capabilities, Mode, Reviewer};
    use crate::verdict::{Decision, FindingKind};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A [`CommandRunner`] that returns scripted outputs in order, recording the
    /// specs it was asked to run.
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

    fn failed(stderr: &str) -> CommandOutput {
        CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn reviewer() -> Reviewer {
        Reviewer {
            name: "demo".into(),
            trigger: vec!["**".into()].into(),
            mode: Mode::Gate,
            backend: reviewer::Backend::Grok,
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
    ) -> (Result<ReviewOutcome>, ScriptedRunner) {
        let runner = ScriptedRunner::with(outputs);
        let backend = GrokBackend::with_program(runner, "grok-fake");
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: crate::backend::ReviewPurpose::Review,
            native_session_dir: None,
            conversation: None,
        };
        let outcome = backend.review(&request).await;
        (outcome, backend.runner)
    }

    fn pass_envelope() -> String {
        serde_json::json!({
            "text": "{\"verdict\":\"pass\",\"summary\":\"ok\",\"findings\":[]}",
            "stopReason": "end_turn",
            "sessionId": "g-1",
            "structuredOutput": { "verdict": "pass", "summary": "ok", "findings": [] }
        })
        .to_string()
    }

    fn prior_conversation(reviewer: &Reviewer) -> crate::conversation::ConversationRef {
        crate::conversation::ConversationRef::from_backend(
            reviewer::Backend::Grok,
            "g-prior",
            reviewer,
            None,
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn continues_a_prior_conversation_with_the_full_review_prompt() {
        let reviewer = reviewer();
        let prior = prior_conversation(&reviewer);
        let runner = ScriptedRunner::with(vec![ok(&pass_envelope())]);
        let backend = GrokBackend::with_program(runner, "grok-fake");
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: crate::backend::ReviewPurpose::Review,
            native_session_dir: None,
            conversation: Some(&prior),
        };

        let outcome = backend.review(&request).await.unwrap();
        let seen = backend.runner.seen.lock().unwrap();
        assert_eq!(
            seen[0].kind,
            crate::backend::command::LaunchKind::ConversationResume
        );
        let args: Vec<String> = seen[0]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["--resume", "g-prior"]));
        let prompt = &args[args.iter().position(|arg| arg == "-p").unwrap() + 1];
        assert!(prompt.contains("You are reviewing a changeset"));
        assert_eq!(outcome.conversation.unwrap().id().as_str(), "g-1");
    }

    #[tokio::test]
    async fn unavailable_prior_conversation_falls_back_to_a_fresh_review() {
        let reviewer = reviewer();
        let prior = prior_conversation(&reviewer);
        let runner = ScriptedRunner::with(vec![failed("session not found"), ok(&pass_envelope())]);
        let backend = GrokBackend::with_program(runner, "grok-fake");
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: crate::backend::ReviewPurpose::Review,
            native_session_dir: None,
            conversation: Some(&prior),
        };

        let outcome = backend.review(&request).await.unwrap();
        let seen = backend.runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].kind,
            crate::backend::command::LaunchKind::ConversationResume
        );
        assert_eq!(seen[1].kind, crate::backend::command::LaunchKind::Review);
        assert!(!seen[1].args.iter().any(|arg| arg == "--resume"));
        assert_eq!(outcome.conversation.unwrap().id().as_str(), "g-1");
    }

    #[tokio::test]
    async fn builds_the_headless_json_schema_invocation() {
        let (outcome, runner) = review_with(vec![ok(&pass_envelope())], &reviewer()).await;
        outcome.expect("verdict parses");
        let args = runner.nth_args(0);
        for flag in [
            "--output-format",
            "json",
            "--json-schema",
            "--permission-mode",
            "bypassPermissions",
            "-p",
        ] {
            assert!(
                args.iter().any(|a| a == flag),
                "missing `{flag}` in {args:?}"
            );
        }
        // No model pinned: Grok Build resolves its own, so `--model` is absent.
        assert!(!args.iter().any(|a| a == "--model"));
        // The house default effort always applies.
        let e = args
            .iter()
            .position(|a| a == "--reasoning-effort")
            .expect("effort flag");
        assert_eq!(args[e + 1], "high");
        // The prompt carries the shared preamble and the JSON-schema trailer.
        let p = args.iter().position(|a| a == "-p").expect("-p");
        assert!(args[p + 1].starts_with("You are reviewing a changeset"));
        assert!(args[p + 1].contains("Review it."));
        assert!(args[p + 1].contains("Report every issue"));
        assert!(args[p + 1].contains("return your judgment as structured output"));
    }

    #[tokio::test]
    async fn explicit_model_and_effort_are_forwarded_verbatim() {
        let mut rev = reviewer();
        rev.model = Some(serde_yaml_ng::from_str("grok-4.6").unwrap());
        rev.effort = Some(serde_yaml_ng::from_str("xhigh").unwrap());
        rev.env
            .insert("PREVIEW_URL".into(), "http://localhost:3000".into());
        let (outcome, runner) = review_with(vec![ok(&pass_envelope())], &rev).await;
        outcome.expect("verdict parses");
        let args = runner.nth_args(0);
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "grok-4.6");
        let e = args
            .iter()
            .position(|a| a == "--reasoning-effort")
            .expect("effort");
        assert_eq!(args[e + 1], "xhigh");
        let env = &runner.seen.lock().unwrap()[0].env;
        assert_eq!(
            env.get("PREVIEW_URL").map(String::as_str),
            Some("http://localhost:3000")
        );
    }

    #[tokio::test]
    async fn parses_structured_output_and_usage() {
        let envelope = serde_json::json!({
            "text": "done",
            "sessionId": "g-1",
            "usage": { "input_tokens": 1200, "output_tokens": 80, "cache_read_input_tokens": 950 },
            "total_cost_usd": 0.21,
            "structuredOutput": {
                "verdict": "block",
                "summary": "missing tenant scope",
                "findings": [
                    { "kind": "blocking", "path": "src/db.rs", "line_start": 10, "line_end": 12, "detail": "scope by tenant_id" },
                    { "kind": "optional", "path": "src/db.rs", "line_start": 20, "line_end": 20, "detail": "nit" }
                ]
            }
        })
        .to_string();
        let (outcome, _) = review_with(vec![ok(&envelope)], &reviewer()).await;
        let outcome = outcome.expect("verdict parses");
        assert!(outcome.verdict.decision.is_block());
        assert_eq!(outcome.verdict.findings.len(), 2);
        assert_eq!(outcome.verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(outcome.verdict.findings[1].kind, FindingKind::Optional);
        let usage = outcome.usage.expect("usage reported");
        assert_eq!(usage.tokens_in, 1200);
        assert_eq!(usage.tokens_out, 80);
        assert_eq!(usage.cache_read, 950);
        assert_eq!(usage.cost_usd, Money::from_cents(21));
        assert!(outcome.transcript.is_some());
    }

    #[tokio::test]
    async fn falls_back_to_parsing_the_text_as_json() {
        let envelope = serde_json::json!({
            "sessionId": "g-1",
            "text": "Here is my verdict:\n```json\n{\"verdict\":\"pass\",\"summary\":\"ok\"}\n```"
        })
        .to_string();
        let (outcome, _) = review_with(vec![ok(&envelope)], &reviewer()).await;
        assert_eq!(
            outcome.expect("parses from text").verdict.decision,
            Decision::Pass
        );
    }

    #[tokio::test]
    async fn reprompts_once_by_resuming_the_session_and_sums_usage() {
        let bad = serde_json::json!({
            "sessionId": "g-9",
            "text": "I think it looks good but I forgot the schema.",
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
            "total_cost_usd": 0.20
        })
        .to_string();
        let good = serde_json::json!({
            "sessionId": "g-9",
            "structuredOutput": { "verdict": "pass", "summary": "ok now" },
            "usage": { "input_tokens": 500, "output_tokens": 50 },
            "total_cost_usd": 0.10
        })
        .to_string();
        let (outcome, runner) = review_with(vec![ok(&bad), ok(&good)], &reviewer()).await;
        let outcome = outcome.expect("reprompt succeeds");
        assert_eq!(outcome.verdict.summary, "ok now");
        assert_eq!(runner.calls(), 2);
        let second = runner.nth_args(1);
        let r = second
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume");
        assert_eq!(second[r + 1], "g-9");
        let p = second.iter().position(|a| a == "-p").expect("-p");
        assert_eq!(second[p + 1], super::super::JSON_REPROMPT);
        // Disjoint processes: usage sums.
        let usage = outcome.usage.expect("usage");
        assert_eq!(usage.tokens_in, 1500);
        assert_eq!(usage.tokens_out, 150);
        assert_eq!(usage.cost_usd, Money::from_cents(30));
        assert!(outcome.transcript.unwrap().contains("forgot the schema"));
    }

    #[tokio::test]
    async fn fails_closed_when_reprompt_also_malformed() {
        let bad = serde_json::json!({ "sessionId": "g", "text": "nope" }).to_string();
        let (outcome, _) = review_with(vec![ok(&bad), ok(&bad)], &reviewer()).await;
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("even after re-prompting")
        );
    }

    #[tokio::test]
    async fn inconsistent_block_without_findings_triggers_reprompt() {
        let inconsistent = serde_json::json!({
            "sessionId": "g",
            "structuredOutput": { "verdict": "block", "summary": "no reason given" }
        })
        .to_string();
        let fixed = serde_json::json!({
            "sessionId": "g",
            "structuredOutput": {
                "verdict": "block",
                "summary": "now with reason",
                "findings": [{ "kind": "blocking", "path": "a.rs", "line_start": 1, "line_end": 1, "detail": "fix" }]
            }
        })
        .to_string();
        let (outcome, runner) = review_with(vec![ok(&inconsistent), ok(&fixed)], &reviewer()).await;
        let outcome = outcome.expect("reprompt fixes consistency");
        assert!(outcome.verdict.is_consistent());
        assert_eq!(runner.calls(), 2);
    }

    #[tokio::test]
    async fn missing_session_id_cannot_reprompt() {
        let bad = serde_json::json!({ "text": "no session here" }).to_string();
        let (outcome, _) = review_with(vec![ok(&bad)], &reviewer()).await;
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("no session id to resume")
        );
    }

    #[tokio::test]
    async fn empty_output_is_an_execution_error() {
        let empty = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let (outcome, _) = review_with(vec![empty], &reviewer()).await;
        assert!(outcome.unwrap_err().to_string().contains("no output"));
    }

    #[tokio::test]
    async fn nonzero_exit_with_parseable_pass_is_rejected() {
        // The real CLI prints an in-band error object and exits 1 on failure; a
        // parseable `pass` alongside a bad exit must not be trusted either way.
        let nonzero = CommandOutput {
            code: Some(1),
            stdout: pass_envelope(),
            stderr: "crashed".into(),
        };
        let (outcome, runner) = review_with(vec![nonzero], &reviewer()).await;
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("exited unsuccessfully")
        );
        assert_eq!(runner.calls(), 1);
    }

    #[tokio::test]
    async fn in_band_error_line_surfaces_in_the_message() {
        let errored = CommandOutput {
            code: Some(1),
            stdout: r#"{"type":"error","message":"Couldn't set model 'x': unknown model id"}"#
                .into(),
            stderr: String::new(),
        };
        let (outcome, _) = review_with(vec![errored], &reviewer()).await;
        let msg = outcome.unwrap_err().to_string();
        assert!(msg.contains("exited unsuccessfully"));
        assert!(msg.contains("unknown model id"));
    }

    #[test]
    fn id_is_grok() {
        let backend = GrokBackend::with_program(ScriptedRunner::default(), "grok-fake");
        assert_eq!(backend.id(), reviewer::Backend::Grok);
    }

    /// Compile a real native fake `grok` that prints `envelope_json`, so the test
    /// drives the real [`SystemCommandRunner`] path. Returns `None` (detect-and-skip)
    /// when no `rustc` is on `PATH`.
    fn build_fake_grok(dir: &Path, envelope_json: &str) -> Option<PathBuf> {
        let src = format!("fn main() {{ print!({envelope_json:?}); }}\n");
        let src_path = dir.join("fake_grok.rs");
        std::fs::write(&src_path, src).unwrap();
        let out_path = dir.join(if cfg!(windows) {
            "grok-fake.exe"
        } else {
            "grok-fake"
        });
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
            _ => None,
        }
    }

    #[tokio::test]
    async fn end_to_end_through_a_real_fake_executable() {
        let dir = tempfile::tempdir().unwrap();
        let envelope = serde_json::json!({
            "text": "done",
            "sessionId": "g-e2e",
            "structuredOutput": { "verdict": "pass", "summary": "real subprocess ok", "findings": [] }
        })
        .to_string();
        let Some(program) = build_fake_grok(dir.path(), &envelope) else {
            eprintln!("skipping end-to-end test: no usable rustc on PATH");
            return;
        };
        let backend = GrokBackend::with_program(SystemCommandRunner, &program);
        let r = reviewer();
        let run = RunId("r-e2e".into());
        let root = dir.path().to_path_buf();
        let request = ReviewRequest {
            reviewer: &r,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: crate::backend::ReviewPurpose::Review,
            native_session_dir: None,
            conversation: None,
        };
        let outcome = backend.review(&request).await.expect("verdict");
        assert_eq!(outcome.verdict.summary, "real subprocess ok");
    }
}
