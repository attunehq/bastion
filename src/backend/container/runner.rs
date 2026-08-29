use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result, bail};

use super::plan::ImageReference;
use super::teardown::{ContainerGuard, unique_container_name};
use super::{ContainerEngine, WORKDIR};
use crate::backend::command::{CommandOutput, CommandRunner, CommandSpec, resolve_executable};

/// A [`CommandRunner`] decorator that runs each spec inside a container.
///
/// It rewrites the backend's [`CommandSpec`] into a `docker run` invocation: the
/// checkout is bind-mounted at `/workspace` and made the working directory, and stdin
/// flows through `docker run -i`. The reviewer's declared env is forwarded through a
/// `--env-file` (see `write_env_file`), and the configured provider-credential
/// variable names are forwarded by name (`-e NAME`, the engine reading each value
/// from Bastion's own environment). Neither path puts a value in the `docker run`
/// argv, where a process listing or CI log could leak a secret, and neither sets a
/// reviewer key on the engine *client* process: an env-file injects only into the
/// container, so a reviewer `DOCKER_HOST` / `HTTP_PROXY` cannot reconfigure the engine
/// client and split the build, run, and teardown across daemons. The program and args
/// run inside the container verbatim.
#[derive(Debug, Clone)]
pub struct ContainerRunner<R> {
    inner: R,
    engine: ContainerEngine,
    image: ImageReference,
    credentials: Vec<String>,
    /// Host directory bind-mounted onto the same path inside the container so
    /// isolation env vars that name a host path still resolve.
    session_mount: Option<PathBuf>,
}

impl<R> ContainerRunner<R> {
    /// Build a container runner over `inner` for `image`, forwarding `credentials`
    /// (variable names present in Bastion's environment) into the container. Requiring
    /// an [`ImageReference`] (minted only by [`ContainerPlan::ensure_image`](super::ContainerPlan::ensure_image))
    /// keeps an arbitrary or option-like string out of the `docker run` image slot.
    #[must_use]
    pub fn new(
        inner: R,
        engine: ContainerEngine,
        image: ImageReference,
        credentials: Vec<String>,
    ) -> Self {
        Self {
            inner,
            engine,
            image,
            credentials,
            session_mount: None,
        }
    }

    /// Bind-mount `host` onto the same path inside the container.
    #[must_use]
    pub fn with_session_mount(mut self, host: PathBuf) -> Self {
        self.session_mount = Some(host);
        self
    }

    /// Rewrite a backend spec into the `docker run` spec that executes it in the
    /// container, naming the container `name` so it can be torn down on cancellation
    /// and forwarding the reviewer's env from `env_file` (when one was written).
    fn wrap(&self, spec: &CommandSpec, name: &str, env_file: Option<&Path>) -> CommandSpec {
        let mut docker = CommandSpec::new(self.engine.program().to_os_string(), spec.cwd.clone());
        docker.arg("run").arg("--rm").arg("--name").arg(name);
        if spec.stdin.is_some() {
            docker.arg("-i");
        }
        // Bind-mount the checkout and run there.
        let mount = format!("{}:{WORKDIR}", spec.cwd.display());
        docker.arg("-v").arg(mount).arg("-w").arg(WORKDIR);
        if let Some(host) = &self.session_mount {
            let mount = format!("{}:{}", host.display(), host.display());
            docker.arg("-v").arg(mount);
        }
        // Forward the reviewer's declared env through an env-file: the engine injects
        // those pairs into the container without them entering the argv (no secret in
        // a process listing) or the engine *client* process (a reviewer `DOCKER_HOST`
        // / `HTTP_PROXY` must not retarget the client and split the build/run/teardown
        // across daemons). See `write_env_file`.
        if let Some(path) = env_file {
            docker
                .arg("--env-file")
                .arg(path.as_os_str().to_os_string());
        }
        // Provider credentials are a fixed allowlist of provider variables, none of
        // them engine-client settings, so passing them by name is safe: the engine
        // reads each value from Bastion's own environment (which it inherits) and
        // Bastion never handles the secret value. Skip any credential the reviewer
        // also set in its own `env`: the engine gives an explicit `-e NAME` precedence
        // over an `--env-file` entry, so forwarding the host value here would override
        // the reviewer's declared one and silently swap which credential (and billing
        // account) the container uses. The native path lets reviewer env override the
        // parent, so dropping the passthrough here keeps the two surfaces matched.
        for name in &self.credentials {
            if spec.env.contains_key(name) {
                continue;
            }
            docker.arg("-e").arg(name.as_str());
        }
        // Run the backend program as the container's entrypoint. A bare
        // `docker run <image> <program>` only overrides the image's CMD, not its
        // ENTRYPOINT: an image that declares an ENTRYPOINT would run *that* with
        // `claude`/`codex` as mere arguments, so Bastion might never execute the
        // selected backend, and the image's entrypoint would receive the forwarded
        // credentials and the mounted checkout. Setting `--entrypoint` to the program
        // makes the backend the process the container runs, whatever the image
        // declares; the program resolves on the container's `PATH`.
        docker.arg("--entrypoint").arg(spec.program.clone());
        docker.arg(self.image.as_str());
        // Everything after the image is an argument to the entrypoint (the backend),
        // exactly as the backend built it.
        for arg in &spec.args {
            docker.arg(arg.clone());
        }
        if let Some(stdin) = &spec.stdin {
            docker.stdin(stdin.clone());
        }
        docker
    }
}

