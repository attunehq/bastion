//! Containerized reviewers (the `runner` block).
//!
//! Carved out of the former monolithic `main.rs`; that file's module doc
//! explains how the suite drives the real compiled binary against a fake agent.

use crate::fakes::*;
use crate::fixtures::*;

use bastion::verdict::Decision;

/// A reviewer with a `runner` runs its backend inside the container engine, end to
/// end: dispatch takes the container branch, resolves the image through the engine
/// (a `dockerfile` build here), and the `docker run` line carries the in-container
/// `claude` invocation that the fake engine re-executes as the agent. A clean pass
/// still passes, proving the whole container wiring (image build, the `docker run`
/// argv, the in-container program name, env forwarding, output capture) is real.
#[test]
fn a_containerized_reviewer_runs_in_the_engine() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e", "claude-code", "gate")
        .behavior("pass")
        .dockerfile("Dockerfile")
        .network()]));
    // The Dockerfile only needs to exist: the fake engine's `build` is a no-op, but
    // image-tag derivation reads the file's bytes.
    std::fs::write(repo.path().join("Dockerfile"), "FROM scratch\n").unwrap();

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.completed().0, Decision::Pass);
    assert_eq!(run.resolved("e2e").0, Decision::Pass);
    // The engine actually ran, and ran the bare in-image `claude` (not a host path):
    // a regression to the native path would never reach the fake engine, so the log
    // would be missing entirely. The `dockerfile` source also builds before it runs:
    // `ensure_image` fires the `build` first, then `docker run` re-execs the agent. A
    // regression that stopped building (or ran before building) would reorder or drop
    // the `build` line.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(
        lines,
        ["build", "claude"],
        "expected a build before the run"
    );
}

/// `backend: any` resolves to Claude Code inside a container too: the container path
/// must pin the bare in-image `claude`, not a host-resolved path. A regression that
/// resolved `any` differently across the native and container paths would surface as a
/// different (or missing) in-container program here.
#[test]
fn a_containerized_any_backend_runs_claude_in_the_engine() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e-any", "any", "gate")
        .behavior("pass")
        .image("ghcr.io/acme/e2e:latest")
        .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.resolved("e2e-any").0, Decision::Pass);
    // `any` ran the bare in-image `claude` off the prebuilt image, with no build.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    assert_eq!(logged.lines().collect::<Vec<_>>(), ["claude"]);
}

/// A containerized gate does not launder a block: when the in-container agent
/// blocks, the gate blocks and the binary exits nonzero. Drives the Codex backend
/// off a prebuilt `image` source, so the prompt rides stdin through `docker run -i`
/// and no build step runs.
#[test]
fn a_containerized_gate_still_fails_closed_on_a_block() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new("e2e-block", "codex", "gate")
        .behavior("block")
        .image("ghcr.io/acme/e2e:latest")
        .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-block").0, Decision::Block);
    // The block is real: with the fake engine clearing inherited env, the agent saw
    // `FAKE_BEHAVIOR=block` only because Bastion forwarded the reviewer's `env` through
    // the `--env-file`. Had it not crossed, the agent would default to `pass` and this
    // would not block. The bare in-image `codex` ran, off the prebuilt image with no
    // build: an `image` source is used as-is, so the log holds the run and no `build`
    // line.
    let logged = std::fs::read_to_string(&log).expect("the fake engine ran and logged");
    assert_eq!(logged.lines().collect::<Vec<_>>(), ["codex"]);
}

/// A containerized gate whose agent never emits a parseable verdict fails closed. The
/// agent returns malformed output on every turn, so the backend reprompts once and
/// still cannot parse a verdict; a gate must then block, exactly as on the native
/// path. This pins the documented fail-closed behavior for containerized reviewers
/// whose first turn is malformed: each `docker run` is a separate `--rm` container, so
/// a real engine cannot resume first-turn session state, and the safe outcome is a
/// block, never a laundered pass. (The fake engine does not model cross-container
/// session loss, so this asserts the always-true fail-closed case, persistent
/// malformed output, rather than a recovery the fake would falsely allow.)
#[test]
fn a_containerized_malformed_gate_fails_closed() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-malformed",
        "codex",
        "gate",
    )
    .behavior("malformed")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
        ],
    );

    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-malformed").0, Decision::Block);
}

