---
title: Concepts
summary: "The vocabulary Bastion runs on: reviewers, triggers, modes, verdicts, and the merge gate."
order: 3
---

# Concepts

> The vocabulary Bastion runs on: reviewers, triggers, modes, verdicts, and the
> merge gate.

This chapter defines the terms the rest of the guide uses. It is short on purpose;
each idea has a deeper home later, linked as it comes up.

## The reviewer

A **reviewer** is the unit of the system: a focused agent prompt responsible for
exactly one property of a changeset. It is a bundle of *prompt + trigger + mode*,
plus an optional execution profile (backend, timeout, environment, inputs, a
container `runner`, and `capabilities`, among others). All of it is declared
statically in `.bastion.yaml`; [Authoring reviewers](./authoring-reviewers.md)
is the full field reference. The repository's `.bastion.yaml` is the shared, governed
set; locally you can also keep fallback reviewers in a user-level `.bastion.yaml`,
or merge them explicitly with `--with-user-reviewers` (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)).

Two properties matter most:

- **Single concern.** A reviewer checks one thing and checks it well. You scale
  coverage by adding reviewers, never by widening one. This is what keeps recall
  high (see [Introduction](./introduction.md#the-core-idea)).
- **Declarative and static.** Reviewers are data, not code. Bastion never
  generates them on the fly. That keeps the trigger set stable and makes every
  reviewer reviewable, which is the foundation of [governance](./governance.md).

## The trigger and the changeset

A reviewer's **trigger** decides whether the reviewer applies to a changeset.
The usual form is an ordered list of path globs; the reviewer runs when at least
one changed file is included. A leading `!` excludes a path, and the last
matching pattern wins. A docs-only change then wakes the docs reviewers and
nothing else.

```yaml
trigger: [src/server/**, src/client/**]   # runs when server or client code changed
trigger: ["docs/**", "!docs/audit-reports/**"]   # skips audit snapshots
```

For a concern that paths cannot identify narrowly, `kind: agent` asks a cheaper
model whether the full reviewer applies. Optional `paths` run first as a cheap
prefilter. The agent sees the actual changeset, and any failure or uncertainty
runs the full reviewer. A confident skip is recorded separately from a pass
verdict. See [Agent triggers](./authoring-reviewers.md#trigger) for the schema and
cost model.

The **changeset** is everything in your working tree that differs from the point
where your branch forked from the base branch (the merge base), *including
uncommitted edits and new untracked files*, not just committed history. This is
deliberate on both ends. Including uncommitted work lets an author loop against
reviewers before committing anything. Diffing at the merge base rather than the
base branch's tip means the changeset is only ever *your* work: changes that
landed on the base after you forked are never routed on, never shown to a
reviewer, and never flagged as yours. (Locally, this means a reviewer sees your
work in progress; in CI the head is already committed, so the same definition
gives the same result.)

## The mode: gate vs. advisor

Every reviewer has a **mode** that decides whether it can block a merge:

| Mode | Blocks the merge? | On crash/timeout/bad output |
| --- | --- | --- |
| `gate` | Yes, when it returns `block` | **Fails closed**: resolves to `block` |
| `advisor` | No, ever | **Fails open**: ignored in the aggregate |

A **gate** is a hard requirement when its trigger says the reviewer applies: the
full reviewer must produce a clean `pass` for the merge to proceed. An agent trigger
may instead record a semantic skip, which is counted separately from a pass. If an
applicable gate crashes, times out, or cannot produce a valid verdict, it resolves
to a block, never a silent pass. An **advisor** comments but never holds up the
merge; even a clean `block` verdict from an advisor is treated as a pass for
aggregation, and its findings are recorded as `optional` (an advisor's findings
are advice, never a merge blocker) so they still surface as suggestions. A failed
advisor is dropped.

Use a gate for properties that must hold (tenant isolation, fail-closed error
handling). Use an advisor for guidance you want surfaced but not enforced (test
coverage, doc gaps, style preferences).

## The verdict

Every full reviewer execution returns a structured **verdict**, captured through the
backend's structured-output mechanism (a JSON schema for Claude Code, a requested
verdict block for Codex) so Bastion can parse and aggregate it. An agent trigger
that skips the full reviewer records `reviewer.skipped` instead, with no verdict or
findings. A full reviewer's verdict has this shape:

```yaml
verdict: pass | block    # the authoritative gate decision (ignored for advisors)
summary: "..."           # a human-friendly one-paragraph explanation
findings:                # specific, located comments
  - kind: blocking       # blocking | optional
    path: src/server/db.rs
    line_start: 88
    line_end: 91
    detail: "scope this query by tenant_id"
```

The top-level `verdict` is the decision; `findings` explain it. A `block` should
carry at least one `blocking` finding (the reason), and a `pass` may still carry
`optional` findings as non-blocking suggestions. A finding's `kind` changes how it
is *surfaced*, not whether the merge proceeds; only `verdict` decides that. A
`pass` never carries a `blocking` finding: the two would contradict each other.
Because an advisor is always resolved to a `pass`, its findings are recorded as
`optional` regardless of what the reviewer emitted.

**Findings are the actionable surface.** An agent fixing a PR gets everything it
needs from the findings: a file, a line range, and what to change. It should never
have to open a transcript to learn what to do.

A reviewer reports the complete actionable set in one pass, one finding per
distinct instance, not just one representative reason. The author can then fix
everything from a single run instead of meeting the next issue on the following
review cycle. Bastion requests this from every reviewer automatically, so a prompt
does not need to ask for it.

## The merge gate

Bastion resolves the reviewer candidates in parallel (they have wildly different
latencies, one might take 90 seconds, another 15 minutes) and **aggregates** their
terminal outcomes into a single decision. A candidate may execute, record an
agent-trigger skip, replay from a verified attestation in CI, or carry an unchanged
prior pass on a re-run:

- **Every applicable gate must pass.** An agent trigger may decide its reviewer
  does not apply; that gate increments `gates.skipped` without producing a pass
  verdict. The aggregate is `pass` when every gate that did apply passed.
- **Any blocked, errored, or timed-out gate blocks the aggregate.** "All gates
  pass" never includes a gate that failed to produce a verdict.
- **Advisors never affect the aggregate.** They contribute findings, not gate
  decisions.

Locally, that aggregate is the exit code of `bastion review`. In CI it is the result
of the Bastion review job, and `bastion github report` also posts it as a single
always-present check named `bastion`. Either way the aggregation rule is the same, and
CI runs the repository's reviewers. The decision matches when both runs see the same
reviewers and context; two things can make a local run differ: CI can add the PR's
description and discussion that a default local run does not, and a purely local run
can include your personal user-level reviewers with `--with-user-reviewers`, which
CI never runs (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)).

## The backend

A **backend** is the agent harness a reviewer runs on. Bastion does not implement
its own agent loop; it translates the reviewer into the backend's native config and
shells out to its CLI, reusing your local auth and billing.

- `any` (the default): Bastion chooses; that resolves to Claude Code.
- `claude-code`: Anthropic's Claude Code CLI.
- `codex`: OpenAI's Codex CLI.
- `pi`: the Pi CLI; uses whatever provider you have configured it with locally,
  unless a reviewer pins a `model` (Pi's `provider/id` form selects the provider too).
- `grok`: xAI's Grok Build CLI.

You pin a backend when a subscription's terms require a specific harness, or when
one model is better at a given concern. See
[Authoring reviewers](./authoring-reviewers.md#backend) and, for CI
billing, [Continuous integration](./continuous-integration.md#authentication--billing).

By default the backend CLI runs **natively** on the host, using the `claude`,
`codex`, `pi`, or `grok` already on your `PATH` and the auth and billing that CLI is
configured with.
A reviewer that declares a [`runner`](./authoring-reviewers.md#runner-and-capabilities)
instead runs that same backend **inside a container** (which requires
`capabilities.network: true`; without it the reviewer is rejected before it runs, so a
gate blocks and an advisor is skipped): Bastion invokes the container engine on the
host, and the backend CLI resolves inside the image. A fixed set of
model-provider credential variables (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`,
`OPENAI_BASE_URL`, `CODEX_API_KEY`, `XAI_API_KEY`) is forwarded from Bastion's environment into the
container by name, so the in-container agent can still reach its provider; an image
can also bake in its own auth. If the reviewer's own `env` sets one of those names,
that value wins and the host's is not also forwarded, so the reviewer can pin a
specific credential. Nothing else from your host environment crosses that boundary. To
give the in-container agent another value, set it as a literal in the reviewer's `env`,
which is forwarded in alongside the credentials. The fixed set covers the Anthropic,
OpenAI, and xAI variables only, so a containerized Pi reviewer on another provider
authenticates from auth baked into its image or from a credential written into its
`env`.

## How it all fits

```text
.bastion.yaml                 you author this
        |
        v
   bastion review  --->  compute changeset (working tree vs merge base)
        |
        v
   route: apply path prefilters, then resolve agent triggers
        |
        v
   run matched reviewers in parallel; a reviewer may instead replay from a verified
   attestation (CI), or carry an unchanged prior pass forward from the branch's
   previous run (local or CI), both with no backend dispatch (each executed reviewer
   is timeout-bounded)
        |
        v
   each returns a verdict, or records an agent-trigger skip
        |
        v
   aggregate: every applicable gate must pass  --->  one decision (exit code locally;
                                                       the review gate in CI)
```

---

Next: [Authoring reviewers](./authoring-reviewers.md). The full registry schema,
from the four required fields out to timeouts, environment, and prompt inputs.
