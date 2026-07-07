//! The repo + data-directory fixtures, the reviewer-registry builder, the binary
//! under test, and the run-event (JSONL) parsing helpers.
//!
//! Each scenario stands up an isolated [`TestRepo`] (a throwaway `git` repo with a
//! committed base and a dirty working tree, plus a private `BASTION_DATA_DIR`),
//! writes a registry built from [`Reviewer`] values, drives the real binary, and
//! reads back the result as typed [`ReviewRun`] events.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use bastion::event::{Gates, RunEvent, RunId};
use bastion::paths::Layout;
use bastion::verdict::{Decision, Finding, Money, Usage};

/// The `GITHUB_API_URL`/`GITHUB_TOKEN` env pair every CI-path scenario points at
/// its in-process fake GitHub. Returned as an owned array borrowing `url`, so a
/// call site spreads `&ci_env(&github.url)` straight into a `review_ci` env
/// argument.
pub(crate) fn ci_env(url: &str) -> [(&str, &str); 2] {
    [("GITHUB_API_URL", url), ("GITHUB_TOKEN", "ghs-fake-token")]
}

/// `git` settings that make a throwaway repo deterministic regardless of the
/// developer's global configuration (identity, signing, default branch).
const GIT_ISOLATE: &[&str] = &[
    "-c",
    "user.email=test@bastion.dev",
    "-c",
    "user.name=Bastion Test",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "init.defaultBranch=main",
];

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(GIT_ISOLATE)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("git {args:?} failed to launch: {e}"));
    assert!(status.success(), "git {args:?} exited unsuccessfully");
    // The `-c` isolation only covers commands issued through this helper. The
    // compiled binary under test runs plain `git` in the same repo (writing an
    // attestation note, say) and needs an identity from config on a host that
    // has none (CI), so persist one repo-locally at init.
    if args.first() == Some(&"init") {
        git(dir, &["config", "user.email", "grace@bastion.dev"]);
        git(dir, &["config", "user.name", "Grace Hopper"]);
    }
}

// ---------------------------------------------------------------------------
// Building a reviewer registry.
// ---------------------------------------------------------------------------

/// A reviewer to write into a scenario's `.bastion.yaml`. All reviewers trigger
/// on `src/**/*.rs`, which the test repo always dirties, so each one runs.
pub(crate) struct Reviewer {
    pub(crate) name: &'static str,
    backend: &'static str,
    mode: &'static str,
    /// `FAKE_BEHAVIOR` and any other env the fake (or test) wants propagated.
    env: Vec<(&'static str, &'static str)>,
    /// `${name}` inputs interpolated into the prompt before the agent sees it.
    inputs: Vec<(&'static str, &'static str)>,
    /// A human-form `timeout` (e.g. `"500ms"`), when the reviewer needs one.
    timeout: Option<&'static str>,
    /// A `runner.dockerfile` path (relative to the repo), when containerized.
    runner_dockerfile: Option<&'static str>,
    /// A `runner.image` reference, when containerized off a prebuilt image.
    runner_image: Option<&'static str>,
    /// `capabilities.network: true`, required for a containerized reviewer to run
    /// (a container with the default `network: false` fails closed).
    network: bool,
    /// A pinned `model`, when the reviewer selects one explicitly.
    model: Option<&'static str>,
    /// A pinned `effort`, when the reviewer selects one explicitly.
    effort: Option<&'static str>,
    /// The prompt body (defaults to a generic instruction).
    prompt: Option<&'static str>,
    /// `attestation: never`, opting this reviewer out of attestation replay.
    attestation_never: bool,
    /// The trigger glob (defaults to `src/**/*.rs`, which the test repo always
    /// dirties).
    trigger: &'static str,
}

impl Reviewer {
    pub(crate) fn new(name: &'static str, backend: &'static str, mode: &'static str) -> Self {
        Self {
            name,
            backend,
            mode,
            env: Vec::new(),
            inputs: Vec::new(),
            timeout: None,
            runner_dockerfile: None,
            runner_image: None,
            network: false,
            model: None,
            effort: None,
            prompt: None,
            attestation_never: false,
            trigger: "src/**/*.rs",
        }
    }