impl<R: CommandRunner> CommandRunner for ContainerRunner<R> {
    // Each call is its own `docker run --rm` container with no persisted home, so a
    // backend's reprompt-on-malformed-output retry (`--resume` / `exec resume`) cannot
    // recover first-turn session state across the two turns: a containerized reviewer
    // with a malformed first turn fails closed rather than recovering. That is safe (a
    // gate never launders a pass), just less forgiving; persisting a shared agent home
    // is a deliberate later pass (see docs/developer-guide/containers.md).
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
        // Name the container so the teardown guard (armed below) can force-remove it if
        // this future is cancelled.
        let name = unique_container_name();
        // The env-file is held for the whole run (so the engine can read it) and
        // dropped at the end, which deletes it, including on cancellation. Build it
        // before arming the guard: this validation can fail (an env value the env-file
        // format cannot represent), and no container has started yet, so a guard armed
        // earlier would `docker rm -f` a name that never ran on a purely local error.
        let env_file = write_env_file(&spec.env)?;
        // Arm the teardown guard now that the run is about to launch. `docker run --rm`
        // removes the container only on a clean exit; when the runner times a reviewer
        // out it drops this future, and the inner runner's `kill_on_drop` kills the
        // engine *client* but not the container running in the daemon. The guard's drop
        // force-removes the named container, so a hung in-container agent cannot keep
        // using tools or tokens after Bastion has already resolved the reviewer closed
        // (the same guarantee the native path gets from `kill_on_drop`). On a normal
        // return we defuse it: `--rm` has already removed the container. The engine
        // program is resolved the same way `SystemCommandRunner` resolves it for the
        // build and run (Windows `PATHEXT` lookup for a bare name or a `.cmd` shim), so
        // a bare engine name that builds and runs can also be spawned for teardown; the
        // guard's `Drop` spawns it directly, outside that runner.
        let guard = ContainerGuard::new(resolve_executable(self.engine.program()), name.clone());
        let docker = self.wrap(
            spec,
            &name,
            env_file.as_ref().map(tempfile::NamedTempFile::path),
        );
        let output = self.inner.run(&docker).await?;
        guard.defuse();
        Ok(output)
    }
}

