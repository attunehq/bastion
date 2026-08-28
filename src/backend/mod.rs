//! The backend execution boundary.
//!
//! Bastion does not run its own agent loop; it translates a reviewer's execution
//! profile into a backend's native config and shells out to that backend's CLI
//! (`docs/developer-guide/design.md`). This module defines the contract: [`Backend`], what it is
//! handed ([`ReviewRequest`]) and what it must return ([`ReviewOutcome`]), plus
//! the concrete backends and the dispatch that picks one for a reviewer.
//!
//! The subprocess boundary lives behind [`command::CommandRunner`] so backends can
//! be driven against a fake executable in tests, with no real agent or network.

pub mod claude_code;
pub mod codex;
pub mod command;
pub mod container;
pub mod governor;
pub mod grok;
pub mod muse;
pub mod pi;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use color_eyre::eyre::Result;

use crate::context::ReviewContext;
use crate::event::RunId;
use crate::reviewer::{self, Reviewer};
use crate::verdict::{Decision, Money, Usage, Verdict};

use self::claude_code::ClaudeCodeBackend;
use self::codex::CodexBackend;
use self::command::{CommandRunner, OverlayEnvRunner, SystemCommandRunner};
use self::container::{ContainerEngine, ContainerRunner, ExecutionPlan, credential_passthrough};
use self::governor::{GovernedRunner, SpawnGovernor};
use self::grok::GrokBackend;
use self::muse::MuseBackend;
use self::pi::PiBackend;

/// Why an agent call is being made, which controls review-only prompt material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPurpose {
    /// Perform the full concern review and enumerate every finding.
    Review,
    /// Decide whether the full reviewer applies to the changeset.
    Routing,
}

/// Everything a backend is handed to run one reviewer.
///
/// The runner gives every reviewer a full checkout at the changeset head plus the
/// request metadata; the prompt, not the runner, scopes attention.
#[derive(Debug)]
pub struct ReviewRequest<'a> {
    /// The reviewer to execute.
    pub reviewer: &'a Reviewer,
    /// The run this review belongs to.
    pub run: &'a RunId,
    /// The repository root the backend operates within.
    pub repo_root: &'a Path,
    /// The base branch the changeset is computed against, for orienting prose
    /// in the prompt; the diff itself is taken at `merge_base`.
    pub base: &'a str,
    /// The resolved merge-base commit of HEAD and `base`: the comparison point
    /// the changeset is diffed against. Handed to the agent pre-resolved so it
    /// can never diff against the base branch's tip, which would put the
    /// base's own changes under review (see [`changeset_preamble`]).
    pub merge_base: &'a str,
    /// What the reviewer is told about the changeset beyond the diff: the author's
    /// stated intent, the surrounding discussion, and this reviewer's prior findings.
    /// Empty when no producer supplied any, in which case the prompt is unchanged.
    pub context: &'a ReviewContext,
    /// Whether this call reviews the concern or only routes it.
    pub purpose: ReviewPurpose,
    /// Directory native agent session files should be written to when Akari
    /// handoff is on. `None` leaves the agent writing to its default location.
    pub native_session_dir: Option<&'a Path>,
}

/// What a backend returns for one reviewer.
#[derive(Debug, Clone)]
pub struct ReviewOutcome {
    /// The structured verdict.
    pub verdict: Verdict,
    /// Token and cost accounting, when the backend reports it.
    pub usage: Option<Usage>,
    /// The full session transcript, saved to disk but never streamed.
    pub transcript: Option<String>,
}

/// A backend capable of executing a reviewer and returning a structured verdict.
///
/// Implementors translate the reviewer's execution profile into their native
/// configuration and capture the agent's structured output. The trait is kept
/// deliberately small and stable: sibling backends (Codex, Pi, Grok Build) implement the same
/// signature, and [`dispatch`] selects between them by [`reviewer::Backend`].
#[allow(
    async_fn_in_trait,
    reason = "single-crate trait; backends are constructed and consumed internally, not across a public API boundary"
)]
pub trait Backend {
    /// Which backend this is.
    fn id(&self) -> reviewer::Backend;

