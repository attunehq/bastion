---
title: Authoring reviewers
summary: "The registry schema in full, and how to write a reviewer that stays sharp."
order: 4
---

# Authoring reviewers

> The registry schema in full, and how to write a reviewer that stays sharp.

Reviewers are the whole policy. This chapter is the reference for writing them:
the file, the required fields, the optional execution profile, and the craft of a
prompt that keeps recall high. It progresses from the minimum you need to the
fields you will reach for only occasionally.

## The registry file

The repository's reviewers live in a file at its root: `.bastion.yaml` (the
`.bastion.yml` spelling is also honored). Bastion finds it by walking up from the
current directory, so the command works anywhere inside the repo. The file is a
top-level mapping with a `reviewers:` list plus four optional keys, `defaults:`,
`attestations:`, `limits:`, and `include:`:

```yaml
attestations: true   # optional, default off; see below

defaults:             # optional; see "Registry-wide defaults"
  model: gpt-5
  effort: high

limits:               # optional; see "Bounding a run's spend"
  max_concurrent: 8

include:              # optional; see "Splitting the registry across files"
  - reviewers/security.yaml

reviewers:
  - name: single-responsibility
    trigger: [src/**/*.rs]
    mode: gate
    prompt: |
      ...
  - name: test-coverage
    trigger: [src/**/*.rs]
    mode: advisor
    prompt: |
      ...
```

