//! The Pi backend.
//!
//! Translates a reviewer's execution profile into a headless `pi -p --mode json`
//! invocation, runs it through the injectable [`CommandRunner`] seam, and parses
//! the agent's final message into a [`Verdict`] (`docs/developer-guide/design.md`,
//! "Agent backends").
//!
//! # The Pi invocation contract
//!
//! Bastion drives Pi in its non-interactive print mode and asks for the
//! machine-readable event stream (`pi -p --mode json`). Each line of stdout is a
//! JSON event; Bastion reconstructs the transcript from those events, takes the
//! final assistant message as the reviewer's structured output, and sums the
//! per-call token/cost accounting Pi reports on each assistant message. The
//! reviewer's prompt (with [`inputs`](crate::reviewer::Reviewer::inputs)
//! interpolated and a trailing instruction pinning the verdict schema) is piped to
//! Pi over stdin (in print mode with no positional message, Pi reads the task from
//! stdin), so a long, multi-line prompt is never an OS argument. Reviewer
//! [`env`](crate::reviewer::Reviewer::env) is propagated into the child process.
//!
//! Pi runs unattended: in print mode it executes its tools (read, bash, edit,
//! write) without an interactive approval prompt, the same latitude the Claude
//! Code and Codex backends are given over a trusted checkout (see the threat model
//! in `docs/developer-guide/design.md`). Bastion forwards a pinned `model` as
//! `--model` and `effort` as `--thinking` (default `high`), so a review is
//! reproducible across machines rather than deferring to Pi's own config. Pi is
//! multi-provider, and its `--model` accepts a `provider/id` form (e.g.
//! `openai-codex/gpt-5.5`) that selects the provider too, so the provider rides in
//! the model string instead of a separate field. When no model is pinned Bastion
//! sends no `--model`, and Pi falls back to its configured default provider/model;
//! Pi's built-in default provider is `google`, so a Pi reviewer's `model` should
//! carry its provider.
//!
//! # Fail-closed parsing
//!
//! Pi has no native structured-output schema flag, so (like Codex) Bastion asks
//! for a fenced YAML verdict block (the shared [`SCHEMA_INSTRUCTION`]) and
//! parses it out of the final message with the shared [`extract_verdict`]. If the
//! final message does not carry a schema-conforming verdict, the backend resumes
//! the same session once (by id) for *just* the structured output (per
//! `docs/developer-guide/design.md`), then gives up with an error. The runner turns
//! that error into a fail-closed `block` for gates; this backend never invents a
//! verdict.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use color_eyre::eyre::{Result, bail, eyre};
use serde::Deserialize;

use crate::reviewer::{self};
use crate::verdict::{Money, Usage, Verdict};

use super::command::{CommandRunner, CommandSpec, resolve_program};
use super::{
    Backend, REPROMPT_SUFFIX, ReviewOutcome, ReviewRequest, SCHEMA_INSTRUCTION, extract_verdict,
};

/// Environment variable that overrides the `pi` program path (tests point this at a
/// fake executable; deployments can pin a specific binary).
pub const PROGRAM_ENV: &str = "BASTION_PI_BIN";

/// The default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "pi";

/// Bastion's house default Pi thinking level, sent on `--thinking` when a reviewer
/// (and the registry default) pin no `effort`. Pi's `--thinking` vocabulary is
/// `off`/`minimal`/`low`/`medium`/`high`/`xhigh`; `high` mirrors the cross-backend
/// [`DEFAULT_EFFORT`](reviewer::DEFAULT_EFFORT) so an unpinned Pi reviewer reasons
/// at the same level as the other backends rather than following Pi's own config.
const DEFAULT_THINKING: &str = reviewer::DEFAULT_EFFORT;

/// The Pi agent backend.
///
/// Generic over the [`CommandRunner`] so production wires a real subprocess while
/// tests drive a fake executable through the identical path.
#[derive(Debug, Clone)]
pub struct PiBackend<R> {
    runner: R,
    program: OsString,
    /// Leading args before the (stdin-delivered) prompt for a first-pass run
    /// (default `["-p", "--mode", "json"]`).
    base_args: Vec<String>,
    /// Leading args that resume a session by id, used for the reprompt so the
    /// schema is requested in the *same* session (default
    /// `["-p", "--mode", "json", "--session"]`, followed by the session id).
    resume_args: Vec<String>,
}

