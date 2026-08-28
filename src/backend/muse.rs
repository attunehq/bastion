//! The Muse Code backend.
//!
//! Translates a reviewer's execution profile into a headless `muse exec --json`
//! invocation, runs it through the injectable [`CommandRunner`] seam, and parses
//! the agent's final message into a [`Verdict`] (`docs/developer-guide/design.md`,
//! "Agent backends").
//!
//! # The Muse Code invocation contract
//!
//! Bastion drives Meta's Muse Code CLI (`muse`, the harness for the Muse Spark
//! models) in its non-interactive `exec` mode and asks for the machine-readable
//! event stream (`muse exec --json`). Each line of stdout is a JSON record with a
//! `payload_type`; Bastion takes the `run.terminal.*` record's `text` as the
//! reviewer's final message, records `tool.result` texts as transcript asides, and
//! reads the session id off the record envelope's `stream` so a reprompt can
//! resume the same session (`--session-id <id>`). The reviewer's prompt (with
//! [`inputs`](crate::reviewer::Reviewer::inputs) interpolated and a trailing
//! instruction pinning the verdict schema) is the positional `exec` argument.
//! Reviewer [`env`](crate::reviewer::Reviewer::env) is propagated into the child
//! process.
//!
//! Muse runs unattended under `--yolo` (no approval prompts, no sandbox, workspace
//! trusted for the run), the same latitude the other backends are given over a
//! trusted checkout (see the threat model in `docs/developer-guide/design.md`).
//! Bastion forwards a pinned `model` as `--model` and `effort` as
//! `--reasoning-effort` (default `high`); with no model pinned the CLI resolves its
//! own configured default, as Codex, Pi, and Grok Build do.
//!
//! The `--json` stream carries no token or cost accounting (Muse keeps that in its
//! on-disk session log), so this backend reports no usage; a Muse reviewer
//! contributes nothing to a run's usage totals.
//!
//! # Fail-closed parsing
//!
//! Muse has no native structured-output schema flag, so (like Codex and Pi)
//! Bastion asks for a fenced YAML verdict block (the shared
//! [`SCHEMA_INSTRUCTION`]) and parses it out of the final message with the shared
//! [`extract_verdict`]. If the final message does not carry a schema-conforming
//! verdict, the backend resumes the same session once (by id) for *just* the
//! structured output (per `docs/developer-guide/design.md`), then gives up with an
//! error. A run that ends `failed` (an auth rejection, a step cap, a provider
//! error) is an error too, never a reprompt. The runner turns those errors into a
//! fail-closed `block` for gates; this backend never invents a verdict.

use std::ffi::OsString;

use color_eyre::eyre::{Result, bail, eyre};
use serde::Deserialize;

use crate::reviewer;
use crate::verdict::Verdict;

use super::command::{CommandRunner, CommandSpec, resolve_program};
use super::{Backend, ReviewOutcome, ReviewRequest, SCHEMA_INSTRUCTION, extract_verdict};

/// Environment variable that overrides the `muse` program path (tests point this
/// at a fake executable; deployments can pin a specific binary).
pub const PROGRAM_ENV: &str = "BASTION_MUSE_BIN";

/// The default program name, resolved on `PATH` when [`PROGRAM_ENV`] is unset.
pub const DEFAULT_PROGRAM: &str = "muse";

/// The Muse Code agent backend.
///
/// Generic over the [`CommandRunner`] so production wires a real subprocess while
/// tests drive a fake executable through the identical path.
#[derive(Debug, Clone)]
pub struct MuseBackend<R> {
    runner: R,
    program: OsString,
}