    /// Route this reviewer on `trigger` instead of the default `src/**/*.rs`,
    /// for scenarios that need reviewers scoped to disjoint parts of the tree
    /// (the incremental-review carry scenarios, say).
    pub(crate) fn trigger(mut self, trigger: &'static str) -> Self {
        self.trigger = trigger;
        self
    }

    /// Pin the reviewer's `model`.
    pub(crate) fn model(mut self, model: &'static str) -> Self {
        self.model = Some(model);
        self
    }

    /// Pin the reviewer's `effort`.
    pub(crate) fn effort(mut self, effort: &'static str) -> Self {
        self.effort = Some(effort);
        self
    }

    pub(crate) fn behavior(mut self, behavior: &'static str) -> Self {
        self.env.push(("FAKE_BEHAVIOR", behavior));
        self
    }

    pub(crate) fn env(mut self, key: &'static str, value: &'static str) -> Self {
        self.env.push((key, value));
        self
    }

    pub(crate) fn input(mut self, key: &'static str, value: &'static str) -> Self {
        self.inputs.push((key, value));
        self
    }

    pub(crate) fn prompt(mut self, prompt: &'static str) -> Self {
        self.prompt = Some(prompt);
        self
    }

    pub(crate) fn timeout(mut self, timeout: &'static str) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Run this reviewer in a container built from `dockerfile` (relative to the
    /// repo root).
    pub(crate) fn dockerfile(mut self, dockerfile: &'static str) -> Self {
        self.runner_dockerfile = Some(dockerfile);
        self
    }

    /// Run this reviewer in a container off the prebuilt `image` reference.
    pub(crate) fn image(mut self, image: &'static str) -> Self {
        self.runner_image = Some(image);
        self
    }

    /// Opt into `capabilities.network: true`. A containerized reviewer must set this
    /// to run (a container with the default `network: false` fails closed).
    pub(crate) fn network(mut self) -> Self {
        self.network = true;
        self
    }

    /// Opt this reviewer out of attestation replay (`attestation: never`): CI must
    /// execute it fresh on every run regardless of any verified local attestation.
    pub(crate) fn attestation_never(mut self) -> Self {
        self.attestation_never = true;
        self
    }

    fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("  - name: {}\n", self.name));
        s.push_str(&format!("    trigger: [{}]\n", self.trigger));
        s.push_str(&format!("    mode: {}\n", self.mode));
        s.push_str(&format!("    backend: {}\n", self.backend));
        if let Some(model) = self.model {
            s.push_str(&format!("    model: {model}\n"));
        }
        if let Some(effort) = self.effort {
            s.push_str(&format!("    effort: {effort}\n"));
        }
        if let Some(timeout) = self.timeout {
            s.push_str(&format!("    timeout: {timeout}\n"));
        }
        if self.runner_dockerfile.is_some() || self.runner_image.is_some() {
            s.push_str("    runner:\n");
            if let Some(dockerfile) = self.runner_dockerfile {
                s.push_str(&format!("      dockerfile: {dockerfile}\n"));
            }
            if let Some(image) = self.runner_image {
                s.push_str(&format!("      image: {image}\n"));
            }
        }
        if self.network {
            s.push_str("    capabilities:\n      network: true\n");
        }
        if self.attestation_never {
            s.push_str("    attestation: never\n");
        }
        if !self.env.is_empty() {
            s.push_str("    env:\n");
            for (key, value) in &self.env {
                // Single-quote so values with path separators or spaces stay literal.
                s.push_str(&format!("      {key}: '{value}'\n"));
            }
        }
        if !self.inputs.is_empty() {
            s.push_str("    inputs:\n");
            for (key, value) in &self.inputs {
                s.push_str(&format!("      {key}: '{value}'\n"));
            }
        }
        let prompt = self.prompt.unwrap_or("review the changeset");
        s.push_str(&format!("    prompt: '{prompt}'\n"));
        s
    }
}

pub(crate) fn registry(reviewers: &[Reviewer]) -> String {
    let mut yaml = String::from("reviewers:\n");
    for reviewer in reviewers {
        yaml.push_str(&reviewer.to_yaml());
    }
    yaml
}