impl<R: CommandRunner> PiBackend<R> {
    /// Build a backend over `runner`, resolving the `pi` program from
    /// [`PROGRAM_ENV`] (falling back to [`DEFAULT_PROGRAM`] on `PATH`).
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self::with_program(runner, resolve_program(DEFAULT_PROGRAM, PROGRAM_ENV))
    }

    /// Build a backend over `runner` with an explicit program path and the default
    /// print-mode argument layout.
    #[must_use]
    pub fn with_program(runner: R, program: impl Into<OsString>) -> Self {
        Self {
            runner,
            program: program.into(),
            base_args: vec!["-p".to_string(), "--mode".to_string(), "json".to_string()],
            resume_args: vec![
                "-p".to_string(),
                "--mode".to_string(),
                "json".to_string(),
                "--session".to_string(),
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

    /// The reprompt invocation: resume `session_id` in the same session when one is
    /// known, else fall back to a fresh first-pass invocation.
    fn reprompt_spec(
        &self,
        request: &ReviewRequest<'_>,
        session_id: Option<&str>,
        prompt: &str,
    ) -> CommandSpec {
        match session_id {
            Some(id) => {
                let mut args = self.resume_args.clone();
                args.push(id.to_string());
                self.build_spec(request, args, prompt)
            }
            None => self.first_spec(request, prompt),
        }
    }

    /// Assemble a [`CommandSpec`] from leading `args`, passing `prompt` over the
    /// child's stdin and forwarding the reviewer's env and checkout.
    ///
    /// The prompt goes through stdin rather than argv so a long, multi-line prompt
    /// is never an OS argument: it dodges argument-length limits and, on Windows,
    /// the spawner's refusal to forward special characters to a `.cmd` shim. In
    /// print mode with no positional message, Pi reads the task from stdin.
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
        // Pin the model and thinking level when set. Pi resolves its own default
        // provider/model, so `--model` only appears when a reviewer (or the registry
        // default) pins one; the value is opaque and may carry a `provider/id` prefix
        // (e.g. `openai-codex/gpt-5.5`) that selects the provider too, so it is
        // forwarded verbatim. `--thinking` always applies: absent an `effort`, the
        // house default ([`DEFAULT_THINKING`], `high`) flows through, mirroring how
        // Codex always sends `model_reasoning_effort`. Both ride the leading args and
        // so apply to the reprompt/resume spec as well: Pi accepts them alongside
        // `--session` (they re-select the model and thinking level for the resumed
        // turn), so the recovery turn runs with the same configuration as the first.
        if let Some(model) = &request.reviewer.model {
            spec.arg("--model").arg(model.as_str());
        }
        spec.arg("--thinking").arg(
            request
                .reviewer
                .effort
                .as_ref()
                .map_or(DEFAULT_THINKING, reviewer::Effort::as_str),
        );
        spec.stdin(prompt);
        for (key, value) in &request.reviewer.env {
            spec.env.insert(key.clone(), value.clone());
        }
        spec
    }

    /// Run one Pi invocation and parse its event stream into a session.
    async fn run_once(&self, spec: &CommandSpec) -> Result<PiSession> {
        let output = self.runner.run(spec).await?;
        if !output.success() {
            bail!(
                "pi exited with status {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                output.stderr.trim(),
            );
        }
        PiSession::parse(&output.stdout)
    }
}

impl<R: CommandRunner> Backend for PiBackend<R> {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::Pi
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        let prompt = build_prompt(request);

        // First pass: the full review with the schema instruction appended.
        let first = self.first_spec(request, &prompt);
        let session = self.run_once(&first).await?;

        if let Some(error) = &session.error {
            bail!("pi reported an execution error: {error}");
        }

        if let Some(verdict) = session.parse_verdict() {
            return Ok(outcome(verdict, session, None));
        }

        // The agent's final message was not a schema-conforming verdict. Per
        // design.md, re-run the *same session* asking for just the structured
        // output, then fail closed. Resume by session id when Pi reported one; when
        // resuming, the new turn is only the reprompt suffix (the session already
        // holds the review). Without a session id we fall back to a fresh session
        // and must re-send the full prompt.
        let reprompt_text = match session.session_id.as_deref() {
            Some(_) => REPROMPT_SUFFIX.to_string(),
            None => format!("{prompt}\n\n{REPROMPT_SUFFIX}"),
        };
        let retry = self.reprompt_spec(request, session.session_id.as_deref(), &reprompt_text);
        let retry_session = self.run_once(&retry).await?;

        // The reprompt can itself report an in-band execution error, exactly like the
        // first pass above. Check it before trusting any verdict the retry produced:
        // a parseable `pass` riding alongside an error event must still fail closed,
        // or an errored gate would slip through on the recovery path.
        if let Some(error) = &retry_session.error {
            bail!("pi reported an execution error on reprompt: {error}");
        }

        match retry_session.parse_verdict() {
            Some(verdict) => Ok(outcome(verdict, retry_session, Some(&session))),
            None => Err(eyre!(
                "pi did not emit a schema-conforming verdict after one reprompt; \
                 failing closed. final message was:\n{}",
                retry_session
                    .final_message()
                    .unwrap_or("(no agent message)")
            )),
        }
    }
}