impl<R: CommandRunner> MuseBackend<R> {
    /// Build a backend over `runner`, resolving the `muse` program from
    /// [`PROGRAM_ENV`] (falling back to [`DEFAULT_PROGRAM`] on `PATH`).
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self::with_program(runner, resolve_program(DEFAULT_PROGRAM, PROGRAM_ENV))
    }

    /// Build a backend over `runner` with an explicit program path, bypassing the
    /// environment lookup.
    #[must_use]
    pub fn with_program(runner: R, program: impl Into<OsString>) -> Self {
        Self {
            runner,
            program: program.into(),
        }
    }

    /// Assemble one `muse exec` invocation: the headless flags, the model and
    /// effort selectors, an optional `--session-id` to resume, and the prompt as
    /// the positional argument.
    fn spec(
        &self,
        request: &ReviewRequest<'_>,
        session_id: Option<&str>,
        prompt: &str,
    ) -> CommandSpec {
        let reviewer = request.reviewer;
        let mut spec = CommandSpec::new(self.program.clone(), request.repo_root);
        spec.arg("exec").arg("--json").arg("--yolo");

        // Pin the model only when a reviewer (or the registry default) sets one;
        // otherwise Muse resolves its own configured default. The effort always
        // applies so an unpinned reviewer reasons at the house default rather than
        // the CLI's, keeping a review reproducible across machines. Both ride the
        // resumed reprompt too, so the recovery turn runs with the same
        // configuration as the first.
        if let Some(model) = &reviewer.model {
            spec.arg("--model").arg(model.as_str());
        }
        spec.arg("--reasoning-effort").arg(
            reviewer
                .effort
                .as_ref()
                .map_or(reviewer::DEFAULT_EFFORT, reviewer::Effort::as_str),
        );
        if let Some(id) = session_id {
            spec.arg("--session-id").arg(id);
        }
        spec.arg(prompt);

        for (key, value) in &reviewer.env {
            spec.env.insert(key.clone(), value.clone());
        }
        spec
    }

    /// Run one Muse invocation and parse its event stream into a session.
    async fn run_once(&self, spec: &CommandSpec) -> Result<MuseSession> {
        let output = self.runner.run(spec).await?;
        if !output.success() {
            bail!(
                "muse exited with status {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                super::truncate(&failure_detail(&output.stdout, &output.stderr), 2_000),
            );
        }
        let session = MuseSession::parse(&output.stdout)?;
        if let Some(reason) = &session.failure {
            bail!("muse run ended failed: {reason}");
        }
        Ok(session)
    }
}

/// The most useful text to quote for a failed process: stderr when it says
/// anything, else the failure reason from the stream, else raw stdout.
fn failure_detail(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    if let Ok(session) = MuseSession::parse(stdout)
        && let Some(reason) = session.failure
    {
        return reason;
    }
    stdout.trim().to_string()
}

impl<R: CommandRunner> Backend for MuseBackend<R> {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::Muse
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        let prompt = super::review_prompt(request, SCHEMA_INSTRUCTION);

        // First pass: the full review with the schema instruction appended.
        let session = self.run_once(&self.spec(request, None, &prompt)).await?;
        if let Some(verdict) = session.parse_verdict() {
            return Ok(outcome(verdict, session, None));
        }

        // The agent's final message was not a schema-conforming verdict. Per
        // design.md, re-run the *same session* asking for just the structured
        // output, then fail closed. Resume by session id when Muse reported one; the
        // new turn is then only the reprompt suffix (the session already holds the
        // review). Without a session id, fall back to a fresh session and re-send
        // the full prompt.
        let reprompt_text = super::reprompt_text(&prompt, session.session_id.is_some());
        let retry = self.spec(request, session.session_id.as_deref(), &reprompt_text);
        let retry_session = self.run_once(&retry).await?;

        match retry_session.parse_verdict() {
            Some(verdict) => Ok(outcome(verdict, retry_session, Some(&session))),
            None => Err(eyre!(
                "muse did not emit a schema-conforming verdict after one reprompt; \
                 failing closed. final message was:\n{}",
                retry_session
                    .final_message()
                    .unwrap_or("(no agent message)")
            )),
        }
    }
}

/// Assemble a [`ReviewOutcome`] from a parsed verdict and the session it came from,
/// optionally prepending an earlier session's transcript (the original review, when
/// the verdict was recovered on a reprompt). Muse reports no usage in its stream.
fn outcome(verdict: Verdict, session: MuseSession, prior: Option<&MuseSession>) -> ReviewOutcome {
    let transcript =
        super::stitch_transcript(prior.map(|p| p.transcript.as_str()), session.transcript);
    ReviewOutcome {
        verdict,
        usage: None,
        transcript: Some(transcript),
    }
}

/// A parsed `muse exec --json` session: the reconstructed transcript, the final
/// message, the session id used to resume, and the failure reason if the run ended
/// `failed`.
#[derive(Debug, Clone, Default)]
struct MuseSession {
    /// The human-readable transcript, reconstructed from the event stream.
    transcript: String,
    /// The text of the terminal record, if the run reached one.
    last_message: Option<String>,
    /// The session id, when the stream carried one, for resuming on a reprompt.
    session_id: Option<String>,
    /// The failure reason, when the run's terminal record was not `completed`.
    failure: Option<String>,
}

