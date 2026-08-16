# Backends

> The agent execution boundary: the trait, the subprocess seam, dispatch, and how
> to add a backend.

[<- Architecture](./architecture.md) | [Developer guide index](./README.md) | Next: [Containers](./containers.md) ->

---

Bastion does not implement an agent loop. It translates a reviewer's execution
profile into an existing agent CLI's native config and shells out to it, capturing
the structured verdict. Everything agent- and subprocess-specific is confined to
[`src/backend/`](../../src/backend/) so the [runner](./architecture.md) above it
stays pure orchestration.

## The pieces

| File | Role |
| --- | --- |
| [`mod.rs`](../../src/backend/mod.rs) | The `Backend` trait, `ReviewRequest`/`ReviewOutcome`, `MockBackend`, `dispatch`, and the shared prompt/helpers (`changeset_preamble`, `EXHAUSTIVE_FINDINGS_INSTRUCTION`, `context_segment`, `interpolate`, `money_from_dollars`, plus the JSON-schema set `VERDICT_JSON_SCHEMA`/`JSON_SCHEMA_INSTRUCTION`/`JSON_REPROMPT` and the fenced-YAML set `SCHEMA_INSTRUCTION`/`REPROMPT_SUFFIX`/`extract_verdict`). |
| [`command.rs`](../../src/backend/command.rs) | The `CommandRunner` subprocess seam: `CommandSpec` and `SystemCommandRunner`, plus a fake runner for tests. |
| [`governor.rs`](../../src/backend/governor.rs) | The spawn governor: `GovernedRunner` (a `CommandRunner` decorator) and `SpawnGovernor`, the per-run spend caps enforced at the seam. See [The spawn governor](#the-spawn-governor). |
| [`claude_code.rs`](../../src/backend/claude_code.rs) | The Claude Code backend. |
| [`codex.rs`](../../src/backend/codex.rs) | The Codex backend. |
| [`pi.rs`](../../src/backend/pi.rs) | The Pi backend. |
| [`grok.rs`](../../src/backend/grok.rs) | The Grok Build backend. |
| [`container/`](../../src/backend/container/) | The container runner, split by concern: `plan.rs` (`ExecutionPlan` and image resolution), `runner.rs` (the `CommandRunner` decorator), `credentials.rs`, and `teardown.rs`. See [Containers](./containers.md). |

## The trait

```rust
pub trait Backend {
    fn id(&self) -> reviewer::Backend;
    async fn review(&self, request: &ReviewRequest<'_>) -> Result<ReviewOutcome>;
}
```

A backend is handed a `ReviewRequest` (the reviewer, the run id, the repo root, the
base branch, and the untrusted `ReviewContext` for the run) and returns a
`ReviewOutcome` (the structured `Verdict`, optional `Usage`, and the optional full
transcript). The trait is deliberately small and
stable: sibling backends implement the same signature, and `dispatch` is the single
place that grows when one lands; the trait does not.

An error from `review` is *not* a verdict. The runner turns it into a fail-closed
`block` for a gate (with a synthetic blocking finding) and drops it for an advisor.
A backend should return `Err` when it cannot produce a valid verdict, never a
fabricated pass.

## Dispatch

`dispatch` resolves the reviewer's [`ExecutionPlan`](./containers.md), then selects
a concrete backend. It takes the run's shared [`SpawnGovernor`](#the-spawn-governor)
and wraps the host `SystemCommandRunner` in a `GovernedRunner` so every launch on
either path passes through the spend caps:

```rust
match ExecutionPlan::resolve(request.reviewer)? {
    ExecutionPlan::Native        => {
        let runner = GovernedRunner::new(SystemCommandRunner, governor.clone());
        run_backend(request, runner, Program::HostDefault).await
    }
    ExecutionPlan::Container(plan) => {
        // The image build shells out too, so it is governed like any other launch.
        let base  = GovernedRunner::new(SystemCommandRunner, governor.clone());
        let image = plan.ensure_image(&engine, &base, repo_root).await?;
        run_backend(request, ContainerRunner::new(base, ..., image, ...), Program::InContainer).await
    }
}
```

Resolving the plan is the **single place an unprovisioned capability tier fails
closed** (see [Containers](./containers.md)), so a backend is only ever reached for a
reviewer this build can actually run. `run_backend` then maps `reviewer::Backend` to
the concrete backend, shared by the native and container paths so backend selection
lives in one place:

```rust
// `program` is HostDefault on the native path, InContainer on the container path.
match request.reviewer.backend {
    Backend::Any | Backend::ClaudeCode => match program {
        Program::HostDefault => ClaudeCodeBackend::new(runner).review(request).await,
        Program::InContainer => ClaudeCodeBackend::with_program(runner, claude_code::DEFAULT_PROGRAM)
            .review(request).await,
    },
    Backend::Codex => match program {
        Program::HostDefault => CodexBackend::new(runner).review(request).await,
        Program::InContainer => CodexBackend::with_program(runner, codex::DEFAULT_PROGRAM)
            .review(request).await,
    },
    Backend::Pi => match program {
        Program::HostDefault => PiBackend::new(runner).review(request).await,
        Program::InContainer => PiBackend::with_program(runner, pi::DEFAULT_PROGRAM)
            .review(request).await,
    },
    Backend::Grok => match program {
        Program::HostDefault => GrokBackend::new(runner).review(request).await,
        Program::InContainer => GrokBackend::with_program(runner, grok::DEFAULT_PROGRAM)
            .review(request).await,
    },
}
```

All four named backends are wired; the match is exhaustive with a real arm each,
and `Any` defaults to Claude Code until routing by availability/subscription exists.
A backend still **fails closed** when it cannot produce a valid, consistent verdict:
it returns an error (never a fabricated pass) and the runner turns that into a block
for a gate. The only difference between the native and container paths is how the
program is resolved, the `program` branch above: natively `new` takes it from the
host (`BASTION_CLAUDE_BIN` / `BASTION_CODEX_BIN` / `BASTION_PI_BIN` / `BASTION_GROK_BIN`
/ `PATH`), while in a container `with_program` pins the bare default name (`claude` /
`codex` / `pi` / `grok`) so
it resolves on the image's `PATH` rather than a host path that means nothing inside
the image.

## The subprocess seam

Backends never call `std::process::Command` directly. They build a `CommandSpec`
(program, args, working directory, environment) and hand it to a `CommandRunner`.
Production uses `SystemCommandRunner`; tests inject a fake that records the specs it
was given and returns canned stdout. This is what lets every backend module be
tested against a *fake executable* with no real agent, network, or
cost, while still exercising the real argument-building, env-injection, output
parsing, and retry logic.

The `BASTION_CLAUDE_BIN`/`BASTION_CODEX_BIN`/`BASTION_PI_BIN`/`BASTION_GROK_BIN`
overrides that point a backend at a fake executable are recorded in the run seal as an active test seam
(`src/seal.rs`); a run sealed with one active cannot be attested (see
[Attestation](./attestation.md)).

### The spawn governor

Because every agent launch is a `CommandRunner::run` call, that seam is also the one
place to *bound* launches. [`governor.rs`](../../src/backend/governor.rs) defines
`GovernedRunner<R>`, a `CommandRunner` decorator wrapping the real runner, and
`SpawnGovernor`, the per-run state it shares. One `Arc<SpawnGovernor>` is built in the
runner (`src/runner/mod.rs`) for the whole review and threaded through `dispatch` so
every reviewer, container image build included, draws from the same budget.

`GovernedRunner::run` does three things around the inner call: it `admit`s (awaiting a
concurrency permit and refusing once a cap has tripped), runs the inner runner, then
`record`s whether the launch was a *dead spawn*. A dead spawn is the incident's
signature: a process that exits non-zero having produced no stdout (`Err`, or a
non-success `CommandOutput` with empty stdout), the shape of an agent CLI that dies at
0 tokens on a bad install, a failed login, or exit 127. The caps come from
[`SpawnLimits`](../../src/limits.rs): `max_concurrent` bounds fan-out width via a
semaphore, `max_total_spawns` caps launch attempts over the whole run, and
`max_consecutive_failures` trips the moment that many dead spawns land back to back. A
productive launch resets the consecutive counter, so a run that is merely slow or has
the odd failure is unaffected; only a genuine respawn storm trips it.

Crucially the counter increments on *every* admitted launch, dead ones included, so a
loop that respawns a crashing CLI still marches toward `max_total_spawns` and trips the
consecutive breaker fast. When a cap trips, `admit` refuses further launches and the
runner aborts the whole review: it forces the aggregate to `block`, skips the run seal
(an aborted run is not attestable, like a partial one), and returns a loud error naming
what was capped and how many agents launched. This is the fail-closed direction: the
gate blocks, it does not silently pass. See [Bounding a run's
spend](../user-guide/authoring-reviewers.md#bounding-a-runs-spend) for the operator
view and the defaults.

`ContainerRunner`'s drop guard is the one exception to this seam. `ContainerGuard`
runs the container teardown (`docker rm -f`) with a direct `std::process::Command` in
`Drop`. The seam is async and a `Drop` is not, so the cancellation teardown cannot
route through `CommandRunner::run`. It is a fixed engine invocation (`rm -f` on the
container's own generated name, no reviewer-controlled input), bounded by a teardown
budget and run on its own thread so it never blocks the runtime. See
[Containers](./containers.md#timeouts-and-teardown).

## Shared behavior in `mod.rs`

These shared helpers keep the backends consistent so a reviewer behaves the same
regardless of which agent runs it:

- **`changeset_preamble`**: the instruction prepended to every prompt telling the
  agent how to see its changeset. It hands the agent the pre-resolved merge-base
  commit and steers to `git diff {merge_base}` (working tree vs. merge base) plus
  an untracked-file scan, and explicitly warns *off* both nearby forms:
  `git diff {base}` (against the base tip) also shows everything the base gained
  since the branch forked, so a reviewer using it blames the base's own changes on
  the changeset under review; `{base}...HEAD` shows only committed history and
  would miss the uncommitted work an author iterates on locally. In CI the head is
  committed and there are no untracked files, so the same instruction is correct
  there too.
- **`EXHAUSTIVE_FINDINGS_INSTRUCTION`**: a fixed instruction appended to every
  reviewer prompt (after the reviewer's own text, before the schema instruction)
  telling the agent to enumerate *every* qualifying finding in one pass rather than
  stopping at the first. A verdict is consistent with a single blocking finding, so
  without this an agent tends to report one issue and stop, forcing the author
  through a fresh review cycle per issue. It changes only how completely a reviewer
  reports, never the gate decision: a clean changeset still returns `pass` with no
  findings, and the reviewer's own prompt still decides what counts as an issue.
- **`context_segment`**: renders the run's `ReviewContext` (author intent, prior
  findings, discussion) for the reviewer, or the empty string when there is nothing to
  add. Every backend splices it into the same slot, after the interpolated reviewer
  prompt and before `EXHAUSTIVE_FINDINGS_INSTRUCTION` and the schema instruction. The
  block leads with an untrusted-input preamble and scopes prior findings and routed
  replies to the running reviewer, so each agent consumes context identically and reads
  it as claims to weigh, never as instructions.
- **`interpolate`**: substitutes `${key}` placeholders in a prompt from the
  reviewer's `inputs`. Unknown placeholders are left as literal text (the reviewer
  author is trusted; a literal `${...}` is harmless).
- **`money_from_dollars`**: converts a backend-reported dollar cost into the exact
  `Money` (cents) type, clamping negative or non-finite values to zero so a
  malformed cost can never produce a nonsensical charge.

## What a backend applies from the profile

The reviewer schema is fuller than the current native execution path. Be precise
about what is honored, so the code does not over-promise:

| Field | Status in this build |
| --- | --- |
| `prompt`, `trigger`, `mode`, `name` | Fully honored. |
| `backend` | Honored (`claude-code`, `codex`, `pi`, `grok`; `any` -> Claude Code). |
| `model` | **Honored.** Forwarded to the backend's model selector (`--model` for Claude Code, Pi, and Grok Build, `-m` for Codex). Backend-specific, so the registry rejects a `model` (own or inherited) under `backend: any`. Pi's `--model` takes a `provider/id` form (e.g. `openai-codex/gpt-5.5`) that selects the provider too (Pi's bare default provider is `google`), so a Pi model carries its provider in the string. Absent, Claude Code defaults to `claude-opus-4-8`; Codex, Pi, and Grok Build resolve their own. |
| `effort` | **Honored.** An opaque level forwarded verbatim to each backend's native control (Claude Code's `--effort`, Codex's `model_reasoning_effort`, Pi's `--thinking`, Grok Build's `--reasoning-effort`; see below). Default `high`. |
| `defaults` (registry-wide `model`/`effort`) | **Honored.** Folded into each reviewer at load time (a reviewer's own field wins); resolution happens once, at registry load, so the persisted run record carries the effective values. |
| `timeout` | Honored by the runner. |
| `inputs` | Honored, interpolated into the prompt. |
| `env` | Honored, injected into the child process environment. |
| `runner` (`dockerfile`, `image`) | **Honored, with `capabilities.network: true`.** A reviewer with a `runner` block and `capabilities.network: true` runs its backend inside a container; `dockerfile` is built (cached by content hash), `image` is used as-is. A `runner` without `network: true` fails closed (see the `capabilities.network` row below). See [Containers](./containers.md). |
| `capabilities.network` | **`network: true` is honored in a container; the default `network: false` fails closed.** `network: true` gives a containerized reviewer general (unscoped) egress: the container attaches the engine's default network. The default `network: false` fails closed in a container because provider-only scoped egress (an allowlisting proxy) is unbuilt, so `ExecutionPlan::resolve` rejects it rather than silently granting general egress under a flag that reads as restricted (a gate blocks, an advisor is skipped). A containerized reviewer must opt into `network: true` to run. A *native* `network: true` (no `runner`) also fails closed: with no container there is nothing to scope. |
| `capabilities` (`mcp`, `skills`) | **Not provisioned: fails closed.** A reviewer that declares either is failed closed by `ExecutionPlan::resolve` in `dispatch`. |
| `attestation` | **Honored, but not by backend dispatch.** The CI attestation replay planner (`src/attest/replay.rs`) reads `attestation: never` to skip replay for that reviewer, so it always executes fresh; absent, the reviewer is eligible for replay. A registry-wide `attestations: true` switch (`src/config.rs`) gates whether CI attempts replay at all. See [Attestation](./attestation.md). |

### How `model` and `effort` reach each backend

Model and effort are the two knobs Bastion sets on the agent CLI rather than
leaving to its config, so a review is reproducible across machines. They resolve in
three layers (highest first): the reviewer's own field, the registry `defaults`
block (folded in by `Config::from_yaml`), then the backend's built-in default.

Both are passed through **opaquely**: Bastion does not parse or remap either value,
so a reviewer can use whatever vocabulary its backend accepts (Claude Code's
`--effort` takes `low`/`medium`/`high`/`xhigh`/`max`; Codex's
`model_reasoning_effort` takes `minimal`/`low`/`medium`/`high`; Pi's `--thinking`
takes `off`/`minimal`/`low`/`medium`/`high`/`xhigh`; Grok Build's
`--reasoning-effort` takes what its current models expose: `low`/`medium`/`high`/`xhigh`
on `grok-4.6`, and its docs also list `none`/`minimal`/`max` and per-model ids). The
shared
`low`/`medium`/`high` levels are portable; the backend-specific ones are not, and a
mismatch is the backend's problem, not a load error (Grok Build rejects an unknown
level with a non-zero exit, which a gate fails closed on).

`model` differs from `effort` in one respect: because a model id almost never
overlaps across backends, a `model` under `backend: any` is a load error
(`Config::validate`), whereas `effort` is allowed under `any` (its common values
port). Claude Code always sends a `--model` (its built-in default is
`claude-opus-4-8`) and always an `--effort` (default `high`); Codex always sends
`model_reasoning_effort` (default `high`) and sends `-m` only when a model is
pinned, otherwise it resolves its own. Pi mirrors Codex: it always sends
`--thinking` (default `high`) and sends `--model` only when a model is pinned,
otherwise it falls back to its configured default provider/model. Pi is
multi-provider, so the provider rides inside the model string using Pi's native
`provider/id` form (e.g. `openai-codex/gpt-5.5`): a bare model id would resolve
under Pi's default provider (`google`), so a Pi reviewer's `model` should name its
provider. Grok Build follows the Codex/Pi shape too: it always sends
`--reasoning-effort` (default `high`) and sends `--model` only when a model is
pinned; `grok models` lists the ids the CLI accepts.

The unprovisioned opt-ins **fail closed** rather than silently degrading: a gate that
declares a tier it cannot get must block, never run degraded and report a pass (see
[`ExecutionPlan::resolve`](../../src/backend/container/plan.rs) and the
[core design](./design.md#aggregation--the-merge-gate)). As each tier is wired, its
arm of the preflight is removed and this row flips to "honored".

When you wire the next tier (`mcp`, then `skills`), this is the table to update, and
with it the
[authoring guide's note](../user-guide/authoring-reviewers.md#runner-and-capabilities)
and the [user-facing status](../user-guide/README.md#status).

## Adding a backend

1. Add a variant to `reviewer::Backend` in [`reviewer.rs`](../../src/reviewer.rs)
   (it is `#[non_exhaustive]`; keep `as_str` and the kebab-case serde form in sync).
2. Create `src/backend/<name>.rs` implementing the `Backend` trait, building its
   `CommandSpec` and parsing its CLI's structured-output envelope into a `Verdict`.
   Reuse `changeset_preamble`, `interpolate`, and `money_from_dollars`, splice
   `context_segment(request)` in after the reviewer prompt, and append
   `EXHAUSTIVE_FINDINGS_INSTRUCTION` so the new backend feeds the review context and
   enumerates every finding in one pass like the others. If the CLI has no native
   structured-output enforcement, reuse the shared fenced-YAML `SCHEMA_INSTRUCTION`,
   `REPROMPT_SUFFIX`, and `extract_verdict` (as the Codex and Pi backends do) rather
   than re-implementing verdict-block parsing. If it enforces a JSON schema natively,
   reuse `VERDICT_JSON_SCHEMA`, `JSON_SCHEMA_INSTRUCTION`, `JSON_REPROMPT`, and
   `parse_verdict_from_text` (as Claude Code and Grok Build do).
3. Wire the variant into `dispatch` in [`mod.rs`](../../src/backend/mod.rs).
4. Test it against a fake `CommandRunner`, following `claude_code.rs` / `codex.rs` /
   `pi.rs` / `grok.rs`: assert the args and env you build, and the parsing of a
   representative envelope, including the malformed-output retry path. Then teach the
   integration fake agent (`tests/integration/fakes.rs`) the new CLI's protocol and
   add its `BASTION_<NAME>_BIN` seam to `seal::seams_active_from`.

`MockBackend` is *not* the template for a new backend; it is a deterministic
always-pass double for testing the runner without any agent. Real backends drive a
fake executable instead.

## The verdict round-trip

Backends capture the agent's structured output, then validate it against the
verdict schema: Claude Code and Grok Build via a JSON schema (`--json-schema`); Codex
and Pi via a requested fenced YAML verdict block parsed from the final message (the shared
`SCHEMA_INSTRUCTION` + `extract_verdict`). If the agent does not produce complying
output, the backend re-runs the *same session* (resumed by its session/thread id)
with a turn that re-states the schema and asks for just the structured output of the
work already done; only after that fails does it give up with an error (which the
runner fails closed). The verdict schema itself is specified in the
[core design](./design.md#the-verdict).

---

Next: [Containers](./containers.md). How a reviewer with a `runner` block and
`capabilities.network: true` executes inside a container.
