//! The subprocess boundary, behind an injectable seam.
//!
//! Backends shell out to an agent CLI (`claude`, `codex`, ...). To keep that
//! testable without the real binary or a network, the actual process spawn lives
//! behind [`CommandRunner`]: production uses [`SystemCommandRunner`] (a real
//! `tokio` child process), while tests inject a runner that drives a fake
//! executable or canned output. The trait is the one place that touches the OS,
//! so everything above it is deterministic. [`OverlayEnvRunner`] is the decorator
//! that injects isolation env when Akari handoff is on.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use color_eyre::eyre::{Context, Result};
use tokio::io::AsyncWriteExt;

/// A fully-specified invocation of an agent CLI.
///
/// This is the parsed, proof-carrying form a backend hands to a [`CommandRunner`]:
/// the program, its arguments, the working directory, and the environment
/// overlay are all resolved, so the runner only has to spawn it.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// The program to execute (e.g. the `claude` binary path).
    pub program: OsString,
    /// The arguments, in order.
    pub args: Vec<OsString>,
    /// The working directory to run in (the repository checkout).
    pub cwd: PathBuf,
    /// Environment variables to set for the child, layered over the parent's.
    pub env: BTreeMap<String, String>,
    /// Text to pipe to the child's standard input, if any. Backends use this to
    /// pass a large or special-character-laden prompt without making it a command
    /// argument -- which also sidesteps the Windows refusal to forward complex
    /// arguments to a `.cmd`/`.bat` shim. `None` connects stdin to null.
    pub stdin: Option<String>,
}

impl CommandSpec {
    /// Start a spec for `program` running in `cwd`, with no args, env, or stdin.
    pub fn new(program: impl Into<OsString>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: BTreeMap::new(),
            stdin: None,
        }
    }

    /// Append one argument.
    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Set the text piped to the child's standard input.
    pub fn stdin(&mut self, input: impl Into<String>) -> &mut Self {
        self.stdin = Some(input.into());
        self
    }
}

/// The captured result of running a [`CommandSpec`] to completion.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// The process exit code, or `None` if it was killed by a signal.
    pub code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl CommandOutput {
    /// Whether the process exited successfully (code 0).
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// The seam over process execution: run a [`CommandSpec`] and capture its output.
///
/// Production wires this to a real child process; tests drive a fake executable
/// or canned responses through the same interface, so backends never special-case
/// being under test.
#[allow(
    async_fn_in_trait,
    reason = "single-crate trait consumed internally, not across a public API boundary"
)]
pub trait CommandRunner: Send + Sync {
    /// Run the command to completion and return its captured output.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be spawned (e.g. the program is not
    /// on `PATH`) or its output cannot be captured. A non-zero exit is *not* an
    /// error here: it is reported via [`CommandOutput::code`] so the caller can
    /// decide what it means.
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput>;
}

/// A [`CommandRunner`] that spawns a real child process via `tokio`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
        let program = resolve_executable(&spec.program);
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The runner bounds each reviewer with `tokio::time::timeout`; on
            // timeout it drops this future. Without `kill_on_drop`, the agent
            // subprocess would keep running detached -- still using tools, mutating
            // the checkout, and burning tokens after Bastion has already failed the
            // reviewer closed. Killing the child on drop makes the timeout real.
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let mut child = command.spawn().wrap_err_with(|| {
            format!(
                "failed to spawn '{}'; is it installed and on PATH?",
                spec.program.to_string_lossy()
            )
        })?;

        // Feed stdin from a concurrent task so a child that writes to stdout while
        // still reading its prompt cannot deadlock against a full stdin pipe.
        if let Some(input) = spec.stdin.clone()
            && let Some(mut sink) = child.stdin.take()
        {
            tokio::spawn(async move {
                let _ = sink.write_all(input.as_bytes()).await;
                let _ = sink.shutdown().await;
            });
        }

        let output = child
            .wait_with_output()
            .await
            .wrap_err_with(|| format!("failed to run '{}'", spec.program.to_string_lossy()))?;

        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Resolve a program to a concrete executable when it is a bare command name on
