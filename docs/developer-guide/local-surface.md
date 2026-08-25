# Bastion locally

> The local surface: the same review data GitHub shows for the repository's reviewers, streamed out of the CLI for an agent, with the noisy parts kept on disk and read on demand. Personal user-level reviewers are local-only fallbacks unless explicitly merged.

The core design ([`design.md`](./design.md)) describes `bastion review` in a single section; this doc is the detail of the local surface, the way the GitHub adapter ([`github-adapter.md`](./github-adapter.md)) is the detail of the CI surface. For the repository's reviewers the two are mirror images: the same reviewers, verdicts, and findings, presented through whatever the surface makes natural. On GitHub that is check runs and PR comments; locally it is a stream on stdout and a few files on disk. The user-level registry is the only deliberate asymmetry. A purely local run can include an author's personal reviewers, which the GitHub surface never sees (see [User-level reviewers](#user-level-reviewers) below).

The guiding rule carries over: Bastion does not own your environment, it plugs into it. Locally that means the agent's loop drives Bastion, the local shell provides whatever the reviewers consume, and Bastion streams results back in a shape an agent can read without help.

---

## How it runs

`bastion review` applies each reviewer's path trigger or agent-assisted trigger to the working tree's changes against the base branch, as CI would for the repository's reviewers; routing, the runner, and aggregation are the shared core, not local-specific. Two things differ from CI. The first is reviewer discovery: a purely local run uses the user-level registry as a fallback when no repository configuration exists, and `--with-user-reviewers` merges both registries. A GitHub-source run (`--repo`/`--pr`) uses the repository's reviewers alone (see [User-level reviewers](#user-level-reviewers)). The second is where inputs come from: there is no preview-deploy job, so anything a reviewer's `env` or `inputs` reference is expected to be in the local environment already. A `precommit` script might boot the service on `http://localhost:3000` and export that as the preview URL. A native reviewer inherits that local environment directly. A containerized reviewer (one with a `runner` and `capabilities.network: true`) inherits none of it; only its literal `env` pairs and the fixed provider-credential set cross into the container, so a local value reaches it only when written into the reviewer's `env` (see [Containers](./containers.md)).

The intended use is the loop from the core design: an agent runs `bastion review`, reads the stream, fixes what blocks, and runs it again. The bundled `using-bastion` skill caps that loop at three full reviews so an unattended agent cannot accumulate spend across invocations (each invocation builds a fresh per-run governor). A human watching the CLI can keep going.

### Incremental re-review

The loop's dominant cost is re-executing reviewers that already passed: after fixing one reviewer's findings, a naive re-run re-executes the whole triggered set even though most of it judged content the fix never touched. So a purely local `bastion review` is incremental by default. Every reviewer that resolves to a real verdict is stamped with a *scope digest* (best effort: a reviewer that crashed, timed out, or returned garbage resolves with no digest, and a digest that fails to compute only makes its reviewer uncarryable): a hash of the reviewer's own effective definition, the diff its verdict judged (the matched files for a path trigger, or the entire changeset for an agent trigger), and the `merge_base..HEAD` commit messages that touched those files. Untracked files are encoded by kind, executable bit, and content, with symlinks represented by their target. The digest deliberately binds the changeset and not the merge-base commit id or anything on the base's side of the fork: a rebase that reproduces the identical scoped diff keeps every pass carryable, one that changes the diff (a conflict resolution, upstream edits inside a hunk's context lines) re-runs the affected reviewers, and what the base changed was reviewed by its own changesets when it merged. Reviewers judge the live working tree, so the runner re-derives every digest after the reviewers finish and stamps only the ones that still match: a fresh verdict whose tree changed mid-run loses its stamp (the next run executes that reviewer again), and a *carried* verdict in that situation fails closed outright, since the pass it reused no longer describes the tree the run reports on. On the next run of the same branch, a reviewer whose prior verdict was a **pass** and whose digest is unchanged is *carried*: its prior verdict folds into the run (`"carried": true` on `reviewer.resolved`, no usage, zero duration) without a backend executing, and it still counts in the gate tally. A reviewer whose scoped content changed, which always includes the ones that blocked (the fix touched the files they flagged), executes fresh. Blocks are never carried: re-confirming a block is exactly the loop's next question.

The trigger is the soundness boundary. A path trigger declares the files its concern depends on, so carry keys the verdict to that matched slice. Agent-trigger `paths` only avoid a routing call when no candidate file changed; once admitted, the routing and review agents see the full changeset, so carry keys their verdict to the full changeset too.

Carry runs on both surfaces, local and CI. Three things keep it sound and under your control:

- **A repository reviewer carries only from a sealed, verified run.** The carried verdict flows into the new run's seal, and (locally) from there into anything the author later attests, so every link in the chain must have been binary-verified: an unsealed prior run, a seal that fails verification (under the release secret embedded in the binary), or a seal recording an active test seam disqualifies carry for every repository reviewer. In CI the run store is a restored artifact, but the seal is verified before any repository reviewer carries, so a restored store cannot pass off a fabricated pass; forging a seal means extracting that embedded secret, the deliberate malice the [threat model](./design.md#threat-model--trust-boundary) already excludes. A user-level reviewer (never sealed, never gating anyone else's PR) carries on the digest alone. A reviewer with `attestation: never` is never carried; that policy asks for fresh execution every time.
- **The digest binds the reviewed content.** A carried pass provably still describes the changeset now under review, so carrying from a prior run reuses a verdict over the same scoped content. Carry and attestation replay stay complementary: replay reuses the *author's* signed local run (crossing from their machine into CI, which is why it needs the SSH signature), carry reuses a run that already executed on the same surface.
- **`--fresh` opts out.** Every triggered reviewer executes even when its scoped diff is unchanged.

### Running a subset by hand

`bastion review --reviewer <name>` (repeatable; alias `--only`) narrows the run to a hand-picked subset of the *triggered* reviewers, for iterating on one stubborn gate. A name that is not in the registry, or whose trigger did not match the changeset, is an error naming what can run, never a silent no-op. The selected reviewers never carry a prior pass (asking for a reviewer by name means asking for it to run), though on a `--repo`/`--pr` run a verified attestation can still replay one. When the selection excludes a triggered reviewer the run is **partial**: `run.started` and `run.completed` carry `"partial": true`, the human rendering and `bastion runs` say so, the run is never sealed, and `bastion attest` refuses it. A filtered green speaks only for the reviewers that ran; it never stands in for a full green. The finishing move after a `--reviewer` iteration is a plain `bastion review`, which re-establishes a sealed green. Carry walks the branch's prior runs newest first and, for each reviewer, uses the newest run that resolved it. A later partial run does not hide earlier sealed passes for reviewers it did not run, so the finishing full run still carries those unchanged passes. The named reviewer itself executes fresh on that finishing run: a partial run is never sealed, so a repository reviewer's pass from the partial cannot carry.

### User-level reviewers

Reviewers can also come from a personal `.bastion.yaml` (or `.bastion.yml`) in your platform config directory, so you can run a reviewer locally whether or not a repository adopts Bastion in CI:

- Linux: `$XDG_CONFIG_HOME/bastion`, defaulting to `~/.config/bastion`.
- macOS: `~/Library/Application Support/bastion`.
- Windows: `%APPDATA%\bastion`.

When a repository registry exists, the user-level registry is skipped by default.
Pass `--with-user-reviewers` to `bastion review` or `bastion validate` to merge
both sets by reviewer name:

- A reviewer only one file defines is included as-is.
- The same reviewer in both files is deduplicated to one, compared by effective config after each file's registry `defaults` are applied (so inheriting a default and spelling out the same value count as identical).
- A genuine collision (the same name with a different effective config in each file) keeps both: the user copy stays under its plain name, and the repository copy is scoped to `repo:<name>` so neither silently wins. The two files are governed separately, so the collision is surfaced rather than resolved by precedence.

This is a local-only layer. A review carrying a GitHub source (`--repo`/`--pr`, as CI runs) rejects `--with-user-reviewers` and skips the user-level registry, so the GitHub adapter sees the repository's reviewers alone and the `repo:` scope never appears there, even on a self-hosted runner that has a config dir. `--config-dir` (or `$BASTION_CONFIG_DIR`) overrides where the user-level file is read from, mirroring `--data-dir` for run history; it does not enable the merge.

### Multi-file registries and `--include`

Either layer's registry can spread across files: a top-level `include:` array merges further registry files (recursively, each path relative to the file that lists it, a file reached twice merging once), and a reviewer's `prompt` can be a `{file: <path>}` reference whose content is inlined at load. Both resolve inside their own layer before the user/repo merge above happens, so a user-file include is a user-layer reviewer and a repo-file include is part of the effective repository config. The global `--include <path>` flag (repeatable, on `review`, `validate`, `attest`, and `github codeowners`) merges extra files into the *repository* layer, like `include:` entries except that a relative path resolves against the current directory. It does not suppress personal fallback reviewers when there is no repository registry. Because the extra files change the effective repository config hash, `bastion attest` (and a CI replay) only agree with such a run when given the same flags. See [Authoring reviewers](../user-guide/authoring-reviewers.md#splitting-the-registry-across-files) for the authoring-level rules (per-file `defaults`, root-only `attestations`, cross-file name uniqueness).

The [review context](./design.md#review-context) uses local inputs. There is no PR, so intent comes from the branch's commit messages (`base..HEAD`), and there is no discussion thread to gather. Prior-findings memory works because every local run is persisted: a second `bastion review` on the same branch shows each reviewer what it raised last time, recalled from the run store. GitHub adds the PR description and discussion on top.

---

## Streaming output

Two audiences, two formats. By default `bastion review` renders human-readable progress for a person watching. An agent passes `--format jsonl` (or sets it once in config) and gets a machine stream instead.

We stream **JSONL**: one JSON object per line, emitted as each thing happens. It is the natural fit for a live, append-only sequence of events; an agent can read it line by line as it arrives, and every agent already parses JSON without a library. A run is a sequence of typed events:

```jsonl
{"type":"run.started","run":"r-0f3a","branch":"feat/cart","base":"main","changed":12,"reviewers":[{"name":"tenant-isolation","mode":"gate"},{"name":"single-responsibility","mode":"gate"}]}
{"type":"reviewer.started","run":"r-0f3a","reviewer":"tenant-isolation","mode":"gate","backend":"claude-code"}
{"type":"reviewer.started","run":"r-0f3a","reviewer":"single-responsibility","mode":"gate","backend":"codex"}
{"type":"reviewer.finished","run":"r-0f3a","reviewer":"single-responsibility","duration_ms":842,"completed":1,"total":2}
{"type":"reviewer.finished","run":"r-0f3a","reviewer":"tenant-isolation","duration_ms":38120,"completed":2,"total":2}
{"type":"reviewer.resolved","run":"r-0f3a","reviewer":"tenant-isolation","verdict":"block","summary":"A new query path reads rows without scoping by tenant id.","findings":[{"kind":"blocking","path":"src/server/db.ts","line_start":88,"line_end":91,"detail":"scope this query by tenant_id"}],"usage":{"tokens_in":18204,"tokens_out":1560,"cache_read":12000,"cost_usd":0.21},"duration_ms":38120,"has_transcript":true}
{"type":"reviewer.skipped","run":"r-0f3a","reviewer":"single-responsibility","mode":"gate","trigger":{"backend":"codex","decision":"skip","reason":"No responsibility boundary changed.","duration_ms":842},"has_transcript":true}
{"type":"run.completed","run":"r-0f3a","verdict":"block","gates":{"total":2,"passed":0,"blocked":1,"skipped":1},"duration_ms":41030,"tokens_in":20480,"tokens_out":1875,"cache_read":13100,"cost_usd":0.37}
```

`reviewer.finished` is live progress for a fresh reviewer task. It records elapsed time and counts over the fresh tasks in this run. It does not carry an outcome because the runner rechecks scope digests against the post-run tree before finalizing verdicts. `reviewer.resolved` carries that final verdict, findings, usage, and any preceding agent-trigger `run` decision. `reviewer.skipped` carries the trigger backend, skip reason, and usage without claiming the reviewer passed. `run.completed` is the aggregate `bastion` check and includes `gates.skipped`; trigger usage is included in its totals. These progress events are local. The GitHub side is posted after the run finishes, so it has no equivalent in-progress surface.

One case ends the run outside this happy path: a [per-run spend cap](./design.md#bounding-a-runs-spend) tripping. If a broken or unauthenticated agent CLI respawns in a loop, the governor aborts the fan-out; the run still emits a `run.completed` with a `block` aggregate (every affected reviewer failed closed) and persists, but it is not sealed, and `bastion review` then exits non-zero with a stderr line naming what was capped and how many agents launched. So an agent reading the stream sees an ordinary `block`; the exit status and the stderr line are what distinguish a spend-cap abort from a code-review block.

Note what is _not_ in the stream: the transcript. Both terminal reviewer events carry a `has_transcript` flag rather than the transcript itself; when it is set, the saved transcript is one command away (`bastion transcript <run> <reviewer>`). The reasoning is in the next section.

A few human-facing notices ride **stderr** alongside the event stream: the skills-freshness advisory below, and on the CI surface a line mirroring the attestation replay callout or fallback reason. Those attestation outcomes are also events in the JSONL stream itself (`run.attested`, `run.attestation-fallback`); the stderr line is the human mirror, not the only record (see [Attestation](attestation.md)). Before the run, `bastion review` checks the repo's bundled agent skills (`.claude/skills` and `.agents/skills`) against the running binary's embedded copy (the same check `bastion skills check` runs) and, when any are missing or drifted, prints a one-line warning naming the affected files and pointing at `bastion skills install`. It goes to stderr so it lands somewhere both a human and the driving agent read while leaving stdout as pure JSONL for a parser, and it is advisory, so it never changes the review's exit status. The notice is gated on the repository having adopted Bastion: `warn_on_stale_skills` routes through `local_skills_warning`, which returns nothing unless a repo-level reviewer registry is found (`config::locate_kind`), so a review running on the author's user-level reviewers alone stays silent even when every bundled skill is missing. Nudging skills into a project that has not configured Bastion would be misdirected. This mirrors the GitHub surface, where the same advisory is folded into the sticky comment (see [the GitHub adapter](github-adapter.md)); that report path is not gated, since CI always has a repo registry.

---

## What we stream, what we save

The principle is the one that put transcripts behind a `<details>` block on GitHub, taken a step further: locally the verbose data is not even sent down the stream. A transcript is mostly noise to an agent that just wants to know what to fix; streaming it on every run would bury the findings under thousands of lines and burn the agent's own context for nothing.

So the split is:

- **Streamed:** the decisions and the things an agent acts on immediately; the reviewer set, the start and resolve events, verdicts, summaries, findings, and per-reviewer usage.
- **Saved, not streamed:** the verbose detail; full session transcripts, raw verdict payloads, and per-reviewer metadata. These go to the data directory and are read on demand.

This keeps the common loop tight: the agent reads a short stream, acts, and re-runs, while nothing is lost; the detail is one command away when a decision is surprising enough to want it.

---

## The data directory

Bastion persists every run under a per-user data directory, resolved by platform convention:

- Linux: `$XDG_DATA_HOME/bastion`, defaulting to `~/.local/share/bastion`.
- macOS: `~/Library/Application Support/bastion`.
- Windows: `%APPDATA%\bastion`.

Each run gets a directory keyed by its run id, holding the full event stream and a subdirectory per reviewer. Full runs at one HEAD reuse `r-<short-sha>` and overwrite the previous full run. A partial `--reviewer` run uses `r-<short-sha>-partial` so it cannot overwrite that full record (carry still needs it):

```
<data-dir>/
  runs/
    r-0f3a/
      run.jsonl                  # the full event stream, always JSONL regardless of display format
      seal.json                  # the run seal, when the run was sealed
      reviewers/
        tenant-isolation/
          transcript.jsonl       # the full agent session
          verdict.json           # the raw structured verdict; absent on a semantic skip
          meta.json              # backend, timing, usage, matched trigger
    latest                       # a plain file holding the most recent run id
```

The runner seals an eligible run on a best-effort basis as it finishes
persisting: a canonical digest of the committed HEAD tree, the merge-base
tree, the `base..HEAD` patch-id, the effective config hash, whether a test
seam was active, whether the working tree was dirty (uncommitted tracked
changes or untracked files), and the sorted terminal `reviewer.resolved` or
`reviewer.skipped` events, MAC'd
with a secret embedded in the binary at build time. `bastion attest` reads
`seal.json` to build an attestation. A run has no `seal.json`, and so cannot
be attested, in a few cases: a zero-match run (persisted without going
through the runner), a partial run (`--reviewer` narrowed the triggered set,
so its aggregate must not become attestable as a full verdict), a run whose
bindings could not be derived, a run that resolved no repository-reviewer
event, or an older run predating sealing.
Sealing failure is non-fatal and never fails the review itself; a review over
a dirty working tree still seals, but the seal records `dirty: true`, which
`bastion attest` also refuses. See [Attestation](./attestation.md) for the
full design.

The run is always persisted as JSONL regardless of the `--format` used on screen, so `run.jsonl` holds the same events whether a human or an agent triggered it; a run can be replayed or inspected after the fact without re-running it, and the per-reviewer files hold what was deliberately kept off the stream. Runs accumulate; `bastion review` does not prune, so history grows until you run `bastion clean` (which keeps the most recent 20 when given no arguments).

---

## On-demand detail

The commands that read saved data back are the local equivalent of clicking "Details" on a check in GitHub. The run-targeted ones (`transcript`, `show`) default to the latest run when a run id is omitted, since that is almost always what an agent wants.

- `bastion transcript [<run>] <reviewer>` prints the saved session transcript for one reviewer. This is the explicit, opt-in way to see the thing we kept off the stream; an agent reaches for it when a verdict is surprising and it wants to know why.
- `bastion show [<run>]` re-emits a past run's summary, verdicts, and findings without re-running it; the same content as the stream's resolve and complete events, on demand.
- `bastion runs` lists recent runs with their id, aggregate verdict, branch, and reviewer count.
- `bastion clean [--keep N | --older-than <dur>]` prunes saved runs.

`show` and `runs` accept `--format human|jsonl`; `transcript` is raw text by default, since a transcript is already a document.

Separate from these run-inspection commands, `bastion validate [FILE]` parses the reviewer registry through the same `Config` load path `review` uses, and reports any load-time error (malformed YAML, an unknown field, a duplicate name, an empty or unreadable prompt file, a missing include, a model under `backend: any`) without running a reviewer or spending a model call. With no `FILE` it validates the repository registry when one exists, otherwise the user-level fallback. Pass `--with-user-reviewers` to validate the merged set. The command names each source it loaded and lists every included registry file and prompt file; an explicit `FILE` is validated on its own (its `include:` entries and any `--include` flags still merge) and rejects `--with-user-reviewers`. A valid registry prints a summary and exits zero; an invalid one prints the error and exits non-zero, so it serves as a cheap pre-commit or CI lint. It has no GitHub mirror: in CI the same validation happens implicitly when `review` loads the registry.

`bastion attest [<run>] [--key <path>]` signs a sealed local run as an attestation note on HEAD, so CI can verify and replay it instead of re-executing the reviewers (see [Attestation](./attestation.md) for the full design). It defaults to the latest recorded run; `--key` picks the SSH signing key, falling back to `git config user.signingkey` when omitted. It refuses to sign when:

- the run was partial (`bastion review --reviewer` ran a subset of the triggered reviewers), whose verdict speaks only for those reviewers;
- the run was never sealed (a zero-match run, one whose bindings could not be derived, one with no repository-reviewer terminal outcomes, or an older run predating sealing);
- the seal recorded that a test seam (a `BASTION_*_BIN` backend override, or the container-engine override) was active, since a run against a stubbed reviewer is not a real review;
- the seal recorded `dirty: true`, meaning the working tree carried uncommitted tracked changes or untracked files at review time: commit the final content, re-run the review, and attest that run instead;
- the run store no longer matches its own seal, meaning it was edited after the run finished, or sealed by a different build of Bastion;
- the repository has moved on since the run: HEAD's tree, the merge base's tree, the diff's patch-id, or the effective config hash no longer match what the seal recorded (a run reviewed with `--include` needs the same `--include` flags here, since the extra files are part of that hash);
- no signing key can be resolved (`--key` was not given and `git config user.signingkey` is unset).

On success it prints the run id and the reviewers it covers, the resolved public key, and the push command for the notes ref:

```
Attested run 'r-0f3a' on HEAD (2 reviewer(s): fail-closed-gates, single-responsibility)
Signed with ssh-ed25519 AAAA... you@example.com
Push the note with: git push origin refs/notes/bastion
```

The note itself does not push automatically; run the printed command (or fold it into your usual `git push`) to make the attestation visible to CI. `bastion attest` is local-only, with no GitHub mirror: it is the only command that writes the note, and `bastion review` in CI verifies and replays it.

`bastion update` keeps the CLI current without re-running the install script. It resolves the latest published release (from the `releases/latest` redirect, so it is not subject to the unauthenticated `api.github.com` rate limit), downloads the `bastion-<target>.tar.gz` built for this binary's target triple, verifies it against the release `checksums.txt`, and swaps it over the running executable in place. It installs the same bits as `scripts/install.sh`, so a self-update and a fresh install converge on the same SHA-256. `bastion update --check` reports the status (`up to date`, an available version, or a development build) without installing; `--force` reinstalls the latest even when already current. This is also local-only, with no GitHub mirror: CI pins the release it runs, and a self-updating CI runner would defeat that. Separately, every command but `update` itself prints a short stderr notice when a release build detects a newer version (an availability line plus the `bastion update` command to run), gated to an interactive terminal and silenced by `BASTION_NO_UPDATE_CHECK`; the check runs off a day-TTL cache refreshed by a detached background process, so it never blocks or fails the command that ran.

---

## Parity with GitHub

For the repository's reviewers, the local and GitHub surfaces carry the same data; only the transport differs. How the events map to the GitHub surfaces:

| GitHub                                                            | Local                                             |
| ----------------------------------------------------------------- | ------------------------------------------------- |
| A per-reviewer check run reaching its conclusion                  | `reviewer.resolved` or `reviewer.skipped` event   |
| Findings in the sticky PR comment and as check-run annotations    | `findings` in `reviewer.resolved`                 |
| Tokens and cost in the check output                               | `usage` in the terminal reviewer event            |
| The aggregate `bastion` check and the sticky PR comment           | `run.completed` event                             |
| Transcript in the uploaded run artifact                           | saved on disk, `bastion transcript`               |
| The `[!NOTE]` replay callout and replayed check-run summary lines | `run.attested`; `replayed` on the terminal event  |
| A sticky-comment `[!WARNING]` block naming why a *refused* attestation was not honored | `run.attestation-fallback` event      |

`bastion github report` runs after `bastion review` finishes, so the per-reviewer checks are created already completed, and the aggregate check and the sticky comment are written once. The local stream additionally carries `run.started` and `reviewer.started` for an agent reacting as the run goes; those have no separate GitHub surface. For the repository's reviewers the data each surface carries is the same; only the local stream is finer-grained than the post-hoc GitHub rendering.

Anyone who understands one surface understands the other; this is deliberate, so that an agent's local loop and the CI gate never disagree about what a review means. An author's personal user-level reviewers run only locally, so a local run can carry reviewer events and findings that the GitHub surface will never report.
