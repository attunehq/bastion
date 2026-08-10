---
title: The local workflow
summary: "Running bastion review for real: the loop, the two output formats, exit codes, and inspecting what was saved."
order: 5
---

# The local workflow

> Running `bastion review` for real: the loop, the two output formats, exit codes,
> and inspecting what was saved.

The local CLI applies the same reviewers and decisions CI enforces: CI executes them
fresh, records an agent-trigger skip, replays an attested local outcome, or carries
an unchanged reviewer forward from the branch's previous CI run. So a green local loop usually means a PR that CI
confirms. Two things can make a local run differ: CI feeds reviewers the PR's
description and discussion that a default local run lacks, and a local run can merge
in personal reviewers with `--with-user-reviewers`, which CI never sees (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)). This chapter
covers the loop in depth.

## The loop

The intended use is a tight loop: run the review, read what blocks, fix it, run
again, until green.

```sh
bastion review --base main
```

`bastion review` computes the changeset (working tree vs. the merge base with
`--base`, including uncommitted and untracked files, and never including the
base branch's own changes), selects reviewer candidates, and renders progress plus
each terminal verdict or agent-trigger skip. Candidates resolve in parallel with
per-reviewer timeouts, and a re-run is incremental (next section): a reviewer that
already passed may carry its verdict forward instead of executing again, locally
and in CI.
A CI review (`--repo`/`--pr`)
against a repository with `attestations: true` first checks for a verified
attestation covering the run: a reviewer the attestation covers replays its
recorded terminal verdict or skip outcome, with no backend dispatch and no timeout;
every other reviewer then resolves the ordinary way, carrying its prior pass if
its scoped content is unchanged and otherwise executing fresh (see
[Attestation](../developer-guide/attestation.md)).

- `--base <branch>`: the branch you are merging into. The changeset is diffed at
  the merge base with it, not at its tip, so the base moving on does not change
  what is under review. Defaults to `main`. The review fails if no merge base
  resolves (an unrelated branch, or a shallow clone). When the base is a local
  branch whose remote-tracking ref would give HEAD a different merge base (a
  local `main` that lags `origin/main`, say), the review warns on stderr that
  the changeset may include upstream commits and suggests reviewing against
  `--base` with the tracking ref, or fetching and bringing the local branch up
  to date with it; the check reads only refs already on disk, never fetches,
  and the review proceeds unchanged.
- `--format <human|jsonl>`: output format. Defaults to `human`.
- `--repo <owner/name>`: the GitHub repository to gather pull request context from. Defaults to `$GITHUB_REPOSITORY`.
- `--pr <number>`: the pull request whose description and discussion the reviewers read as context. Requires a repository, from `--repo` or `$GITHUB_REPOSITORY`; passing `--pr` with no repository is an error.
- `--config-dir <path>`: the user-level config directory to search for personal reviewers (env `BASTION_CONFIG_DIR`). Defaults to your platform config directory (`~/.config/bastion` on Linux, `~/Library/Application Support/bastion` on macOS, `%APPDATA%\bastion` on Windows). Personal reviewers are the fallback when the repository has no reviewer configuration.
- `--with-user-reviewers`: merge personal reviewers into a repository's configured reviewer set. This applies only to a purely local review; a review carrying `--repo`/`--pr` uses the repository's reviewers alone.
- `--include <path>` (repeatable): merge an extra reviewer registry file into the repository registry, like an `include:` entry in the root file except that a relative path resolves against the current directory (see [Splitting the registry across files](./authoring-reviewers.md#splitting-the-registry-across-files)). The extra reviewers become part of the effective repository configuration for the run, so `bastion attest` needs the same `--include` flags to re-derive the same configuration hash.
- `--reviewer <name>` (repeatable; alias `--only`): run only these triggered reviewers. An unknown or untriggered name is an error. Excluding a triggered reviewer makes the run *partial* (see below).
- `--fresh`: disable the incremental carry below, so no reviewer reuses a prior pass (local or CI). It does not affect attestation replay: a `--repo`/`--pr` run still replays reviewers a verified attestation covers.

### Re-runs are incremental

The loop's dominant cost would otherwise be re-executing reviewers that already
passed. So on a re-run of the same branch, a reviewer whose previous verdict was a
pass, and whose *scope digest* is unchanged since that run, is *carried*: its
prior verdict counts in the gate tally, the stream marks it `"carried": true`, and
no agent runs and no tokens are spent on it. The digest covers everything the
verdict was keyed to: the reviewer's own definition, the path-matched diff for
a path trigger or the entire changeset for an agent trigger, the commit
messages that touched the same files, and the content of untracked files in
that scope. So an edit to scoped content, a reworded commit that touched it, or
an edited reviewer re-runs the reviewer; the ones that blocked always re-run,
since your fix touched the files they flagged. Blocks are never carried.

What deliberately does *not* re-run a reviewer: the base branch moving, or a
rebase over it, when your scoped diff comes out identical. The digest binds the
changeset a verdict judged, not the commit it happened to be diffed at, so a
rebase over unrelated upstream changes carries every pass straight through,
while one that changes the diff (a conflict resolution, upstream edits close
enough to shift a hunk's context) re-runs the affected reviewers. What the base
changed was reviewed by its own changesets when it merged; your reviewers judge
only what your branch changes. A path trigger bounds the concern and its carry
digest to the matched files. Agent-trigger `paths` only prefilter whether routing
starts, so an admitted agent-trigger reviewer keys carry to the full changeset.
A reviewer with `attestation: never` in the registry is never carried, and
`--fresh` re-runs everything.

One extra condition applies to the repository's own reviewers (not personal
user-level ones): they carry only from a prior run the binary sealed and can still
verify, with no test seam recorded. A prior run that was never sealed, or whose
seal no longer checks out, executes those reviewers fresh; nothing warns about it,
since carry is an optimization and fresh execution is always correct.

CI carries too. A workflow that persists and restores the run store across pushes
(the example workflow in
[Continuous integration](./continuous-integration.md#the-workflow) includes the
restore and upload steps that do this) lets a push carry an unchanged reviewer
forward from the branch's previous CI run, the same way your local loop does and on
the same verified-seal condition. This is separate
from [attestation replay](#attesting-a-run-for-ci): replay reuses your signed local
run so CI need not re-execute it at all, while carry reuses CI's own prior run when a
later push leaves a reviewer's scoped content untouched.

### Running a subset by hand

`--reviewer <name>` narrows the run to reviewers you name, for iterating on one
stubborn gate without waiting on the rest. The named reviewers never carry a prior
pass, local or CI (asking for a reviewer by name means asking for it to run); on a
`--repo`/`--pr` run, a verified attestation can still replay a selected
reviewer. When the selection excludes at least one triggered reviewer, the run is
marked **partial** everywhere it is recorded: the `run.started`/`run.completed`
events carry `"partial": true`, the human output and `bastion runs` say so, and
the run cannot be attested. (Naming every triggered reviewer is a full run: the
selection reduced nothing, so nothing is marked.) A partial green speaks only
for the reviewers that ran. Finish with a plain `bastion review`: only a full run
seals a real green. Carry spares any reviewer whose scoped content has not moved
since an eligible prior run, but not from the partial run itself: a partial run is
never sealed, so the repository's own reviewers re-execute rather than carry from
it.

The CI workflow passes `--repo`/`--pr` so reviewers see the PR's stated intent and discussion. Locally you rarely need them: with no PR, intent comes from your branch's commit messages (`base..HEAD`), and each reviewer's prior findings come from the run store. When you do pass them, Bastion builds its GitHub REST client from `GITHUB_TOKEN` and `GITHUB_API_URL` (the latter defaults to the public API and points at a GitHub Enterprise host when set). Discussion gathering reads the first 100 conversation comments and the first 100 review comments and does not paginate, so later comments on a very long thread are not included. Gathering PR context is read-only and best effort, so an API or token failure never fails the review; it just drops back to the local context.

### Exit codes

The exit code *is* the gate, so a loop can branch on it:

| Aggregate verdict | Exit code |
| --- | --- |
| `pass` (every applicable gate passed; other gates may be semantically skipped) | `0` |
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
{"type":"run.started","run":"r-0f3a","branch":"feat/cart","base":"main","changed":12,"reviewers":[{"name":"tenant-isolation","mode":"gate"},{"name":"single-responsibility","mode":"gate"}]}
{"type":"reviewer.started","run":"r-0f3a","reviewer":"tenant-isolation","mode":"gate","backend":"claude-code"}
{"type":"reviewer.started","run":"r-0f3a","reviewer":"single-responsibility","mode":"gate","backend":"codex"}
{"type":"reviewer.finished","run":"r-0f3a","reviewer":"single-responsibility","duration_ms":842,"completed":1,"total":2}
{"type":"reviewer.finished","run":"r-0f3a","reviewer":"tenant-isolation","duration_ms":38120,"completed":2,"total":2}
{"type":"reviewer.resolved","run":"r-0f3a","reviewer":"tenant-isolation","verdict":"block","summary":"A new query path reads rows without scoping by tenant id.","findings":[{"kind":"blocking","path":"src/server/db.rs","line_start":88,"line_end":91,"detail":"scope this query by tenant_id"}],"usage":{"tokens_in":18204,"tokens_out":1560,"cache_read":12000,"cost_usd":0.21},"duration_ms":38120,"has_transcript":true}
{"type":"reviewer.skipped","run":"r-0f3a","reviewer":"single-responsibility","mode":"gate","trigger":{"backend":"codex","decision":"skip","reason":"No responsibility boundary changed.","duration_ms":842},"has_transcript":true}
{"type":"run.completed","run":"r-0f3a","verdict":"block","gates":{"total":2,"passed":0,"blocked":1,"skipped":1},"duration_ms":41030,"tokens_in":20480,"tokens_out":1875,"cache_read":13100,"cost_usd":0.37}
```

The event types:

| Event | Meaning |
| --- | --- |
| `run.started` | The run began; lists the reviewer candidates in the plan. Each executes, semantically skips, replays from a verified attestation, or carries from the branch's previous run. Under `--reviewer` the list holds only the selected reviewers, and the event carries `partial: true` when that selection excludes a candidate. |
| `reviewer.started` | One reviewer candidate began resolving: dispatched to its trigger or reviewer backend, reconstructed from a verified attestation bundle, or carried from the branch's previous run. |
| `reviewer.finished` | One fresh reviewer task stopped executing. `completed` and `total` count only the fresh tasks because replayed and carried reviewers dispatch no backend. This event is progress only. The final outcome follows after post-run scope-digest checks. |
| `reviewer.resolved` | One reviewer was finalized; carries its `verdict`, `summary`, `findings`, `usage`, and a `has_transcript` flag. An agent-triggered reviewer that ran also carries its preceding `trigger` decision and usage. Carries `replayed: true` when the terminal outcome came from a verified attestation, and `carried: true` when the verdict was carried forward from the branch's previous run instead of a fresh execution. A reviewer that produced a real verdict this run is also stamped with `scope_digest`, a hash of everything the verdict was keyed to; a later run carries a prior pass only when its own digest is identical. |
| `reviewer.skipped` | An agent trigger decided that its full reviewer did not apply. Carries the trigger backend, decision, reason, usage, duration, and transcript availability without recording a pass verdict. It can also carry `replayed: true` when CI restored the terminal outcome from an attestation. |
| `run.completed` | The aggregate decision and gate tally, including `gates.skipped`, plus the run's wall-clock `duration_ms` and usage totals summed across trigger and full-reviewer calls. Carries `partial: true` (as does `run.started`) when `--reviewer` narrowed the run. |
| `run.attested` | A signed local run was replayed; carries the replayed `reviewers`, the attesting `public_key`, and `attested_at`. |
| `run.attestation-fallback` | An attestation was *offered but refused*; carries the `reason` (an unreadable or unverifiable note, an unregistered key, a stale binding, and so on). A dirty CI checkout is the one refusal that needs no note: it is checked before note lookup, so a dirty tree emits this event even when HEAD carries no note. Otherwise a commit that offered no note is not a refusal and emits no such event: it resolves through the ordinary carry-or-execute path silently. |

How an agent should consume it:

- **Only need the outcome?** Ignore everything until `run.completed` and read its
  `verdict`.
- **Want live progress?** Read each `reviewer.finished` event as it lands. Act on
  `reviewer.resolved` findings after finalization; record `reviewer.skipped` as an
  intentional omission, not a pass that needs fixing.

### For agents: the consumption contract

If you are an agent driving the loop, this is the whole contract:

1. Run `bastion review --base <branch> --format jsonl`.
2. Parse stdout one line at a time as JSON; each line has a `type`.
3. Act on every `reviewer.resolved` with `verdict: "block"` using its `findings`
   (`path` + `line_start`/`line_end` + `detail`). Do not open transcripts; the
   findings already say what to change.
4. Treat `reviewer.skipped` as a recorded routing outcome. It has no verdict or
   findings, so never count it as a pass.
5. The aggregate decision is `run.completed.verdict`. The process also exits
   non-zero on `block`, so you can branch on the exit code alone if you only need
   pass/fail.
6. Fix what blocked and re-run. Loop until `run.completed.verdict` is `pass` (exit
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
on `run.completed` they are the totals summed across every agent call that reported
usage, including trigger calls that skipped the full reviewer. A resolved reviewer's
usage is top-level on `reviewer.resolved`; trigger usage is nested under `trigger`
on either terminal reviewer event. `cache_read` is the input tokens served from the
provider's prompt cache (cache hits); each backend names it differently natively
(Claude's `cache_read_input_tokens`, Codex's `cached_input_tokens`, Pi's
`cacheRead`) and Bastion normalizes them to one field. It is 0 when a backend
reports no cache usage.

## What is streamed vs. what is saved

The stream deliberately leaves out the verbose detail. A transcript is mostly noise
to an agent that just wants to know what to fix; streaming thousands of lines on
every run would bury the findings and burn the agent's own context.

- **Streamed:** the decisions and the things you act on immediately: the reviewer
  set, start and terminal events, verdicts or skip reasons, summaries, findings,
  and per-reviewer usage.
- **Saved, not streamed:** the verbose detail: full session transcripts, raw
  verdict payloads when a review ran, and per-reviewer metadata. Written to disk,
  read on demand.

That is why both `reviewer.resolved` and `reviewer.skipped` carry the boolean
`has_transcript` rather than the transcript itself. When it is `true` and a decision
surprises you, the transcript is one command away (next section); a replayed outcome
may have no local transcript and carry `false`.

## Inspecting saved runs

Every run is persisted, so you can inspect history without re-running anything.
These commands are the local equivalent of clicking "Details" on a CI check. The
run-targeted ones (`show`, `transcript`) default to the latest run when you omit a
run id; `runs` and `clean` operate over all saved runs.

```sh
bastion runs                         # list recent runs: id, verdict, branch, reviewer count
bastion show [<run>]                 # re-print terminal verdicts, skips, and findings
bastion transcript [<run>] <reviewer>   # the full agent session for one reviewer
bastion clean [--keep N | --older-than <dur>]   # prune saved runs
```

- **`runs`** is the index: what ran recently and how each landed.
- **`show`** re-emits a past run's terminal outcomes and aggregate: verdicts with
  findings, or semantic skip reasons with no findings. It accepts
  `--format human|jsonl`.
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
      seal.json                  # the run seal, when the run was sealed (what `bastion attest` reads)
      reviewers/
        tenant-isolation/
          transcript.jsonl       # the full agent session
          verdict.json           # the raw structured verdict; absent on a semantic skip
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
and in CI can pay for each review roughly twice: once in your loop, once again when
CI confirms it. Incremental carry recovers some of that across CI pushes (an
unchanged reviewer carries from the branch's previous CI run), but the first CI run
of a changeset has nothing to carry from. If your repository has set
`attestations: true` in its registry
(see [Continuous integration](./continuous-integration.md#attesting-a-run-so-ci-can-replay-it)),
you can sign your last green local run so CI reuses it instead of re-running
every reviewer:

```sh
git commit -am "final change"        # attest needs a review over committed content
git fetch origin
git rebase origin/main               # or merge; get up to date with the base tip
bastion review --base origin/main    # ends green
bastion attest                       # signs the run that just finished
git push origin refs/notes/bastion
```

The sequence syncs with the base branch before the review because CI does not
take the note's word for what was reviewed: it re-derives the merge base against
the PR's base branch, the diff's patch id, and HEAD's tree from its own checkout,
and replays only when all of them match what the run sealed. A review against a
stale view of the base seals a merge base CI will not derive, so CI refuses the
note and runs every reviewer fresh, the duplicate spend attestation exists to
avoid. Diff against `origin/main` after fetching rather than a local `main` ref,
which can lag it. And sync before the review, not after: a rebase or merge moves
HEAD, and the note binds to the reviewed HEAD. If the base moves again before CI
runs and the PR reports an attestation fallback, repeat the sequence, and expect
it to be cheap: a rebase moves the merge base but not your changeset, so every
reviewer whose scoped diff comes out identical carries instead of re-running,
and `bastion attest` signs the carried run like any other.

`bastion attest [RUN]` takes an optional run id positional; omit it and it signs
the latest recorded run, which is what you want right after `bastion review`.
Pass one explicitly (`bastion attest r-0f3a`) to attest an older run instead.

The review has to run over committed content for this to work. To use attestation,
commit your final change, then run `bastion review`, then `bastion attest`. A review
over a dirty working tree (uncommitted tracked changes or untracked files) still
runs and seals, but the seal records that the tree was dirty, and `bastion attest`
refuses that run outright and tells you to commit the final content, re-run the
review, and attest that run instead. `bastion attest` also refuses a run recorded
while any backend or container override was set (`BASTION_CLAUDE_BIN`,
`BASTION_CODEX_BIN`, `BASTION_PI_BIN`, `BASTION_CONTAINER_ENGINE`): such a run
exercised a stubbed reviewer, not a real review, so it cannot be attested either.
Re-run `bastion review` without those variables set, then attest that run. A
partial run (`bastion review --reviewer`) is refused too: its verdict speaks only
for the reviewers you selected, so run a full `bastion review` and attest that.

`bastion attest` also re-checks that your repository has not moved on since a
clean review (the same tree, the same diff, the same effective reviewer config) and
refuses to sign if it has, so the note can never claim the reviewers saw
something they did not. It signs with your SSH key (`git config
user.signingkey`, or `--key <path>` to name one explicitly), prompting for a
hardware token or keychain if your key requires it, and prints the exact push
command. The signed bundle carries each repository reviewer's terminal outcome:
either its verdict and findings or its agent-trigger skip reason and usage. A
repository reviewer that blocked locally still blocks in CI when its verdict
replays, and a skip replays as a skip rather than a pass. Your personal user-level
reviewers are excluded from the bundle (they never gate anyone else's PR), so
a run blocked only by a personal reviewer still attests, and CI sees only the
repository reviewers' results.

Push the printed command (or fold it into your normal `git push`) before
opening the PR, so CI has the note when it runs. CI verifies the signature
against the SSH signing keys you have registered with GitHub, so this only
works for a key you have added there; a key freshly generated on the machine
you are pushing from, with nothing registered, never verifies.

Whether to use a plain key file or a presence-gated one (a hardware token or an
OS keychain entry that prompts you per signature) is your call to make. A plain
file key means an agent running on your machine could sign an attestation
without you noticing, the same trust you already extend to that machine
through your commit access. See [Attestation](https://github.com/attunehq/bastion/blob/main/docs/developer-guide/attestation.md)
for the full trust model.

## The same surface in CI

For the repository's reviewers, these local events are not a separate system from CI;
their terminal outcomes have GitHub twins (check runs, comments, and annotations),
laid out side by side in the
[Continuous integration](./continuous-integration.md#how-a-run-maps-to-github)
chapter. The local `run.started`, `reviewer.started`, and `reviewer.finished` progress
events have no separate GitHub surface. A green local loop predicts a green PR when
both runs see the same reviewers and context. The two surfaces run the repository's
reviewers and aggregation, and CI adds the PR's description and discussion that a
default local run does not, so a reviewer that weighs that context can decide
differently. A purely local run can also include your personal user-level reviewers
with `--with-user-reviewers`;
their `run.started` and terminal `reviewer.resolved` or `reviewer.skipped` events are
local-only and never become checks or comments (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)).

---

Next: [Continuous integration](./continuous-integration.md). Promoting these same
reviewers into GitHub Actions as a required merge check.