    /// Execute one reviewer and return its outcome.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend fails to run or cannot produce a verdict;
    /// the runner translates that into a fail-closed `block` for gates.
    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome>;
}

/// A deterministic backend that always passes, for tests and local dry-runs of
/// the surrounding machinery without invoking a real agent.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockBackend;

impl Backend for MockBackend {
    fn id(&self) -> reviewer::Backend {
        reviewer::Backend::Any
    }

    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome> {
        Ok(ReviewOutcome {
            verdict: Verdict {
                decision: Decision::Pass,
                summary: format!("mock backend approved '{}'", request.reviewer.name),
                findings: Vec::new(),
            },
            usage: None,
            transcript: Some("(mock transcript)".to_string()),
        })
    }
}

/// Run one reviewer on the backend its profile selects, natively or in a container.
///
/// Dispatch first resolves the reviewer's [`ExecutionPlan`]: that is the single
/// place an unprovisioned capability tier fails closed, so a backend is only ever
/// reached for a reviewer this build can actually run. It then selects the concrete
/// backend by [`reviewer::Backend`] (the match that grows when a sibling backend
/// lands; the [`Backend`] trait does not). A containerized plan wraps the backend's
/// subprocess seam in a [`ContainerRunner`], so the identical backend code runs
/// inside the image with its program resolved on the container's `PATH`.
///
/// Every agent launch, native or containerized, runs through a [`GovernedRunner`]
/// over the shared `governor`, so the run's [`SpawnLimits`](crate::limits::SpawnLimits)
/// bound the whole fan-out: a launch refused by a tripped breaker or an exhausted
/// cap returns an error here, which becomes a fail-closed reviewer and, run-wide, an
/// aborted review rather than an endless retry.
///
/// # Errors
///
/// Returns an error if the reviewer opts into a tier this build does not provision or
/// declares an invalid `runner` (a `runner` with neither source, an absolute or
/// repo-escaping `dockerfile`, or an option-like `image`; all from the
/// [`ExecutionPlan::resolve`] preflight), if the container image cannot be built, if the
/// spawn governor refuses the launch (a tripped breaker or an exhausted cap), or if the
/// selected backend fails to run or cannot produce a verdict. The runner turns any of
/// these into a fail-closed block for gates.
pub async fn dispatch(
    request: &ReviewRequest<'_>,
    governor: &Arc<SpawnGovernor>,
) -> Result<ReviewOutcome> {
    let overlay = request
        .native_session_dir
        .map(|dir| crate::akari::session_env(request.reviewer.backend, dir))
        .unwrap_or_default();
    match ExecutionPlan::resolve(request.reviewer)? {
        ExecutionPlan::Native => {
            let runner = GovernedRunner::new(SystemCommandRunner, governor.clone());
            let runner = OverlayEnvRunner::new(runner, overlay);
            run_backend(request, runner, Program::HostDefault).await
        }
        ExecutionPlan::Container(plan) => {
            let engine = ContainerEngine::from_env();
            // The image build shells out too, so it is governed like any other launch:
            // a `docker build` that fails to start counts toward the same caps.
            let base = GovernedRunner::new(SystemCommandRunner, governor.clone());
            let image = plan.ensure_image(&engine, &base, request.repo_root).await?;
            tracing::debug!(
                reviewer = %request.reviewer.name,
                image = %image,
                "running reviewer in a container"
            );
            let mut runner = ContainerRunner::new(base, engine, image, credential_passthrough());
            if let Some(dir) = request.native_session_dir {
                runner = runner.with_session_mount(dir.to_path_buf());
            }
            let runner = OverlayEnvRunner::new(runner, overlay);
            run_backend(request, runner, Program::InContainer).await
        }
    }
}

/// How a backend resolves its program: from the host, or as the bare in-container
/// name.
#[derive(Debug, Clone, Copy)]
enum Program {
    /// Resolve from the host (`BASTION_CLAUDE_BIN` / `PATH`); the native path.
    HostDefault,
    /// The default program name, resolved on the container's `PATH`.
    InContainer,
}

