//! The Codex backend.
//!
//! Translates a reviewer's execution profile into a headless `codex exec`-style
//! invocation, runs it through the injectable [`CommandRunner`] seam, and parses
//! the agent's final structured output into a [`Verdict`] (`docs/developer-guide/design.md`,
//! "Agent backends").
//!
//! # The Codex invocation contract
//!
//! Bastion drives Codex in its headless `exec` mode and asks for a machine
//! readable event stream (`codex exec --json
//! --dangerously-bypass-approvals-and-sandbox`). Each line of stdout is a JSON
//! event; Bastion reconstructs the transcript from those events, takes the final
//! agent message as the reviewer's structured output, and reads token/cost
//! accounting from the usage event when Codex reports it. The reviewer's prompt
//! (with [`inputs`](crate::reviewer::Reviewer::inputs) interpolated and a trailing
//! instruction pinning the verdict schema) is piped to Codex over stdin -- the
//! final `-` argument tells Codex to read the task from there -- so a long,
//! multi-line prompt is never an OS argument. Reviewer
//! [`env`](crate::reviewer::Reviewer::env) is propagated into the child process.
//! The bypass flag is the counterpart to the Claude backend's
//! `--permission-mode bypassPermissions`: an unattended reviewer must not stop to
//! ask, and it also lets Codex run in an untrusted, fresh CI checkout.
//!
//! # Fail-closed parsing
//!
//! If the final message does not carry a schema-conforming verdict, the backend
//! re-prompts the same session once for *just* the structured output (per
//! `docs/developer-guide/design.md`), then gives up with an error. The runner turns that error
//! into a fail-closed `block` for gates; this backend never invents a verdict.

use std::ffi::OsString;

use color_eyre::eyre::{Result, bail, eyre};
use serde::Deserialize;

use crate::reviewer::{self};
use crate::verdict::{Money, Usage, Verdict};

use super::command::{CommandRunner, CommandSpec, resolve_program};
use super::{Backend, ReviewOutcome, ReviewRequest};

/// Environment variable that overrides the `codex` program path (tests point this
/// at a fake executable; deployments can pin a specific binary).
pub const PROGRAM_ENV: &str = "BASTION_CODEX_BIN";

/// The default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "codex";

/// The Codex agent backend.
///
/// Generic over the [`CommandRunner`] so production wires a real subprocess while
/// tests drive a fake executable through the identical path.
#[derive(Debug, Clone)]
pub struct CodexBackend<R> {
    runner: R,
    program: OsString,
    /// Leading args before the prompt for a first-pass run (default
    /// `["exec", "--json"]`).
    base_args: Vec<String>,
    /// Leading args that resume a session by id, used for the reprompt so the
    /// schema is requested in the *same* session (default
    /// `["exec", "resume", "--json"]`, followed by the thread id).
    resume_args: Vec<String>,
}

