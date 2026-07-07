# Containers

> How a reviewer with a `runner` block and `capabilities.network: true` executes inside a container.

[<- Backends](./backends.md) | [Developer guide index](./README.md) | Next: [Conventions](./conventions.md) ->

---

Most reviewers run natively, with Bastion shelling out to the agent CLI on the host.
A reviewer that declares a `runner` block (and opts into `capabilities.network: true`,
without which it fails closed before any container work) runs that same backend inside
a container instead, in a pinned environment with tools the host does not have. The container path
is confined to
[`src/backend/container/`](../../src/backend/container/), and it reuses the
backend boundary rather than forking it: the backend builds the same
`CommandSpec` it always does, and a decorator turns that spec into a `docker run`.

## The pieces

| Type | Role |
| --- | --- |
| `ExecutionPlan` | Parsed once from a `Reviewer`. `Native` or `Container(ContainerPlan)`. Resolving it is the single place an unprovisioned tier fails closed, so reaching a backend is proof the reviewer is runnable. |
| `ContainerEngine` | The engine CLI (`docker` by default; `podman` is a drop-in), resolved from `BASTION_CONTAINER_ENGINE`. |
| `ContainerPlan` | The resolved image source; `ensure_image` builds or resolves the image. A plan is built only for a container that opted into general egress (`network: true`), so it carries no network flag. |
| `ContainerRunner` | A `CommandRunner` decorator that rewrites a backend's `CommandSpec` into a `docker run` invocation. |

## The flow

`dispatch` resolves `ExecutionPlan::resolve(reviewer)` (the single place an
unprovisioned capability tier fails closed, before any image work). For a container
plan it then:

1. **Resolves the image** (`ContainerPlan::ensure_image`). A `dockerfile` source is
   built with the repo root as the build context (`docker build -t <tag> -f
   <dockerfile> <repo>`), tagged by a hash of the canonical repo root plus the
   Dockerfile bytes so an unchanged file hits the engine's layer cache and a changed
   one forces a rebuild. Folding the build context (the repo root) into the tag keeps
   two worktrees or repos with byte-identical Dockerfiles from colliding on one global
   tag and overwriting each other's image between build and run. An `image` source is
   used as-is; the engine pulls it on demand at run time. `dockerfile` takes
   precedence over `image`, and a `runner` with neither fails closed.
2. **Wraps the backend** (`ContainerRunner`). The backend is constructed with the
   bare in-container program name (`claude` / `codex` / `pi`) rather than a
   host-resolved path, since `BASTION_CLAUDE_BIN` means nothing inside the image. Every spec the
   backend produces is rewritten to:

   ```
   docker run --rm --name <unique> [-i] -v <repo>:/workspace -w /workspace \
     [--env-file <reviewer-env>] -e <CREDENTIAL_NAME> ... \
     --entrypoint <program> <image> <args...>
   ```

   The checkout is bind-mounted at `/workspace` and made the working directory, and
   stdin (the Codex prompt) flows through `docker run -i`. The container is given a
   unique `--name` so it can be torn down on cancellation (see Timeouts below). The
   backend program (`claude` / `codex` / `pi`) is set as the container `--entrypoint`
   rather than appended as a command argument: a bare `docker run <image> <program>` overrides
   only the image's CMD, so an image that declares an ENTRYPOINT would run that with the
   program as an argument, and the entrypoint, not the backend, would receive the
   forwarded credentials and the mounted checkout. Setting `--entrypoint` makes the
   selected backend the process the container runs, whatever the image declares. When
   the reviewer's `env` is non-empty it is written to a temp file passed as
   `--env-file` (an empty `env` omits the flag entirely): the engine injects those
   pairs into the container without their values entering the argv (where a process
   listing could expose a secret) and without their keys touching the engine *client*
   process. The env-file format is one `KEY=VALUE` per line with no escaping, so a key
   or value carrying a newline (or a key carrying `=`) is not representable: rather
   than let the engine silently split it into a truncated value plus stray variables,
   `write_env_file` fails the reviewer closed and names the offending key. That second property matters because keys like
   `DOCKER_HOST` or `HTTP_PROXY` are interpreted by the engine client itself: setting
   them on the client would retarget it and split the build, run, and teardown across
   daemons, so reviewer env stays scoped to the container. Above the `CommandRunner`
   seam, the prompt building, structured-output parse, and reprompt-once retry use the
   same code as the native path.

## Credentials