/// Assemble a [`ReviewOutcome`] from a parsed verdict and the session it came from,
/// optionally prepending an earlier session's transcript and summing its usage (the
/// original review, when the verdict was recovered on a reprompt).
///
/// Unlike a single Claude session whose totals are cumulative, each Pi process
/// reports usage only for its own turns, so the original review and the reprompt are
/// disjoint and their usage is summed rather than max'd.
fn outcome(verdict: Verdict, session: PiSession, prior: Option<&PiSession>) -> ReviewOutcome {
    let usage = sum_usage(prior.and_then(PiSession::usage), session.usage());
    let transcript = match prior {
        Some(prior) if !prior.transcript.is_empty() => {
            format!("{}\n{}", prior.transcript.trim_end(), session.transcript)
        }
        _ => session.transcript,
    };
    ReviewOutcome {
        verdict,
        usage,
        transcript: Some(transcript),
    }
}

/// Sum the usage of two disjoint Pi processes (the original review and a reprompt).
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

/// Build the full prompt handed to Pi for `request`: the shared changeset preamble
/// (how to see the diff against the base branch), the interpolated review
/// instruction, the untrusted review-context block (intent, discussion, and this
/// reviewer's prior findings, when a producer supplied any), the shared
/// exhaustive-findings instruction (report every issue in one pass), and the shared
/// schema instruction (end with a fenced YAML verdict).
fn build_prompt(request: &ReviewRequest<'_>) -> String {
    let reviewer = request.reviewer;
    let preamble = super::changeset_preamble(request.base);
    let interpolated = super::interpolate(&reviewer.prompt, &reviewer.inputs);
    let context = super::context_segment(request);
    let exhaustive = super::EXHAUSTIVE_FINDINGS_INSTRUCTION;
    format!("{preamble}\n\n{interpolated}\n\n{context}{exhaustive}\n\n{SCHEMA_INSTRUCTION}")
}

/// A parsed Pi `--mode json` session: the reconstructed transcript, the final
/// assistant message, summed usage, the session id used to resume, and any reported
/// execution error.
#[derive(Debug, Clone, Default)]
struct PiSession {
    /// The human-readable transcript, reconstructed from the event stream.
    transcript: String,
    /// The text of the final assistant message, if any.
    last_message: Option<String>,
    /// Input tokens, summed across the assistant turns Pi reported.
    tokens_in: u64,
    /// Output tokens, summed across the assistant turns Pi reported.
    tokens_out: u64,
    /// Cache-read input tokens, summed across the assistant turns Pi reported.
    cache_read: u64,
    /// Cost in dollars, summed across the assistant turns. Accumulated in dollars
    /// (not cents) and rounded once in [`PiSession::usage`], so per-turn fractional
    /// cents (Pi reports cost to six decimals) do not each round and drift.
    cost_dollars: f64,
    /// Whether any assistant turn reported usage at all.
    saw_usage: bool,
    /// The session id, when Pi reported it, for resuming on a reprompt.
    session_id: Option<String>,
    /// A reported execution error, if Pi emitted an `error` event.
    error: Option<String>,
}

impl PiSession {
    /// Parse a Pi `--mode json` stdout stream (JSON-lines) into a session.
    ///
    /// Unknown event types are tolerated, so it survives Pi adding events. Non-JSON
    /// lines are kept in the transcript verbatim (defensive: the stream should be
    /// pure JSONL, but a stray log line must not lose the rest).
    ///
    /// # Errors
    ///
    /// Returns an error if the stream carried neither a recognized event nor any
    /// other output to record.
    fn parse(stdout: &str) -> Result<Self> {
        let mut acc = PiSession::default();
        let mut saw_event = false;

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<PiEvent>(trimmed) {
                Ok(event) => {
                    saw_event = true;
                    event.fold_into(&mut acc);
                }
                Err(_) => {
                    acc.transcript.push_str(line);
                    acc.transcript.push('\n');
                }
            }
        }

        if !saw_event && acc.transcript.is_empty() {
            bail!("pi produced no output to parse");
        }

        Ok(acc)
    }

    /// The final assistant message text, if the session produced one.
    fn final_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    /// Parse the final assistant message into a [`Verdict`], if it carries one.
    fn parse_verdict(&self) -> Option<Verdict> {
        let message = self.last_message.as_deref()?;
        extract_verdict(message)
    }

    /// Record an assistant message in the transcript and as the latest one.
    fn record_message(&mut self, message: String) {
        self.transcript.push_str(&message);
        self.transcript.push('\n');
        self.last_message = Some(message);
    }

    /// Record non-assistant text (a tool result or user turn) in the transcript.
    fn record_aside(&mut self, text: &str) {
        self.transcript.push_str(text);
        self.transcript.push('\n');
    }

    /// Sum one assistant turn's token/cost usage into the running total.
    fn add_usage(&mut self, usage: &PiUsage) {
        self.saw_usage = true;
        self.tokens_in += usage.input;
        self.tokens_out += usage.output;
        self.cache_read += usage.cache_read;
        self.cost_dollars += usage.cost.as_ref().map_or(0.0, |c| c.total);
    }

    /// The token/cost accounting summed across this session's assistant turns, or
    /// `None` if no turn reported usage.
    fn usage(&self) -> Option<Usage> {
        self.saw_usage.then(|| Usage {
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read: self.cache_read,
            cost_usd: super::money_from_dollars(self.cost_dollars),
        })
    }
}