/// Select the concrete backend for `request` and run it over `runner`.
///
/// Shared by the native and container paths so backend selection lives in one place;
/// only how the program is resolved differs. The match is exhaustive over the wired
/// backends; every named [`reviewer::Backend`] has a real implementation, so no arm
/// fails closed for being unimplemented.
async fn run_backend<R: CommandRunner>(
    request: &ReviewRequest<'_>,
    runner: R,
    program: Program,
) -> Result<ReviewOutcome> {
    match concrete_backend(request.reviewer.backend) {
        reviewer::Backend::ClaudeCode => match program {
            Program::HostDefault => ClaudeCodeBackend::new(runner).review(request).await,
            Program::InContainer => {
                ClaudeCodeBackend::with_program(runner, claude_code::DEFAULT_PROGRAM)
                    .review(request)
                    .await
            }
        },
        reviewer::Backend::Codex => match program {
            Program::HostDefault => CodexBackend::new(runner).review(request).await,
            Program::InContainer => {
                CodexBackend::with_program(runner, codex::DEFAULT_PROGRAM)
                    .review(request)
                    .await
            }
        },
        reviewer::Backend::Pi => match program {
            Program::HostDefault => PiBackend::new(runner).review(request).await,
            Program::InContainer => {
                PiBackend::with_program(runner, pi::DEFAULT_PROGRAM)
                    .review(request)
                    .await
            }
        },
        reviewer::Backend::Grok => match program {
            Program::HostDefault => GrokBackend::new(runner).review(request).await,
            Program::InContainer => {
                GrokBackend::with_program(runner, grok::DEFAULT_PROGRAM)
                    .review(request)
                    .await
            }
        },
        reviewer::Backend::Muse => match program {
            Program::HostDefault => MuseBackend::new(runner).review(request).await,
            Program::InContainer => {
                MuseBackend::with_program(runner, muse::DEFAULT_PROGRAM)
                    .review(request)
                    .await
            }
        },
        reviewer::Backend::Any => unreachable!("concrete_backend resolves any"),
    }
}

/// Resolve a configured backend selector to the backend Bastion actually runs.
///
/// Keeping this separate from process dispatch lets terminal events and reports
/// name the concrete agent even when the registry delegates selection to Bastion.
#[must_use]
pub(crate) const fn concrete_backend(backend: reviewer::Backend) -> reviewer::Backend {
    match backend {
        reviewer::Backend::Any => reviewer::Backend::ClaudeCode,
        concrete => concrete,
    }
}

/// The shared preamble that tells a reviewing agent what its changeset is and how
/// to see it, regardless of backend.
///
/// Bastion's notion of "the changeset" is whatever the working tree differs from
/// the *merge base* by (see [`crate::git::changed_files`]): the work this branch
/// introduces, tracked edits *and* new untracked files, committed or not. The
/// agent gets the merge base pre-resolved and is steered off the two nearby
/// forms that both misreview: `git diff {base}` (two-dot against the base tip)
/// also shows everything `base` gained since the branch forked, so a reviewer
/// using it blames the base's own changes on this changeset; and `{base}...HEAD`
/// shows only committed history, silently missing the uncommitted work an author
/// is iterating on in the local loop. In CI the head is already committed and
/// there are no untracked files, so the same instruction is correct there too.
fn changeset_preamble(base: &str, merge_base: &str) -> String {
    format!(
        "You are reviewing a changeset computed against the base branch `{base}`. \
         Bastion defines the changeset as everything in the current working tree that \
         differs from the merge base with `{base}`, commit `{merge_base}`: the work \
         this branch introduces, which may include uncommitted edits and new, \
         untracked files, and never includes changes that landed on `{base}` itself. \
         To see exactly what changed, run `git diff {merge_base}` for changes to \
         tracked files, and `git status --short` (or \
         `git ls-files --others --exclude-standard`) to find untracked files, then read \
         those files directly. Do not run `git diff {base}`: if `{base}` has moved on \
         since this branch forked, that diff mixes the base branch's own changes into \
         the changeset, and those are not under review here. Do not rely on \
         `git diff {base}...HEAD` either: it shows only committed work and will miss \
         local changes that have not been committed yet."
    )
}