impl<R: CommandRunner> CodexBackend<R> {
    /// Build a backend over `runner`, resolving the `codex` program from
    /// [`PROGRAM_ENV`] (falling back to [`DEFAULT_PROGRAM`] on `PATH`).
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self::with_program(runner, resolve_program(DEFAULT_PROGRAM, PROGRAM_ENV))
    }

    /// Build a backend over `runner` with an explicit program path and the
    /// default `exec` argument layout.
    #[must_use]
    pub fn with_program(runner: R, program: impl Into<OsString>) -> Self {
        Self {
            runner,
            program: program.into(),
            // Reviewers run unattended, so Codex must never stop to ask: this is
            // the counterpart to the Claude backend's `--permission-mode
            // bypassPermissions`. `--dangerously-bypass-approvals-and-sandbox`
            // disables the approval prompts and the command sandbox, and also lets
            // Codex run in a checkout it has not interactively "trusted" (a fresh CI
            // clone), so it subsumes `--skip-git-repo-check`. This matches Bastion's
            // threat model: the checkout is trusted and the reviewer runs with the
            // same latitude on both backends.
            base_args: vec![
                "exec".to_string(),
                "--json".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ],
            resume_args: vec![
                "exec".to_string(),
                "resume".to_string(),
                "--json".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ],
        }
    }

    /// Build a backend with full control over the program and argument layout.
    /// Used by tests that drive a fake executable through a launcher.
    #[must_use]
    pub fn with_command(
        runner: R,
        program: impl Into<OsString>,
        base_args: Vec<String>,
        resume_args: Vec<String>,
    ) -> Self {
        Self {
            runner,
            program: program.into(),
            base_args,
            resume_args,
        }
    }

    /// The first-pass invocation: the full review prompt with the schema appended.
    fn first_spec(&self, request: &ReviewRequest<'_>, prompt: &str) -> CommandSpec {
        self.build_spec(request, self.base_args.clone(), prompt)
    }

    /// The reprompt invocation: resume `thread_id` in the same session when one is
    /// known, else fall back to a fresh first-pass invocation.
    fn reprompt_spec(
        &self,
        request: &ReviewRequest<'_>,
        thread_id: Option<&str>,
        prompt: &str,
    ) -> CommandSpec {
        match thread_id {
            Some(id) => {
                let mut args = self.resume_args.clone();
                args.push(id.to_string());
                self.build_spec(request, args, prompt)
            }
            None => self.first_spec(request, prompt),
        }
    }

    /// Assemble a [`CommandSpec`] from leading `args`, passing `prompt` over the
    /// child's stdin (with a trailing `-` telling Codex to read it from there) and
    /// forwarding the reviewer's env and checkout.
    ///
    /// The prompt goes through stdin rather than argv so a long, multi-line prompt
    /// is never an OS argument: it dodges argument-length limits and, on Windows,
    /// the spawner's refusal to forward special characters to a `.cmd` shim.
    fn build_spec(
        &self,
        request: &ReviewRequest<'_>,
        leading: Vec<String>,
        prompt: &str,
    ) -> CommandSpec {
        let mut spec = CommandSpec::new(self.program.clone(), request.repo_root);
        for arg in &leading {
            spec.arg(arg);
        }
        // Pin model and reasoning effort when set. Codex has no Bastion house
        // default model (it resolves its own), so a model only appears when a
        // reviewer or the registry default pins one. Effort always applies: absent,
        // the house default (high) flows through. The effort value rides `-c
        // model_reasoning_effort=...`, whose RHS is parsed as TOML, so it is quoted
        // to land as a string literal.
        if let Some(model) = &request.reviewer.model {
            spec.arg("-m").arg(model.as_str());
        }
        spec.arg("-c").arg(format!(
            "model_reasoning_effort=\"{}\"",
            request
                .reviewer
                .effort
                .as_ref()
                .map_or(reviewer::DEFAULT_EFFORT, reviewer::Effort::as_str)
        ));
        spec.arg("-");
        spec.stdin(prompt);
        for (key, value) in &request.reviewer.env {
            spec.env.insert(key.clone(), value.clone());
        }
        spec
    }

    /// Run one Codex invocation and parse its event stream into a session.
    async fn run_once(&self, spec: &CommandSpec) -> Result<CodexSession> {
        let output = self.runner.run(spec).await?;
        if !output.success() {
            bail!(
                "codex exited with status {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                output.stderr.trim(),
            );
        }
        CodexSession::parse(&output.stdout)
    }
}

impl<R: CommandRunner> Backend for CodexBackend<R> {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::Codex
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        let prompt = build_prompt(request);

        // First pass: the full review with the schema instruction appended.
        let first = self.first_spec(request, &prompt);
        let session = self.run_once(&first).await?;

        if let Some(verdict) = session.parse_verdict() {
            return Ok(outcome(verdict, session, None));
        }

        // The agent's final message was not a schema-conforming verdict. Per
        // design.md, re-run the *same session* asking for just the structured
        // output, then fail closed. Resume by thread id when Codex reported one;
        // when resuming, the new turn is only the reprompt suffix (the session
        // already holds the review). Without a thread id we fall back to a fresh
        // session and must re-send the full prompt.
        let reprompt_text = super::reprompt_text(&prompt, session.thread_id.is_some());
        let retry = self.reprompt_spec(request, session.thread_id.as_deref(), &reprompt_text);
        let retry_session = self.run_once(&retry).await?;

        match retry_session.parse_verdict() {
            Some(verdict) => Ok(outcome(verdict, retry_session, Some(&session))),
            None => Err(eyre!(
                "codex did not emit a schema-conforming verdict after one reprompt; \
                 failing closed. final message was:\n{}",
                retry_session
                    .final_message()
                    .unwrap_or("(no agent message)")
            )),
        }
    }
}

/// Assemble a [`ReviewOutcome`] from a parsed verdict and the session it came
/// from, optionally prepending an earlier session's transcript (the original
/// review, when the verdict was recovered on a reprompt).
fn outcome(verdict: Verdict, session: CodexSession, prior: Option<&CodexSession>) -> ReviewOutcome {
    let transcript =
        super::stitch_transcript(prior.map(|p| p.transcript.as_str()), session.transcript);
    ReviewOutcome {
        verdict,
        // Prefer the reprompt session's usage, falling back to the original's so
        // accounting is not lost when the resume turn reports none.
        usage: session.usage.or_else(|| prior.and_then(|p| p.usage)),
        transcript: Some(transcript),
    }
}