/// Write the reviewer's env to a temp file in `docker run --env-file` format
/// (`KEY=VALUE` lines), or `None` when there is no env.
///
/// Forwarding reviewer env this way keeps arbitrary reviewer keys out of both the
/// `docker run` argv (no secret in a process listing) and the engine *client*
/// process's own environment (so a reviewer `DOCKER_HOST`, `HTTP_PROXY`, etc. cannot
/// retarget the client away from the daemon the build and teardown use). The file is
/// returned so the caller can keep it alive across the run; its drop deletes it.
///
/// The env-file format is line-oriented (`KEY=VALUE` per line) with no quoting or
/// escaping, so it cannot represent a key or value containing a newline or a key
/// containing `=`: the engine would silently split such a pair into extra lines,
/// handing the container a truncated value and stray variables the reviewer never
/// declared, diverging from the native path (`Command::env`, which preserves any
/// bytes). Rather than corrupt the value, this fails closed at the boundary, naming
/// the offending key. A multiline secret in a containerized reviewer's `env` is not
/// representable today; pass it some other way (a mounted file, an image-baked value).
///
/// # Errors
///
/// Returns an error if a key or value is not env-file representable, or if the temp
/// file cannot be created or written.
fn write_env_file(env: &BTreeMap<String, String>) -> Result<Option<tempfile::NamedTempFile>> {
    if env.is_empty() {
        return Ok(None);
    }
    let mut file = tempfile::Builder::new()
        .prefix("bastion-env-")
        .tempfile()
        .wrap_err("creating the container env file")?;
    for (key, value) in env {
        if key.contains(['\n', '\r']) || key.contains('=') {
            bail!(
                "reviewer env key `{key}` cannot be forwarded into a container: \
                 a `docker run --env-file` key cannot contain a newline or `=`"
            );
        }
        if value.contains(['\n', '\r']) {
            bail!(
                "reviewer env value for `{key}` cannot be forwarded into a container: \
                 a `docker run --env-file` value cannot contain a newline"
            );
        }
        // `--env-file` parses one `KEY=VALUE` per line; values are taken literally.
        writeln!(file, "{key}={value}").wrap_err("writing the container env file")?;
    }
    file.flush().wrap_err("flushing the container env file")?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::container::testutil::*;
    use std::ffi::OsString;

    #[tokio::test]
    async fn wrap_translates_a_spec_into_a_docker_run() {
        let runner = ContainerRunner::new(
            RecordingRunner::default(),
            ContainerEngine::with_program("docker"),
            ImageReference::for_test("bastion-reviewer:abc"),
            vec!["ANTHROPIC_API_KEY".into()],
        );
        let mut spec = CommandSpec::new("claude", "/repo");
        spec.arg("-p").arg("review this");
        spec.env.insert("PREVIEW_URL".into(), "http://x".into());
        spec.stdin("the prompt");

        runner.run(&spec).await.unwrap();
        let docker = runner.inner.last();
        assert_eq!(docker.program, OsString::from("docker"));
        let args = args_of(&docker);
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_string()));
        // stdin present -> interactive flag, and stdin forwarded.
        assert!(args.contains(&"-i".to_string()));
        assert_eq!(docker.stdin.as_deref(), Some("the prompt"));
        // Mount and workdir.
        assert!(args.iter().any(|a| a.ends_with(":/workspace")));
        assert!(args.contains(&"-w".to_string()));
        assert!(args.contains(&"/workspace".to_string()));
        // Reviewer env is forwarded via `--env-file`: neither the key nor the value
        // appears in the argv, and (crucially) the key is not set on the engine
        // *client* process env, so a reviewer var cannot reconfigure the engine.
        assert!(args.iter().any(|a| a == "--env-file"));
        assert!(
            !args
                .iter()
                .any(|a| a == "PREVIEW_URL" || a.contains("http://x")),
            "reviewer env leaked into argv: {args:?}"
        );
        assert!(
            docker.env.is_empty(),
            "reviewer env must not touch the engine client process env: {:?}",
            docker.env
        );
        // The provider credential is forwarded by name only (value read from the
        // engine's inherited env), never as a value in the argv.
        assert!(args.iter().any(|a| a == "ANTHROPIC_API_KEY"));
        // The container is named (before the image) so it can be torn down on
        // cancellation.
        let name_at = args
            .iter()
            .position(|a| a == "--name")
            .expect("a --name flag");
        assert!(args[name_at + 1].starts_with("bastion-reviewer-"));
        // The backend program is the container entrypoint, so an image ENTRYPOINT
        // cannot hijack the run or intercept the forwarded credentials. The
        // `--entrypoint <program>` pair is a `docker run` option, so it precedes the
        // image; the image then precedes the program's args.
        let entrypoint_at = args
            .iter()
            .position(|a| a == "--entrypoint")
            .expect("an --entrypoint flag");
        assert_eq!(args[entrypoint_at + 1], "claude");
        let image_at = args
            .iter()
            .position(|a| a == "bastion-reviewer:abc")
            .unwrap();
        let prompt_at = args.iter().position(|a| a == "review this").unwrap();
        assert!(name_at < image_at);
        assert!(entrypoint_at < image_at);
        assert!(image_at < prompt_at);
        // The bare program is not repeated after the image (it rides `--entrypoint`).
        assert!(args[image_at + 1..].iter().all(|a| a != "claude"));
    }

    #[tokio::test]
    async fn wrap_omits_the_interactive_flag_without_stdin() {
        let runner = ContainerRunner::new(
            RecordingRunner::default(),
            ContainerEngine::with_program("docker"),
            ImageReference::for_test("img"),
            Vec::new(),
        );
        let mut spec = CommandSpec::new("claude", "/repo");
        spec.arg("--version");
        runner.run(&spec).await.unwrap();
        let args = args_of(&runner.inner.last());
        assert!(!args.contains(&"-i".to_string()));
        // No reviewer env -> no env-file.
        assert!(!args.contains(&"--env-file".to_string()));
    }

    #[test]
    fn write_env_file_emits_key_value_lines() {
        // No env -> no file at all (so `wrap` adds no `--env-file`).
        assert!(write_env_file(&BTreeMap::new()).unwrap().is_none());

        let mut env = BTreeMap::new();
        env.insert("PREVIEW_URL".to_string(), "http://x".to_string());
        env.insert("DOCKER_HOST".to_string(), "tcp://evil".to_string());
        let file = write_env_file(&env)
            .unwrap()
            .expect("a file for non-empty env");
        let body = std::fs::read_to_string(file.path()).unwrap();
        // One `KEY=VALUE` line per pair: these reach the container, not the engine
        // client process, so even an engine-client variable like `DOCKER_HOST` only
        // sets the container's env.
        assert!(body.lines().any(|l| l == "PREVIEW_URL=http://x"));
        assert!(body.lines().any(|l| l == "DOCKER_HOST=tcp://evil"));
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn write_env_file_fails_closed_on_unrepresentable_pairs() {
        // The env-file format is one `KEY=VALUE` per line with no escaping, so a
        // newline in a value would split it into extra lines (a truncated value plus
        // an injected variable). Rather than corrupt the value, we fail closed.
        let mut value_with_newline = BTreeMap::new();
        value_with_newline.insert("TOKEN".to_string(), "line1\nINJECTED=evil".to_string());
        let err = write_env_file(&value_with_newline).unwrap_err();
        assert!(err.to_string().contains("TOKEN"), "{err}");
        assert!(err.to_string().contains("newline"), "{err}");

        // A key with a newline or an `=` is likewise not representable.
        let mut key_with_newline = BTreeMap::new();
        key_with_newline.insert("BAD\nKEY".to_string(), "v".to_string());
        assert!(write_env_file(&key_with_newline).is_err());
        let mut key_with_equals = BTreeMap::new();
        key_with_equals.insert("BAD=KEY".to_string(), "v".to_string());
        assert!(write_env_file(&key_with_equals).is_err());
    }

    #[tokio::test]
    async fn reviewer_env_overrides_a_credential_passthrough() {
        // A reviewer that sets a provider credential in its own `env` must win over the
        // host passthrough: the engine gives an explicit `-e NAME` precedence over an
        // `--env-file` entry, so we must drop the `-e NAME` for a credential the
        // reviewer declared. Otherwise the host value would override the reviewer's,
        // diverging from native (where reviewer env overrides the parent).
        let runner = ContainerRunner::new(
            RecordingRunner::default(),
            ContainerEngine::with_program("docker"),
            ImageReference::for_test("img"),
            vec!["ANTHROPIC_API_KEY".into(), "OPENAI_API_KEY".into()],
        );
        let mut spec = CommandSpec::new("claude", "/repo");
        spec.env
            .insert("OPENAI_API_KEY".into(), "reviewer-declared".into());
        runner.run(&spec).await.unwrap();
        let args = args_of(&runner.inner.last());
        // The reviewer-declared credential is not also forwarded by name (its value
        // crosses via the env-file instead), but the undeclared one still is.
        let forwarded: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, a)| *a == "-e" && i + 1 < args.len())
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert!(
            forwarded.iter().any(|a| *a == "ANTHROPIC_API_KEY"),
            "an undeclared credential is still forwarded: {forwarded:?}"
        );
        assert!(
            !forwarded.iter().any(|a| *a == "OPENAI_API_KEY"),
            "a reviewer-declared credential must not be overridden by the host value: {forwarded:?}"
        );
        // It does cross via the env-file (so the reviewer's value reaches the container).
        assert!(args.iter().any(|a| a == "--env-file"));
    }
}