impl MuseSession {
    /// Parse a `muse exec --json` stdout stream (JSON-lines) into a session.
    ///
    /// Unknown record types are tolerated, so it survives Muse adding events.
    /// Non-JSON lines are kept in the transcript verbatim (defensive: the stream
    /// should be pure JSONL, but a stray log line must not lose the rest).
    ///
    /// # Errors
    ///
    /// Returns an error if the stream carried neither a recognized record nor any
    /// other output to record.
    fn parse(stdout: &str) -> Result<Self> {
        let mut acc = MuseSession::default();
        let mut saw_record = false;

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<MuseRecord>(trimmed) {
                Ok(record) => {
                    saw_record = true;
                    record.fold_into(&mut acc);
                }
                Err(_) => {
                    acc.transcript.push_str(line);
                    acc.transcript.push('\n');
                }
            }
        }

        if !saw_record && acc.transcript.is_empty() {
            bail!("muse produced no output to parse");
        }

        Ok(acc)
    }

    /// The final message text, if the run produced one.
    fn final_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    /// Parse the final message into a [`Verdict`], if it carries one.
    fn parse_verdict(&self) -> Option<Verdict> {
        extract_verdict(self.last_message.as_deref()?)
    }
}

/// One record in a `muse exec --json` stream. Every record carries the session
/// stream it belongs to and a `payload_type` naming its payload; Bastion reads
/// `run.terminal.*` (the final text and outcome) and `tool.result` (a transcript
/// aside) and ignores the rest beyond the session id.
#[derive(Debug, Deserialize)]
struct MuseRecord {
    #[serde(default)]
    stream: Option<MuseStream>,
    #[serde(default)]
    payload_type: String,
    #[serde(default)]
    payload: MusePayload,
}

impl MuseRecord {
    /// Fold this record into `acc`.
    fn fold_into(self, acc: &mut MuseSession) {
        if let Some(stream) = &self.stream
            && stream.kind == "session"
            && !stream.id.is_empty()
        {
            acc.session_id = Some(stream.id.clone());
        }
        if self.payload_type.starts_with("run.terminal.") {
            let text = self.payload.text.unwrap_or_default();
            if !text.is_empty() {
                acc.transcript.push_str(&text);
                acc.transcript.push('\n');
            }
            acc.last_message = Some(text);
            if self.payload.terminal.as_deref() != Some("completed") {
                acc.failure = Some(self.payload.reason.unwrap_or_else(|| {
                    format!(
                        "run ended {}",
                        self.payload
                            .terminal
                            .as_deref()
                            .unwrap_or("without completing")
                    )
                }));
            }
        } else if self.payload_type == "tool.result"
            && let Some(text) = self.payload.text.filter(|t| !t.is_empty())
        {
            acc.transcript.push_str(&text);
            acc.transcript.push('\n');
        }
    }
}

/// The stream a record belongs to; `kind: session` carries the id to resume by.
#[derive(Debug, Deserialize)]
struct MuseStream {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    id: String,
}

/// The subset of payload fields Bastion consumes, across the record types it
/// reads. Fields absent on a given payload default to `None`.
#[derive(Debug, Default, Deserialize)]
struct MusePayload {
    /// The final text (`run.terminal.*`) or a tool's result text (`tool.result`).
    #[serde(default)]
    text: Option<String>,
    /// The terminal outcome of a `run.terminal.*` record: `completed` or `failed`.
    #[serde(default)]
    terminal: Option<String>,
    /// The failure reason of a `run.terminal.failed` record.
    #[serde(default)]
    reason: Option<String>,
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

    /// The positional prompt: the last argument of the spec.
    fn prompt_of(spec: &CommandSpec) -> String {
        args_of(spec)
            .last()
            .cloned()
            .expect("spec carries a prompt")
    }