/// A containerized reviewer's environment is isolated: the container does not
/// inherit Bastion's arbitrary environment, only the reviewer's literal `env` (and
/// the fixed credential allowlist). The fake engine clears inherited env, so this
/// asserts the boundary directly. The host sets `FAKE_SUMMARY=leaked-from-host` on
/// the Bastion process. One reviewer declares its own `FAKE_SUMMARY` and must see
/// that (reviewer env forwarded via `--env-file` reaches the container); a second
/// reviewer declares none and must fall back to the agent's default summary, proving
/// the host value did *not* leak across the boundary. Both observe the value through
/// the summary the agent echoes.
#[test]
fn a_containerized_reviewer_sees_only_forwarded_env() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[
        // Declares `FAKE_SUMMARY`: the forwarded value must cross.
        Reviewer::new("e2e-declared", "claude-code", "advisor")
            .behavior("pass")
            .env("FAKE_SUMMARY", "from-reviewer-env")
            .image("ghcr.io/acme/e2e:latest")
            .network(),
        // Declares no `FAKE_SUMMARY`: the host's value must not leak in, so the agent
        // falls back to its built-in default summary.
        Reviewer::new("e2e-isolated", "claude-code", "advisor")
            .behavior("pass")
            .image("ghcr.io/acme/e2e:latest")
            .network(),
    ]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            // A host-only variable neither reviewer forwards: it must not leak in.
            ("FAKE_SUMMARY", "leaked-from-host"),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    // The declared reviewer env crossed into the container.
    assert_eq!(run.resolved("e2e-declared").1, "from-reviewer-env");
    // The undeclared host variable did not: the agent used its default summary.
    let isolated = run.resolved("e2e-isolated").1;
    assert_eq!(isolated, "fake reviewer verdict");
    assert_ne!(isolated, "leaked-from-host");
}

/// A provider credential reaches the in-container agent without being listed in the
/// reviewer's `env`. `dispatch` wires `credential_passthrough()` into the container
/// runner, which forwards the fixed allowlist of provider credential names by `-e`.
/// Here `ANTHROPIC_API_KEY` is set on the Bastion process but *not* in the reviewer's
/// `env`; with the fake engine clearing inherited env, the agent can only see it
/// because the credential passthrough forwarded it. The agent echoes it into its
/// summary so the test can observe it crossed. This guards the dispatch wiring: an
/// empty credential list would leave the value absent and fail the assertion.
#[test]
fn a_provider_credential_crosses_into_the_container() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // `FAKE_ECHO_ENV` (a reviewer env) tells the agent to echo `ANTHROPIC_API_KEY`,
    // which is *not* listed in `env`: it can only arrive via credential passthrough.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-cred",
        "claude-code",
        "advisor",
    )
    .behavior("pass")
    .env("FAKE_ECHO_ENV", "ANTHROPIC_API_KEY")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            // A provider credential on the Bastion process, not in the reviewer env.
            ("ANTHROPIC_API_KEY", "cred-sentinel-xyz"),
        ],
    );

    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    let summary = run.resolved("e2e-cred").1;
    assert!(
        summary.contains("cred-sentinel-xyz"),
        "the provider credential did not reach the in-container agent; summary: {summary:?}"
    );
}

