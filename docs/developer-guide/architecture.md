# Architecture

> The module map and the life of a single `bastion review`.

[<- Developer guide index](./README.md) | Next: [Backends](./backends.md) ->

---

Bastion is a small, flat crate: a thin binary over a library, with one module per
concern. This chapter is the map: where each thing lives, and how a review flows
through it.

## The module map

| Module | Responsibility |
| --- | --- |
| [`build.rs`](../../build.rs) | Derives `BASTION_VERSION` from `git describe --always --tags --dirty=-dirty`, with a `BASTION_VERSION` env override and a `Cargo.toml` fallback; bakes the rustc target triple as `BASTION_TARGET` for `bastion update` to name its release asset; also resolves the run-seal secret (`BASTION_SEAL_SECRET` env override, else a generated secret cached under `OUT_DIR`) that `src/seal.rs` embeds at build time. |
| [`src/main.rs`](../../src/main.rs) | Thin binary entrypoint; wires the tokio runtime to `bastion::run`. |
| [`src/lib.rs`](../../src/lib.rs) | Library root; installs `color_eyre` + `tracing` and dispatches. |
| [`src/version.rs`](../../src/version.rs) | Exposes the build-derived version string. |
| [`src/cli.rs`](../../src/cli.rs) | The clap derive command tree and dispatch; maps a `block` aggregate to a non-zero exit. |
| [`src/commands/`](../../src/commands/) | One module per subcommand (`review`, `validate`, `read_back`, `codeowners`, `attest`, `update`, `github_report`, `skills`); `mod.rs` re-exports the CLI surface `cli.rs` calls. |
| [`src/reviewer.rs`](../../src/reviewer.rs) | The declarative reviewer schema (`Reviewer`, `Mode`, `Backend`, `Capabilities`, `RunnerSpec`, `AttestationPolicy`). |
| [`src/config.rs`](../../src/config.rs) | Registry loading, discovery, and merge. Walks up for a repository `.bastion.yaml` or `.bastion.yml` (with a deprecated `bastion/reviewers.yaml` fallback that warns) and, via `discover_merged`, layers in a user-level registry from the platform config dir (`user_config_dir`, override `BASTION_CONFIG_DIR`). The merge is a set keyed by name: an identical reviewer in both files is deduplicated, and a same-name-different-config collision keeps both with the repo side scoped to `REPO_SCOPE_PREFIX` (`repo:`). Validates name uniqueness and run-store path-component uniqueness over the merged set. |
| [`src/routing.rs`](../../src/routing.rs) | Compiling trigger globs and matching them against changed files. |
| [`src/verdict.rs`](../../src/verdict.rs) | The structured verdict (`Decision`, `Verdict`, `Finding`, `Usage`, and `Money`, which carries cents but serializes as dollars). |
| [`src/context.rs`](../../src/context.rs) | The transport-neutral review context (`ReviewContext`): the author's stated intent, the surrounding discussion (`ContextComment` with a generic `Standing`), and a reviewer's prior findings (`PriorFinding`, keyed by a content-derived `FindingId`). A producer fills it; the backends consume it through `render_for`. Everything in it is untrusted input. |
| [`src/event.rs`](../../src/event.rs) | The run-event schema streamed as JSONL and persisted to `run.jsonl`. |
| [`src/git.rs`](../../src/git.rs) | The git queries the CLI needs (changed files, branch, repo root, and the `base..HEAD` commit messages that serve as local intent when there is no PR body). |
| [`src/paths.rs`](../../src/paths.rs) | The data-directory layout (`Layout`), resolved by platform convention or `BASTION_DATA_DIR`. Maps a reviewer name to a portable run-store path component (`path_component`), so the `repo:` merge sentinel cannot produce an unwritable path; `config.rs` enforces that distinct names never collapse to the same component. |
| [`src/store.rs`](../../src/store.rs) | Run-history persistence: writing/reading `run.jsonl`, listing and pruning runs, and resolving a branch's most recent run once per review (`latest_run_on_branch`), from which the review context takes its prior findings (`findings_from_events`) and carry planning takes the run to reuse. |
| [`src/render.rs`](../../src/render.rs) | Human and JSONL output (`Format`). |
| [`src/text.rs`](../../src/text.rs) | Shared text helpers (`truncate`), used by `render.rs` and the GitHub reporter. |
| [`src/runner/`](../../src/runner/) | The parallel, timeout-bounded runner: fans matched reviewers out over a `JoinSet`, fails closed on error/timeout, streams events, folds in replayed and carried verdicts, persists each run, and seals an eligible (full, never partial) run on a best-effort basis at persist time. It also builds one per-run [`SpawnGovernor`](../../src/backend/governor.rs) from the effective [`SpawnLimits`](../../src/limits.rs) and aborts a run (persisted, never sealed) whose fan-out trips a cap. `mod.rs` is the orchestration core; `verdicts.rs`, `seal.rs`, and `persist.rs` split out verdict folding, run sealing, and persistence. |
| [`src/limits.rs`](../../src/limits.rs) | The per-run spend caps (`SpawnLimits`): the concurrency, total-launch, and consecutive-dead-launch ceilings that bound one review's agent fan-out. Parsed from the root registry's optional `limits:` block (conservative defaults otherwise), enforced by the spawn governor. Not part of the attestation hash: a cap is an operational safety net, not review policy. |
| [`src/carry.rs`](../../src/carry.rs) | Incremental re-review: computes each triggered reviewer's trigger-scoped diff digest (`scope_digest`) and plans which prior passes carry forward on a re-run of the same branch (`plan`). Both local and CI runs; a repository reviewer carries only from a prior run whose seal verifies. See [the local surface](./local-surface.md#incremental-re-review). |
| [`src/seal.rs`](../../src/seal.rs) | The run seal: an HMAC-SHA256 over a canonical digest of the committed HEAD tree, the merge-base tree, the `base..HEAD` patch-id, the effective reviewer config, whether a test seam was active, whether the working tree was dirty (sampled before reviewers ran and again at seal time, dirty if either sample was), and the resolved reviewer events, keyed by a secret embedded in the binary at build time. Sealed by the runner, verified by `bastion attest` and CI; a run sealed dirty cannot be attested. See [Attestation](./attestation.md). |
| [`src/attest/`](../../src/attest/) | Attestation, split by concern: `mod.rs` is the `bastion attest` flow (verifies a run's seal, re-derives the repository state, signs a bundle, writes the git note), `bundle.rs` the bundle and note envelope, `sign.rs` SSH signing and signing-key resolution, and `replay.rs` the CI-side verify-and-replay planner (`plan`, `AttestationOutcome`) plus note lookup. See [Attestation](./attestation.md). |
| [`src/skills.rs`](../../src/skills.rs) | The agent skills bundled into the binary (from `skills/<slug>/SKILL.md`) and installed into a consuming repo by `bastion skills install`/`check`/`list`. The rendered file is deterministic so `check` is a version-independent drift guard. Distinct from these bundled skills are the repo-local skills that guide agents working *on* Bastion (the Rust skills and `stop-slop`), which are **not** bundled and sit outside `skills install`/`check`; every skill lives under both `.agents/skills/` (agent-neutral) and `.claude/skills/` (Claude Code's path) as exact copies, and `tests/skills_mirror.rs` fails the build if the two trees drift. |
| [`src/update.rs`](../../src/update.rs) | The native self-updater behind `bastion update`: resolves the latest release from the `releases/latest` redirect, downloads and checksum-verifies the `bastion-<target>.tar.gz` for `BASTION_TARGET` over `reqwest`, extracts it (`flate2` + `tar`), and swaps it over the running binary (`self-replace`). Also drives the passive out-of-date nag (`warn_if_outdated`, called from `cli::run` for every command but `update`), which prints to stderr for interactive release builds and refreshes a day-TTL cache in a detached `bastion __update-check` process. `BASTION_REPO` and `BASTION_BASE_URL` override the release source (tests point them at a local server). |
| [`src/backend/`](../../src/backend/) | The agent execution boundary. See [Backends](./backends.md). |
| [`src/github/`](../../src/github/) | The GitHub adapter (CI surface): `codeowners.rs` generates the governance block, `client.rs` is the `reqwest`-backed REST seam (a proof-carrying `ApiRequest` plus a `GitHubApi` trait and a recording test double, modeled on the backend's `CommandRunner`), `report/` posts a finished run as a sticky PR comment and check runs (split into `comment`, `callouts`, `checks`, `requests`, and `post`), `context.rs` produces the review context (a PR's description and discussion, the author's login, and the head SHA), and `signing.rs` fetches a user's registered SSH signing keys (`GET /users/{username}/ssh_signing_keys`) for attestation verification. See the [GitHub adapter](./github-adapter.md). |
| [`tests/integration/`](../../tests/integration/) | The end-to-end suite: it drives the *real compiled binary* against a `rustc`-compiled fake agent, each scenario in its own throwaway `git` repo and private `BASTION_DATA_DIR`, with the fake wired in via `BASTION_CLAUDE_BIN`/`BASTION_CODEX_BIN`/`BASTION_PI_BIN` (and a fake engine via `BASTION_CONTAINER_ENGINE`). Scenarios are grouped into per-theme files (`aggregation`, `carry`, `container`, `accounting`, `persistence`, `cli_surface`, `github_report`, `attestation`) over shared `fakes`/`fixtures`/`github` support. Sibling structural targets guard the repo's shape: `tests/skills_mirror.rs`, `tests/script_safety.rs`, and `tests/user_guide_integrity.rs`. |
| [`scripts/install.sh`](../../scripts/install.sh) / [`install.ps1`](../../scripts/install.ps1) | The public install scripts: detect the platform, download the matching archive plus `checksums.txt`, verify the SHA-256, and fail closed on any checksum problem. `tests/script_safety.rs` pins that behavior. |

## The two boundaries that shape the design

Two seams are worth understanding before you change anything, because most of the
structure exists to keep them honest.

- **The backend boundary** ([`src/backend/`](../../src/backend/)). Bastion does not
  run agent loops; it shells out to existing agent CLIs. The `Backend` trait, the
  `CommandRunner` subprocess seam, and `dispatch` isolate everything agent- and
  subprocess-specific so the runner above stays pure orchestration and the tests
  drive real backends against a fake executable. Covered in
  [Backends](./backends.md).
- **The parse-don't-validate boundary** (`config.rs` -> `reviewer.rs` ->
  `routing.rs`). Untrusted text (the YAML registry, git output, CLI args) is parsed
  *once* at the edge into precise types (a `Reviewer`, a compiled glob matcher, a
  `RunId`) rather than carried around stringly-typed and re-checked. Covered in
  [Conventions](./conventions.md).

## The life of a `bastion review`

Following one review top to bottom touches most of the crate:

1. **Parse & resolve** (`cli.rs`). clap parses the command. The data directory is
   resolved into a `Layout` (`paths.rs`), from `--data-dir`/`BASTION_DATA_DIR` or
   the platform default. The user-level config directory is resolved the same way,
   from `--config-dir`/`BASTION_CONFIG_DIR` or the platform default; it is passed
   into a purely local review but withheld from a governed one (a review carrying a
   GitHub source via `--repo`/`--pr`), so CI never merges ungoverned reviewers.
2. **Load policy** (`config.rs`). `discover_merged` finds the repository registry by
   walking up from the cwd for `.bastion.yaml` (or `.bastion.yml`) and merges in the
   user-level registry from the config dir, layering the two reviewer lists into one
   validated set (identical reviewers deduplicated, a same-name-different-config
   collision keeping both with the repo side scoped to `repo:`). Within each layer,
   loading first resolves the file layout: `include:`d registry files merge in
   (recursively, each loaded once), `--include` files join the repository layer,
   and `prompt: {file: ...}` references are inlined, so a `Config` in hand always
   carries flat reviewers with real prompt text. Any one source (a repo registry, a
   user registry, or a `--include` file) suffices; discovery errors only when all
   are absent. The merged set is parsed into
   `Config` and validated (unique names, unique run-store path components, and
   non-empty prompts). Malformed input fails here, before any agent runs.
3. **Compute the changeset** (`git.rs`). Bastion asks git for the files that differ
   from `--base` (tracked edits *and* untracked files, committed or not) plus the
   current branch and repo root.
4. **Route** (`routing.rs`). Each reviewer's trigger globs are compiled and matched
   against the changed files; the matched reviewers are the ones in scope for this
   run (each will execute, replay, or carry). An
   explicit `--reviewer` selection then narrows that set (an unknown or untriggered
   name errors here), and a selection that excludes a triggered reviewer marks the
   run partial: persisted and rendered as such, and never sealed.
5. **Gather context** (`context.rs`, `git.rs`, `store.rs`, `github/context.rs`).
   Bastion assembles a `ReviewContext` for the run: the author's stated intent (a
   non-empty PR body when reviewing a pull request, otherwise this branch's commit
   messages as the fallback), the prior findings recalled from the last run of this
   branch, and (on GitHub) the PR's discussion. It is best effort: a failure to reach GitHub falls back to the local
   context. Empty context leaves every reviewer's prompt unchanged.
5a. **Verify and plan attestation replay** (`attest/replay.rs`, GitHub CI only). When the
    repository registry sets `attestations: true` and the run carries a GitHub
    source (`--repo`/`--pr`), `commands::review` first checks whether the CI
    checkout is dirty (uncommitted tracked changes or untracked files). A dirty
    checkout skips note lookup entirely: it records a `run.attestation-fallback`
    event, and its reviewers resolve through the ordinary carry-or-execute path
    (step 5b). Only a clean checkout proceeds to
    look up the note on HEAD (falling back to the PR's head SHA), verify its
    signature against the PR author's GitHub-registered SSH signing keys, verify
    the run seal, and check every binding against CI's own re-derived values,
    before the runner fans anything out. A routed reviewer the bundle covers and
    that has not opted out replays; everything else continues to carry planning
    (step 5b), which carries an eligible prior pass or executes it fresh. An
    attestation that was offered and *refused* (an unreadable or unverifiable note,
    a seal or binding mismatch, a dirty checkout) degrades the affected reviewers to
    that same path and records a `run.attestation-fallback` event the report surfaces
    as a `[!WARNING]`. A commit that offered *no* note is not a refusal: it resolves
    to `NotAttested`, records no event, and says nothing about attestation. A
    purely local review skips this step entirely. See [Attestation](./attestation.md).
5b. **Plan carry** (`carry.rs`, both surfaces). Every reviewer about to run
    gets, best effort, a trigger-scoped diff digest (a digest that fails to
    compute leaves that reviewer executing fresh and uncarryable): its own
    effective definition, the diff of the changed files its trigger matched
    against the merge base (untracked matched files encoded by kind,
    executable bit, and content), and the scoped commit messages that touched
    those files. The digest deliberately binds the changeset and not the
    merge-base commit id, so a rebase that reproduces the identical scoped
    diff keeps the verdict carryable; see the module docs in
    [`src/carry.rs`](../../src/carry.rs). The runner re-derives each digest after the reviewers finish (re-scanning
    the changed-file set, so a file created mid-run is seen) and stamps it onto
    `reviewer.resolved` only when the reviewer produced a real verdict and the
    digest still matches; a failed or timed-out reviewer resolves with no digest,
    a changed tree leaves nothing to carry from, and a carried verdict in that
    situation fails closed. On a
    re-run of the same branch, a reviewer whose prior verdict was a pass with an
    identical digest is *carried*: its verdict folds into the run without a
    backend executing. A repository reviewer carries only from a prior run whose
    seal verifies (and records no test seam); `--fresh` disables carry, and an
    explicit `--reviewer` selection disables carry for the selected reviewers,
    though a `--repo`/`--pr` run can still replay one from a verified attestation
    (replay is planned in step 5a, before this one). A CI run carries
    from its own prior CI run the same way, since that seal verifies under the
    release secret and the digest binds the content; carry and attestation replay
    stay complementary (replay reuses the author's signed local run, carry reuses
    CI's own prior run). See
    [the local surface](./local-surface.md#incremental-re-review).
6. **Run** (`runner/`). `execute` builds one per-run `SpawnGovernor` from the
   effective `SpawnLimits` (`src/backend/governor.rs`, `src/limits.rs`), then spawns
   every matched reviewer that is neither replaying nor carrying onto a `JoinSet`,
   bounds each by its `timeout` (default 15m), and emits `reviewer.started` up front
   (including for replayed and carried reviewers, so the plan reads the same either
   way). Each spawned task calls `backend::dispatch`
   (`backend/mod.rs`), which resolves the reviewer's `ExecutionPlan` (failing closed
   on an unprovisioned capability tier), selects the concrete backend, wraps its
   subprocess seam in the shared governor, and runs the
   agent either natively or inside a container for a reviewer with a `runner` block
   and `capabilities.network: true` (`backend/container/`; see
   [Containers](./containers.md)). A replayed or carried reviewer skips dispatch
   entirely and is never handed to the `JoinSet`: its verdict is folded in from
   the attested bundle's event (replay) or the prior run's persisted
   `reviewer.resolved` event (carry) instead.
7. **Resolve & aggregate** (`runner/`). Each result has fail-closed/fail-open
   policy applied: a gate that blocks, errors, or times out resolves to `block`
   (with a synthetic blocking finding); an advisor that fails is dropped. A replayed
   verdict carries the same policy as if it had executed, so a replayed block still
   blocks. The aggregate is `block` if any gate blocked, else `pass`.
8. **Emit & persist** (`render.rs`, `store.rs`, `seal.rs`). Events stream out as
   human text or JSONL as they happen; the full event stream, plus per-reviewer
   transcript, verdict, and metadata, is written under the run's directory, and
   `latest` is updated. The runner then seals an eligible run on a best-effort
   basis: a canonical digest of the committed HEAD tree, the merge-base tree,
   the `base..HEAD` patch-id, the effective config hash, whether the working
   tree was dirty (sampled before reviewers ran and again at seal time, dirty
   if either sample was), and the sorted `reviewer.resolved` events, MAC'd with the
   binary's embedded secret and written to `runs/<id>/seal.json`. The zero-match
   fast path persists without a seal, and sealing skips a run whose bindings
   cannot be derived or that resolved no repository-reviewer event. Sealing
   never fails the review; an unsealed run simply cannot later be attested, and
   a run over a dirty working tree seals as `dirty: true`, which `bastion
   attest` refuses to attest.
9. **Exit** (`cli.rs`). The aggregate `Decision` maps to the process exit code:
   `pass` -> success, `block` -> failure, so an agent loop and CI agree on the gate.

The read-back commands (`transcript`, `show`, `runs`, `clean`) skip steps 3-7 and
read the persisted run store directly.

## Why the runner owns persistence

`execute` owns both event emission *and* persistence, so `commands::review` only
has to render the stream and map the aggregate to an exit code. This is deliberate:
it keeps the `run.jsonl` on disk identical to the live stream (it even reconstructs
the authoritative `run.started` and prepends the retained `reviewer.started` events
so a replay sees the exact sequence the live run emitted), and it means there is
one place, not two, that decides what a run records.

---

Next: [Backends](./backends.md). The agent execution boundary in detail.