The in-container agent still needs to reach its model provider. `ContainerRunner`
forwards a fixed set of provider credential variable names (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, and the like) that are present and non-empty in Bastion's own
environment (an unset or empty variable is treated as absent and not forwarded),
passing them by name (`-e NAME`) so the engine reads the value and Bastion never
copies a secret through its own argv. If a reviewer also declares one of these names
in its own `env`, the passthrough is dropped for that name so the reviewer's value
wins: the engine gives an explicit `-e NAME` precedence over an `--env-file` entry,
so forwarding the host value too would silently override the reviewer's, diverging
from the native path (where reviewer env overrides the parent). An image may also
bake in its own auth, and a reviewer's `env` block can carry more. If the agent
cannot reach the provider, the backend error follows runner policy like any other
failure: a gate blocks and an advisor is skipped.

## Network

`network: true` gives a containerized reviewer general outbound network (the container
attaches the engine's default network). The default
`network: false` **fails closed in a container**: scoping egress to the model provider
needs an allowlisting proxy that is unbuilt, so rather than silently grant general
egress under a flag that reads as restricted, `ExecutionPlan::resolve` rejects a
container with `network: false` (a gate blocks, an advisor is skipped). A containerized
reviewer must therefore opt into `network: true` to run, and accept that today that
means general egress, not provider-only (provider-only scoped egress is unbuilt). A
native `network: true` (no `runner`) also fails closed: with no container there is
nothing to scope. The general egress `network: true` grants does not provide
adversarial isolation (see the
[threat model](./design.md#threat-model--trust-boundary)).

## Reprompt recovery is not persisted across turns

When a backend's first turn returns output that does not parse as a verdict, it
reprompts once in the *same session* (`claude --resume`, `codex exec resume`) to ask
for just the structured output before failing closed (see [Backends](./backends.md)).
That recovery depends on first-turn session state on disk. Each `docker run` here is a
separate `--rm` container with no persisted home, so a resume in the second container
cannot find the first turn's session: a containerized reviewer whose first turn is
malformed blocks instead of returning a pass, but a flaky first turn fails instead of
recovering. On Codex, when the first
turn yields no thread id, Bastion reprompts with the full prompt in a fresh session,
which works in a new container.

## Timeouts and teardown

The runner bounds every reviewer with a timeout and drops the dispatch future when it
elapses. Natively, the inner runner's `kill_on_drop` kills the agent child, so a hung
reviewer stops using tools and tokens the moment it is failed closed. Containers need
an extra teardown path, because `docker run --rm` removes the container only on a
clean exit, and killing the engine *client* (the `docker run` process) leaves the
container running in the daemon. So `ContainerRunner` names each container
(`--name <unique>`) and holds a drop guard across the inner `docker run`. If the
future is cancelled, the guard force-removes the named container (`docker rm -f`). The
guard's `Drop` spawns that `rm` child synchronously, then hands only the bounded
wait (poll up to the teardown budget, then kill a hung `rm`) to a detached OS thread.
Spawning the child before `Drop` returns matters because the drop fires on Bastion's
current-thread Tokio runtime: blocking on the wait there would freeze every other
reviewer, but fully detaching the cleanup could lose it, since a detached thread may
not run before the process exits (a timed-out reviewer can be the last work). With the
child already launched, the removal proceeds in the engine even if Bastion exits
immediately. On a clean return the guard is defused, since `--rm` has already removed
the container. That gives a containerized reviewer the same fail-closed teardown the
native path gets from `kill_on_drop`.

The `BASTION_CONTAINER_ENGINE` override is likewise recorded in the run seal as an
active test seam; a run sealed with it active cannot be attested (see
[Attestation](./attestation.md)).

## Testing

The `container/` submodules unit-test the pure parts against a recording
`CommandRunner`: plan resolution and its fail-closed arms (unprovisioned tiers, an
absolute or escaping `dockerfile`, an option-like `image`),
content-and-context-addressed image tags, the build invocation, and the `docker run`
translation. The end-to-end path is covered in
[`tests/integration/`](../../tests/integration/main.rs) by a compiled fake engine that
honors `build`, `run`, and `rm`: on `run` it clears inherited env (modeling the
container boundary) and re-executes the fake agent with the in-container program,
args, forwarded env, and piped stdin. The integration scenarios drive the `dispatch`
container branch, image resolution, and the `docker run` argv without a Docker daemon,
and cover a clean pass, a fail-closed block, environment isolation (only forwarded
`env` crosses, a host variable does not), provider-credential passthrough, and a hung
reviewer that times out and is torn down (the fake engine records the `rm -f`). The
scenarios detect-and-skip when `rustc` is absent, like the native fake agent, and fail
closed in CI rather than skipping silently.

---

Next: [Conventions](./conventions.md). The coding rules this crate holds itself to.