`attestations: true` opts the repository into letting CI verify and replay a
signed local run instead of re-executing every reviewer; see [Attesting a run
so CI can replay it](./continuous-integration.md#attesting-a-run-so-ci-can-replay-it).
Only the root registry file may set it; an included file that tries is a load
error.

`limits:` bounds how much one `bastion review` may spend on its agent fan-out, so
a broken or looping run fails fast instead of multiplying cost; see [Bounding a
run's spend](#bounding-a-runs-spend). Conservative defaults apply even with no
`limits:` block, and like `attestations:`, only the root registry file may set it.

Reviewer **names must be unique** across the merged registry (the root file plus
everything it includes); a duplicate name is a load error that names both files. A
name also has to work as a directory name in the run store, so a name that
reduces to an empty, `.`, or `..` component is rejected, as are two names that
collapse to the same component once non-portable characters are normalized (for
example `repo:test` and `repo-test`); plain names are unaffected. Every reviewer
also needs a non-empty `prompt`; an empty one (a blank inline value, or a prompt
file with no content) is a load error rather than a reviewer that silently checks
nothing. Because these files *are* the review policy, changes to them should
require human review; see [Governance](./governance.md) and `bastion github
codeowners`.

> **Migrating from `bastion/reviewers.yaml`.** Bastion still loads the legacy
> `bastion/reviewers.yaml` location but prints a deprecation warning; the supported
> location is `.bastion.yaml` at your repository root. Move the file (the contents
> are unchanged) and regenerate your CODEOWNERS block with `bastion github
> codeowners`.

## Splitting the registry across files

A registry can spread across files without changing what it means. The top-level
`include:` array names further registry files whose reviewers merge into the
including file's, in order: the including file's own reviewers first, then each
include's. An included file has the same schema as the root one (its own
`reviewers:`, `defaults:`, and even `include:`, so includes nest), with two
exceptions: only the root file may set `attestations:` or `limits:`.

```yaml
# .bastion.yaml
include:
  - reviewers/security.yaml
  - reviewers/docs.yaml
reviewers:
  - name: single-responsibility
    ...
```

Paths resolve relative to the file that lists them, so `reviewers/security.yaml`
above is relative to the repository root, and an `include:` inside it would be
relative to `reviewers/`. A file reached twice (two files including the same
third one, or a cycle) merges once. The result behaves exactly like one big
file: names must be unique across all of it, and each file's `defaults:` apply
to the reviewers declared in that same file only, so an included file means the
same thing no matter who includes it.

You can also merge extra registry files for a single invocation with the
repeatable `--include <path>` flag, on `review`, `validate`, `attest`, and
`github codeowners`. The files merge into the repository layer like `include:`
entries, with one difference: a relative `--include` path resolves against the
directory you run the command from, not against the registry file. The extra
reviewers become part of the effective repository configuration for that run,
which has one consequence for
[attestation](./continuous-integration.md#attesting-a-run-so-ci-can-replay-it):
`bastion attest` and CI re-derive the configuration themselves, so a run
reviewed with `--include` only attests and replays if they are given the same
files.

## Prompt files

A `prompt:` can name a file instead of carrying the text inline, with the
`{file: <path>}` form. The file's whole content becomes the prompt, so a long
review instruction can live in markdown with real editor support instead of a
YAML block scalar:

```yaml
reviewers:
  - name: tenant-isolation
    trigger: [src/**]
    mode: gate
    prompt:
      file: reviewers/prompts/tenant-isolation.md
```

The path resolves relative to the registry file that declares the reviewer (an
included file reads its prompt files relative to itself). A missing or empty
prompt file is a load error. Everything downstream sees the resolved text: the
run record, the attestation hash, and carry all bind the prompt's *content*, so
editing the markdown file is a policy change exactly like editing an inline
prompt, and prompt files belong under the same human review as the registry.
`bastion github codeowners` lists prompt files that live inside the repository
automatically; one outside the repository still works as a prompt, but
CODEOWNERS cannot protect a path outside the tree, so it is left out of the
generated block.

## User-level reviewers

You can also keep personal reviewers in a user-level `.bastion.yaml` (or
`.bastion.yml`) in your platform config directory. Bastion uses them as the
fallback in repositories that have not configured reviewers:

- Linux: `$XDG_CONFIG_HOME/bastion`, defaulting to `~/.config/bastion`.
- macOS: `~/Library/Application Support/bastion`.
- Windows: `%APPDATA%\bastion`.

By default, Bastion uses this file only when the repository has no reviewer
registry. This keeps a local run aligned with the repository's configured policy
and avoids extra model calls. Pass `--with-user-reviewers` to `bastion review` or
`bastion validate` to merge both files. The merge combines reviewers by name:

- A reviewer only one file defines is included as-is.
- The same reviewer in both files is deduplicated to one. Sameness is compared by the
  *effective* configuration after each file's registry `defaults` are applied, so a
  reviewer that inherits a default `model` or `effort` and one that spells out the
  same value count as identical.
- A name in both files with a *different* effective configuration is a collision; both
  are kept, your copy under its plain name and the repository's scoped to
  `repo:<name>`, so neither silently wins. The two files are governed separately, so
  the collision is surfaced rather than resolved by precedence.

This layer is local-only. A review carrying a GitHub source (with `--repo`/`--pr`, as
CI runs) skips the user-level registry, so a pull request is gated by the
repository's reviewers alone, the `repo:` scope never appears there, and a personal
reviewer can never gate someone else's change.
`--config-dir <path>` (or `$BASTION_CONFIG_DIR`) overrides where the user-level file
is read from. It does not enable merging by itself.
`--include` adds files to the repository layer but does not suppress the personal
fallback when no repository registry exists.

The user-level file supports the full schema, though `attestations:` only has
meaning in the repository's registry (a personal reviewer never gates a PR, so
there is nothing to attest). Its `include:` entries and prompt files resolve
relative to the config directory and merge into the user layer, never the
repository's, so splitting your personal registry across files cannot leak a
reviewer into a repository's attested configuration.

## Registry-wide defaults

An optional top-level `defaults:` block sets a house `model` and `effort` that
every reviewer inherits unless it sets its own. A reviewer's explicit field always
wins; the default just fills the gap, so you set the model and effort once instead
of repeating them on every reviewer:

```yaml
defaults:
  model: gpt-5
  effort: high
reviewers:
  - name: single-responsibility
    trigger: [src/**/*.rs]
    mode: gate
    backend: codex      # required: an inherited model needs a pinned backend
    prompt: |
      ...
```

A default `model` is still backend-specific, so a reviewer that inherits it must
pin a `backend`; an inherited model under `backend: any` is rejected the same way
an explicit one is. `defaults` sits *above* each backend's own built-in default
(Opus 4.8 at `high` effort on Claude Code), so the resolution order is: the
reviewer's own field, then `defaults`, then the backend default.

## Bounding a run's spend

Every reviewer shells out to an agent, and an agent run costs tokens. Nothing in
the routing bounds the *total* cost of a fan-out on its own: a reviewer whose
agent fails to start is retried, a transient spawn or authentication failure can
turn into a respawn loop, and a run that is quietly broken keeps launching agents
until something outside Bastion notices. An optional top-level `limits:` block is
the spend cap that stops that. Every field is optional and takes a conservative
default, so a registry with no `limits:` block still runs fully capped:

```yaml
limits:
  max_concurrent: 8            # most agents running at once (default 8)
  max_total_spawns: 60         # most agent launches in one review run (default 60)
  max_consecutive_failures: 4  # dead launches in a row before aborting (default 4)
```

- **`max_concurrent`** bounds how many agents run at the same time, and so the
  peak concurrent spend. A run with more matched reviewers than this still runs
  them all; the extras queue until a slot frees.
- **`max_total_spawns`** caps how many agent launches one review run may make in
  total. Every launch counts, including a reprompt for a malformed verdict and a
  launch that dies immediately, so a respawn storm trips this even though its
  spawns did no real work. A healthy full run spends a small fraction of the
  default.
- **`max_consecutive_failures`** trips a breaker when this many agent launches in
  a row produce no output at all: the signature of a broken or unauthenticated
  agent CLI (an exit-127, a failed login) that launches, dies at zero tokens, and
  would otherwise be retried forever. A single productive launch resets the count,
  so an occasional transient failure never trips it.

Reaching any cap **aborts the whole run** with a clear error rather than
continuing to spend: the run is recorded (every reviewer that had launched fails
closed) but never sealed, and `bastion review` exits non-zero. The message names
what was capped, how many agents launched, and why it stopped. The defaults leave
a healthy run untouched while turning a runaway one into a loud, fast failure; set
the block only if your fan-out is genuinely larger than the defaults allow, or if
you want tighter caps than the defaults. Like `attestations:`, only the root
registry file may set `limits:`.

## The required fields

Four fields are mandatory. A reviewer with just these is complete and runnable.

### `name`

A unique identifier. It is also the reviewer's check-run name in CI
(`bastion / single-responsibility`), so keep it short and descriptive.

### `trigger`

A path trigger is an ordered list of globs matched against the changed files.
The reviewer runs when at least one changed file is included. A leading `!`
excludes a match, and the last matching pattern wins, so a later positive
pattern can include a path again. Globs use the usual `**` (any depth) and `*`
(one segment) syntax:

```yaml
trigger: [src/**/*.rs]                       # all Rust under src, any depth
trigger: [src/server/**, src/client/**]      # either subtree
trigger: [src/**/*.rs, docs/**/*.md, ".bastion.yaml"]   # multiple kinds
trigger: ["docs/**/*.md", "!docs/audit-reports/**"]    # docs except snapshots
trigger: ["docs/**", "!docs/generated/**", docs/generated/index.md] # include one again
```

Quote a glob if YAML would otherwise mis-parse it (a bare leading `*`, for
instance). Quote every exclusion because YAML treats a bare leading `!` as a
tag. A path trigger must contain at least one positive pattern. Empty and
exclusion-only lists are invalid.

When paths cannot express the concern without waking an expensive reviewer on
most changes, use an agent trigger:

```yaml
reviewers:
  - name: single-responsibility
    trigger:
      kind: agent
      prompt: >
        Run when the changeset creates or materially expands a responsibility
        boundary in production code. Skip tests, docs, and mechanical edits.
      backend: codex
      model: gpt-5.6-luna
      effort: high
      timeout: 45s
      paths: [src/**/*.rs]
    mode: gate
    prompt: |
      Review each changed production file for single responsibility.
      ...
```

`paths` is an optional cheap prefilter and uses the same ordered inclusion and
exclusion rules. With it, both conditions must hold: a changed path is included
and the trigger agent decides `run`. Without it, Bastion
asks the trigger agent on every non-empty changeset. The trigger agent receives
the actual changeset but not the author's description or PR discussion, so a
claim about relevance cannot suppress review. It runs with no capabilities,
environment, or container even when the full reviewer has them.

The trigger's `prompt` is inline text. The trigger agent returns `run` or `skip`
with a reason. A timeout, backend error, malformed response, or uncertain answer
becomes `run`, then the full reviewer applies its normal gate or advisor policy. An explicit
`--reviewer <name>` also bypasses the agent trigger because naming a reviewer is
a request to run it. A skip is recorded as `reviewer.skipped`; it is not a pass
verdict. Its tokens count toward the run, and a full run seals and attests the
skip like any other terminal reviewer outcome.

`timeout` bounds only the routing call and defaults to 2 minutes. A trigger that
reaches the limit resolves to `run`; the full reviewer then uses its own top-level
`timeout` (15 minutes by default).

Agent triggers inherit `model` and `effort` from registry-wide defaults when
they omit those fields. `trigger.backend` defaults to `any`, which currently
resolves to Claude Code, and is independent of the full reviewer's backend. A
trigger model requires its own pinned `backend`; the full reviewer's backend does
not implicitly select the trigger backend. Keep the routing prompt narrower and
cheaper than the reviewer prompt, since every candidate pays for the routing call
before Bastion can save the full review.

### `mode`

`gate` (blocks the merge when it returns `block`; fails closed) or `advisor`
(never blocks; fails open). See [Concepts](./concepts.md#the-mode-gate-vs-advisor)
for the full semantics.

### `prompt`

The instruction handed to the reviewing agent, written inline or as a
`{file: <path>}` reference to a markdown file (see [Prompt
files](#prompt-files)). Either way it must be non-empty. This is where the craft
lives; see [Writing a good prompt](#writing-a-good-prompt) below.

## The optional execution profile

The remaining fields tune *how* a reviewer runs. All have defaults; omit them
until you need them.

### `backend`

Which agent harness runs the reviewer. Default `any` (resolves to Claude Code).
Pin `claude-code`, `codex`, `pi`, or `grok` to force a specific harness, usually
because a subscription's terms require it, or because one model is better at a
given concern.

```yaml
backend: codex
```

> `pi` is multi-provider. Pin its provider and model together in the [`model`](#model)
> field using Pi's `provider/id` form (e.g. `openai-codex/gpt-5.5`); omit `model` to
> run against whatever provider and model your local Pi CLI defaults to.

### `model`

The specific model the backend should use, for example `claude-opus-4-8` on Claude
Code, `gpt-5` on Codex, or `grok-4.6` on Grok Build. A model id is **backend-specific**, so pinning one
requires a pinned `backend`: a `model` under `backend: any` is rejected when the
registry loads, since Bastion cannot know which backend the id is meant for.

```yaml
backend: codex
model: gpt-5
```

Under `backend: pi` the model also names its **provider**, written in Pi's
`provider/id` form, because Pi is multi-provider and its bare default provider is
`google`. So a Pi reviewer that wants an OpenAI Codex model writes the provider into
the id rather than a separate field:

```yaml
backend: pi
model: openai-codex/gpt-5.5
```

Omit it to take the backend's default. On Claude Code that default is **Opus 4.8**;
on Codex, Pi, and Grok Build it is whatever the harness itself resolves (for Pi, its
configured default provider and model; for Grok Build, `grok models` shows it). To set a model once for the whole registry rather than
per reviewer, use the [`defaults`](#registry-wide-defaults) block.

### `effort`

The reasoning-effort level, forwarded verbatim to the active backend's effort
control (Claude Code's `--effort`, Codex's `model_reasoning_effort`, Pi's
`--thinking`, Grok Build's `--reasoning-effort`). Like `model`, the value is opaque:
use whatever vocabulary your backend accepts. Claude Code takes `low`, `medium`,
`high`, `xhigh`, or `max`; Codex takes `minimal`, `low`, `medium`, or `high`; Pi
takes `off`, `minimal`, `low`, `medium`, `high`, or `xhigh`; Grok Build takes `low`,
`medium`, `high`, or `xhigh`. The shared `low`/`medium`/`high` levels work on any
backend; the backend-specific ones do not, so a value that does not match the
reviewer's backend is the backend's problem (Claude Code, for instance, warns and
falls back to its own default; Grok Build exits with an error, which fails a gate
closed).

```yaml
effort: high
```

The default is **`high`** (accepted by every backend). Lower it on cheap,
mechanical reviewers to save tokens; raise it on the ones that need to reason hard.

> **The `model:effort` shorthand.** People often write a model and effort together
> as `gpt-5.5:high` or `claude-opus-4-8:max`. Bastion has no combined field: that is
> just `model:` plus `effort:`. Split it across the two fields, with a `backend`
> pinned so the model id is unambiguous:
>
> ```yaml
> backend: codex
> model: gpt-5.5      # the part before the colon
> effort: high        # the part after it
> ```

### `timeout`

A per-reviewer wall-clock limit, written in human form (`90s`, `15m`). When a
reviewer exceeds it, a gate fails closed (block) and an advisor is skipped. The
default is **15 minutes**. Set a short timeout on cheap reviewers and a long one on
heavy end-to-end checks:

```yaml
timeout: 15m
```

### `attestation`

Set to `never` to ask for fresh execution every time, on both surfaces: the
reviewer is never replayed from a signed local run in CI (even when the
repository sets `attestations: true`), and a re-run never carries its prior pass
forward, locally or in CI (see
[the local workflow](./local-workflow.md#re-runs-are-incremental)). Absent
means both apply: CI may replay this reviewer's terminal verdict or skip outcome
from an attested run, and a re-run may carry its unchanged pass. Use `never` for
a gate your team wants executed unconditionally regardless of any prior run. See
[Attesting a run so CI can replay it](./continuous-integration.md#attesting-a-run-so-ci-can-replay-it).

```yaml
attestation: never
```

### `env`

Environment variables injected into the reviewer's process, so the agent and any
tool it runs can see them. Use this to hand a reviewer a value your environment
already provides, say a preview URL:

```yaml
env:
  PREVIEW_URL: http://localhost:3000
```

Values are **literal**: Bastion does not perform shell `$VAR` expansion, so write
the actual value, not `${SOMETHING}`. Bastion consumes environments, it does not
provision them: locally the value must already exist (a precommit script might boot
the service and export it), and in CI the workflow stands it up. See
[Continuous integration](./continuous-integration.md#environments--inputs).

How the value reaches the agent depends on where the reviewer runs:

- **Native reviewers** (no `runner`) also inherit Bastion's own environment, so a
  variable your shell or CI has already exported is visible to the agent even
  without listing it here; the `env` block sets additional values explicitly.
- **Containerized reviewers** (with a `runner` and `capabilities.network: true`) do
  *not* inherit Bastion's arbitrary environment. Into the container go exactly the `env` pairs written here
  (as literal values, the same as everywhere else) plus a fixed set of
  model-provider credential variables (see [Backends](./concepts.md#the-backend)).
  Nothing else crosses, so a value an outer shell or CI job exported reaches a
  containerized reviewer only if its literal value is written into this `env` block
  (template the registry if the value is dynamic, for example a per-PR preview URL).
  For a containerized reviewer the `env` pairs are written to a temporary file handed
  to the engine as `--env-file`, so their values never appear on the `docker run`
  command line (a secret in `env` stays out of a process listing) and their names
  never touch the engine *client* process; the provider credentials are the only
  variables forwarded by name from Bastion's own environment. If you set one of those
  provider credential names in this `env` block, your value wins: Bastion does not also
  forward the host's value for that name, so the reviewer's `env` overrides it (matching
  how a native reviewer's `env` overrides the inherited environment). One container-only
  constraint follows from that env-file format (one `KEY=VALUE` per line, no escaping):
  a containerized reviewer's `env` cannot carry a key containing a newline or `=`, or a
  value containing a newline. Such a pair is rejected and the reviewer fails closed
  rather than receive a corrupted value; a multiline value (a PEM key, say) has to
  reach a containerized reviewer some other way (a file in the image, or one its
  Dockerfile copies in). Native reviewers have no such limit.

### `inputs`

Values interpolated into the prompt *before* it reaches the agent. Reference an
input as `${name}` in the prompt; Bastion substitutes the value. Unknown
placeholders are left untouched.

```yaml
inputs:
  preview_url: http://localhost:3000
prompt: |
  Run the checkout flow against the preview environment at `${preview_url}`.
  If it fails, block the PR and explain; otherwise approve it.
```

`env` puts a value in the *process*; `inputs` puts a value in the *prompt text*.
They are independent: use `env` for tools the agent invokes, `inputs` for values
the agent should read in its instructions. Input values are literal as well: a
`${name}` in the prompt is substituted only from this `inputs` map, never from your
shell environment.

### `runner` and `capabilities`

The schema also accepts a `runner` block (`dockerfile` / `image`) and a
`capabilities` block (`network`, `mcp`, `skills`) to opt into an execution
environment beyond the least-privilege default. Where these stand:

- **`runner` is provisioned (paired with `network: true`).** A reviewer with a
  `runner` block and `capabilities.network: true` runs its backend
  inside a container: a `dockerfile` is built (tagged by a content hash of the
  Dockerfile, so an unchanged file reuses the engine's layer cache), an `image` is used
  as-is (the engine pulls it on demand at run time). If both are set, `dockerfile`
  wins; a `runner` with neither
  fails closed. The `dockerfile` path is relative to the repository root and must
  resolve inside it: an absolute path, any path with a `..` component (rejected
  outright, even one that would resolve back inside), or one that canonicalizes outside
  the repo through a symlink all fail closed. The build runs
  with the repository root as its build context, so the Dockerfile's `COPY` and `ADD`
  can reference files anywhere in the repo. An `image` reference beginning with `-`
  fails closed, since the engine would read it as a command-line option rather than an
  image name. The selected backend's executable must exist inside the image on `PATH`
  (`claude` for `claude-code`, `codex` for `codex`, `pi` for `pi`, `grok` for `grok`).
  This lets a
  reviewer carry tools or a pinned toolchain the host does not have.
- **`capabilities.network: true` is required to run a container; the default
  `network: false` fails closed.** `network: true` gives a containerized reviewer
  general (unscoped) outbound network. A container's egress cannot be scoped to the
  model provider yet (the allowlisting proxy is unbuilt), so the default
  `network: false` reads as restricted but cannot be enforced: rather than silently
  attach general egress, `ExecutionPlan::resolve` rejects a container with
  `network: false` before it runs. As with `mcp`/`skills`, that rejection **fails
  closed**: a gate blocks and an advisor is skipped, with a message naming the field. A
  containerized reviewer must opt into `network: true` to run, accepting general egress
  for now. A *native* `network: true` (no `runner`) also fails closed, since with no
  container there is nothing to scope.
- **`capabilities.mcp` and `capabilities.skills` are not provisioned.** A
  reviewer that declares either **fails closed**: a gate blocks and an advisor is
  skipped, with a message naming the unprovisioned field, rather than running
  degraded (a gate that quietly ran without a privilege it asked for would be a
  silent fail-open). Leave them out.

The least-privilege default (no `runner`, `network: false`, no `mcp` or `skills`)
runs natively on the host.

## A fully-loaded example

Putting the optional fields together. As written, this reviewer runs in the container
built from its Dockerfile. It must declare `network: true` to run (a containerized
reviewer needs general egress, since provider-only scoping is unbuilt), and Bastion
forwards its `env` into that container.

```yaml
reviewers:
  - name: e2e-checkout-flow
    trigger: [src/**]
    mode: gate
    backend: claude-code
    timeout: 15m
    env:
      PREVIEW_URL: http://localhost:3000     # literal value, no shell expansion
    inputs:
      preview_url: http://localhost:3000     # substituted into the prompt as ${preview_url}
    runner:                                  # provisioned: runs the backend in this image
      dockerfile: ./.bastion/e2e.Dockerfile
    capabilities:
      network: true                          # required to run a container; grants general (unscoped) egress
    prompt: |
      Run the e2e checkout flow against the preview environment at `${preview_url}`
      using Playwright. If it fails, block the PR and explain; otherwise approve it.
```

Adding an unprovisioned capability flips the whole reviewer to fail closed. For
example, adding `mcp: [playwright]` under `capabilities` would block this gate before
it ever reaches the container, since `mcp` is checked first. Leave `mcp` and `skills`
out until those tiers land.

## Writing a good prompt

The prompt is the reviewer. A few habits keep recall high:

- **Say what to block on, explicitly.** End with a clear instruction: "block the
  PR if X; otherwise approve it." The reviewer's job is a decision, not an essay.
- **Name the one concern and stay on it.** If you find yourself writing "also
  check...", that "also" is a second reviewer. Split it.
- **Carve out the false positives you can predict.** "A single large but cohesive
  module is not a violation." "Panics in `#[cfg(test)]` code are acceptable."
  Pre-empting the obvious wrong flags keeps false positives down.
- **Match the mode to the language.** A gate's prompt should be decisive; an
  advisor's should say "report as optional findings... do not block," so its
  output stays advisory even if the model is tempted to be firm.
- **Let the agent explore.** Every reviewer gets a full checkout and is told how to
  see the changeset (the diff against the merge base, plus untracked files). You do
  not need to paste the diff into the prompt; point the reviewer at the property.
- **You do not need to ask for completeness.** Bastion appends an instruction to
  every reviewer prompt telling the agent to report every distinct finding in one
  pass, not just the first. Write the prompt for the concern and phrase findings
  per instance (one per file and line range), and the agent enumerates them all so
  the author fixes the whole set from one run.

Some worked examples, taken from Bastion's own registry
([`.bastion.yaml`](https://github.com/attunehq/bastion/blob/main/.bastion.yaml)):

```yaml
  - name: error-handling
    trigger: [src/**/*.rs]
    mode: gate
    backend: codex
    prompt: |
      Review the changeset for error-handling discipline: no `.unwrap()` or
      `.expect()` on recoverable errors in non-test code, errors propagated with
      `?` and given context, and gates that fail closed. Block the PR if you find
      a recoverable error that can panic in production; otherwise approve it.
      Panics in `#[cfg(test)]` code and in genuinely-unreachable invariants that
      are documented as such are acceptable.

  - name: test-coverage
    trigger: [src/**/*.rs]
    mode: advisor
    backend: codex
    prompt: |
      Check whether new or changed behavior in this changeset is covered by
      tests. This is advisory: report uncovered behavior as optional findings so
      the author can decide, but do not block.
```

## Validating your registry

Run `bastion validate` to parse the registry and report any problem without running
a single reviewer or spending a model call:

```sh
bastion validate                          # validate the default set review would run
bastion validate --with-user-reviewers    # validate the explicit merged set
bastion validate path/to/.bastion.yaml    # check one registry and its include tree
```

With no file argument it validates the same set a local `bastion review` would run:
the repository registry when one exists, otherwise your user-level registry. Pass
`--with-user-reviewers` to validate the merged set. The output names each source it
loaded and lists every included registry file and prompt file it pulled in. An
explicit `FILE` rejects `--with-user-reviewers` and still resolves the
file fully: its `include:` entries, its prompt files, and any `--include` flags
all load, as they would on a real review. It loads through the same path `bastion review` uses, so
it catches exactly the errors a real review would hit at load time: malformed
YAML, an unknown field, a duplicate name (including one that survives the
user/repo merge or arrives via an include), a reviewer missing a required field,
an empty or unreadable prompt file, a missing include, or a model pinned under
`backend: any`. A valid registry prints a one-line summary and the reviewers it
parsed, and exits zero; an invalid one prints the error and exits non-zero, so
the command works as a pre-commit or CI lint as well as a quick local check.

The registry is also validated whenever it loads for a real `bastion review`, so a
malformed file fails fast there too. `bastion validate` just lets you check it on its
own, for free, before you run anything.

---

Next: [The local workflow](./local-workflow.md). Running `bastion review` in
depth, the JSONL agent stream, and inspecting saved runs.