/// Build the full prompt handed to Codex for `request`: the [shared review
/// body](super::review_prompt) closed with the fenced-YAML
/// [`SCHEMA_INSTRUCTION`](super::SCHEMA_INSTRUCTION).
fn build_prompt(request: &ReviewRequest<'_>) -> String {
    super::review_prompt(request, super::SCHEMA_INSTRUCTION)
}

/// A parsed Codex `exec --json` session: the reconstructed transcript, the final
/// agent message, usage when reported, and the thread id used to resume.
#[derive(Debug, Clone, Default)]
struct CodexSession {
    /// The human-readable transcript, reconstructed from the event stream.
    transcript: String,
    /// The text of the final agent message, if any.
    last_message: Option<String>,
    /// Token/cost accounting, when Codex reported it.
    usage: Option<Usage>,
    /// The thread/session id, when Codex reported it, for resuming on a reprompt.
    thread_id: Option<String>,
}

impl CodexSession {
    /// Parse a Codex `exec --json` stdout stream (JSON-lines) into a session.
    ///
    /// Unknown event types are tolerated, so it survives Codex adding events.
    /// Non-JSON lines are kept in the transcript verbatim (some builds interleave
    /// plain log lines).
    ///
    /// # Errors
    ///
    /// Returns an error if the stream carried neither a recognized event nor any
    /// other output to record.
    fn parse(stdout: &str) -> Result<Self> {
        let mut acc = CodexSession::default();
        let mut saw_event = false;

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<CodexEvent>(trimmed) {
                Ok(event) => {
                    saw_event = true;
                    event.fold_into(&mut acc);
                }
                Err(_) => {
                    // Not a recognized JSON event line; keep it in the transcript.
                    acc.transcript.push_str(line);
                    acc.transcript.push('\n');
                }
            }
        }

        if !saw_event && acc.transcript.is_empty() {
            bail!("codex produced no output to parse");
        }

        Ok(acc)
    }

    /// Record a message in the transcript and as the latest agent message.
    fn record_message(&mut self, message: String) {
        self.transcript.push_str(&message);
        self.transcript.push('\n');
        self.last_message = Some(message);
    }

    /// Record reasoning text in the transcript only.
    fn record_reasoning(&mut self, text: &str) {
        self.transcript.push_str(text);
        self.transcript.push('\n');
    }

    /// Record token/cost usage.
    fn record_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cost_usd: Option<f64>,
    ) {
        self.usage = Some(Usage {
            tokens_in: input_tokens,
            tokens_out: output_tokens,
            cache_read: cached_input_tokens,
            cost_usd: cost_usd.map_or_else(Money::default, super::money_from_dollars),
        });
    }

    /// The final agent message text, if the session produced one.
    fn final_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    /// Parse the final agent message into a [`Verdict`], if it carries one.
    fn parse_verdict(&self) -> Option<Verdict> {
        let message = self.last_message.as_deref()?;
        super::extract_verdict(message)
    }
}

/// One event in a Codex `exec --json` stream.
///
/// Modeled as a tagged union over the `type` field. This matches the current
/// Codex threaded event schema (`thread.started`, `item.completed`,
/// `turn.completed`) and also tolerates the older flat schema (`agent_message`,
/// `token_count`) so the backend works across Codex versions. Anything else
/// deserializes to [`CodexEvent::Other`] and is ignored beyond the transcript.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    /// The session opened; carries the thread id used to resume for the reprompt.
    #[serde(rename = "thread.started")]
    ThreadStarted {
        /// The thread/session identifier.
        thread_id: String,
    },
    /// A completed conversation item (current schema). Agent-message items carry
    /// the text; the final one is the structured verdict.
    #[serde(rename = "item.completed")]
    ItemCompleted {
        /// The completed item.
        item: CodexItem,
    },
    /// A turn finished; carries token usage in the current schema.
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        /// Token usage for the turn.
        usage: CodexUsage,
    },
    /// A message from the agent (legacy flat schema).
    #[serde(rename = "agent_message")]
    AgentMessage {
        /// The message text.
        message: String,
    },
    /// A reasoning/thinking trace (legacy flat schema); transcript only.
    #[serde(rename = "agent_reasoning")]
    AgentReasoning {
        /// The reasoning text.
        text: String,
    },
    /// Token and cost accounting (legacy flat schema).
    #[serde(rename = "token_count")]
    TokenCount {
        /// Input tokens consumed.
        #[serde(default)]
        input_tokens: u64,
        /// Output tokens produced.
        #[serde(default)]
        output_tokens: u64,
        /// Cached input tokens (prompt-cache hits), a subset of `input_tokens`.
        #[serde(default)]
        cached_input_tokens: u64,
        /// Session cost in US dollars, when Codex computes it.
        #[serde(default)]
        cost_usd: Option<f64>,
    },
    /// Any other event; ignored beyond being recorded as "seen".
    #[serde(other)]
    Other,
}