/// One event in a Pi `--mode json` stream.
///
/// Modeled as a tagged union over the `type` field. Bastion consumes the `session`
/// event (for the resume id), every `message_end` (the authoritative, non-partial
/// message; `message_start`/`message_update` are streaming deltas it ignores), and
/// any `error`. Everything else (`agent_*`, `turn_*`, `tool_execution_*`) is
/// [`PiEvent::Other`] and ignored beyond the transcript.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PiEvent {
    /// The session opened; carries the id used to resume for the reprompt.
    #[serde(rename = "session")]
    Session {
        /// The session identifier.
        id: String,
    },
    /// A completed message (assistant, tool result, or user). The assistant ones
    /// carry the text and per-turn usage; the final assistant text is the verdict.
    #[serde(rename = "message_end")]
    MessageEnd {
        /// The completed message.
        message: PiMessage,
    },
    /// A reported execution error.
    #[serde(rename = "error")]
    Error {
        /// The error text, when present.
        #[serde(default)]
        message: Option<String>,
    },
    /// Any other event; ignored beyond being recorded as "seen".
    #[serde(other)]
    Other,
}

impl PiEvent {
    /// Fold this event into `acc`.
    fn fold_into(self, acc: &mut PiSession) {
        match self {
            PiEvent::Session { id } => acc.session_id = Some(id),
            PiEvent::MessageEnd { message } => message.fold_into(acc),
            PiEvent::Error { message } => {
                acc.error = Some(message.unwrap_or_else(|| "pi reported an error".to_string()));
            }
            PiEvent::Other => {}
        }
    }
}

/// A completed message in a Pi event stream.
#[derive(Debug, Deserialize)]
struct PiMessage {
    /// The message role (`assistant`, `toolResult`, `user`, ...).
    #[serde(default)]
    role: String,
    /// The message content parts.
    #[serde(default)]
    content: Vec<PiContent>,
    /// Token/cost accounting, present on assistant messages.
    #[serde(default)]
    usage: Option<PiUsage>,
}

impl PiMessage {
    /// The concatenated text of this message's text parts (tool calls and other
    /// non-text parts are dropped).
    fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let PiContent::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }

    /// Fold this message into `acc`: an assistant message records its text as the
    /// latest message and sums its usage; any other role's text is a transcript
    /// aside.
    fn fold_into(self, acc: &mut PiSession) {
        let text = self.text();
        if self.role == "assistant" {
            if let Some(usage) = &self.usage {
                acc.add_usage(usage);
            }
            if !text.is_empty() {
                acc.record_message(text);
            }
        } else if !text.is_empty() {
            acc.record_aside(&text);
        }
    }
}

/// One content part of a Pi message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PiContent {
    /// A text part.
    #[serde(rename = "text")]
    Text {
        /// The text.
        #[serde(default)]
        text: String,
    },
    /// Any other part kind (e.g. `toolCall`); ignored for text extraction.
    #[serde(other)]
    Other,
}

/// Token usage as carried by an assistant `message_end` event.
///
/// Pi reports a per-call figure on each assistant message (not a cumulative session
/// total), and a `cost` block in US dollars, so Bastion sums across the assistant
/// turns. `cacheRead` (prompt-cache hits) is summed too; `cacheWrite` is not
/// consumed.
#[derive(Debug, Deserialize)]
struct PiUsage {
    /// Input tokens consumed by this turn.
    #[serde(default)]
    input: u64,
    /// Output tokens produced by this turn.
    #[serde(default)]
    output: u64,
    /// Cache-read input tokens (prompt-cache hits) for this turn.
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
    /// The cost block, when Pi reports it.
    #[serde(default)]
    cost: Option<PiCost>,
}

/// The cost block inside a Pi usage figure; `total` is the turn's cost in dollars.
#[derive(Debug, Deserialize)]
struct PiCost {
    /// Total cost of this turn in US dollars.
    #[serde(default)]
    total: f64,
}