/// A registry with a top-level `defaults:` block. Each `(key, value)` becomes one
/// line under `defaults:` (for example `("model", "gpt-5")`, `("effort", "high")`),
/// so a scenario can prove the block resolves into reviewers through the real load
/// path.
pub(crate) fn registry_with_defaults(defaults: &[(&str, &str)], reviewers: &[Reviewer]) -> String {
    let mut yaml = String::from("defaults:\n");
    for (key, value) in defaults {
        yaml.push_str(&format!("  {key}: {value}\n"));
    }
    yaml.push_str("reviewers:\n");
    for reviewer in reviewers {
        yaml.push_str(&reviewer.to_yaml());
    }
    yaml
}

/// A registry with the top-level `attestations: true` switch set, so a CI-path
/// review looks up an attestation note instead of ignoring the feature entirely.
/// `attestations` is a sibling of `reviewers:`, not a `defaults:` sub-key, so it
/// needs its own builder.
pub(crate) fn registry_with_attestations(reviewers: &[Reviewer]) -> String {
    let mut yaml = String::from("attestations: true\nreviewers:\n");
    for reviewer in reviewers {
        yaml.push_str(&reviewer.to_yaml());
    }
    yaml
}

// ---------------------------------------------------------------------------
// An isolated repo + data directory, and the binary under test.
// ---------------------------------------------------------------------------

/// The compiled `bastion` binary Cargo built for this test.
fn bastion_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bastion")
}

/// One throwaway environment: a git repo with a committed base and a dirty working
/// tree, plus a private data directory for run history.
pub(crate) struct TestRepo {
    repo: tempfile::TempDir,
    data: tempfile::TempDir,
    /// A private user-level config directory, isolated from the developer's real
    /// `~/.config/bastion` so a personal registry cannot leak into a scenario.
    /// Empty unless a test writes a user-level registry into it.
    config: tempfile::TempDir,
}

