//! The compiled fakes and the detect-and-skip tooling guards.
//!
//! These stand in for the heavyweight external programs the real backends shell
//! out to: a deterministic, contract-checking fake agent for claude/codex/pi/grok,
//! and a
//! fake container engine for docker/podman. Both are compiled once with `rustc`
//! and reused for the whole test process; scenarios that need a toolchain we
//! cannot guarantee detect-and-skip through [`tooling`] / [`container_tooling`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// The fake agent: a deterministic, contract-checking stand-in for the agent CLIs.
// ---------------------------------------------------------------------------

/// Source for a tiny native program that emulates the four agent CLIs.
///
/// It detects which protocol it is being driven as from its argv (Codex is
/// invoked with an `exec` subcommand; Pi with `--mode`; Grok Build with
/// `--reasoning-effort`; Claude with `--output-format` and none of those), detects
/// a reprompt turn (Codex `resume` / Pi `--session` / Claude and Grok `--resume`),
/// validates the invocation
/// against the contract each backend promises, and then chooses its output from
/// the `FAKE_BEHAVIOR` environment variable Bastion propagates from the reviewer:
///
/// - `pass`             -- a consistent passing verdict (the default).
/// - `block`            -- a consistent blocking verdict with one blocking finding.
/// - `inconsistent`     -- a `block` with no blocking finding (an internally
///   inconsistent verdict the backend must reject, then reprompt, then fail closed).
/// - `malformed`        -- output with no parseable verdict, on every turn (drives
///   the backend's single reprompt, then a fail-closed block for a gate).
/// - `reprompt-recover` -- malformed on the first turn, valid on the resumed turn
///   (exercises the reprompt-and-recover path through the real subprocess).
/// - `crash`            -- exit non-zero (an execution failure -> fail closed).
/// - `slow`             -- sleep `FAKE_SLEEP_MS` then pass (pair with a short
///   reviewer `timeout` to force a timeout; or a generous one to test concurrency).
///
/// Extra knobs, all read from the propagated environment: `FAKE_COST_CENTS`
/// (default 5), `FAKE_TOKENS_IN`/`FAKE_TOKENS_OUT`, `FAKE_CACHE_READ` (default 0,
/// emitted under each backend's native cache-read field name), `FAKE_SUMMARY`,
/// `FAKE_EXPECT_PROMPT_CONTAINS` (assert the delivered prompt contains a marker --
/// used to verify `${...}` interpolation end to end), and `FAKE_MARKER_FILE`
/// (written only *after* the sleep, so a killed-on-timeout child never writes it).
pub(crate) const FAKE_AGENT_SRC: &str = r##"
use std::io::Read;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a.as_str() == flag)
}