    fn ok_output(stdout: impl Into<String>) -> CommandOutput {
        CommandOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// One `muse exec --json` record on `session`.
    fn record(session: &str, payload_type: &str, payload: serde_json::Value) -> String {
        let record = serde_json::json!({
            "schema_version": 1,
            "stream": { "kind": "session", "id": session },
            "record_type": "event",
            "payload_type": payload_type,
            "payload": payload,
        });
        serde_json::to_string(&record).unwrap()
    }

    /// A completed stream on `session`: a lifecycle record, then a `run.terminal.completed`
    /// carrying `text`.
    fn stream(session: &str, text: &str) -> String {
        format!(
            "{}\n{}\n",
            record(
                session,
                "run.lifecycle.started",
                serde_json::json!({ "kind": "run_started", "prompt": "..." })
            ),
            record(
                session,
                "run.terminal.completed",
                serde_json::json!({ "kind": "run_terminal", "terminal": "completed", "reason": null, "text": text })
            ),
        )
    }

    fn reviewer() -> Reviewer {
        Reviewer {
            name: "demo".into(),
            trigger: vec!["**".into()].into(),
            mode: Mode::Gate,
            backend: reviewer::Backend::Muse,
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
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: crate::backend::ReviewPurpose::Review,
            native_session_dir: None,
        }
    }

    async fn review_with(
        reviewer: &Reviewer,
        responses: impl IntoIterator<Item = CommandOutput>,
    ) -> (Result<ReviewOutcome>, Vec<CommandSpec>) {
        let backend = MuseBackend::with_program(FakeRunner::new(responses), DEFAULT_PROGRAM);
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let req = request(reviewer, &run, &root);
        let outcome = backend.review(&req).await;
        let specs = backend.runner.specs();
        (outcome, specs)
    }

    const PASS: &str = "```yaml\nverdict: pass\nsummary: ok\nfindings: []\n```";

    #[tokio::test]
    async fn id_is_muse() {
        let backend = MuseBackend::with_program(FakeRunner::default(), DEFAULT_PROGRAM);
        assert_eq!(backend.id(), reviewer::Backend::Muse);
    }

    #[tokio::test]
    async fn headless_flags_house_effort_and_positional_prompt() {
        let (_, specs) = review_with(&reviewer(), [ok_output(stream("s-1", PASS))]).await;
        let spec = &specs[0];
        assert_eq!(spec.program, OsString::from(DEFAULT_PROGRAM));
        let args = args_of(spec);
        // No model pinned: Muse resolves its own default, so `--model` is absent.
        // The effort always applies at the house default. The prompt is the trailing
        // positional argument.
        assert_eq!(
            &args[..5],
            ["exec", "--json", "--yolo", "--reasoning-effort", "high"]
        );
        assert_eq!(args.len(), 6);
        assert!(!args.iter().any(|a| a == "--model"), "got args: {args:?}");
        assert!(spec.stdin.is_none());
        let prompt = prompt_of(spec);
        assert!(prompt.contains("Check the thing."));
        assert!(prompt.contains("base branch `main`"));
        assert!(prompt.contains("Report every issue you can identify"));
        // The exhaustive instruction precedes the schema instruction.
        let exhaustive_at = prompt.find("Report every issue").expect("present");
        let schema_at = prompt.find("structured verdict").expect("present");
        assert!(exhaustive_at < schema_at);
    }

    #[tokio::test]
    async fn pins_model_and_forwards_effort_verbatim() {
        let mut rev = reviewer();
        rev.model = Some(serde_yaml_ng::from_str("muse-spark-1.2").unwrap());
        // A Muse-specific level: forwarded as-is, no remapping.
        rev.effort = Some(serde_yaml_ng::from_str("ultra").unwrap());
        let (_, specs) = review_with(&rev, [ok_output(stream("s-1", PASS))]).await;
        let args = args_of(&specs[0]);
        let m = args
            .iter()
            .position(|a| a == "--model")
            .expect("model flag");
        assert_eq!(args[m + 1], "muse-spark-1.2");
        let e = args
            .iter()
            .position(|a| a == "--reasoning-effort")
            .expect("effort flag");
        assert_eq!(args[e + 1], "ultra");
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
        let (outcome, specs) = review_with(&reviewer(), [ok_output(stream("s-1", message))]).await;
        let outcome = outcome.expect("verdict parses");
        assert_eq!(outcome.verdict.decision, Decision::Block);
        assert_eq!(outcome.verdict.findings.len(), 1);
        assert_eq!(outcome.verdict.findings[0].kind, FindingKind::Blocking);
        assert_eq!(outcome.verdict.findings[0].path, "src/db.ts");
        assert!(outcome.verdict.is_consistent());
        // Muse reports no usage in its stream.
        assert!(outcome.usage.is_none());
        assert!(outcome.transcript.unwrap().contains("Found an issue."));
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn malformed_output_reprompts_in_the_same_session_then_succeeds() {
        let bad = ok_output(stream("s-abc", "I reviewed it but forgot the schema."));
        let good = ok_output(stream(
            "s-abc",
            "```yaml\nverdict: pass\nsummary: resumed\n```",
        ));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        let outcome = outcome.expect("recovers on reprompt");
        assert_eq!(outcome.verdict.summary, "resumed");
        assert_eq!(specs.len(), 2);
        let retry = args_of(&specs[1]);
        // The resume rides the same headless flags and effort, then the session id.
        assert_eq!(
            &retry[..7],
            [
                "exec",
                "--json",
                "--yolo",
                "--reasoning-effort",
                "high",
                "--session-id",
                "s-abc"
            ]
        );
        // On resume the new turn is only the reprompt suffix, not the full review.
        assert!(!prompt_of(&specs[0]).contains("did not contain"));
        assert!(prompt_of(&specs[1]).contains("ONLY the fenced YAML"));
        assert!(!prompt_of(&specs[1]).contains("Check the thing."));
        // The recovered transcript keeps the original session's text.
        let transcript = outcome.transcript.unwrap();
        assert!(transcript.contains("forgot the schema"));
        assert!(transcript.contains("verdict: pass"));
    }

    #[tokio::test]
    async fn reprompt_without_session_id_falls_back_to_a_fresh_session() {
        // A stream whose records carry no session stream leaves no id to resume by.
        let bad = ok_output(
            r#"{"payload_type":"run.terminal.completed","payload":{"terminal":"completed","text":"no verdict"}}"#,
        );
        let good = ok_output(stream("s-new", PASS));
        let (outcome, specs) = review_with(&reviewer(), [bad, good]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        let retry = args_of(&specs[1]);
        assert!(!retry.iter().any(|a| a == "--session-id"));
        // Without a session id the fresh session must re-send the full prompt.
        assert!(prompt_of(&specs[1]).contains("Check the thing."));
        assert!(prompt_of(&specs[1]).contains("ONLY the fenced YAML"));
    }

    #[tokio::test]
    async fn malformed_twice_fails_closed() {
        let bad1 = ok_output(stream("s-1", "no verdict here"));
        let bad2 = ok_output(stream("s-1", "still no verdict"));
        let (outcome, specs) = review_with(&reviewer(), [bad1, bad2]).await;
        let err = outcome.expect_err("fails closed after one reprompt");
        assert!(
            err.to_string()
                .contains("did not emit a schema-conforming verdict")
        );
        assert!(err.to_string().contains("still no verdict"));
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn inconsistent_block_is_rejected_and_reprompted() {
        let inconsistent = ok_output(stream(
            "s-1",
            "```yaml\nverdict: block\nsummary: no reason\nfindings: []\n```",
        ));
        let recovered = ok_output(stream("s-1", PASS));
        let (outcome, specs) = review_with(&reviewer(), [inconsistent, recovered]).await;
        assert_eq!(outcome.expect("recovers").verdict.decision, Decision::Pass);
        assert_eq!(specs.len(), 2);
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_quoting_stderr() {
        let failed = CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "model `nope` is not in the catalog".into(),
        };
        let (outcome, specs) = review_with(&reviewer(), [failed]).await;
        let err = outcome.expect_err("non-zero exit errors");
        assert!(err.to_string().contains("muse exited with status 1"));
        assert!(err.to_string().contains("not in the catalog"));
        // An execution failure is never reprompted.
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn failed_terminal_quotes_the_reason_and_fails_closed() {
        // A real failure: `run.terminal.failed` with a reason, exit 1, and only a
        // generic line on stderr. The reason is the useful message.
        let stdout = record(
            "s-1",
            "run.terminal.failed",
            serde_json::json!({ "kind": "run_terminal", "terminal": "failed", "reason": "your API key from META_API_KEY was rejected", "text": "" }),
        );
        let failed = CommandOutput {
            code: Some(1),
            stdout,
            stderr: "run ended with Failed".into(),
        };
        let (outcome, _) = review_with(&reviewer(), [failed]).await;
        let err = outcome.expect_err("failed run errors");
        assert!(err.to_string().contains("run ended with Failed"));

        // Belt and braces: a `failed` terminal on a zero exit still fails closed,
        // even when the text happens to parse as a pass.
        let stdout = record(
            "s-1",
            "run.terminal.failed",
            serde_json::json!({ "terminal": "failed", "reason": "model did not reach a terminal state", "text": PASS }),
        );
        let (outcome, specs) = review_with(&reviewer(), [ok_output(stdout)]).await;
        let err = outcome.expect_err("failed terminal fails closed");
        assert!(err.to_string().contains("run ended failed"));
        assert!(err.to_string().contains("did not reach a terminal state"));
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn prompt_inputs_are_interpolated_and_env_propagated() {
        let mut reviewer = reviewer();
        reviewer.prompt = "Test against ${preview_url} now.".into();
        reviewer
            .inputs
            .insert("preview_url".into(), "http://localhost:3000".into());
        reviewer.env.insert("PREVIEW_URL".into(), "x".into());
        let (_, specs) = review_with(&reviewer, [ok_output(stream("s-1", PASS))]).await;
        let prompt = prompt_of(&specs[0]);
        assert!(prompt.contains("Test against http://localhost:3000 now."));
        assert!(!prompt.contains("${preview_url}"));
        assert_eq!(
            specs[0].env.get("PREVIEW_URL").map(String::as_str),
            Some("x")
        );
    }

    // -- Pure parsing unit tests ----------------------------------------------

    #[test]
    fn parse_rejects_empty_stream() {
        let err = MuseSession::parse("   \n\n").unwrap_err();
        assert!(err.to_string().contains("no output"));
    }

    #[test]
    fn parse_keeps_non_json_lines_and_tool_results_in_the_transcript() {
        let mut stdout = "plain log line\n".to_string();
        stdout.push_str(&record(
            "s-1",
            "tool.result",
            serde_json::json!({ "kind": "tool_result", "text": "Read text file `README.md`.\n1|hello" }),
        ));
        stdout.push('\n');
        stdout.push_str(&record(
            "s-1",
            "run.output.delta",
            serde_json::json!({ "kind": "run_output_delta", "text": "fin" }),
        ));
        stdout.push('\n');
        stdout.push_str(&stream("s-1", "final answer"));
        let session = MuseSession::parse(&stdout).expect("parses");
        assert!(session.transcript.contains("plain log line"));
        assert!(session.transcript.contains("1|hello"));
        // Streaming deltas are ignored; only the terminal text is the final message.
        assert!(!session.transcript.contains("fin\n"));
        assert_eq!(session.final_message(), Some("final answer"));
        assert_eq!(session.session_id.as_deref(), Some("s-1"));
        assert!(session.failure.is_none());
    }

    #[test]
    fn a_stream_without_a_terminal_record_has_no_final_message() {
        let stdout = record(
            "s-1",
            "run.lifecycle.started",
            serde_json::json!({ "kind": "run_started" }),
        );
        let session = MuseSession::parse(&stdout).expect("parses");
        assert_eq!(session.final_message(), None);
        assert!(session.parse_verdict().is_none());
    }

    // -- Real-subprocess test against a fake executable on disk ----------------

    /// Write a fake `muse` program into `dir` that echoes a fixed event stream and
    /// exits zero. Returns the program to invoke; skipped on Windows, where a script
    /// needs a launcher this backend does not model.
    #[cfg(unix)]
    fn write_fake_muse(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake_muse.sh");
        let body = stream(
            "s-real",
            "```yaml\nverdict: pass\nsummary: from a real process\n```",
        );
        let script = format!("#!/bin/sh\ncat <<'EOF'\n{body}EOF\n");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_subprocess_against_a_fake_muse_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let program = write_fake_muse(tmp.path());
        if !program_available(&program) {
            eprintln!("skipping: fake not runnable at {}", program.display());
            return;
        }
        let backend = MuseBackend::with_program(SystemCommandRunner, program);
        let reviewer = reviewer();
        let run = RunId("r-real".into());
        let root = tmp.path().to_path_buf();
        let req = request(&reviewer, &run, &root);
        let outcome = backend.review(&req).await.expect("real subprocess parses");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert_eq!(outcome.verdict.summary, "from a real process");
        assert!(outcome.usage.is_none());
    }
}