/// Where (and whether) to write a scenario repo's reviewer registry.
enum Registry<'a> {
    /// No registry at all (the discovery error path).
    None,
    /// A `.bastion.yaml` at the repository root (the supported location).
    Root(&'a str),
    /// A `bastion/reviewers.yaml` (the deprecated back-compat location).
    Legacy(&'a str),
}

impl TestRepo {
    /// Stand up a repo whose `.bastion.yaml` is `registry_yaml`, with a
    /// committed `src/lib.rs` base and an uncommitted changeset on top (an edit
    /// plus a new file), so a `review --base main` always has files to route.
    pub(crate) fn new(registry_yaml: &str) -> Self {
        Self::build(Registry::Root(registry_yaml))
    }

    /// A repo with no reviewer registry at all (for the discovery error path).
    pub(crate) fn without_registry() -> Self {
        Self::build(Registry::None)
    }

    /// Stand up a repo whose registry lives at the deprecated
    /// `bastion/reviewers.yaml` location, to exercise the back-compat shim.
    pub(crate) fn new_legacy(registry_yaml: &str) -> Self {
        Self::build(Registry::Legacy(registry_yaml))
    }

    fn build(registry: Registry) -> Self {
        let repo = tempfile::tempdir().expect("repo tempdir");
        let dir = repo.path();

        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn base() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "scenario repo\n").unwrap();
        match registry {
            Registry::None => {}
            Registry::Root(yaml) => {
                std::fs::write(dir.join(".bastion.yaml"), yaml).unwrap();
            }
            Registry::Legacy(yaml) => {
                std::fs::create_dir_all(dir.join("bastion")).unwrap();
                std::fs::write(dir.join("bastion/reviewers.yaml"), yaml).unwrap();
            }
        }

        git(dir, &["init"]);
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "base"]);

        // Dirty the tree: edit a tracked source file and add an untracked one. Both
        // match `src/**/*.rs`, so every reviewer in the registry triggers.
        Self::dirty(dir);

        let data = tempfile::tempdir().expect("data tempdir");
        let config = tempfile::tempdir().expect("config tempdir");
        Self { repo, data, config }
    }

    /// Write a user-level `.bastion.yaml` into this repo's isolated config dir, so a
    /// `review`/`validate` run layers its reviewers beneath the repository's. This is
    /// the local-only path: a reviewer a user keeps in their config dir runs even when
    /// the repository does not adopt Bastion in CI.
    pub(crate) fn with_user_registry(self, yaml: &str) -> Self {
        std::fs::write(self.config.path().join(".bastion.yaml"), yaml).unwrap();
        self
    }

    /// Re-dirty the working tree (used between runs to keep a changeset present).
    fn dirty(dir: &Path) {
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn base() {}\npub fn added() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    }

    pub(crate) fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Commit the current working tree, advancing HEAD (and the run id).
    pub(crate) fn commit_all(&self, message: &str) {
        git(self.path(), &["add", "."]);
        git(self.path(), &["commit", "-m", message]);
    }

    /// Create a branch at the current HEAD, as a stable base marker for a
    /// scenario that commits the dirtied tree and then reviews the committed
    /// changeset against where it started.
    pub(crate) fn branch(&self, name: &str) {
        git(self.path(), &["branch", name]);
    }

    /// Run `bastion <args>` in this repo with the fake agent wired in for both
    /// backends, this repo's private data directory, and any `extra_env` (which
    /// Bastion inherits and propagates to the agent child).
    pub(crate) fn run(&self, fake: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(bastion_bin());
        command
            .args(args)
            .current_dir(self.repo.path())
            .env("BASTION_DATA_DIR", self.data.path())
            // Point the user-level config dir at this repo's isolated (empty by
            // default) directory, so the developer's real `~/.config/bastion` never
            // leaks reviewers into a scenario.
            .env("BASTION_CONFIG_DIR", self.config.path())
            .env("BASTION_CLAUDE_BIN", fake)
            .env("BASTION_CODEX_BIN", fake)
            .env("BASTION_PI_BIN", fake)
            // Keep stdout pure JSONL; route library logging away from the stream.
            .env("RUST_LOG", "error")
            .stdin(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("bastion binary runs")
    }

    /// Run `bastion review --base <base> --format jsonl` and parse the stream.
    pub(crate) fn review_base(
        &self,
        fake: &Path,
        base: &str,
        extra_env: &[(&str, &str)],
    ) -> ReviewRun {
        let output = self.run(
            fake,
            &["review", "--base", base, "--format", "jsonl"],
            extra_env,
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = parse_events(&stdout, &stderr);
        ReviewRun {
            code: output.status.code(),
            events,
            stderr,
        }
    }

    pub(crate) fn review(&self, fake: &Path) -> ReviewRun {
        self.review_base(fake, "main", &[])
    }

    /// Run `bastion review --base main --format jsonl` with extra CLI arguments
    /// (`--include`, say) and parse the stream.
    pub(crate) fn review_with_args(&self, fake: &Path, extra_args: &[&str]) -> ReviewRun {
        let mut args = vec!["review", "--base", "main", "--format", "jsonl"];
        args.extend_from_slice(extra_args);
        let output = self.run(fake, &args, &[]);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = parse_events(&stdout, &stderr);
        ReviewRun {
            code: output.status.code(),
            events,
            stderr,
        }
    }

    /// Run `bastion review --base <base> --repo <repo> --pr <pr> --format jsonl`
    /// against a GitHub source (the CI path), with any `extra_env` (typically the
    /// fake GitHub's `GITHUB_API_URL`/`GITHUB_TOKEN`).
    pub(crate) fn review_ci(
        &self,
        fake: &Path,
        base: &str,
        repo: &str,
        pr: &str,
        extra_env: &[(&str, &str)],
    ) -> ReviewRun {
        let output = self.run(
            fake,
            &[
                "review", "--base", base, "--repo", repo, "--pr", pr, "--format", "jsonl",
            ],
            extra_env,
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = parse_events(&stdout, &stderr);
        ReviewRun {
            code: output.status.code(),
            events,
            stderr,
        }
    }

    /// Run `bastion review` on the CI path (`--repo`/`--pr`) with the codex backend
    /// resolved as `codex` on `PATH` and *no* `BASTION_*_BIN` override, so the run
    /// seals `seams: false` and is carry-eligible.
    ///
    /// The ordinary [`run`](Self::run) helper points every backend at the fake agent
    /// through the `BASTION_*_BIN` overrides, which every scenario wants but which
    /// also trips the seal's test-seam flag, so a seam-tainted run can never be
    /// carried (see the incremental-review scenarios). This mirrors the real dogfood
    /// workflow instead: the Codex CLI is on `PATH` and no override is set, which is
    /// the only way to exercise a clean, carry-eligible seal end to end. It copies the
    /// fake agent to a `codex` on a `PATH` directory and prepends that directory, so
    /// the codex backend resolves the fake by name with no override.
    pub(crate) fn review_ci_backend_on_path(
        &self,
        fake: &Path,
        base: &str,
        repo: &str,
        pr: &str,
        extra_env: &[(&str, &str)],
    ) -> ReviewRun {
        let bindir = tempfile::tempdir().expect("bin tempdir");
        let codex = bindir
            .path()
            .join(if cfg!(windows) { "codex.exe" } else { "codex" });
        std::fs::copy(fake, &codex).expect("copy the fake agent to a PATH `codex`");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))
                .expect("mark the PATH `codex` executable");
        }

        // Prepend our `codex` directory so it wins over any real codex on the host.
        let mut path = std::ffi::OsString::from(bindir.path());
        path.push(if cfg!(windows) { ";" } else { ":" });
        if let Some(existing) = std::env::var_os("PATH") {
            path.push(existing);
        }

        let mut command = Command::new(bastion_bin());
        command
            .args([
                "review", "--base", base, "--repo", repo, "--pr", pr, "--format", "jsonl",
            ])
            .current_dir(self.repo.path())
            .env("BASTION_DATA_DIR", self.data.path())
            .env("BASTION_CONFIG_DIR", self.config.path())
            // Resolve the codex backend from PATH rather than a BASTION_*_BIN
            // override, so `seams_active()` is false and the seal stays carry-eligible.
            // Clear any override the ambient environment might carry, so the seam-free
            // seal this helper promises does not depend on the developer's shell.
            .env_remove("BASTION_CLAUDE_BIN")
            .env_remove("BASTION_CODEX_BIN")
            .env_remove("BASTION_PI_BIN")
            .env("PATH", &path)
            .env("RUST_LOG", "error")
            .stdin(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let output = command.output().expect("bastion binary runs");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let events = parse_events(&stdout, &stderr);
        ReviewRun {
            code: output.status.code(),
            events,
            stderr,
        }
    }

    /// Write a garbage (non-bundle) note on HEAD under `refs/notes/bastion`, using
    /// this repo's own isolated git identity. Used to exercise the CI attestation
    /// path's refusal of an unreadable note.
    pub(crate) fn write_garbage_note(&self, text: &str) {
        git(
            self.path(),
            &["notes", "--ref=refs/notes/bastion", "add", "-m", text],
        );
    }

    /// A [`Layout`] over this repo's data directory, for asserting on what was
    /// persisted using the crate's real store API.
    pub(crate) fn layout(&self) -> Layout {
        Layout::with_root(self.data.path().to_path_buf())
    }

    /// The id of the most recent persisted run. `store::list_runs` is
    /// most-recent-first, so this is its head; panics if no run exists yet.
    pub(crate) fn latest_run_id(&self) -> RunId {
        bastion::store::list_runs(&self.layout()).unwrap()[0]
            .run
            .clone()
    }
}