fn fail(reason: &str) -> ! {
    eprintln!("fake agent contract violation: {reason}");
    std::process::exit(3);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_codex = has(&args, "exec");
    // Pi is driven in print mode with `--mode json`; neither Codex nor Claude uses
    // `--mode`, so it is the unambiguous discriminator.
    let is_pi = has(&args, "--mode");
    // Grok Build's headless flags mirror Claude's (`--output-format`, `--json-schema`,
    // `-p`, `--resume`); its effort flag is the discriminator.
    // Muse Code is driven as `muse exec --json`; `exec` alone would collide with
    // Codex, so the `--yolo` unattended flag is the discriminator, checked first.
    let is_muse = has(&args, "--yolo");
    let is_codex = !is_muse && is_codex;
    let is_grok = !is_muse && has(&args, "--reasoning-effort");
    let is_reprompt = has(&args, "resume")
        || has(&args, "--resume")
        || has(&args, "--session")
        || has(&args, "--session-id");

    // Drain stdin so a parent piping a prompt over it never blocks on a full pipe.
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);

    // The prompt is delivered over stdin by Codex (the trailing `-`) and Pi (print
    // mode reads the task from stdin), as the trailing positional by Muse, and over
    // argv by Claude and Grok (`-p <prompt>`). Recover whichever applies so the
    // contract checks can confirm it arrived.
    let prompt = if is_codex || is_pi {
        stdin.clone()
    } else if is_muse {
        args.last().cloned().unwrap_or_default()
    } else {
        let mut found = String::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if arg == "-p" {
                if let Some(value) = iter.next() {
                    found = value.clone();
                }
                break;
            }
        }
        found
    };

    // --- contract checks: the invocation must match what the backend promises ---
    if is_codex {
        // `exec` is already implied by is_codex; assert the rest of the contract.
        if !has(&args, "--json") {
            fail("codex: missing `--json`");
        }
        if !has(&args, "--dangerously-bypass-approvals-and-sandbox") {
            fail("codex: missing `--dangerously-bypass-approvals-and-sandbox` (unattended mode)");
        }
        if args.last().map(String::as_str) != Some("-") {
            fail("codex: prompt is not read from stdin (last arg is not `-`)");
        }
        if stdin.trim().is_empty() {
            fail("codex: no prompt was piped over stdin");
        }
        if is_reprompt && !has(&args, "th-fake") {
            fail("codex: reprompt did not resume the reported thread id `th-fake`");
        }
    } else if is_pi {
        for flag in ["-p", "--mode", "json"] {
            if !has(&args, flag) {
                fail(&format!("pi: missing `{flag}`"));
            }
        }
        if stdin.trim().is_empty() {
            fail("pi: no prompt was piped over stdin");
        }
        if is_reprompt && !has(&args, "pi-fake") {
            fail("pi: reprompt did not resume the reported session id `pi-fake`");
        }
    } else if is_muse {
        for flag in ["exec", "--json", "--yolo", "--reasoning-effort"] {
            if !has(&args, flag) {
                fail(&format!("muse: missing `{flag}`"));
            }
        }
        if is_reprompt && !has(&args, "m-fake") {
            fail("muse: reprompt did not resume the reported session id `m-fake`");
        }
    } else if is_grok {
        for flag in ["--output-format", "json", "--json-schema", "--permission-mode", "bypassPermissions", "-p"] {
            if !has(&args, flag) {
                fail(&format!("grok: missing `{flag}`"));
            }
        }
        if is_reprompt && !has(&args, "g-fake") {
            fail("grok: reprompt did not resume the reported session id `g-fake`");
        }
    } else {
        for flag in ["--output-format", "--json-schema", "--permission-mode", "bypassPermissions", "-p"] {
            if !has(&args, flag) {
                fail(&format!("claude: missing `{flag}`"));
            }
        }
        if is_reprompt && !has(&args, "s-fake") {
            fail("claude: reprompt did not resume the reported session id `s-fake`");
        }
    }

    // If asked, assert the model/effort selectors reached the argv, so a test can
    // prove the registry's model/effort (own or inherited from `defaults`) is
    // honored end to end. The selectors differ per backend.
    if let Ok(model) = std::env::var("FAKE_EXPECT_MODEL") {
        if !model.is_empty() {
            let flag = if is_codex { "-m" } else { "--model" };
            if !has(&args, flag) || !has(&args, &model) {
                fail(&format!("expected model `{model}` via `{flag}` not on the argv"));
            }
        }
    }
    if let Ok(effort) = std::env::var("FAKE_EXPECT_EFFORT") {
        if !effort.is_empty() {
            let needle = if is_codex {
                format!("model_reasoning_effort=\"{effort}\"")
            } else {
                effort.clone()
            };
            if !has(&args, &needle) {
                fail(&format!("expected effort `{effort}` (needle `{needle}`) not on the argv"));
            }
        }
    }

    // Every turn carries the verdict schema instruction; the first turn also
    // carries the changeset preamble. (A reprompt re-sends only the schema ask.)
    if !prompt.contains("verdict") {
        fail("prompt did not carry the verdict schema instruction");
    }
    if !is_reprompt {
        if !prompt.contains("changeset") {
            fail("first-turn prompt did not carry the changeset preamble");
        }
        if let Ok(expected) = std::env::var("FAKE_EXPECT_PROMPT_CONTAINS") {
            if !expected.is_empty() && !prompt.contains(&expected) {
                fail(&format!("prompt did not contain expected marker `{expected}` (interpolation?)"));
            }
        }
    }

    let behavior = env_or("FAKE_BEHAVIOR", "pass");

    let sleep_ms: u64 = env_or("FAKE_SLEEP_MS", "0").parse().unwrap_or(0);
    if sleep_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }

    // Written only after the sleep: a child killed on timeout never reaches here,
    // which is how the timeout test proves `kill_on_drop` actually fires.
    if let Ok(marker) = std::env::var("FAKE_MARKER_FILE") {
        if !marker.is_empty() {
            let _ = std::fs::write(&marker, "alive");
        }
    }

    if behavior == "crash" {
        eprintln!("fake agent: simulated crash");
        std::process::exit(7);
    }

    // reprompt-recover is malformed on the first turn and valid once resumed.
    let effective = if behavior == "reprompt-recover" {
        if is_reprompt { "pass" } else { "malformed" }
    } else {
        behavior.as_str()
    };

    let cost_cents: u64 = env_or("FAKE_COST_CENTS", "5").parse().unwrap_or(5);
    let tin: u64 = env_or("FAKE_TOKENS_IN", "100").parse().unwrap_or(100);
    let tout: u64 = env_or("FAKE_TOKENS_OUT", "10").parse().unwrap_or(10);
    let cache: u64 = env_or("FAKE_CACHE_READ", "0").parse().unwrap_or(0);
    let dollars = format!("{}.{:02}", cost_cents / 100, cost_cents % 100);
    let mut summary = env_or("FAKE_SUMMARY", "fake reviewer verdict");
    // If asked, echo the value of a named environment variable into the summary, so a
    // test can observe whether that variable actually reached the agent (used to prove
    // provider-credential passthrough crosses the container boundary).
    if let Ok(var) = std::env::var("FAKE_ECHO_ENV") {
        if !var.is_empty() {
            let value = std::env::var(&var).unwrap_or_else(|_| "unset".to_string());
            summary = format!("{summary} echo {value}");
        }
    }

    if is_codex {
        emit_codex(effective, &summary, &dollars, tin, tout, cache);
    } else if is_pi {
        emit_pi(effective, &summary, &dollars, tin, tout, cache);
    } else if is_grok {
        emit_grok(effective, &summary, &dollars, tin, tout, cache);
    } else if is_muse {
        emit_muse(effective, &summary);
    } else {
        emit_claude(effective, &summary, &dollars, tin, tout, cache);
    }
}