/// The shared instruction that tells a reviewing agent to report *every*
/// qualifying finding in one pass, not just the first one it sees.
///
/// A verdict is internally consistent when a `block` carries at least one blocking
/// finding (see [`Verdict::is_consistent`](crate::verdict::Verdict::is_consistent)),
/// so an agent left to its own devices tends to surface one representative issue
/// and stop the moment the verdict is satisfiable. That makes the author fix one
/// thing, push, and burn another full review cycle to see the next, once per issue.
/// Appending this to every reviewer prompt, on both backends, makes a single pass
/// enumerate the complete finding set so the author can fix everything at once. It
/// changes only how completely a reviewer reports, never the gate decision: a clean
/// changeset still returns `pass` with no findings, and the reviewer's own prompt
/// still decides what counts as an issue.
pub(crate) const EXHAUSTIVE_FINDINGS_INSTRUCTION: &str = "\
    Report every issue you can identify across the whole changeset in this single \
    review pass, not just the first or most obvious one. Emit a separate finding \
    for each distinct instance, each with its own location, even when several \
    instances share a cause or fall under the same rule. Do not stop after the \
    first finding or once the verdict is already decided: when you block, a single \
    finding does not discharge the review if other issues remain. Scan every \
    changed file and list them all so the author can fix the complete set in one \
    pass. If the changeset is clean, still return a pass with no findings; do not \
    invent issues to pad the list.";

/// The untrusted review-context block to splice into a prompt for `request`'s
/// reviewer, already followed by a blank-line separator, or an empty string when no
/// producer supplied context.
///
/// Shared by the backends so the context is rendered and positioned identically
/// regardless of which agent runs the reviewer, the same way [`changeset_preamble`]
/// and [`EXHAUSTIVE_FINDINGS_INSTRUCTION`] are. The block carries its own untrusted
/// framing (see [`crate::context`]); the routing and scoping live in
/// [`ReviewContext::render_for`](crate::context::ReviewContext::render_for).
pub(crate) fn context_segment(request: &ReviewRequest<'_>) -> String {
    match request.context.render_for(&request.reviewer.name) {
        Some(block) => format!("{block}\n\n"),
        None => String::new(),
    }
}

/// Assemble the full review prompt for `request`, ending with `trailer`.
///
/// Full reviews include the shared exhaustive-findings instruction. Routing calls
/// omit it because they answer one applicability question rather than inspect the
/// changeset for defects. The trailing structured-output instruction still differs
/// by backend, so it remains a parameter.
pub(crate) fn review_prompt(request: &ReviewRequest<'_>, trailer: &str) -> String {
    let reviewer = request.reviewer;
    let preamble = changeset_preamble(request.base, request.merge_base);
    let interpolated = interpolate(&reviewer.prompt, &reviewer.inputs);
    let context = context_segment(request);
    match request.purpose {
        ReviewPurpose::Review => format!(
            "{preamble}\n\n{interpolated}\n\n{context}{EXHAUSTIVE_FINDINGS_INSTRUCTION}\n\n{trailer}"
        ),
        ReviewPurpose::Routing => {
            format!("{preamble}\n\n{interpolated}\n\n{context}{trailer}")
        }
    }
}