/// Whether `program` resolves to an executable on `PATH` or as a direct path.
///
/// Used by real-binary tests to detect-and-skip when the Pi CLI is absent.
#[must_use]
pub fn program_available(program: impl AsRef<OsStr>) -> bool {
    let program = program.as_ref();
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return path.is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
            || candidate.with_extension("exe").is_file()
            || candidate.with_extension("cmd").is_file()
            || candidate.with_extension("bat").is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Mutex;

    use crate::backend::command::{CommandOutput, SystemCommandRunner};
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

    /// The prompt a spec pipes over stdin.
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

    /// Build a Pi `--mode json` stream: a `session` event followed by one assistant
    /// `message_end` per text, with the given usage attached to each assistant turn.
    /// `usage` is `(input, output, cacheRead, cost)`.
    fn stream(session: &str, messages: &[&str], usage: Option<(u64, u64, u64, f64)>) -> String {
        let mut out = String::new();
        let session_event = serde_json::json!({ "type": "session", "id": session });
        out.push_str(&serde_json::to_string(&session_event).unwrap());
        out.push('\n');
        for message in messages {
            let mut msg = serde_json::json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": message }],
            });
            if let Some((tin, tout, cache_read, cost)) = usage {
                msg["usage"] = serde_json::json!({
                    "input": tin,
                    "output": tout,
                    "cacheRead": cache_read,
                    "cost": { "total": cost },
                });
            }
            let event = serde_json::json!({ "type": "message_end", "message": msg });
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
            backend: reviewer::Backend::Pi,
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
        let backend = PiBackend::with_program(FakeRunner::new(responses), DEFAULT_PROGRAM);
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let req = request(reviewer, &run, &root);
        let outcome = backend.review(&req).await;
        let specs = backend.runner.specs();
        (outcome, specs)
    }

    #[tokio::test]
    async fn id_is_pi() {
        let backend = PiBackend::with_program(FakeRunner::default(), DEFAULT_PROGRAM);
        assert_eq!(backend.id(), reviewer::Backend::Pi);
    }

    #[test]
    fn build_prompt_splices_a_non_empty_review_context_after_the_instruction() {
        // A non-empty context must reach the Pi prompt, spliced between the reviewer's own
        // instruction and the exhaustive-findings instruction. Removing or misplacing the
        // `context_segment` splice would fail this without needing a full review.
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
    async fn applies_house_default_thinking_and_no_model_when_unset() {
        let message = "```yaml\nverdict: pass\nsummary: ok\nfindings: []\n```";
        let (_, specs) =
            review_with(&reviewer(), [ok_output(stream("s-1", &[message], None))]).await;
        let args = args_of(&specs[0]);
        // No model pinned: Pi resolves its own default provider/model, so `--model`
        // is absent.
        assert!(!args.iter().any(|a| a == "--model"), "got args: {args:?}");
        // Thinking always applies; absent an effort, the house default (high) flows
        // through, immediately after the `--thinking` flag.
        let t = args
            .iter()
            .position(|a| a == "--thinking")
            .expect("thinking flag present");
        assert_eq!(args[t + 1], "high");
    }

    #[tokio::test]
    async fn pins_model_and_forwards_thinking_verbatim() {
        let message = "```yaml\nverdict: pass\nsummary: ok\nfindings: []\n```";
        let mut rev = reviewer();
        // Pi's `provider/id` form: the provider rides inside the model string and is
        // forwarded verbatim (Bastion does not parse or split it).
        rev.model = Some(serde_yaml_ng::from_str("openai-codex/gpt-5.5").unwrap());
        // A Pi-specific level: forwarded as-is, no remapping.
        rev.effort = Some(serde_yaml_ng::from_str("xhigh").unwrap());
        let (_, specs) = review_with(&rev, [ok_output(stream("s-1", &[message], None))]).await;
        let args = args_of(&specs[0]);
        let m = args
            .iter()
            .position(|a| a == "--model")
            .expect("model flag present");
        assert_eq!(args[m + 1], "openai-codex/gpt-5.5");
        let t = args
            .iter()
            .position(|a| a == "--thinking")
            .expect("thinking flag present");
        assert_eq!(args[t + 1], "xhigh");
    }

    #[tokio::test]
    async fn reprompt_carries_model_and_thinking_on_resume() {
        // Both selectors ride the leading args, so the resumed reprompt turn carries
        // them too: a recovery turn runs with the same model and thinking level as
        // the first pass, alongside `--session`.
        let mut rev = reviewer();
        rev.model = Some(serde_yaml_ng::from_str("openai-codex/gpt-5.5").unwrap());
        rev.effort = Some(serde_yaml_ng::from_str("xhigh").unwrap());
        let bad = ok_output(stream("s-r", &["no verdict yet"], None));
        let good = ok_output(stream(
            "s-r",
            &["```yaml\nverdict: pass\nsummary: resumed\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&rev, [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.summary, "resumed");
        assert_eq!(specs.len(), 2);
        let retry = args_of(&specs[1]);
        assert!(retry.contains(&"--session".to_string()));
        assert!(retry.contains(&"s-r".to_string()));
        let m = retry
            .iter()
            .position(|a| a == "--model")
            .expect("model on resume");
        assert_eq!(retry[m + 1], "openai-codex/gpt-5.5");
        let t = retry
            .iter()
            .position(|a| a == "--thinking")
            .expect("thinking on resume");
        assert_eq!(retry[t + 1], "xhigh");
    }

    #[tokio::test]
    async fn happy_path_pass_verdict_parses() {
        let message = "Looks fine.\n\n```yaml\nverdict: pass\nsummary: all good\nfindings: []\n```";
        let (outcome, specs) =
            review_with(&reviewer(), [ok_output(stream("s-1", &[message], None))]).await;
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
        let (outcome, _) =
            review_with(&reviewer(), [ok_output(stream("s-1", &[message], None))]).await;
        let verdict = outcome.expect("verdict parses").verdict;
        assert_eq!(verdict.decision, Decision::Block);
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(verdict.findings[0].path, "src/db.ts");
        assert!(verdict.is_consistent());
    }

    #[tokio::test]
    async fn every_finding_in_the_verdict_is_surfaced_not_just_the_first() {
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
        let (outcome, _) =
            review_with(&reviewer(), [ok_output(stream("s-1", &[message], None))]).await;
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
        assert_eq!(verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(verdict.findings[2].kind, FindingKind::Optional);
        assert_eq!(verdict.findings[2].path, "docs/guide.md");
    }

    #[tokio::test]
    async fn prompt_is_piped_over_stdin_and_asks_for_exhaustive_findings() {
        let (_, specs) = review_with(
            &reviewer(),
            [ok_output(stream(
                "s-1",
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))],
        )
        .await;
        // Print-mode JSON args, then the always-present thinking level, and the
        // prompt rides stdin (no positional message).
        let args = args_of(&specs[0]);
        assert_eq!(args, ["-p", "--mode", "json", "--thinking", "high"]);
        let prompt = stdin_of(&specs[0]);
        assert!(prompt.contains("Report every issue you can identify"));
        assert!(prompt.contains("Do not stop after the"));
        // The exhaustive instruction precedes the schema instruction.
        let exhaustive_at = prompt.find("Report every issue").expect("present");
        let schema_at = prompt.find("structured verdict").expect("present");
        assert!(exhaustive_at < schema_at);
    }

    #[tokio::test]
    async fn usage_is_summed_across_assistant_turns() {
        // Two assistant messages (e.g. a tool-using turn then the final answer),
        // each reporting its own per-call usage: the totals sum, the cost too.
        let stdout = stream(
            "s-1",
            &[
                "intermediate thoughts",
                "```yaml\nverdict: pass\nsummary: ok\n```",
            ],
            Some((10_000, 500, 3_000, 0.10)),
        );
        let (outcome, _) = review_with(&reviewer(), [ok_output(stdout)]).await;
        let usage = outcome.expect("parses").usage.expect("usage present");
        assert_eq!(usage.tokens_in, 20_000);
        assert_eq!(usage.tokens_out, 1_000);
        // Cache-read tokens sum across the turns too.
        assert_eq!(usage.cache_read, 6_000);
        assert_eq!(usage.cost_usd, Money::from_cents(20));
    }

    #[test]
    fn usage_without_cache_read_defaults_to_zero() {
        // A usage block that omits `cacheRead` (a turn with no prompt-cache hits, or
        // an older Pi that did not report it) must still parse, defaulting cache_read
        // to 0 rather than failing the whole stream.
        let stdout = concat!(
            r#"{"type":"session","id":"s-1"}"#,
            "\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input":300,"output":20,"cost":{"total":0.02}}}}"#,
            "\n",
        );
        let usage = PiSession::parse(stdout)
            .expect("parses")
            .usage()
            .expect("usage");
        assert_eq!(usage.tokens_in, 300);
        assert_eq!(usage.tokens_out, 20);
        assert_eq!(usage.cache_read, 0);
    }

    #[test]
    fn usage_sums_fractional_cents_before_rounding() {
        // Two turns at $0.025815 each: summing the dollars first gives $0.05163 ->
        // 5 cents. Rounding each turn to cents *before* summing would give 3 + 3 = 6
        // cents. Pi reports cost to six decimals, so this drift is real; the session
        // must accumulate dollars and round once.
        let stdout = stream("s-1", &["a", "b"], Some((10, 1, 0, 0.025815)));
        let usage = PiSession::parse(&stdout)
            .expect("parses")
            .usage()
            .expect("usage");
        assert_eq!(usage.cost_usd, Money::from_cents(5));
    }

    #[tokio::test]
    async fn usage_absent_leaves_none() {
        let message = "```yaml\nverdict: pass\nsummary: ok\n```";
        let (outcome, _) =
            review_with(&reviewer(), [ok_output(stream("s-1", &[message], None))]).await;
        assert!(outcome.expect("parses").usage.is_none());
    }

    #[tokio::test]
    async fn malformed_output_triggers_one_reprompt_then_succeeds() {
        let bad = ok_output(stream(
            "s-1",
            &["I reviewed it but forgot the schema."],
            None,
        ));
        let good = ok_output(stream(
            "s-1",
            &["```yaml\nverdict: pass\nsummary: recovered\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        let verdict = outcome.expect("recovers on reprompt").verdict;
        assert_eq!(verdict.decision, Decision::Pass);
        assert_eq!(verdict.summary, "recovered");
        assert_eq!(specs.len(), 2);
        assert!(!stdin_of(&specs[0]).contains("did not contain"));
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
    }

    #[tokio::test]
    async fn malformed_twice_fails_closed() {
        let bad1 = ok_output(stream("s-1", &["no verdict here"], None));
        let bad2 = ok_output(stream("s-1", &["still no verdict"], None));
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
            "s-1",
            &["```yaml\nverdict: block\nsummary: no reason\nfindings: []\n```"],
            None,
        ));
        let recovered = ok_output(stream(
            "s-1",
            &["```yaml\nverdict: pass\nsummary: ok\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [inconsistent, recovered]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn reprompt_resumes_the_same_session_by_id() {
        let bad = ok_output(stream("s-abc", &["no verdict yet"], None));
        let good = ok_output(stream(
            "s-abc",
            &["```yaml\nverdict: pass\nsummary: resumed\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.summary, "resumed");
        assert_eq!(specs.len(), 2);
        let retry = args_of(&specs[1]);
        // The resume args carry the session id, then the thinking level rides along
        // (Pi re-selects it for the resumed turn), exactly as on the first pass.
        assert_eq!(
            retry,
            [
                "-p",
                "--mode",
                "json",
                "--session",
                "s-abc",
                "--thinking",
                "high"
            ]
        );
        // On resume the new turn is only the reprompt suffix, not the full review.
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
        assert!(!stdin_of(&specs[1]).contains("Check the thing."));
    }

    #[tokio::test]
    async fn reprompt_without_session_id_falls_back_to_a_fresh_session() {
        // A stream with no `session` event leaves no id to resume by.
        let bad = ok_output(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"no verdict\"}]}}\n",
        );
        let good = ok_output(stream(
            "s-new",
            &["```yaml\nverdict: pass\nsummary: ok\n```"],
            None,
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        let retry = args_of(&specs[1]);
        // No `--session`: a fresh first-pass invocation (with the thinking level).
        assert_eq!(retry, ["-p", "--mode", "json", "--thinking", "high"]);
        // Without a session id the fresh session must re-send the full prompt.
        assert!(stdin_of(&specs[1]).contains("Check the thing."));
        assert!(stdin_of(&specs[1]).contains("ONLY the fenced YAML"));
    }

    #[tokio::test]
    async fn recovered_transcript_and_usage_include_the_original_session() {
        let bad = ok_output(stream(
            "s-x",
            &["I did a thorough review of the database layer."],
            Some((500, 40, 100, 0.10)),
        ));
        let good = ok_output(stream(
            "s-x",
            &["```yaml\nverdict: pass\nsummary: ok\n```"],
            Some((200, 10, 50, 0.05)),
        ));
        let (outcome, _) = review_with(&reviewer(), [bad, good]).await;
        let outcome = outcome.expect("recovers");
        let transcript = outcome.transcript.unwrap();
        assert!(transcript.contains("thorough review of the database layer"));
        assert!(transcript.contains("verdict: pass"));
        // Usage sums the two disjoint Pi processes.
        let usage = outcome.usage.expect("usage carried over");
        assert_eq!(usage.tokens_in, 700);
        assert_eq!(usage.tokens_out, 50);
        assert_eq!(usage.cache_read, 150);
        assert_eq!(usage.cost_usd, Money::from_cents(15));
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
        assert!(err.to_string().contains("pi exited with status 1"));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn error_event_fails_closed() {
        // A successful exit but an in-band `error` event must still fail closed: a
        // verdict recovered from an errored session would be a fail-open hole.
        let mut stdout = stream(
            "s-1",
            &["```yaml\nverdict: pass\nsummary: ignore\n```"],
            None,
        );
        stdout.push_str("{\"type\":\"error\",\"message\":\"model overloaded\"}\n");
        let (outcome, _) = review_with(&reviewer(), [ok_output(stdout)]).await;
        let err = outcome.expect_err("error event fails closed");
        assert!(err.to_string().contains("execution error"));
        assert!(err.to_string().contains("model overloaded"));
    }

    #[tokio::test]
    async fn error_event_on_the_reprompt_also_fails_closed() {
        // The first pass is malformed, so the backend reprompts. The retry then
        // emits an in-band `error` event alongside a parseable `pass`: like the
        // first pass, the errored session must fail closed, never launder the pass.
        let bad = ok_output(stream("s-1", &["I forgot the schema."], None));
        let mut retry = stream(
            "s-1",
            &["```yaml\nverdict: pass\nsummary: ignore\n```"],
            None,
        );
        retry.push_str("{\"type\":\"error\",\"message\":\"model overloaded\"}\n");
        let (outcome, specs) = review_with(&reviewer(), [bad, ok_output(retry)]).await;
        let err = outcome.expect_err("error on reprompt fails closed");
        assert!(err.to_string().contains("execution error"));
        assert!(err.to_string().contains("model overloaded"));
        assert_eq!(specs.len(), 2);
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
                "s-1",
                &["```yaml\nverdict: pass\nsummary: ok\n```"],
                None,
            ))],
        )
        .await;
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
                "s-1",
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
        let backend = PiBackend::with_program(
            FakeRunner::new([ok_output(stream(
                "s-1",
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
        assert_eq!(
            args_of(spec),
            ["-p", "--mode", "json", "--thinking", "high"]
        );
        assert!(stdin_of(spec).contains("Check the thing."));
        assert_eq!(spec.cwd, root);
    }

    // -- Pure parsing unit tests ----------------------------------------------

    #[test]
    fn parse_rejects_empty_stream() {
        let err = PiSession::parse("   \n\n").unwrap_err();
        assert!(err.to_string().contains("no output"));
    }

    #[test]
    fn parse_keeps_non_json_lines_in_transcript() {
        let stdout = "plain log line\n".to_string() + &stream("s-1", &["hi"], None);
        let session = PiSession::parse(&stdout).expect("parses");
        assert!(session.transcript.contains("plain log line"));
        assert_eq!(session.final_message(), Some("hi"));
    }

    #[test]
    fn tool_result_text_is_a_transcript_aside_not_the_final_message() {
        // A `toolResult` message contributes to the transcript but is not the
        // assistant's final message, so it never becomes the verdict source.
        let mut stdout = String::new();
        stdout.push_str("{\"type\":\"session\",\"id\":\"s-1\"}\n");
        stdout.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"toolResult\",\"content\":[{\"type\":\"text\",\"text\":\"branch: main\"}]}}\n",
        );
        stdout.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}\n",
        );
        let session = PiSession::parse(&stdout).expect("parses");
        assert!(session.transcript.contains("branch: main"));
        assert_eq!(session.final_message(), Some("final answer"));
    }

    #[test]
    fn tool_call_parts_are_ignored_for_text() {
        // An assistant message whose only content is a tool call yields no text, so
        // it does not overwrite a later text message as the final one.
        let mut stdout = String::new();
        stdout.push_str("{\"type\":\"session\",\"id\":\"s-1\"}\n");
        stdout.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}],\"usage\":{\"input\":5,\"output\":1,\"cost\":{\"total\":0.01}}}}\n",
        );
        stdout.push_str(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}],\"usage\":{\"input\":6,\"output\":2,\"cost\":{\"total\":0.01}}}}\n",
        );
        let session = PiSession::parse(&stdout).expect("parses");
        assert_eq!(session.final_message(), Some("done"));
        // Usage still sums across both assistant turns (the tool-call turn counts).
        let usage = session.usage().expect("usage");
        assert_eq!(usage.tokens_in, 11);
        assert_eq!(usage.tokens_out, 3);
        assert_eq!(usage.cost_usd, Money::from_cents(2));
    }

    #[test]
    fn program_available_detects_missing_binary() {
        assert!(!program_available("definitely-not-a-real-program-xyz123"));
    }

    // -- Real-subprocess test against a fake executable on disk ----------------

    /// Write a fake `pi` program into `dir` that echoes a fixed Pi JSON event stream
    /// and exits zero. Returns the `(program, base_args)` to invoke it by: on Windows
    /// a `.cmd` driven through `cmd /c`; elsewhere a `chmod +x` script.
    fn write_fake_pi(dir: &Path) -> (PathBuf, Vec<String>) {
        let session = r#"{"type":"session","id":"s-real"}"#;
        let message = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"```yaml\nverdict: pass\nsummary: from a real process\n```"}],"usage":{"input":10,"output":2,"cost":{"total":0.05}}}}"#;
        if cfg!(windows) {
            let path = dir.join("fake_pi.cmd");
            let script = format!("@echo off\r\necho {session}\r\necho {message}\r\n");
            std::fs::write(&path, script).unwrap();
            (
                PathBuf::from("cmd"),
                vec!["/c".to_string(), path.to_string_lossy().into_owned()],
            )
        } else {
            let path = dir.join("fake_pi.sh");
            let script = format!("#!/bin/sh\ncat <<'EOF'\n{session}\n{message}\nEOF\n");
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
    async fn real_subprocess_against_a_fake_pi_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let (program, base_args) = write_fake_pi(tmp.path());

        if !program_available(&program) {
            eprintln!("skipping: launcher not available at {}", program.display());
            return;
        }

        let backend = PiBackend::with_command(SystemCommandRunner, program, base_args, Vec::new());
        let reviewer = reviewer();
        let run = RunId("r-real".into());
        let root = tmp.path().to_path_buf();
        let req = request(&reviewer, &run, &root);

        let outcome = backend.review(&req).await.expect("real subprocess parses");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "from a real process");
        let usage = outcome.usage.expect("usage parsed");
        assert_eq!(usage.tokens_in, 10);
        assert_eq!(usage.cost_usd, Money::from_cents(5));
    }
}