/// Parse a JSONL event stream into typed [`RunEvent`]s, failing loudly (with the
/// process's stderr) if any line is not a valid event -- a stray write to stdout
/// would corrupt the contract this whole system depends on.
pub(crate) fn parse_events(stdout: &str, stderr: &str) -> Vec<RunEvent> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<RunEvent>(line).unwrap_or_else(|e| {
                panic!("stdout line is not a valid run event: {e}\nline: {line}\nstderr:\n{stderr}")
            })
        })
        .collect()
}

/// The dotted wire name of a run event, for asserting on stream ordering.
pub(crate) fn event_kind(event: &RunEvent) -> &'static str {
    match event {
        RunEvent::RunStarted { .. } => "run.started",
        RunEvent::ReviewerStarted { .. } => "reviewer.started",
        RunEvent::ReviewerResolved { .. } => "reviewer.resolved",
        RunEvent::RunCompleted { .. } => "run.completed",
        // `RunEvent` is `#[non_exhaustive]`; a new variant should surface here.
        _ => "unknown",
    }
}

/// The parsed result of one `bastion review`.
pub(crate) struct ReviewRun {
    pub(crate) code: Option<i32>,
    pub(crate) events: Vec<RunEvent>,
    pub(crate) stderr: String,
}

impl ReviewRun {
    pub(crate) fn exited_zero(&self) -> bool {
        self.code == Some(0)
    }