/// Windows.
///
/// The OS process spawner on Windows does not consult `PATHEXT`, so spawning a
/// bare `codex` will not find an npm-installed `codex.cmd` shim (there is no
/// `codex.exe`). Here we mirror the shell's lookup: for a bare name on Windows,
/// search each `PATH` entry for the name plus each `PATHEXT` extension and return
/// the first hit. Path-like programs, and every program on other platforms (where
/// `execvp` already searches `PATH`), are returned unchanged.
///
/// [`SystemCommandRunner`] applies this before every spawn; anything that spawns a
/// program outside that runner (the container teardown guard, which runs in a `Drop`
/// and so cannot route through the async seam) must apply it too, or a bare engine
/// name that builds and runs would fail to spawn at teardown on Windows.
pub(crate) fn resolve_executable(program: &OsStr) -> OsString {
    if !cfg!(windows) {
        return program.to_os_string();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return program.to_os_string();
    };
    let path_dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    let pathext =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    let pathext = pathext.to_string_lossy();
    resolve_windows_executable(program, &path_dirs, &pathext, |candidate| {
        candidate.is_file()
    })
    .unwrap_or_else(|| program.to_os_string())
}

/// Resolve a bare Windows command name against a PATH directory list and PATHEXT,
/// mirroring the shell lookup the OS spawner skips.
///
/// Returns `Some(full_path)` for the first `dir\name+ext` the `exists` predicate
/// accepts, trying extensions in PATHEXT order so a native `.exe` wins over a `.cmd`
/// shim. Returns `None` when `program` is already concrete (absolute, has a directory
/// component, or already carries an extension) or when nothing matches, so the caller
/// leaves it unchanged.
///
/// Split out and pure over its inputs (the path list, the PATHEXT string, and a
/// file-exists predicate) so the resolution decision, the crux of the Windows launch
/// fix, is unit-tested without mutating the process environment or touching the real
/// filesystem, the way the rest of this seam is exercised.
fn resolve_windows_executable(
    program: &OsStr,
    path_dirs: &[PathBuf],
    pathext: &str,
    exists: impl Fn(&Path) -> bool,
) -> Option<OsString> {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 || path.extension().is_some() {
        return None;
    }
    for dir in path_dirs {
        for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
            let mut name = program.to_os_string();
            name.push(ext);
            let candidate = dir.join(&name);
            if exists(&candidate) {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

/// A [`CommandRunner`] decorator that layers extra environment variables onto
/// every spec before the inner runner sees it.
///
/// Isolation env for native agent sessions is applied here so every backend
/// (and an agent trigger) picks it up without each `build_spec` copying the
/// overlay. Empty overlay is a no-op. Overlay keys win over the spec's own env
/// so a reviewer cannot relocate sessions out of the isolation directory.
#[derive(Debug, Clone)]
pub struct OverlayEnvRunner<R> {
    inner: R,
    overlay: BTreeMap<String, String>,
}

impl<R> OverlayEnvRunner<R> {
    /// Wrap `inner`, inserting `overlay` into every spec's env.
    #[must_use]
    pub fn new(inner: R, overlay: BTreeMap<String, String>) -> Self {
        Self { inner, overlay }
    }
}

impl<R: CommandRunner> CommandRunner for OverlayEnvRunner<R> {
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
        if self.overlay.is_empty() {
            return self.inner.run(spec).await;
        }
        let mut spec = spec.clone();
        for (key, value) in &self.overlay {
            spec.env.insert(key.clone(), value.clone());
        }
        self.inner.run(&spec).await
    }
}

/// Resolve the program path for a backend CLI, honoring an environment override.
///
/// Each backend has a default program name (e.g. `claude`) found on `PATH`; the
/// `override_env` variable lets a deployment or a test point at a specific binary
/// or a fake script instead.
#[must_use]
pub fn resolve_program(default: &str, override_env: &str) -> OsString {
    match std::env::var_os(override_env).filter(|v| !v.is_empty()) {
        Some(path) => path,
        None => OsString::from(default),
    }
}

/// Whether `program` resolves to an executable on `PATH` or as a direct path.
///
/// A path-like program (absolute, or carrying a directory component) must point at
/// an existing file; a bare name is searched on `PATH`, probing the Windows
/// executable extensions so a `.cmd`/`.bat` shim counts. Backends use this in their
/// real-subprocess tests to detect-and-skip when the agent CLI is absent, so a
/// machine without it installed does not spuriously fail.
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

    struct RecordingRunner {
        specs: std::sync::Mutex<Vec<CommandSpec>>,
    }

    impl CommandRunner for RecordingRunner {
        async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
            self.specs.lock().expect("specs").push(spec.clone());
            Ok(CommandOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn overlay_env_runner_inserts_keys_and_wins_over_spec() {
        let inner = RecordingRunner {
            specs: std::sync::Mutex::new(Vec::new()),
        };
        let overlay = BTreeMap::from([
            ("PI_CODING_AGENT_SESSION_DIR".into(), "/native/pi".into()),
            ("KEEP".into(), "overlay".into()),
        ]);
        let runner = OverlayEnvRunner::new(inner, overlay);
        let mut spec = CommandSpec::new("pi", ".");
        spec.env.insert("KEEP".into(), "spec".into());
        spec.env.insert("OTHER".into(), "spec".into());
        runner.run(&spec).await.expect("runs");
        let specs = runner.inner.specs.lock().expect("specs");
        assert_eq!(
            specs[0]
                .env
                .get("PI_CODING_AGENT_SESSION_DIR")
                .map(String::as_str),
            Some("/native/pi")
        );
        assert_eq!(
            specs[0].env.get("KEEP").map(String::as_str),
            Some("overlay")
        );
        assert_eq!(specs[0].env.get("OTHER").map(String::as_str), Some("spec"));
    }

    #[test]
    fn resolve_program_prefers_override() {
        // Safety: single-threaded test; no other thread reads the environment here.
        unsafe { std::env::set_var("BASTION_TEST_PROG_OVERRIDE", "/opt/fake/claude") };
        let resolved = resolve_program("claude", "BASTION_TEST_PROG_OVERRIDE");
        assert_eq!(resolved, OsString::from("/opt/fake/claude"));
        unsafe { std::env::remove_var("BASTION_TEST_PROG_OVERRIDE") };
    }

    #[test]
    fn resolve_program_falls_back_to_default() {
        unsafe { std::env::remove_var("BASTION_TEST_PROG_MISSING") };
        let resolved = resolve_program("claude", "BASTION_TEST_PROG_MISSING");
        assert_eq!(resolved, OsString::from("claude"));
    }

    #[test]
    fn program_available_rejects_a_missing_path_program() {
        assert!(!program_available("/no/such/bin/claude"));
    }

    #[test]
    fn program_available_rejects_a_missing_bare_program() {
        assert!(!program_available("definitely-not-a-real-program-xyz123"));
    }

    #[test]
    fn resolve_executable_leaves_path_like_programs_unchanged() {
        // A program with a directory component is already concrete on every
        // platform; resolution must not rewrite it.
        let p = OsString::from("some/dir/agent");
        assert_eq!(resolve_executable(&p), p);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_executable_finds_a_cmd_shim_on_windows() {
        // `cmd` has no `.exe` next to a bare name on PATH search done by the OS,
        // but our resolver mirrors PATHEXT and finds `cmd.exe`.
        let resolved = resolve_executable(OsStr::new("cmd"));
        let path = Path::new(&resolved);
        assert!(path.is_file(), "expected a concrete file, got {resolved:?}");
        assert_eq!(
            path.extension().map(|e| e.to_ascii_lowercase()),
            Some(OsString::from("exe"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_executable_is_a_noop_off_windows() {
        // `execvp` already searches PATH for a bare name, so we leave it alone.
        let p = OsString::from("sh");
        assert_eq!(resolve_executable(&p), p);
    }

    // -- Windows resolution decision (pure, no env or filesystem) --------------
    //
    // These run on every host: `resolve_windows_executable` is decoupled from the
    // process environment and the real filesystem, so the Windows launch decision
    // is asserted directly instead of depending on what happens to be installed.

    #[test]
    fn resolve_windows_executable_resolves_a_bare_name_to_a_cmd_shim() {
        // The regression at the heart of the Windows launch bug: an npm `codex` is a
        // `codex.cmd` shim with no `.exe`, and the OS spawner does not consult
        // PATHEXT, so a bare `codex` must be resolved to the full `.cmd` path here or
        // the spawn fails "program not found" and every Codex reviewer fails closed.
        let dir = PathBuf::from(r"C:\npm");
        let dirs = [dir.clone()];
        let only_cmd = |candidate: &Path| candidate == dir.join("codex.CMD");
        let resolved =
            resolve_windows_executable(OsStr::new("codex"), &dirs, ".COM;.EXE;.BAT;.CMD", only_cmd)
                .expect("resolves the bare name to the shim");
        assert_eq!(resolved, dir.join("codex.CMD").into_os_string());
    }

    #[test]
    fn resolve_windows_executable_prefers_a_native_exe_over_a_cmd_shim() {
        // PATHEXT order decides, so a real `.exe` wins over a `.cmd` shim in the same
        // directory: we route through cmd.exe only when there is no native binary.
        let dir = PathBuf::from(r"C:\bin");
        let dirs = [dir.clone()];
        let both = |candidate: &Path| {
            candidate == dir.join("agent.EXE") || candidate == dir.join("agent.CMD")
        };
        let resolved =
            resolve_windows_executable(OsStr::new("agent"), &dirs, ".COM;.EXE;.BAT;.CMD", both)
                .expect("resolves");
        assert_eq!(resolved, dir.join("agent.EXE").into_os_string());
    }

    #[test]
    fn resolve_windows_executable_leaves_concrete_programs_unchanged() {
        let dirs = [PathBuf::from(r"C:\bin")];
        // Already carries an extension: concrete, so no resolution.
        assert!(
            resolve_windows_executable(OsStr::new("codex.cmd"), &dirs, ".CMD", |_| true).is_none()
        );
        // Has a directory component: concrete.
        assert!(
            resolve_windows_executable(OsStr::new("sub/codex"), &dirs, ".CMD", |_| true).is_none()
        );
    }

    #[test]
    fn resolve_windows_executable_returns_none_when_nothing_matches() {
        let dirs = [PathBuf::from(r"C:\bin")];
        assert!(
            resolve_windows_executable(OsStr::new("nope"), &dirs, ".EXE;.CMD", |_| false).is_none()
        );
    }

    // -- Real-subprocess `.cmd` execution --------------------------------------

    #[cfg(windows)]
    #[tokio::test]
    async fn system_runner_executes_a_cmd_shim_directly() {
        // The believed-impossible case ("a `.cmd` shim can't be spawned directly by a
        // native binary"): a resolved `codex.cmd` is handed to the runner as a
        // concrete path. `std` routes a `.cmd`/`.bat` through cmd.exe with correct
        // argument escaping, so it does run. Drive a real `.cmd` end-to-end, with a
        // prompt on stdin exactly as the backends do, to guard against a regression
        // that would make every Windows Codex reviewer fail to launch.
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("verdict-shim.cmd");
        std::fs::write(&shim, "@echo off\r\necho hello-from-cmd-shim\r\n").unwrap();

        let mut spec = CommandSpec::new(&shim, tmp.path());
        spec.stdin("a prompt the shim ignores");

        let output = SystemCommandRunner.run(&spec).await.expect("cmd shim runs");
        assert!(
            output.success(),
            "code={:?} stderr={:?}",
            output.code,
            output.stderr
        );
        assert!(
            output.stdout.contains("hello-from-cmd-shim"),
            "stdout={:?}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn stdin_is_piped_to_the_child() {
        // A program that echoes stdin verbatim, present on every platform: `cat`
        // off Windows, `sort` (which reads stdin when given no file) on Windows.
        let program = if cfg!(windows) { "sort" } else { "cat" };
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = CommandSpec::new(program, tmp.path());
        spec.stdin("hello-from-stdin");

        let output = SystemCommandRunner.run(&spec).await.expect("runs");
        assert!(
            output.stdout.contains("hello-from-stdin"),
            "stdout was {:?}",
            output.stdout
        );
    }
}