/// A completed conversation item in the current Codex schema.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexItem {
    /// An assistant message; the final one is the verdict.
    AgentMessage {
        /// The message text.
        text: String,
    },
    /// A reasoning trace; transcript only.
    Reasoning {
        /// The reasoning text.
        text: String,
    },
    /// Any other item kind; ignored.
    #[serde(other)]
    Other,
}

/// Token usage as carried by a `turn.completed` event in the current schema.
#[derive(Debug, Deserialize)]
struct CodexUsage {
    /// Input tokens consumed.
    #[serde(default)]
    input_tokens: u64,
    /// Output tokens produced.
    #[serde(default)]
    output_tokens: u64,
    /// Cached input tokens (prompt-cache hits), a subset of `input_tokens`.
    #[serde(default)]
    cached_input_tokens: u64,
    /// Session cost in US dollars, when Codex reports it.
    #[serde(default)]
    cost_usd: Option<f64>,
}

impl CodexEvent {
    /// Fold this event into `acc`.
    fn fold_into(self, acc: &mut CodexSession) {
        match self {
            CodexEvent::ThreadStarted { thread_id } => acc.thread_id = Some(thread_id),
            CodexEvent::ItemCompleted { item } => match item {
                CodexItem::AgentMessage { text } => acc.record_message(text),
                CodexItem::Reasoning { text } => acc.record_reasoning(&text),
                CodexItem::Other => {}
            },
            CodexEvent::TurnCompleted { usage } => {
                acc.record_usage(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.cost_usd,
                );
            }
            CodexEvent::AgentMessage { message } => acc.record_message(message),
            CodexEvent::AgentReasoning { text } => acc.record_reasoning(&text),
            CodexEvent::TokenCount {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cost_usd,
            } => acc.record_usage(input_tokens, output_tokens, cached_input_tokens, cost_usd),
            CodexEvent::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::backend::command::{CommandOutput, SystemCommandRunner, program_available};
    use crate::event::RunId;
    use crate::reviewer::{Capabilities, Mode, Reviewer};
    use crate::verdict::{Decision, FindingKind};

    /// A [`CommandRunner`] that returns canned outputs in sequence and records the
    /// command specs it was handed, so tests can assert on the translated call.
    #[derive(Debug, Default)]
    struct FakeRunner {
        responses: Mutex<std::collections::VecDeque<CommandOutput>>,
        seen: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        fn new(responses: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn specs(&self) -> Vec<CommandSpec> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
            self.seen.lock().unwrap().push(spec.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| eyre!("FakeRunner ran out of canned responses"))
        }
    }

    /// The arguments of a recorded spec as plain strings, for assertions.
    fn args_of(spec: &CommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The prompt a spec pipes over stdin (where the backend now puts it), for
    /// assertions that used to read the final positional argument.
    fn stdin_of(spec: &CommandSpec) -> &str {
        spec.stdin.as_deref().expect("spec carries a stdin prompt")
    }

    fn ok_output(stdout: impl Into<String>) -> CommandOutput {
        CommandOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A JSON-lines stream in the legacy flat schema. `usage` is
    /// `(input, output, cached_input, cost)`.
    fn stream(messages: &[&str], usage: Option<(u64, u64, u64, f64)>) -> String {
        let mut out = String::new();
        for message in messages {
            let event = serde_json::json!({ "type": "agent_message", "message": message });
            out.push_str(&serde_json::to_string(&event).unwrap());
            out.push('\n');
        }
        if let Some((tin, tout, cached, cost)) = usage {
            let event = serde_json::json!({
                "type": "token_count",
                "input_tokens": tin,
                "output_tokens": tout,
                "cached_input_tokens": cached,
                "cost_usd": cost,
            });
            out.push_str(&serde_json::to_string(&event).unwrap());
            out.push('\n');
        }
        out
    }

    /// A JSON-lines stream in the current threaded schema. `usage` is
    /// `(input, output, cached_input, cost)`.
    fn threaded_stream(
        thread_id: &str,
        messages: &[&str],
        usage: Option<(u64, u64, u64, f64)>,
    ) -> String {
        let mut out = String::new();
        let started = serde_json::json!({ "type": "thread.started", "thread_id": thread_id });
        out.push_str(&serde_json::to_string(&started).unwrap());
        out.push('\n');
        for message in messages {
            let event = serde_json::json!({
                "type": "item.completed",
                "item": { "type": "agent_message", "text": message },
            });
            out.push_str(&serde_json::to_string(&event).unwrap());
            out.push('\n');
        }
        if let Some((tin, tout, cached, cost)) = usage {
            let event = serde_json::json!({
                "type": "turn.completed",
                "usage": {
                    "input_tokens": tin,
                    "output_tokens": tout,
                    "cached_input_tokens": cached,
                    "cost_usd": cost,
                },
            });
            out.push_str(&serde_json::to_string(&event).unwrap());
            out.push('\n');
        }
        out
    }

    fn reviewer() -> Reviewer {
        Reviewer {
            name: "demo".into(),
            trigger: vec!["**".into()],
            mode: Mode::Gate,
            backend: reviewer::Backend::Codex,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Capabilities::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "Check the thing.".into(),
        }
    }

    fn request<'a>(reviewer: &'a Reviewer, run: &'a RunId, root: &'a Path) -> ReviewRequest<'a> {
        ReviewRequest {
            reviewer,
            run,
            repo_root: root,
            base: "main",
            context: crate::context::ReviewContext::empty(),
        }
    }

    async fn review_with(
        reviewer: &Reviewer,
        responses: impl IntoIterator<Item = CommandOutput>,
    ) -> (Result<ReviewOutcome>, Vec<CommandSpec>) {
        let backend = CodexBackend::with_program(FakeRunner::new(responses), DEFAULT_PROGRAM);
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let req = request(reviewer, &run, &root);
        let outcome = backend.review(&req).await;
        let specs = backend.runner.specs();
        (outcome, specs)
    }

    #[test]
    fn build_prompt_splices_a_non_empty_review_context_after_the_instruction() {
        // A non-empty context must reach the Codex prompt, spliced between the reviewer's
        // own instruction and the exhaustive-findings instruction. Removing or misplacing
        // the `context_segment` splice would fail this without needing a full review.
        let reviewer = reviewer();
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let context = crate::context::ReviewContext {
            intent: Some("Deliberate schema change; the migration self-heals.".into()),
            ..Default::default()
        };
        let req = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            context: &context,
        };
        let prompt = build_prompt(&req);
        let prompt_at = prompt
            .find("Check the thing.")
            .expect("reviewer prompt present");
        let context_at = prompt
            .find("### Author's stated intent")
            .expect("review context spliced in");
        let exhaustive_at = prompt
            .find(crate::backend::EXHAUSTIVE_FINDINGS_INSTRUCTION)
            .expect("exhaustive instruction present");
        assert!(
            prompt_at < context_at && context_at < exhaustive_at,
            "context must sit after the reviewer instruction and before the exhaustive one"
        );
    }

    #[tokio::test]
    async fn applies_house_default_effort_and_no_model_when_unset() {
        let message = "```yaml\nverdict: pass\nsummary: ok\nfindings: []\n```";
        let (_, specs) = review_with(&reviewer(), [ok_output(stream(&[message], None))]).await;
        let args = args_of(&specs[0]);
        // No model pinned: Codex resolves its own, so `-m` is absent.
        assert!(!args.iter().any(|a| a == "-m"), "got args: {args:?}");
        // Effort always applies; absent, the house default (high) flows through.
        assert!(
            args.contains(&"model_reasoning_effort=\"high\"".to_string()),
            "got args: {args:?}"
        );
    }

    #[tokio::test]
    async fn pins_model_and_forwards_effort_verbatim() {
        let message = "```yaml\nverdict: pass\nsummary: ok\nfindings: []\n```";
        let mut rev = reviewer();
        rev.model = Some(serde_yaml_ng::from_str("gpt-5").unwrap());
        // A Codex-specific level: forwarded as-is, no remapping.
        rev.effort = Some(serde_yaml_ng::from_str("minimal").unwrap());
        let (_, specs) = review_with(&rev, [ok_output(stream(&[message], None))]).await;
        let args = args_of(&specs[0]);
        let m = args
            .iter()
            .position(|a| a == "-m")
            .expect("model flag present");
        assert_eq!(args[m + 1], "gpt-5");
        assert!(args.contains(&"model_reasoning_effort=\"minimal\"".to_string()));
    }

    #[tokio::test]
    async fn id_is_codex() {
        let backend = CodexBackend::with_program(FakeRunner::default(), DEFAULT_PROGRAM);
        assert_eq!(backend.id(), reviewer::Backend::Codex);
    }

    #[tokio::test]
    async fn happy_path_pass_verdict_parses() {
        let message = "Looks fine.\n\n```yaml\nverdict: pass\nsummary: all good\nfindings: []\n```";
        let (outcome, specs) =
            review_with(&reviewer(), [ok_output(stream(&[message], None))]).await;
        let outcome = outcome.expect("verdict parses");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "all good");
        assert!(outcome.verdict.findings.is_empty());
        assert!(outcome.usage.is_none());
        assert!(outcome.transcript.unwrap().contains("Looks fine."));
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn happy_path_block_verdict_with_findings_parses() {
        let message = "\
Found an issue.

```yaml
verdict: block
summary: unscoped query
findings:
  - kind: blocking
    path: src/db.ts
    line_start: 10
    line_end: 12
    detail: scope by tenant_id
```";
        let (outcome, _) = review_with(&reviewer(), [ok_output(stream(&[message], None))]).await;
        let verdict = outcome.expect("verdict parses").verdict;
        assert_eq!(verdict.decision, Decision::Block);
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(verdict.findings[0].path, "src/db.ts");
        assert!(verdict.is_consistent());
    }

    #[tokio::test]
    async fn every_finding_in_the_verdict_is_surfaced_not_just_the_first() {
        // Regression for under-reporting: a block that lists several findings must
        // surface all of them, so the author fixes the complete set in one pass
        // instead of one review cycle per issue. Nothing in the parse path may
        // collapse the list to the first (or first blocking) finding.
        let message = "\
Found several issues.

```yaml
verdict: block
summary: three prose tells
findings:
  - kind: blocking
    path: README.md
    line_start: 1
    line_end: 1
    detail: aphorism opener
  - kind: blocking
    path: README.md
    line_start: 5
    line_end: 6
    detail: manufactured antithesis
  - kind: optional
    path: docs/guide.md
    line_start: 9
    line_end: 9
    detail: dramatic colon
```";
        let (outcome, _) = review_with(&reviewer(), [ok_output(stream(&[message], None))]).await;
        let verdict = outcome.expect("verdict parses").verdict;
        assert_eq!(verdict.decision, Decision::Block);
        assert_eq!(verdict.findings.len(), 3);
        let details: Vec<&str> = verdict.findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "aphorism opener",
                "manufactured antithesis",
                "dramatic colon"
            ]
        );
        // The mix of blocking and optional findings is preserved, in order.
        assert_eq!(verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(verdict.findings[2].kind, FindingKind::Optional);
        assert_eq!(verdict.findings[2].path, "docs/guide.md");
    }