/// Replace `${key}` occurrences in `template` with values from `inputs`.
///
/// Shared by the backends so prompt interpolation is identical regardless of
/// which agent runs the reviewer. Unknown placeholders are left untouched rather
/// than erroring: the reviewer author is trusted, and a literal `${...}` in a
/// prompt is harmless.
fn interpolate(template: &str, inputs: &BTreeMap<String, String>) -> String {
    if inputs.is_empty() || !template.contains("${") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match inputs.get(key) {
                    Some(value) => out.push_str(value),
                    None => {
                        // Unknown key: keep the literal placeholder.
                        out.push_str("${");
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated placeholder: emit the rest verbatim and stop.
                out.push_str("${");
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Convert a dollar amount to [`Money`] (exact cents), rounding to the nearest
/// cent. Negative or non-finite values clamp to zero, so a malformed cost from a
/// backend can never produce a nonsensical charge. Shared by the backends so cost
/// accounting is consistent.
fn money_from_dollars(dollars: f64) -> Money {
    if !dollars.is_finite() || dollars <= 0.0 {
        return Money::from_cents(0);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rounded, non-negative cents within u64 range for any realistic cost"
    )]
    let cents = (dollars * 100.0).round() as u64;
    Money::from_cents(cents)
}

/// The JSON Schema handed to a backend with native structured-output enforcement
/// (Claude Code's and Grok Build's `--json-schema`) to constrain its final output.
/// It is the wire form of [`Verdict`]: `verdict`, `summary`, and `findings`.
pub(super) const VERDICT_JSON_SCHEMA: &str = r#"{
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

/// The structured-output instruction closing every review prompt on a JSON-schema
/// backend (Claude Code, Grok Build), pinning the reviewer's judgment to the native
/// JSON verdict schema.
pub(super) const JSON_SCHEMA_INSTRUCTION: &str = "\
When you have finished reviewing, return your judgment as structured output \
conforming to the requested JSON schema: a top-level `verdict` of \"pass\" or \"block\", \
a human-friendly `summary`, and a `findings` array locating specific comments. Mark a \
finding `blocking` if it is a reason to block, or `optional` if it is a non-blocking \
suggestion. If you block, include a blocking finding for each issue you are blocking on.";

/// The reprompt sent on a resumed JSON-schema session when the first turn's output
/// did not conform to the verdict schema.
pub(super) const JSON_REPROMPT: &str = "Your previous response did not include a valid structured verdict. \
     Do not perform any further review work. Reply with ONLY the structured output for the \
     review you already performed, conforming exactly to the requested JSON schema: a top-level \
     `verdict` of \"pass\" or \"block\", a `summary` string, and an optional `findings` array. \
     A `block` must include at least one finding with kind \"blocking\".";

/// Parse a [`Verdict`] from a free-form result string, tolerating a fenced or
/// prose-wrapped JSON object by extracting the outermost `{...}`. The fallback for
/// a JSON-schema backend whose envelope carries no validated structured output.
pub(super) fn parse_verdict_from_text(text: &str) -> Option<Verdict> {
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

/// Truncate `s` to at most `max` bytes (on a char boundary) for error messages.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// The instruction appended to every review prompt pinning the verdict schema, for
/// backends that have no native structured-output enforcement and instead ask the
/// agent to end its final message with a fenced YAML verdict block (Codex, Pi,
/// Muse Code).
pub(super) const SCHEMA_INSTRUCTION: &str = "\
When you are done reviewing, end your final message with the structured verdict \
and nothing after it, as a fenced YAML code block matching exactly this schema:\n\
```yaml\n\
verdict: pass | block        # the authoritative gate decision\n\
summary: \"...\"               # one-line human-friendly summary\n\
findings:                    # list every issue you find; a block carries >=1 blocking\n\
  - kind: blocking | optional\n\
    path: relative/file/path\n\
    line_start: 1\n\
    line_end: 1\n\
    detail: \"...\"\n\
```";

/// The instruction used when re-prompting a fenced-YAML backend for just the
/// structured output, after its first message lacked a conforming verdict.
pub(super) const REPROMPT_SUFFIX: &str = "\
Your previous response did not contain a valid structured verdict. Do not perform \
any further work. Reply with ONLY the fenced YAML verdict block described above, \
for the review you already completed, and nothing else.";

/// The text of the reprompt turn for a fenced-YAML backend whose first message
/// lacked a conforming verdict. A resumable session already holds the review, so the
/// turn is just [`REPROMPT_SUFFIX`]; a fresh session has no memory of the prompt, so
/// the full `prompt` is re-sent ahead of the suffix. Shared by Codex, Pi, and Muse
/// Code.
pub(super) fn reprompt_text(prompt: &str, resumable: bool) -> String {
    if resumable {
        REPROMPT_SUFFIX.to_string()
    } else {
        format!("{prompt}\n\n{REPROMPT_SUFFIX}")
    }
}

/// Join an earlier session's transcript ahead of the `current` one, for when a
/// verdict was recovered on a reprompt and both turns should be preserved. An empty
/// or absent prior transcript collapses to just `current`. Shared by Codex, Pi, Grok
/// Build, and Muse Code.
pub(super) fn stitch_transcript(prior: Option<&str>, current: String) -> String {
    match prior {
        Some(prior) if !prior.is_empty() => format!("{}\n{}", prior.trim_end(), current),
        _ => current,
    }
}

/// Extract a schema-conforming, internally-consistent [`Verdict`] from `message`.
///
/// Tries the last fenced code block, then the entire message, parsing each as
/// YAML (a superset of JSON, so JSON verdicts parse too). A verdict that parses
/// but is inconsistent (e.g. a `block` with no blocking finding) is rejected so
/// the caller fails closed rather than trusting a malformed gate decision. Shared
/// by the fenced-YAML backends (Codex, Pi, Muse Code) so they read an agent's final message
/// identically.
pub(super) fn extract_verdict(message: &str) -> Option<Verdict> {
    for candidate in verdict_candidates(message) {
        // The first candidate that parses structurally as a `Verdict` is the
        // agent's authoritative verdict: the schema asks for it as the final fenced
        // block, and candidates are tried final-block-first. Commit to it. If it is
        // internally consistent, accept it; otherwise reject (fail closed) rather
        // than scanning earlier blocks for a consistent-looking one, which would
        // launder a self-contradictory final verdict into an earlier example's pass.
        if let Ok(verdict) = serde_yaml_ng::from_str::<Verdict>(&candidate) {
            return verdict.is_consistent().then_some(verdict);
        }
    }
    None
}

/// Candidate verdict texts to attempt parsing, most-specific first: each fenced
/// code block (last to first), then the whole message.
fn verdict_candidates(message: &str) -> Vec<String> {
    let mut candidates: Vec<String> = fenced_blocks(message);
    candidates.reverse();
    candidates.push(message.to_string());
    candidates
}

/// Extract the contents of every fenced code block in `message`, in source
/// order, following CommonMark fence matching: a fence opens with three or more
/// backticks and only closes on a line with at least as many backticks. An
/// optional info string (e.g. ```` ```yaml ````) on the opening fence is dropped.
///
/// Tracking the opening fence length means a longer outer fence can wrap a block
/// that itself contains shorter ` ``` ` runs without being closed prematurely.
fn fenced_blocks(message: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open_len: Option<usize> = None;
    let mut current = String::new();
    for line in message.lines() {
        let ticks = leading_backticks(line);
        match open_len {
            // Inside a block: close only on a fence at least as long, that is
            // *only* backticks (a closing fence carries no info string).
            Some(len) if ticks >= len && is_bare_fence(line) => {
                blocks.push(std::mem::take(&mut current));
                open_len = None;
            }
            Some(_) => {
                current.push_str(line);
                current.push('\n');
            }
            // Outside a block: a run of three or more backticks opens one.
            None if ticks >= 3 => {
                open_len = Some(ticks);
                current.clear();
            }
            None => {}
        }
    }
    blocks
}

/// The number of leading backticks on `line` after stripping indentation.
fn leading_backticks(line: &str) -> usize {
    line.trim_start().chars().take_while(|&c| c == '`').count()
}

/// Whether `line` (after indentation) is only backticks, a valid closing fence.
fn is_bare_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '`')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::Mode;
    use std::path::PathBuf;

    #[test]
    fn interpolate_substitutes_known_keys_and_keeps_unknown() {
        let mut inputs = BTreeMap::new();
        inputs.insert("a".to_string(), "X".to_string());
        assert_eq!(interpolate("${a} ${b}", &inputs), "X ${b}");
        assert_eq!(interpolate("no placeholders", &inputs), "no placeholders");
        assert_eq!(interpolate("${unterminated", &inputs), "${unterminated");
        // No inputs short-circuits to the original.
        assert_eq!(interpolate("${a}", &BTreeMap::new()), "${a}");
    }

    #[test]
    fn changeset_preamble_steers_to_the_merge_base_working_tree_diff() {
        let preamble = changeset_preamble("origin/main", "abc1234");
        // Names the base for orientation, but diffs at the resolved merge base...
        assert!(preamble.contains("base branch `origin/main`"));
        assert!(preamble.contains("git diff abc1234"));
        // ...covers untracked files Bastion also counts as changed...
        assert!(preamble.contains("untracked"));
        // ...warns off the base-tip diff that would put the base's own changes
        // under review...
        assert!(preamble.contains("Do not run `git diff origin/main`"));
        // ...and explicitly warns off the committed-only three-dot form.
        assert!(preamble.contains("Do not rely on `git diff origin/main...HEAD`"));
    }

    #[test]
    fn exhaustive_findings_instruction_demands_a_complete_pass() {
        let text = EXHAUSTIVE_FINDINGS_INSTRUCTION;
        let lower = text.to_lowercase();
        // It must ask for the full set, not the first finding.
        assert!(lower.contains("every issue"));
        assert!(lower.contains("do not stop after the"));
        // And it must not weaken the gate: a clean changeset still passes clean.
        assert!(lower.contains("clean"));
        assert!(lower.contains("return a pass"));
    }

    #[test]
    fn routing_prompt_omits_review_only_exhaustive_instruction() {
        let reviewer = reviewer(reviewer::Backend::Codex);
        let run = RunId("r-routing".into());
        let root = PathBuf::from(".");
        let review = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "origin/main",
            merge_base: "abc1234",
            context: ReviewContext::empty(),
            purpose: ReviewPurpose::Review,
            native_session_dir: None,
        };
        let routing = ReviewRequest {
            purpose: ReviewPurpose::Routing,
            ..review
        };

        let full_prompt = review_prompt(&review, "structured output");
        let routing_prompt = review_prompt(&routing, "structured output");

        assert!(full_prompt.contains("Do not stop after the"));
        assert!(!routing_prompt.contains("Do not stop after the"));
        assert!(routing_prompt.contains("structured output"));
        assert!(routing_prompt.contains(&reviewer.prompt));
        assert!(routing_prompt.contains("git diff abc1234"));
    }

    #[test]
    fn money_from_dollars_rounds_and_clamps() {
        assert_eq!(money_from_dollars(0.21).cents(), 21);
        assert_eq!(money_from_dollars(0.215).cents(), 22);
        assert_eq!(money_from_dollars(-1.0).cents(), 0);
        assert_eq!(money_from_dollars(f64::NAN).cents(), 0);
        assert_eq!(money_from_dollars(f64::INFINITY).cents(), 0);
    }

    #[test]
    fn any_resolves_to_the_backend_dispatch_uses() {
        assert_eq!(
            concrete_backend(reviewer::Backend::Any),
            reviewer::Backend::ClaudeCode
        );
        assert_eq!(
            concrete_backend(reviewer::Backend::Codex),
            reviewer::Backend::Codex
        );
    }

    // -- Shared fenced-YAML verdict extraction (used by the Codex and Pi backends) --

    #[test]
    fn fenced_blocks_extracts_last_block() {
        let message = "intro\n```\nfirst\n```\nmiddle\n```yaml\nsecond\n```\n";
        let blocks = fenced_blocks(message);
        assert_eq!(blocks, vec!["first\n".to_string(), "second\n".to_string()]);
    }

    #[test]
    fn fenced_blocks_respects_longer_outer_fences() {
        let message = "````markdown\nhere is an example:\n```\ninner\n```\ndone\n````\n";
        let blocks = fenced_blocks(message);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("inner"));
        assert!(blocks[0].contains("```"));
        assert!(blocks[0].contains("done"));
    }

    #[test]
    fn fenced_blocks_does_not_treat_info_string_lines_as_closers() {
        let message = "```\nverdict: pass\n```yaml not a closer\nsummary: ok\n```\n";
        let blocks = fenced_blocks(message);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("```yaml not a closer"));
        assert!(blocks[0].contains("summary: ok"));
    }

    #[test]
    fn extract_verdict_prefers_last_fenced_block() {
        let message = "\
```text
not a verdict
```
```yaml
verdict: pass
summary: chosen
```";
        let verdict = extract_verdict(message).expect("finds the verdict block");
        assert_eq!(verdict.summary, "chosen");
    }

    #[test]
    fn extract_verdict_falls_back_to_whole_message() {
        let message = "verdict: pass\nsummary: bare yaml\n";
        let verdict = extract_verdict(message).expect("parses bare yaml");
        assert_eq!(verdict.summary, "bare yaml");
    }

    #[test]
    fn extract_verdict_rejects_an_inconsistent_block() {
        // A `block` with no blocking finding is internally inconsistent, so it must
        // not be accepted: the caller reprompts and ultimately fails closed.
        let message = "```yaml\nverdict: block\nsummary: no reason\nfindings: []\n```";
        assert!(extract_verdict(message).is_none());
    }

    #[test]
    fn extract_verdict_does_not_launder_an_inconsistent_final_verdict() {
        // The final fenced block is the authoritative verdict. When it parses as a
        // verdict but is inconsistent (a `block` with no blocking finding), the
        // function must fail closed, not fall back to an earlier consistent-looking
        // block (here a `pass` example) and launder it into a passing decision.
        let message = "\
```yaml
verdict: pass
summary: an earlier example block
findings: []
```
```yaml
verdict: block
summary: the real, but inconsistent, final verdict
findings: []
```";
        assert!(extract_verdict(message).is_none());
    }

    fn reviewer(backend: reviewer::Backend) -> Reviewer {
        Reviewer {
            name: "demo".into(),
            trigger: vec!["**".into()].into(),
            mode: Mode::Gate,
            backend,
            model: None,
            effort: None,
            timeout: None,
            runner: None,
            env: Default::default(),
            capabilities: Default::default(),
            inputs: Default::default(),
            attestation: None,
            prompt: "p".into(),
        }
    }

    #[tokio::test]
    async fn mock_backend_passes_deterministically() {
        let reviewer = reviewer(reviewer::Backend::Any);
        let run = RunId("r-test".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: ReviewPurpose::Review,
            native_session_dir: None,
        };

        let outcome = MockBackend.review(&request).await.expect("mock runs");
        assert_eq!(outcome.verdict.decision, Decision::Pass);
        assert!(outcome.verdict.summary.contains("demo"));
        assert_eq!(MockBackend.id(), reviewer::Backend::Any);
    }

    #[tokio::test]
    async fn dispatch_fails_closed_on_unprovisioned_capability() {
        // The plan resolves ahead of backend selection, so a reviewer that opts into
        // an unprovisioned tier (here, skills) never reaches a backend: dispatch
        // returns the fail-closed error the runner turns into a block for a gate. The
        // plan-resolution cases themselves are covered in `container`.
        let mut reviewer = reviewer(reviewer::Backend::ClaudeCode);
        reviewer.capabilities.skills = vec!["stop-slop".into()];
        let run = RunId("r".into());
        let root = PathBuf::from(".");
        let request = ReviewRequest {
            reviewer: &reviewer,
            run: &run,
            repo_root: &root,
            base: "main",
            merge_base: "deadbeef",
            context: crate::context::ReviewContext::empty(),
            purpose: ReviewPurpose::Review,
            native_session_dir: None,
        };
        let err = dispatch(&request, &SpawnGovernor::shared_default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skills"));
    }

    #[test]
    fn the_shipped_registry_is_fully_provisionable() {
        // Every reviewer in the shipped registry must resolve to an execution plan:
        // a declaration this build cannot honor would fail that reviewer closed at
        // review time. Reintroducing an unprovisioned `mcp`/`skills`, a native
        // `network: true`, or a containerized `network: false` parses fine and loads
        // fine, so without this guard it would slip past the registry-load test and
        // only surface as a self-wedged gate. With no in-review gate over the
        // registry, this build-time check is the guard against an unprovisionable
        // reviewer reaching the shipped set.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(crate::config::REGISTRY_FILE);
        let config = crate::config::Config::load(&path).expect("shipped registry loads");
        for reviewer in &config.reviewers {
            ExecutionPlan::resolve(reviewer).unwrap_or_else(|err| {
                panic!(
                    "shipped reviewer '{}' is not provisionable: {err}",
                    reviewer.name
                )
            });
        }
    }
}