fn emit_muse(behavior: &str, summary: &str) {
    // Muse's `exec --json` stream is a sequence of records, each stamped with the
    // session stream (whose id the backend resumes by on a reprompt) and a
    // `payload_type`. The verdict travels as the `text` of the `run.terminal.completed`
    // record, a fenced YAML block. The stream carries no usage. The `\n` are JSON
    // string escapes that serde_json decodes into real newlines on parse.
    let mut text = String::new();
    match behavior {
        "block" => {
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings:\n  - kind: blocking\n    path: src/extra.rs\n    line_start: 1\n    line_end: 1\n    detail: simulated blocking finding\n```"#);
        }
        "inconsistent" => {
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
        "malformed" => {
            text.push_str("I looked at the changeset but I will not give a verdict.");
        }
        _ => {
            text.push_str(r#"```yaml\nverdict: pass\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
    }
    println!(
        "{}",
        r#"{"schema_version":1,"stream":{"kind":"session","id":"m-fake"},"record_type":"event","payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}"#
    );
    let mut line = String::new();
    line.push_str(r#"{"schema_version":1,"stream":{"kind":"session","id":"m-fake"},"record_type":"event","payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","reason":null,"text":""#);
    line.push_str(&text);
    line.push_str(r#""}}"#);
    println!("{}", line);
}

fn emit_grok(behavior: &str, summary: &str, dollars: &str, tin: u64, tout: u64, cache: u64) {
    // Grok Build's envelope is Claude-shaped with camelCase keys: `sessionId`,
    // `structuredOutput`, `text`, plus `usage` and `total_cost_usd`.
    let mut body = String::new();
    match behavior {
        "malformed" => {
            body.push_str(r#"{"sessionId":"g-fake","text":"I reviewed it but I forgot the schema."}"#);
        }
        "block" => {
            body.push_str(r#"{"sessionId":"g-fake","total_cost_usd":"#);
            body.push_str(dollars);
            body.push_str(r#","usage":{"input_tokens":"#);
            body.push_str(&tin.to_string());
            body.push_str(r#","output_tokens":"#);
            body.push_str(&tout.to_string());
            body.push_str(r#","cache_read_input_tokens":"#);
            body.push_str(&cache.to_string());
            body.push_str(r#"},"structuredOutput":{"verdict":"block","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[{"kind":"blocking","path":"src/extra.rs","line_start":1,"line_end":1,"detail":"simulated blocking finding"}]}}"#);
        }
        "inconsistent" => {
            body.push_str(r#"{"sessionId":"g-fake","structuredOutput":{"verdict":"block","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[]}}"#);
        }
        _ => {
            body.push_str(r#"{"sessionId":"g-fake","total_cost_usd":"#);
            body.push_str(dollars);
            body.push_str(r#","usage":{"input_tokens":"#);
            body.push_str(&tin.to_string());
            body.push_str(r#","output_tokens":"#);
            body.push_str(&tout.to_string());
            body.push_str(r#","cache_read_input_tokens":"#);
            body.push_str(&cache.to_string());
            body.push_str(r#"},"structuredOutput":{"verdict":"pass","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[]}}"#);
        }
    }
    print!("{}", body);
}

fn emit_pi(behavior: &str, summary: &str, dollars: &str, tin: u64, tout: u64, cache: u64) {
    // The session event carries the id the backend resumes by on a reprompt.
    println!("{}", r#"{"type":"session","id":"pi-fake"}"#);

    // The verdict travels as the text of an assistant `message_end`, a fenced YAML
    // block. The `\n` are JSON string escapes: serde_json decodes them into real
    // newlines when the backend parses the line, so the fenced block splits.
    let mut text = String::new();
    match behavior {
        "block" => {
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings:\n  - kind: blocking\n    path: src/extra.rs\n    line_start: 1\n    line_end: 1\n    detail: simulated blocking finding\n```"#);
        }
        "inconsistent" => {
            // A `block` with no blocking finding: internally inconsistent.
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
        "malformed" => {
            text.push_str("I looked at the changeset but I will not give a verdict.");
        }
        _ => {
            text.push_str(r#"```yaml\nverdict: pass\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
    }

    let mut line = String::new();
    line.push_str(r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":""#);
    line.push_str(&text);
    line.push_str(r#""}],"usage":{"input":"#);
    line.push_str(&tin.to_string());
    line.push_str(r#","output":"#);
    line.push_str(&tout.to_string());
    line.push_str(r#","cacheRead":"#);
    line.push_str(&cache.to_string());
    line.push_str(r#","cost":{"total":"#);
    line.push_str(dollars);
    line.push_str(r#"}}}}"#);
    println!("{}", line);
}

fn emit_codex(behavior: &str, summary: &str, dollars: &str, tin: u64, tout: u64, cache: u64) {
    // The verdict travels inside a JSON-lines `agent_message` as a fenced YAML
    // block. The `\n` below are JSON string escapes: serde_json decodes them into
    // real newlines when the backend parses each event line, so the fenced block
    // splits correctly. (Emitting literal newlines here would be invalid JSON.)
    println!("{}", r#"{"type":"thread.started","thread_id":"th-fake"}"#);

    let mut text = String::new();
    match behavior {
        "block" => {
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings:\n  - kind: blocking\n    path: src/extra.rs\n    line_start: 1\n    line_end: 1\n    detail: simulated blocking finding\n```"#);
        }
        "inconsistent" => {
            // A `block` with no blocking finding: internally inconsistent.
            text.push_str(r#"```yaml\nverdict: block\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
        "malformed" => {
            text.push_str("I looked at the changeset but I will not give a verdict.");
        }
        _ => {
            text.push_str(r#"```yaml\nverdict: pass\nsummary: "#);
            text.push_str(summary);
            text.push_str(r#"\nfindings: []\n```"#);
        }
    }

    let mut line = String::new();
    line.push_str(r#"{"type":"item.completed","item":{"type":"agent_message","text":""#);
    line.push_str(&text);
    line.push_str(r#""}}"#);
    println!("{}", line);

    let mut usage = String::new();
    usage.push_str(r#"{"type":"turn.completed","usage":{"input_tokens":"#);
    usage.push_str(&tin.to_string());
    usage.push_str(r#","output_tokens":"#);
    usage.push_str(&tout.to_string());
    usage.push_str(r#","cached_input_tokens":"#);
    usage.push_str(&cache.to_string());
    usage.push_str(r#","cost_usd":"#);
    usage.push_str(dollars);
    usage.push_str(r#"}}"#);
    println!("{}", usage);
}

fn emit_claude(behavior: &str, summary: &str, dollars: &str, tin: u64, tout: u64, cache: u64) {
    let mut body = String::new();
    match behavior {
        "malformed" => {
            body.push_str(r#"{"session_id":"s-fake","result":"I reviewed it but I forgot the schema."}"#);
        }
        "block" => {
            body.push_str(r#"{"session_id":"s-fake","total_cost_usd":"#);
            body.push_str(dollars);
            body.push_str(r#","usage":{"input_tokens":"#);
            body.push_str(&tin.to_string());
            body.push_str(r#","output_tokens":"#);
            body.push_str(&tout.to_string());
            body.push_str(r#","cache_read_input_tokens":"#);
            body.push_str(&cache.to_string());
            body.push_str(r#"},"structured_output":{"verdict":"block","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[{"kind":"blocking","path":"src/extra.rs","line_start":1,"line_end":1,"detail":"simulated blocking finding"}]}}"#);
        }
        "inconsistent" => {
            body.push_str(r#"{"session_id":"s-fake","structured_output":{"verdict":"block","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[]}}"#);
        }
        _ => {
            body.push_str(r#"{"session_id":"s-fake","total_cost_usd":"#);
            body.push_str(dollars);
            body.push_str(r#","usage":{"input_tokens":"#);
            body.push_str(&tin.to_string());
            body.push_str(r#","output_tokens":"#);
            body.push_str(&tout.to_string());
            body.push_str(r#","cache_read_input_tokens":"#);
            body.push_str(&cache.to_string());
            body.push_str(r#"},"structured_output":{"verdict":"pass","summary":""#);
            body.push_str(summary);
            body.push_str(r#"","findings":[]}}"#);
        }
    }
    print!("{}", body);
}
"##;

/// Compile the fake agent once per test process and cache its path.
///
/// The executable lives in a leaked temp directory so it survives for the whole
/// process (all scenarios share the one binary). Returns `None` -- so callers
/// detect-and-skip -- when no usable `rustc` is on `PATH`.
fn fake_agent() -> Option<&'static Path> {
    static FAKE: OnceLock<Option<PathBuf>> = OnceLock::new();
    FAKE.get_or_init(build_fake_agent).as_deref()
}

fn build_fake_agent() -> Option<PathBuf> {
    compile_rust(FAKE_AGENT_SRC, "fake-agent")
}

/// Compile a small standalone Rust `src` into a leaked temp dir, returning the exe
/// path. Returns `None` (so callers detect-and-skip) when no usable `rustc` is on
/// `PATH`. The binary outlives the call (the temp dir is leaked) so it is reused for
/// the whole test process.
fn compile_rust(src: &str, exe_stem: &str) -> Option<PathBuf> {
    let dir = tempfile::tempdir().ok()?;
    let dir: &'static tempfile::TempDir = Box::leak(Box::new(dir));

    let src_path = dir.path().join(format!("{exe_stem}.rs"));
    std::fs::write(&src_path, src).ok()?;

    let exe = dir.path().join(if cfg!(windows) {
        format!("{exe_stem}.exe")
    } else {
        exe_stem.to_string()
    });

    let output = Command::new("rustc")
        .arg(&src_path)
        .arg("--edition")
        .arg("2021")
        .arg("-O")
        .arg("-o")
        .arg(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if output.status.success() && exe.exists() {
        Some(exe)
    } else {
        eprintln!(
            "could not compile {exe_stem} with rustc:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        None
    }
}

/// Source for a fake container engine standing in for `docker`/`podman`.
///
/// It honors the subcommands Bastion drives: `build` does no work (the image is a
/// fiction; the real build invocation is unit-tested in `src/backend/container/`)
/// but records that it ran, so a test can prove `ensure_image` fired before the run;
/// `rm` records the cancellation teardown (so a test can prove a timed-out container
/// is force-removed); and `run` parses the `docker run`
/// line that [`ContainerRunner`] emits. It collects the reviewer env from the
/// `--env-file` it points at, plus credentials passed `-e NAME` (read from its own
/// environment), captures the backend program from `--entrypoint`, skips the
/// `--rm`/`-i`/`-v`/`-w`/`--name` flags, then takes the image and the entrypoint's
/// args, and re-executes the *fake agent* with that program, those args, the forwarded
/// env, and the piped stdin. So a containerized
/// review takes the genuine path: the `dispatch` container branch, image resolution,
/// the `docker run` argv, the agent protocol, and the parse and aggregation all run for
/// real.
const FAKE_DOCKER_SRC: &str = r#"
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "build" => {
            // The image is a fiction, so there is nothing to build, but record that the
            // build ran so a test can prove `ensure_image` fired before `docker run` (a
            // regression that stopped building a `dockerfile` reviewer would drop this
            // line and the run would precede no build).
            if let Ok(log) = std::env::var("FAKE_DOCKER_LOG") {
                if !log.is_empty() {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                        let _ = writeln!(f, "build");
                    }
                }
            }
            std::process::exit(0);
        }
        "run" => {}
        "rm" => {
            // `rm -f <name>`: the cancellation teardown. Record it so a test can prove
            // a timed-out container is force-removed, then exit success.
            let name = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned().unwrap_or_default();
            if let Ok(log) = std::env::var("FAKE_DOCKER_LOG") {
                if !log.is_empty() {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                        let _ = writeln!(f, "rm:{}", name);
                    }
                }
            }
            std::process::exit(0);
        }
        other => {
            eprintln!("fake docker: unexpected subcommand {other:?}");
            std::process::exit(2);
        }
    }

    // Walk the `run` options up to the image, collecting `-e` env and the
    // `--entrypoint` program on the way.
    let mut envs: Vec<(String, String)> = Vec::new();
    let mut entrypoint: Option<String> = None;
    let mut i = 1;
    let image_at = loop {
        let arg = match args.get(i) {
            Some(a) => a,
            None => {
                eprintln!("fake docker: `run` had no image");
                std::process::exit(2);
            }
        };
        match arg.as_str() {
            "--rm" | "-i" => i += 1,
            "-v" | "-w" | "--name" => i += 2,
            "--entrypoint" => {
                // The backend program runs as the container entrypoint (so an image
                // ENTRYPOINT cannot hijack it); capture it as the program to exec.
                entrypoint = args.get(i + 1).cloned();
                i += 2;
            }
            "--env-file" => {
                // Reviewer env arrives in a file of `KEY=VALUE` lines; read them as the
                // engine would inject them into the container.
                let path = args.get(i + 1).cloned().unwrap_or_default();
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    for line in contents.lines() {
                        if let Some((k, v)) = line.split_once('=') {
                            envs.push((k.to_string(), v.to_string()));
                        }
                    }
                }
                i += 2;
            }
            "-e" => {
                let spec = args.get(i + 1).cloned().unwrap_or_default();
                match spec.split_once('=') {
                    Some((k, v)) => envs.push((k.to_string(), v.to_string())),
                    None => {
                        if let Ok(v) = std::env::var(&spec) {
                            envs.push((spec, v));
                        }
                    }
                }
                i += 2;
            }
            _ => break i,
        }
    };

    // The program is the captured `--entrypoint`; everything after the image is its
    // arguments. (A real engine runs the entrypoint with those as argv.)
    let program = match entrypoint {
        Some(p) => p,
        None => {
            eprintln!("fake docker: `run` had no --entrypoint");
            std::process::exit(2);
        }
    };
    let rest: Vec<String> = args.get(image_at + 1..).map(|s| s.to_vec()).unwrap_or_default();

    let agent = std::env::var("FAKE_AGENT_BIN")
        .expect("fake docker: FAKE_AGENT_BIN must point at the fake agent");

    // Record the in-container program so a test can prove the engine actually ran,
    // and ran the bare in-image `claude`/`codex` rather than a host-resolved path: a
    // regression to the native path would never reach this fake engine at all.
    if let Ok(log) = std::env::var("FAKE_DOCKER_LOG") {
        if !log.is_empty() {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                let _ = writeln!(f, "{}", program);
            }
        }
    }

    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    // Model the container's environment isolation: a real container does not inherit
    // the host environment, only what Bastion forwards (reviewer `env` via the
    // `--env-file`, provider credentials via `-e NAME`). Clear the inherited env and
    // add back just the OS essentials the child needs to start, then the forwarded
    // vars. This makes the forwarding load-bearing: a reviewer env var (or credential)
    // reaches the agent only because Bastion forwarded it, not by accidental
    // inheritance, so the block scenario proves reviewer env crosses and the
    // isolation scenario proves an unforwarded host var does not.
    let mut command = Command::new(&agent);
    command.arg(&program).args(&rest).env_clear();
    for key in [
        "PATH", "SYSTEMROOT", "SystemRoot", "windir", "SystemDrive", "TEMP", "TMP",
        "HOME", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .envs(envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fake docker: spawning the fake agent");
    if let Some(mut sink) = child.stdin.take() {
        let _ = sink.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().expect("fake docker: agent runs");
    std::io::stdout().write_all(&out.stdout).unwrap();
    std::io::stderr().write_all(&out.stderr).unwrap();
    std::process::exit(out.status.code().unwrap_or(1));
}
"#;

/// The compiled fake container engine, or `None` when no usable `rustc` is present.
fn fake_docker() -> Option<&'static Path> {
    static FAKE: OnceLock<Option<PathBuf>> = OnceLock::new();
    FAKE.get_or_init(|| compile_rust(FAKE_DOCKER_SRC, "fake-docker"))
        .as_deref()
}

/// The fake agent *and* the fake container engine, for the container scenarios;
/// `None` means skip this run.
///
/// Like [`tooling`], this fails closed in CI rather than skipping: once [`tooling`]
/// has built the fake agent, `rustc` is present, so a `None` from [`fake_docker`]
/// means the fake-engine source itself stopped compiling. Skipping silently there
/// would gut the entire container end-to-end layer without turning CI red.
pub(crate) fn container_tooling() -> Option<(&'static Path, &'static Path)> {
    let fake = tooling()?;
    match fake_docker() {
        Some(docker) => Some((fake, docker)),
        None => {
            assert!(
                std::env::var_os("CI").is_none(),
                "rustc built the fake agent but not the fake container engine; the \
                 container integration coverage must not silently skip in CI"
            );
            eprintln!("skipping container scenario: no usable fake container engine");
            None
        }
    }
}

/// Whether `git` is on `PATH` (scenarios stand up throwaway repos with it).
fn git_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("git")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Whether `ssh-keygen` is on `PATH` (attestation scenarios sign with it).
fn ssh_keygen_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("ssh-keygen")
            .arg("-h")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            // `ssh-keygen -h` prints usage and exits non-zero on most
            // implementations, so its mere ability to launch is the signal, not a
            // clean exit status.
            .is_ok()
    })
}

/// The tooling an attestation scenario needs: the ordinary [`tooling`] set, plus
/// `ssh-keygen` for signing. `None` means skip this run, following the same
/// detect-and-skip idiom (fail closed in CI, skip locally).
pub(crate) fn ssh_tooling() -> Option<&'static Path> {
    let fake = tooling()?;
    let in_ci = std::env::var_os("CI").is_some();
    if !ssh_keygen_available() {
        assert!(
            !in_ci,
            "ssh-keygen must be available to run the attestation integration suite in CI"
        );
        eprintln!("skipping integration scenario: ssh-keygen is not available");
        return None;
    }
    Some(fake)
}

/// The set of tools a scenario needs; `None` means skip this run.
///
/// In CI (where `CI` is set) the tools must be present: a silently-skipped suite
/// is worse than a red one, because it would let the whole end-to-end layer rot
/// undetected. Locally, missing tools just skip.
pub(crate) fn tooling() -> Option<&'static Path> {
    let in_ci = std::env::var_os("CI").is_some();
    if !git_available() {
        assert!(
            !in_ci,
            "git must be available to run the integration suite in CI"
        );
        eprintln!("skipping integration scenario: git is not available");
        return None;
    }
    match fake_agent() {
        Some(path) => Some(path),
        None => {
            assert!(
                !in_ci,
                "rustc must be available to build the fake agent for the integration suite in CI"
            );
            eprintln!("skipping integration scenario: no usable rustc to build the fake agent");
            None
        }
    }
}