/// A hung containerized reviewer is timed out closed *and* its container is torn
/// down. `docker run --rm` only removes the container on a clean exit; when Bastion
/// times the reviewer out it kills the engine client, so the runner force-removes the
/// named container itself. The agent sleeps far past the timeout; the gate must still
/// block (the fail-closed guarantee the native timeout path also gives), and the fake
/// engine must have recorded the `rm -f` teardown.
#[test]
fn a_hung_containerized_reviewer_times_out_and_is_torn_down() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-hang",
        "claude-code",
        "gate",
    )
    .behavior("pass")
    .env("FAKE_SLEEP_MS", "5000")
    .timeout("300ms")
    .image("ghcr.io/acme/e2e:latest")
    .network()]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // Timed out: the gate fails closed.
    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-hang").0, Decision::Block);
    // The container teardown fired: the engine received `rm -f` for the run's
    // container, so a hung agent cannot keep running detached past the timeout.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.lines().any(|line| line.starts_with("rm:")),
        "expected a container teardown (`rm -f`); engine log:\n{logged}"
    );
}

/// A containerized reviewer that does not opt into `network: true` fails closed
/// before any container work. Bastion cannot scope a container's egress to the model
/// provider yet, so the default `network: false` reads as a restriction it cannot
/// enforce; rather than silently attach general egress, `ExecutionPlan::resolve`
/// rejects it. The gate blocks, the binary exits nonzero, and the engine is never
/// invoked (the failure precedes the image build and `docker run`), so no engine log
/// is written. This is the end-to-end face of the `plan.rs` unit test, proving the
/// resolve-time rejection becomes a real fail-closed block through the binary.
#[test]
fn a_containerized_reviewer_without_network_fails_closed() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // A `runner` block but no `capabilities.network: true`: unrunnable today.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-no-net",
        "claude-code",
        "gate",
    )
    .behavior("pass")
    .image("ghcr.io/acme/e2e:latest")]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // The gate fails closed: a container with the default `network: false` does not run.
    assert_eq!(run.code, Some(1));
    assert_eq!(run.completed().0, Decision::Block);
    assert_eq!(run.resolved("e2e-no-net").0, Decision::Block);
    // The failure precedes any container work: the engine was never invoked, so no
    // build and no `docker run` were logged.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.is_empty(),
        "the engine must not run for a reviewer rejected at resolve time; engine log:\n{logged}"
    );
}

/// The advisor side of the same resolve-time rejection: a containerized *advisor*
/// without `network: true` is failed *open*, not closed. The same
/// `ExecutionPlan::resolve` error that blocks a gate is, for an advisor, skipped and
/// kept out of the aggregate, so the run still passes and the binary exits zero. This
/// pins that the new preflight error follows the gate/advisor policy split rather than
/// wedging every containerized advisor, and (as on the gate path) never reaches the
/// engine.
#[test]
fn a_containerized_advisor_without_network_is_skipped() {
    let Some((fake, docker)) = container_tooling() else {
        return;
    };

    // An advisor with a `runner` but no `capabilities.network: true`.
    let repo = TestRepo::new(&registry(&[Reviewer::new(
        "e2e-no-net-advisor",
        "claude-code",
        "advisor",
    )
    .behavior("pass")
    .image("ghcr.io/acme/e2e:latest")]));

    let engine = docker.to_str().unwrap();
    let agent = fake.to_str().unwrap();
    let log = repo.path().join("fake-docker.log");
    let run = repo.review_base(
        fake,
        "main",
        &[
            ("BASTION_CONTAINER_ENGINE", engine),
            ("FAKE_AGENT_BIN", agent),
            ("FAKE_DOCKER_LOG", log.to_str().unwrap()),
        ],
    );

    // The advisor fails open: it is skipped, the aggregate still passes, exit zero.
    assert!(run.exited_zero(), "stderr:\n{}", run.stderr);
    assert_eq!(run.completed().0, Decision::Pass);
    let resolved = run.resolved("e2e-no-net-advisor");
    assert_eq!(resolved.0, Decision::Pass);
    assert!(
        resolved.1.contains("skipped"),
        "a rejected advisor should be recorded as skipped, got: {:?}",
        resolved.1
    );
    // The engine was never invoked, exactly as on the gate path.
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.is_empty(),
        "the engine must not run for an advisor rejected at resolve time; engine log:\n{logged}"
    );
}