    #[tokio::test]
    async fn prompt_asks_for_exhaustive_findings() {
        let (_, specs) = review_with(
            &reviewer(),
            [ok_output(stream(
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))],
        )
        .await;
        let prompt = stdin_of(&specs[0]);
        assert!(prompt.contains("Report every issue you can identify"));
        assert!(prompt.contains("Do not stop after the"));
        // The exhaustive instruction precedes the schema instruction.
        let exhaustive_at = prompt.find("Report every issue").expect("present");
        let schema_at = prompt.find("structured verdict").expect("present");
        assert!(exhaustive_at < schema_at);
    }

    #[tokio::test]
    async fn usage_is_captured_when_reported() {
        let message = "```yaml\nverdict: pass\nsummary: ok\n```";
        let (outcome, _) = review_with(
            &reviewer(),
            [ok_output(stream(
                &[message],
                Some((18204, 1560, 4096, 0.21)),
            ))],
        )
        .await;
        let usage = outcome.expect("parses").usage.expect("usage present");
        assert_eq!(usage.tokens_in, 18204);
        assert_eq!(usage.tokens_out, 1560);
        assert_eq!(usage.cache_read, 4096);
        assert_eq!(usage.cost_usd, Money::from_cents(21));
    }

    #[tokio::test]
    async fn usage_absent_leaves_none() {
        let message = "```yaml\nverdict: pass\nsummary: ok\n```";
        let (outcome, _) = review_with(&reviewer(), [ok_output(stream(&[message], None))]).await;
        assert!(outcome.expect("parses").usage.is_none());
    }

