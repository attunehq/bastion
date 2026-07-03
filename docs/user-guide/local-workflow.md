---
title: The local workflow
summary: "Running bastion review for real: the loop, the two output formats, exit codes, and inspecting what was saved."
order: 5
---

# The local workflow

> Running `bastion review` for real: the loop, the two output formats, exit codes,
> and inspecting what was saved.

The local CLI is the surface an authoring agent optimizes against before opening a
PR. It runs the *same* reviewers CI will run, so a green local loop usually means a PR
that CI confirms. Two things can make a local run differ: CI feeds reviewers the PR's
description and discussion that a default local run lacks, and a local run also merges
in any personal reviewers from your user-level registry, which CI never sees (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)). This chapter
covers the loop in depth.

## The loop

The intended use is a tight loop: run the review, read what blocks, fix it, run
again, until green.

```sh
bastion review --base main
```

`bastion review` computes the changeset (working tree vs. `--base`, including
uncommitted and untracked files), selects the reviewers whose triggers match, runs
them in parallel with per-reviewer timeouts, and renders progress and verdicts.

- `--base <branch>`: the branch to diff against. Defaults to `main`.
- `--format <human|jsonl>`: output format. Defaults to `human`.
- `--repo <owner/name>`: the GitHub repository to gather pull request context from. Defaults to `$GITHUB_REPOSITORY`.
- `--pr <number>`: the pull request whose description and discussion the reviewers read as context. Requires a repository, from `--repo` or `$GITHUB_REPOSITORY`; passing `--pr` with no repository is an error.
- `--config-dir <path>`: the user-level config directory to merge personal reviewers from (env `BASTION_CONFIG_DIR`). Defaults to your platform config directory (`~/.config/bastion` on Linux, `~/Library/Application Support/bastion` on macOS, `%APPDATA%\bastion` on Windows). The user-level layer is applied only to a purely local review; a review carrying `--repo`/`--pr` uses the repository's reviewers alone.

The CI workflow passes `--repo`/`--pr` so reviewers see the PR's stated intent and discussion. Locally you rarely need them: with no PR, intent comes from your branch's commit messages (`base..HEAD`), and each reviewer's prior findings come from the run store. When you do pass them, Bastion builds its GitHub REST client from `GITHUB_TOKEN` and `GITHUB_API_URL` (the latter defaults to the public API and points at a GitHub Enterprise host when set). Discussion gathering reads the first 100 conversation comments and the first 100 review comments and does not paginate, so later comments on a very long thread are not included. Gathering PR context is read-only and best effort, so an API or token failure never fails the review; it just drops back to the local context.

### Exit codes

The exit code *is* the gate, so a loop can branch on it:

| Aggregate verdict | Exit code |
| --- | --- |
| `pass` (all gates passed) | `0` |
| `block` (a gate blocked, errored, or timed out) | non-zero |

```sh
# Keep working until every gate is green.
until bastion review --base main; do
  echo "still blocked; fixing..."
  # ... make changes ...
done
```

A blocked review is an *expected* outcome, not a crash: Bastion still exits
cleanly with structured output, and only the code signals the gate.

## Two audiences, two formats

By default `bastion review` renders human-readable progress for a person watching.
An agent passes `--format jsonl` and gets a machine stream instead. Both describe
the same run; only the presentation differs.

### The JSONL stream

With `--format jsonl`, Bastion emits one JSON object per line, as each thing
happens. A run is a typed sequence of events:

```jsonl
{"type":"run.started","run":"r-0f3a","branch":"feat/cart","base":"main","changed":12,"reviewers":[{"name":"file-responsibility","mode":"gate"},{"name":"tenant-isolation","mode":"gate"}]}
{"type":"reviewer.started","run":"r-0f3a","reviewer":"tenant-isolation","mode":"gate","backend":"claude-code"}
{"type":"reviewer.resolved","run":"r-0f3a","reviewer":"tenant-isolation","verdict":"block","summary":"A new query path reads rows without scoping by tenant id.","findings":[{"kind":"blocking","path":"src/server/db.rs","line_start":88,"line_end":91,"detail":"scope this query by tenant_id"}],"usage":{"tokens_in":18204,"tokens_out":1560,"cache_read":12000,"cost_usd":0.21},"duration_ms":38120,"has_transcript":true}
{"type":"run.completed","run":"r-0f3a","verdict":"block","gates":{"total":2,"passed":1,"blocked":1},"duration_ms":41030,"tokens_in":20480,"tokens_out":1875,"cache_read":13100,"cost_usd":0.37}
```

The event types:

| Event | Meaning |
| --- | --- |
| `run.started` | The run began; lists the reviewers that matched and will run. |
| `reviewer.started` | One reviewer was dispatched. |
| `reviewer.resolved` | One reviewer finished; carries its `verdict`, `summary`, `findings`, `usage`, and a `has_transcript` flag. Carries `replayed: true` when the verdict came from a verified attestation instead of a fresh execution. |
| `run.completed` | The aggregate decision and the gate tally, plus the run's wall-clock `duration_ms` and the usage totals (`tokens_in`, `tokens_out`, `cache_read`, `cost_usd`) summed across reviewers. |
| `run.attested` | A signed local run was replayed; carries the replayed `reviewers`, the attesting `public_key`, and `attested_at`. |
| `run.attestation-fallback` | Attestation was attempted but not honored; carries the `reason` (a missing note, an unregistered key, a stale binding, and so on). |