    /// The aggregate decision and gate tally from the closing `run.completed`.
    pub(crate) fn completed(&self) -> (Decision, Gates, Money) {
        for event in &self.events {
            if let RunEvent::RunCompleted {
                verdict,
                gates,
                cost_usd,
                ..
            } = event
            {
                return (*verdict, *gates, *cost_usd);
            }
        }
        panic!("no run.completed in stream; stderr:\n{}", self.stderr);
    }

    /// The aggregate token and cost totals from the closing `run.completed`:
    /// `(tokens_in, tokens_out, cache_read, cost)`.
    pub(crate) fn completed_usage(&self) -> (u64, u64, u64, Money) {
        for event in &self.events {
            if let RunEvent::RunCompleted {
                tokens_in,
                tokens_out,
                cache_read,
                cost_usd,
                ..
            } = event
            {
                return (*tokens_in, *tokens_out, *cache_read, *cost_usd);
            }
        }
        panic!("no run.completed in stream; stderr:\n{}", self.stderr);
    }

    /// The resolved verdict, summary, findings, and usage for one reviewer.
    pub(crate) fn resolved(&self, name: &str) -> (Decision, String, Vec<Finding>, Option<Usage>) {
        for event in &self.events {
            if let RunEvent::ReviewerResolved {
                reviewer,
                verdict,
                summary,
                findings,
                usage,
                ..
            } = event
                && reviewer == name
            {
                return (*verdict, summary.clone(), findings.clone(), *usage);
            }
        }
        panic!(
            "no reviewer.resolved for '{name}'; stderr:\n{}",
            self.stderr
        );
    }

    pub(crate) fn resolved_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerResolved { .. }))
            .count()
    }

    pub(crate) fn started_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, RunEvent::ReviewerStarted { .. }))
            .count()
    }

    /// Whether the closing `run.completed` (or the opening `run.started`) marked
    /// the run partial (`bastion review --reviewer`).
    pub(crate) fn partial(&self) -> bool {
        self.events.iter().any(|e| {
            matches!(
                e,
                RunEvent::RunStarted { partial: true, .. }
                    | RunEvent::RunCompleted { partial: true, .. }
            )
        })
    }

    /// Whether `name`'s resolved event was carried forward from the branch's
    /// previous run rather than executed fresh.
    pub(crate) fn carried(&self, name: &str) -> bool {
        for event in &self.events {
            if let RunEvent::ReviewerResolved {
                reviewer, carried, ..
            } = event
                && reviewer == name
            {
                return *carried;
            }
        }
        panic!(
            "no reviewer.resolved for '{name}'; stderr:\n{}",
            self.stderr
        );
    }

    /// The reason from the stream's `run.attestation-fallback` event, if one was
    /// emitted.
    pub(crate) fn attestation_fallback_reason(&self) -> Option<&str> {
        self.events.iter().find_map(|e| match e {
            RunEvent::AttestationFallback { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
    }
}

/// Generate a throwaway ed25519 keypair at `<dir>/id`, returning the private key
/// path. Used by the attestation scenarios to exercise `bastion attest --key`
/// without touching a developer's real SSH keys.
pub(crate) fn generate_ssh_key(dir: &Path) -> std::path::PathBuf {
    let key_path = dir.join("id");
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            &key_path.to_string_lossy(),
            "-C",
            "test@bastion.dev",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ssh-keygen is installed");
    assert!(status.success(), "ssh-keygen -t ed25519 failed");
    key_path
}