    #[tokio::test]
    async fn malformed_output_triggers_one_reprompt_then_succeeds() {
        let bad = ok_output(stream(&["I reviewed it but forgot the schema."], None));
        let good = ok_output(stream(
            &["```yaml\nverdict: pass\nsummary: recovered\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        let verdict = outcome.expect("recovers on reprompt").verdict;
        assert_eq!(verdict.decision, Decision::Pass);
        assert_eq!(verdict.summary, "recovered");
        assert_eq!(specs.len(), 2);
        // The prompt rides stdin now; the final argument is the `-` placeholder.
        assert_eq!(args_of(&specs[0]).last().unwrap(), "-");
        assert!(!stdin_of(&specs[0]).contains("did not contain"));
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
    }

    #[tokio::test]
    async fn malformed_twice_fails_closed() {
        let bad1 = ok_output(stream(&["no verdict here"], None));
        let bad2 = ok_output(stream(&["still no verdict"], None));
        let (outcome, specs) = review_with(&reviewer(), [bad1, bad2]).await;
        let err = outcome.expect_err("fails closed after one reprompt");
        assert!(
            err.to_string()
                .contains("did not emit a schema-conforming verdict")
        );
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn inconsistent_block_is_rejected_and_reprompted() {
        let inconsistent = ok_output(stream(
            &["```yaml\nverdict: block\nsummary: no reason\nfindings: []\n```"],
            None,
        ));
        let recovered = ok_output(stream(&["```yaml\nverdict: pass\nsummary: ok\n```"], None));
        let (outcome, specs) = review_with(&reviewer(), [inconsistent, recovered]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn usage_without_cached_input_tokens_defaults_to_zero() {
        // A token_count event that omits `cached_input_tokens` (no prompt-cache hits)
        // must still parse, defaulting cache_read to 0.
        let stdout = concat!(
            "{\"type\":\"agent_message\",\"message\":\"```yaml\\nverdict: pass\\nsummary: ok\\n```\"}\n",
            "{\"type\":\"token_count\",\"input_tokens\":500,\"output_tokens\":40,\"cost_usd\":0.05}\n",
        );
        let (outcome, _) = review_with(&reviewer(), [ok_output(stdout)]).await;
        let usage = outcome.expect("parses").usage.expect("usage present");
        assert_eq!(usage.tokens_in, 500);
        assert_eq!(usage.cache_read, 0);
    }

    #[tokio::test]
    async fn current_threaded_schema_parses_message_and_usage() {
        let message = "```yaml\nverdict: pass\nsummary: threaded\n```";
        let stdout = threaded_stream("th-123", &[message], Some((100, 20, 64, 0.05)));
        let (outcome, specs) = review_with(&reviewer(), [ok_output(stdout)]).await;
        let outcome = outcome.expect("threaded schema parses");
        assert_eq!(outcome.verdict.summary, "threaded");
        let usage = outcome.usage.expect("usage from turn.completed");
        assert_eq!(usage.tokens_in, 100);
        assert_eq!(usage.tokens_out, 20);
        assert_eq!(usage.cache_read, 64);
        assert_eq!(usage.cost_usd, Money::from_cents(5));
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn reprompt_resumes_the_same_session_by_thread_id() {
        let bad = ok_output(threaded_stream("th-abc", &["no verdict yet"], None));
        let good = ok_output(threaded_stream(
            "th-abc",
            &["```yaml\nverdict: pass\nsummary: resumed\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.summary, "resumed");
        assert_eq!(specs.len(), 2);
        let retry = args_of(&specs[1]);
        assert_eq!(retry[0], "exec");
        assert_eq!(retry[1], "resume");
        assert!(retry.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(retry.contains(&"th-abc".to_string()));
        assert_eq!(retry.last().unwrap(), "-");
        // On resume the new turn is only the reprompt suffix, not the full review.
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
        assert!(!stdin_of(&specs[1]).contains("Check the thing."));
    }

    #[tokio::test]
    async fn reprompt_without_thread_id_falls_back_to_a_fresh_session() {
        let bad = ok_output(stream(&["no verdict"], None));
        let good = ok_output(stream(&["```yaml\nverdict: pass\nsummary: ok\n```"], None));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        let retry = args_of(&specs[1]);
        assert_eq!(retry[0], "exec");
        assert_eq!(retry[1], "--json");
        assert_eq!(retry.last().unwrap(), "-");
        // Without a thread id the fresh session must re-send the full prompt.
        assert!(stdin_of(&specs[1]).contains("Check the thing."));
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
    }

    #[tokio::test]
    async fn recovered_transcript_includes_the_original_session() {
        let bad = ok_output(threaded_stream(
            "th-x",
            &["I did a thorough review of the database layer."],
            Some((500, 40, 128, 0.10)),
        ));
        let good = ok_output(threaded_stream(
            "th-x",
            &["```yaml\nverdict: pass\nsummary: ok\n```"],
            None,
        ));
        let (outcome, _) = review_with(&reviewer(), [bad, good]).await;
        let outcome = outcome.expect("recovers");
        let transcript = outcome.transcript.unwrap();
        assert!(transcript.contains("thorough review of the database layer"));
        assert!(transcript.contains("verdict: pass"));
        assert_eq!(outcome.usage.expect("usage carried over").tokens_in, 500);
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error() {
        let failed = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let (outcome, _) = review_with(&reviewer(), [failed]).await;
        let err = outcome.expect_err("non-zero exit errors");
        assert!(err.to_string().contains("codex exited with status 1"));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn prompt_inputs_are_interpolated_and_schema_appended() {
        let mut reviewer = reviewer();
        reviewer.prompt = "Test against ${preview_url} now.".into();
        reviewer
            .inputs
            .insert("preview_url".into(), "http://localhost:3000".into());
        let (_, specs) = review_with(
            &reviewer,
            [ok_output(stream(
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))],
        )
        .await;
        // The prompt is piped over stdin; argv ends with the `-` placeholder.
        assert_eq!(args_of(&specs[0]).last().unwrap(), "-");
        let prompt = stdin_of(&specs[0]);
        assert!(prompt.contains("Test against http://localhost:3000 now."));
        assert!(prompt.contains("structured verdict"));
        assert!(!prompt.contains("${preview_url}"));
        assert!(prompt.contains("base branch `main`"));
    }

    #[tokio::test]
    async fn env_is_propagated_to_the_spec() {
        let mut reviewer = reviewer();
        reviewer.env.insert("PREVIEW_URL".into(), "x".into());
        let (_, specs) = review_with(
            &reviewer,
            [ok_output(stream(
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))],
        )
        .await;
        assert_eq!(
            specs[0].env.get("PREVIEW_URL").map(String::as_str),
            Some("x")
        );
    }

    #[tokio::test]
    async fn base_args_and_cwd_match_config() {
        let backend = CodexBackend::with_program(
            FakeRunner::new([ok_output(stream(
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))]),
            DEFAULT_PROGRAM,
        );
        let run = RunId("r".into());
        let root = PathBuf::from("/some/repo");
        let reviewer = reviewer();
        let req = request(&reviewer, &run, &root);
        backend.review(&req).await.expect("ok");
        let spec = &backend.runner.specs()[0];
        assert_eq!(spec.program, OsString::from(DEFAULT_PROGRAM));
        let args = args_of(spec);
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "--json");
        assert_eq!(args[2], "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(args.last().unwrap(), "-");
        // The prompt travels via stdin, not as an argument.
        assert!(stdin_of(spec).contains("Check the thing."));
        assert_eq!(spec.cwd, root);
    }

    // -- Pure parsing-helper unit tests ---------------------------------------

    #[test]
    fn parse_rejects_empty_stream() {
        let err = CodexSession::parse("   \n\n").unwrap_err();
        assert!(err.to_string().contains("no output"));
    }

    #[test]
    fn parse_keeps_non_json_lines_in_transcript() {
        let stdout = "plain log line\n{\"type\":\"agent_message\",\"message\":\"hi\"}\n";
        let session = CodexSession::parse(stdout).expect("parses");
        assert!(session.transcript.contains("plain log line"));
        assert_eq!(session.final_message(), Some("hi"));
    }

    #[test]
    fn reasoning_events_join_the_transcript_only() {
        let stdout = "\
{\"type\":\"agent_reasoning\",\"text\":\"thinking\"}
{\"type\":\"agent_message\",\"message\":\"done\"}
";
        let session = CodexSession::parse(stdout).expect("parses");
        assert!(session.transcript.contains("thinking"));
        assert_eq!(session.final_message(), Some("done"));
    }

    // -- Real-subprocess test against a fake executable on disk ----------------

    /// Write a fake `codex` program into `dir` that echoes a fixed JSON event
    /// stream and exits zero. Returns the `(program, base_args)` to invoke it by:
    /// on Windows a `.cmd` driven through `cmd /c`; elsewhere a `chmod +x` script.
    fn write_fake_codex(dir: &Path) -> (PathBuf, Vec<String>) {
        let line = r#"{"type":"agent_message","message":"```yaml\nverdict: pass\nsummary: from a real process\n```"}"#;
        if cfg!(windows) {
            let path = dir.join("fake_codex.cmd");
            let script = format!("@echo off\r\necho {line}\r\n");
            std::fs::write(&path, script).unwrap();
            (
                PathBuf::from("cmd"),
                vec!["/c".to_string(), path.to_string_lossy().into_owned()],
            )
        } else {
            let path = dir.join("fake_codex.sh");
            let script = format!("#!/bin/sh\ncat <<'EOF'\n{line}\nEOF\n");
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).unwrap();
            }
            (path, Vec::new())
        }
    }

    #[tokio::test]
    async fn real_subprocess_against_a_fake_codex_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let (program, base_args) = write_fake_codex(tmp.path());

        if !program_available(&program) {
            eprintln!("skipping: launcher not available at {}", program.display());
            return;
        }

        let backend =
            CodexBackend::with_command(SystemCommandRunner, program, base_args, Vec::new());
        let reviewer = reviewer();
        let run = RunId("r-real".into());
        let root = tmp.path().to_path_buf();
        let req = request(&reviewer, &run, &root);

        let outcome = backend.review(&req).await.expect("real subprocess parses");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "from a real process");
    }
}