How an agent should consume it:

- **Only need the outcome?** Ignore everything until `run.completed` and read its
  `verdict`.
- **Want to react as you go?** Read each `reviewer.resolved` as it lands and act on
  its `findings`: a `path`, a `line_start`/`line_end`, and a `detail` telling you
  what to change. The findings are everything you need to fix the code.

### For agents: the consumption contract

If you are an agent driving the loop, this is the whole contract:

1. Run `bastion review --base <branch> --format jsonl`.
2. Parse stdout one line at a time as JSON; each line has a `type`.
3. Act on every `reviewer.resolved` with `verdict: "block"` using its `findings`
   (`path` + `line_start`/`line_end` + `detail`). Do not open transcripts; the
   findings already say what to change.
4. The aggregate decision is `run.completed.verdict`. The process also exits
   non-zero on `block`, so you can branch on the exit code alone if you only need
   pass/fail.
5. Fix what blocked and re-run. Loop until `run.completed.verdict` is `pass` (exit
   zero), then open your PR.

This contract is exactly what `bastion skills install` checks into your repo as the
`using-bastion` agent skill, so your agents follow it without being told each time.
See [Teach your agents to use Bastion](./getting-started.md#7-teach-your-agents-to-use-bastion).

### The skills-freshness notice on stderr

Before it runs, `bastion review` compares the `using-bastion` skill checked into your
repo (under `.claude/skills` and `.agents/skills`) against the copy bundled in the
running binary, the same comparison `bastion skills check` makes. When the checked-in
copy is missing or has drifted, it prints a one-line notice to **stderr** naming the
affected files and pointing at `bastion skills install`. This is the case where your
agents may be following stale guidance, so the driving agent sees the notice inline
with the run.

It goes to stderr on purpose, keeping stdout as pure JSONL for a parser; the notice is
advisory, so it never adds an event to the stream and never changes the exit status. A
`block` still comes only from a reviewer. Run `bastion skills install` (add `--force`
to overwrite a file you edited) and commit the result to clear it.

The notice appears only when this repository has adopted Bastion, meaning a repo-level
reviewer registry is present: a `.bastion.yaml`, its `.bastion.yml` spelling, or the
deprecated `bastion/reviewers.yaml`, the same registry discovery a review already does.
If your review is running solely on your own
[user-level reviewers](./authoring-reviewers.md#user-level-reviewers) in a repo that
has not configured Bastion, the notice stays silent: installing skills into a project
that has not adopted Bastion would be beside the point.

### Money is dollars

Cost fields (`cost_usd`) serialize as dollars (`0.21`) even though Bastion tracks
exact cents internally, so you never see floating-point cent drift in the stream.
Token fields (`tokens_in`, `tokens_out`, `cache_read`) are plain integer counts;
on `run.completed` they are the totals summed across every reviewer that reported
usage, the same way `cost_usd` is. `cache_read` is the input tokens served from the
provider's prompt cache (cache hits); each backend names it differently natively
(Claude's `cache_read_input_tokens`, Codex's `cached_input_tokens`, Pi's
`cacheRead`) and Bastion normalizes them to one field. It is 0 when a backend
reports no cache usage.

## What is streamed vs. what is saved

The stream deliberately leaves out the verbose detail. A transcript is mostly noise
to an agent that just wants to know what to fix; streaming thousands of lines on
every run would bury the findings and burn the agent's own context.

- **Streamed:** the decisions and the things you act on immediately: the reviewer
  set, start and resolve events, verdicts, summaries, findings, per-reviewer usage.
- **Saved, not streamed:** the verbose detail: full session transcripts, raw
  verdict payloads, per-reviewer metadata. Written to disk, read on demand.

That is why `reviewer.resolved` carries `has_transcript: true` rather than the
transcript itself: when a decision surprises you, the transcript is one command
away (next section).

## Inspecting saved runs

Every run is persisted, so you can inspect history without re-running anything.
These commands are the local equivalent of clicking "Details" on a CI check. The
run-targeted ones (`show`, `transcript`) default to the latest run when you omit a
run id; `runs` and `clean` operate over all saved runs.

```sh
bastion runs                         # list recent runs: id, verdict, branch, reviewer count
bastion show [<run>]                 # re-print a run's summaries, verdicts, findings
bastion transcript [<run>] <reviewer>   # the full agent session for one reviewer
bastion clean [--keep N | --older-than <dur>]   # prune saved runs
```

- **`runs`** is the index: what ran recently and how each landed.
- **`show`** re-emits a past run's verdicts and findings, the same content as the
  stream's resolve and complete events, on demand. Accepts `--format human|jsonl`.
- **`transcript`** prints the saved session for one reviewer. This is the explicit,
  opt-in way to see what was kept off the stream; reach for it when a verdict is
  surprising and you want to know why. It is raw text (a transcript is already a
  document). Pass either `<reviewer>` (latest run) or `<run> <reviewer>`.
- **`clean`** prunes old runs. `--keep N` retains the N most recent;
  `--older-than <dur>` (e.g. `7d`, `12h`) removes runs older than a duration. The
  two are mutually exclusive.

## Where runs live

Bastion persists every run under a per-user data directory, by platform
convention:

- Linux: `$XDG_DATA_HOME/bastion`, default `~/.local/share/bastion`
- macOS: `~/Library/Application Support/bastion`
- Windows: `%APPDATA%\bastion`

Override it with `--data-dir <path>` or the `BASTION_DATA_DIR` environment
variable, handy for scratch runs you do not want in your real history. The layout:

```text
<data-dir>/
  runs/
    r-0f3a/
      run.jsonl                  # the full event stream (always JSONL, regardless of display format)
      reviewers/
        tenant-isolation/
          transcript.jsonl       # the full agent session
          verdict.json           # the raw structured verdict
          meta.json              # backend, timing, usage, matched trigger
    latest                       # a plain file holding the most recent run id
```

`run.jsonl` is the same event stream whether a human or an agent triggered the
run, so any run can be replayed or inspected after the fact. Runs accumulate:
`bastion review` does not prune, so history grows until you run `bastion clean`,
which keeps the most recent 20 when given no arguments (or use `--keep N` /
`--older-than <dur>`).

## Providing environments locally

For a **native** reviewer, the reviewer process inherits Bastion's own environment,
so anything your shell or a `precommit` script has exported (a service on
`http://localhost:3000`, say) is visible to the agent; a reviewer's `env` and
`inputs` values are literal text set in the YAML, not shell-expanded. Bastion only
reads values your shell or CI already exported; it does not stand them up. This is
the same boundary CI honors, which keeps the local and CI surfaces in agreement.

A **containerized** reviewer (one with a
[`runner`](./authoring-reviewers.md#runner-and-capabilities), which today must also set
`capabilities.network: true` to run) does not inherit your shell environment, since it
runs in a container. Into it go the reviewer's literal
`env` pairs plus a fixed provider-credential set, and nothing else. (If the reviewer's
`env` sets one of those credential names, its value wins and the host's is not also
forwarded.) So an exported `PREVIEW_URL` that a native reviewer would see for free
reaches a containerized one only if you write its literal value into that reviewer's
`env`, and a containerized
reviewer typically reaches a host service over the container network rather than
`localhost`.

## Attesting a run for CI

Every reviewer is an agent invocation, so a project running Bastion both locally
and in CI pays for each review roughly twice: once in your loop, once again when
CI confirms it. If your repository has set `attestations: true` in its registry
(see [Continuous integration](./continuous-integration.md#attesting-a-run-so-ci-can-replay-it)),
you can sign your last green local run so CI reuses it instead of re-running
every reviewer:

```sh
bastion review --base main   # ends green
bastion attest                # signs the run that just finished
git push origin refs/notes/bastion
```

`bastion attest [RUN]` takes an optional run id positional; omit it and it signs
the latest recorded run, which is what you want right after `bastion review`.
Pass one explicitly (`bastion attest r-0f3a`) to attest an older run instead.

`bastion attest` re-checks that your repository has not moved on since the
review (the same tree, the same diff, the same effective reviewer config) and
refuses to sign if it has, so the note can never claim the reviewers saw
something they did not. It signs with your SSH key (`git config
user.signingkey`, or `--key <path>` to name one explicitly), prompting for a
hardware token or keychain if your key requires it, and prints the exact push
command. The signed bundle carries the run's actual verdicts and findings, not
just a pass/fail flag, so a blocked local run that gets attested and pushed
still blocks in CI.

Push the printed command (or fold it into your normal `git push`) before
opening the PR, so CI has the note when it runs. CI verifies the signature
against the SSH signing keys you have registered with GitHub, so this only
works for a key you have added there; a key freshly generated on the machine
you are pushing from, with nothing registered, never verifies.

Whether to use a plain key file or a presence-gated one (a hardware token or an
OS keychain entry that prompts you per signature) is your call to make. A plain
file key means an agent running on your machine could sign an attestation
without you noticing, the same trust you already extend to that machine
through your commit access. See [Attestation](https://github.com/jssblck/bastion/blob/main/docs/developer-guide/attestation.md)
for the full trust model.

## The same surface in CI

For the repository's reviewers, these local events are not a separate system from CI;
they are the same decisions in a finer-grained form. Each such JSONL event has a
GitHub twin (a check run, a comment, an annotation), laid out side by side in the
[Continuous integration](./continuous-integration.md#how-a-run-maps-to-github)
chapter. A green local loop predicts a green PR when both runs see the same reviewers
and context. The two surfaces run the repository's reviewers and aggregation, and CI
adds the PR's description and discussion that a default local run does not, so a
reviewer that weighs that context can decide differently. A purely local run can also
include your personal user-level reviewers; their `run.started` and
`reviewer.resolved` events are local-only and never become checks or comments (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)).

---

Next: [Continuous integration](./continuous-integration.md). Promoting these same
reviewers into GitHub Actions as a required merge check.
