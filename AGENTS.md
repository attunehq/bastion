# AGENTS.md

Guidance for coding agents working in this repository.

## Project overview

Bastion is a Rust 2024 agentic code-review system. Single-concern reviewers run
as fitness functions over a changeset, both locally (the `bastion` CLI) and in
CI. Each reviewer is a focused agent prompt with a trigger; matched reviewers
run, return a structured verdict, and Bastion aggregates them into one merge
gate. The human sits at the policy layer, authoring and governing reviewers.

**This crate is past the walking-skeleton stage but still partial.** The data and
routing layers are real and tested; the parallel, timeout-bounded runner
(`src/runner.rs`) and all three agent backends (Claude Code, Codex, and Pi, under
`src/backend/`) are implemented and execute reviewers for real over an injectable
subprocess seam. Keep that boundary honest: a backend that cannot produce a valid
verdict returns an error, never a fabricated pass, and gates fail closed on it.

## Threat model and trust boundary

Read this before touching anything that reuses a prior run (carry, attestation
replay, the run seal). The same question gets re-litigated every session: "is it
safe for CI to trust this?" The authoritative statement is
[design.md](docs/developer-guide/design.md#threat-model--trust-boundary); the
working summary:

Bastion is not an adversarial security boundary. It is the agent-era equivalent of
team code review for aligned contributors. It is built to be robust against
*inadvertent* gaming and erosion, not against a deliberately malicious actor, and
the bar is reasonable reduction proportionate to effort: a speed bump and good
defaults that keep aligned agents on the rails, not a proof that gaming is
impossible.

The concrete consequence: the run seal (`src/seal.rs`) is an HMAC keyed by a secret
embedded in a public binary, so it is tamper *evidence*, not proof of origin. An actor who extracts that secret and forges
a seal produces a record CI cannot distinguish from a real one, and that act is the
deliberate malice the threat model already excludes. So the bar a reused verdict
must clear is "a real review of this content by this release, not one the agent
fabricated," never "prove a human demonstrably signed off." The seal plus a
content-binding digest meets the first bar, and nothing here needs to meet the
second.

What each mechanism's extra machinery is actually for:

- The **seal** catches inadvertent run-store edits, stale-run replay, and reviewer
  stubbing. That is enough to reuse a prior run *within a surface*, which is why
  carry (`src/carry.rs`) reuses CI's own prior CI run on the seal alone, with no
  signature.
- Attestation's **SSH signature** (`src/attest/`) is needed only because
  attestation imports a run produced on the *author's own machine* across the
  local-to-CI boundary. The signature ties that run to a forge account CI already
  trusts to merge, and adds a presence speed bump against the coding agent silently
  attesting its own work. It is not evidence that a human attested: keys sit
  unlocked on dev machines, so it is a speed bump, not a wall. Do not lean on it as
  more than that.

Guidance that follows, so we stop re-deriving it:

- Do not design against deliberate malice (extracting the embedded secret, forging a
  store, hand-editing a run). It is out of scope, and for the seal it is already
  possible, so contorting a design to prevent it buys nothing and costs clarity.
- Do not promote a proportionate speed bump into a guarantee it does not provide.
  "The agent did not literally fabricate this review" is the standard; "a human was
  demonstrably in the loop" is not, and chasing the latter fights the whole point of
  Bastion, which is to take the human out of the per-change review loop while keeping
  proportionate guardrails on agents.
- The one hard line that holds regardless: gates fail closed, advisors fail open. A
  gate that cannot produce a valid verdict blocks, never a silent pass. None of the
  above softens that.

## Source of truth

- `README.md`: sparse user-facing intro, install, and links into the guides.
- `docs/user-guide/`: task-oriented guide for people *using* Bastion (concepts,
  authoring reviewers, the local loop, CI, governance). Progressive disclosure.
- `docs/developer-guide/`: guide for people working on Bastion itself
  (architecture, the backend boundary, conventions), plus the design references:
  - `docs/developer-guide/design.md`: the core system: reviewers, the verdict
    contract, the merge gate, the threat model. The authoritative design reference.
  - `docs/developer-guide/github-adapter.md`: the GitHub CI adapter and governance.
  - `docs/developer-guide/local-surface.md`: the local CLI surface this crate
    implements. For the repository's reviewers the local and GitHub surfaces are
    deliberate mirror images; keep them in sync. The user-level registry is a
    local-only exception, so a purely local review can run personal reviewers the
    GitHub adapter does not.
  - `docs/developer-guide/attestation.md`: the design for signed local runs that
    CI verifies and replays instead of re-executing reviewers. Implemented: the
    seal in `src/seal.rs`, `bastion attest` and the CI planner in `src/attest/`.
- `.bastion.yaml`: the example reviewer registry at the repository root (the
  `.bastion.yml` spelling is also honored); update it when the schema changes.
- `.agents/skills/readme.md`: repo-local Rust coding skills and their provenance.
- `CLAUDE.md` is a bare `@AGENTS.md` import so guidance does not drift between
  agent surfaces.

## Build, test, and run

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
nudge check
```

`just check` runs all four (install [`nudge`](https://github.com/attunehq/nudge)
first; see CONTRIBUTING.md). Commands:

```sh
bastion --version
bastion validate
bastion review --base main
bastion review --base main --format jsonl
bastion review --base main --reviewer <name> --reviewer <other>
bastion review --base main --fresh
bastion runs
bastion show
bastion transcript <reviewer>
bastion clean --keep 20
bastion github codeowners --owner @your-org/platform
bastion github report --repo OWNER/NAME --pr N --sha SHA
bastion skills install
bastion skills check
bastion skills list
bastion attest
```

## Architecture map

For the full module map and the life of a `bastion review`, see
[`docs/developer-guide/architecture.md`](docs/developer-guide/architecture.md); the
backend boundary is covered in
[`docs/developer-guide/backends.md`](docs/developer-guide/backends.md). The terse
version:

- `build.rs`: derives `BASTION_VERSION` from `git describe --always --tags
  --dirty=-dirty`, with a `BASTION_VERSION` env override and a `Cargo.toml`
  fallback; also resolves the run-seal secret (`BASTION_SEAL_SECRET` env
  override, else a generated secret cached under `OUT_DIR`) that `src/seal.rs`
  embeds at build time.
- `src/main.rs`: thin binary entrypoint; wires the tokio runtime to
  `bastion::run`.
- `src/lib.rs`: library root; installs `color_eyre` + `tracing` and dispatches.
- `src/version.rs`: exposes the build-derived version string.
- `src/cli.rs`: clap derive command tree and dispatch.
- `src/commands.rs`: one handler per subcommand.
- `src/reviewer.rs` / `src/config.rs`: the declarative reviewer schema and
  registry loading/discovery. Discovery walks up for a repository `.bastion.yaml`
  and merges in a user-level one from the platform config dir (`user_config_dir`,
  overridable with `BASTION_CONFIG_DIR`), so a personal reviewer runs locally even
  when a repo has not adopted Bastion. The merge is a set keyed by name: an
  identical reviewer in both files is deduplicated (compared by effective config
  after each file's `defaults` are applied), and a same-name-different-config
  collision keeps both with the repo side scoped to `REPO_SCOPE_PREFIX` (`repo:`);
  the colon is mapped to a portable run-store path component in `paths.rs`.
- `src/routing.rs`: compiling trigger globs and matching changed files.
- `src/verdict.rs` / `src/event.rs`: the structured verdict and run-event
  schemas (the `Money` type carries cents but serializes as dollars).
- `src/context.rs`: the transport-neutral `ReviewContext` a reviewer sees beyond the
  diff: the author's stated intent, the surrounding discussion (`ContextComment` with a
  generic `Standing`), and a reviewer's prior findings (`PriorFinding`, keyed by a
  content-derived `FindingId` that excludes line numbers so it survives drift). A
  producer fills it; the backends consume it via `render_for`, which scopes prior
  findings and routed replies to the owning reviewer. Everything in it is untrusted
  input: `Standing` lets a reviewer weight a maintainer over an outsider but is read
  only when rendering the prompt, never in gate logic, so no comment can flip a
  decision. The local producer is `commands::review` (commit-message intent +
  `store::prior_findings` recall); the GitHub producer is `src/github/context.rs`.
- `src/git.rs`: the git queries the CLI needs (changed files, branch, root,
  `base..HEAD` commit messages for the local intent).
- `src/paths.rs` / `src/store.rs`: the data-directory layout and run history
  (`store::prior_findings` recalls the last run's findings for a branch).
- `src/render.rs`: human and JSONL output.
- `src/runner.rs`: the parallel, timeout-bounded runner: fans matched reviewers
  out over a `JoinSet`, fails closed on error/timeout, streams run events, folds
  in replayed and carried verdicts, and persists each run, sealing an eligible
  run on a best-effort basis at persist time (a partial run, one narrowed by
  `bastion review --reviewer`, is never sealed).
- `src/carry.rs`: incremental re-review, on both surfaces. Every reviewer
  resolving to a real verdict is stamped, best effort, with a trigger-scoped
  diff digest (`scope_digest`: its
  effective definition, the merge-base commit, the diffs of the changed files its
  trigger matched against both the merge base and the base tip, and the scoped
  commit messages; the runner re-derives it post-execution, drops a fresh stamp
  the tree no longer matches, and fails a carried one closed);
  on a re-run of the same branch, a prior *pass* with an identical digest is
  carried forward (`carried: true` on `reviewer.resolved`, no backend dispatch,
  still counted in the gate tally) while blocks and changed-scope reviewers
  execute fresh. A repository reviewer carries only from a prior run whose seal
  verifies and records no test seam; `attestation: never` opts a reviewer out;
  `--fresh` disables carry. CI carries the same way, from its own branch's
  previous CI run (when the workflow persists and restores the run store): the
  seal verifies under the release secret and the digest binds content, which is
  the bar the threat model sets (see below), so a restored store cannot smuggle in
  a fabricated pass. Do not re-litigate this as "CI must not carry an unsigned
  store": carry is not attestation. Attestation replay imports the *author's*
  signed local run across the machine boundary (hence the SSH signature); carry
  reuses a run that already executed on the same surface, so the seal (tamper
  evidence, not origin proof) plus the digest is all it needs, and CI never runs
  `bastion attest`, so a carried CI verdict is never signed into a bundle. The
  dogfood workflow keeps the backend on PATH with no `BASTION_*_BIN` override so
  its runs seal seam-free and stay carry-eligible.
- `src/seal.rs` / `src/attest/`: signed local-run attestation
  (`docs/developer-guide/attestation.md`). `seal.rs` is the run seal: an
  HMAC-SHA256 keyed by a secret embedded in the binary at build time, over a
  canonical digest of the committed HEAD tree, the merge-base tree, the
  `base..HEAD` patch-id, the effective config hash, whether a test seam was
  active, whether the working tree was dirty (uncommitted tracked changes or
  untracked files), and the sorted `reviewer.resolved` events; the runner
  seals an eligible run on a best-effort basis and persists the seal to
  `runs/<id>/seal.json` (the zero-match fast path persists without a seal, and
  `seal_run` skips when the bindings it needs are absent or no repo reviewer
  resolved). A dirty review still runs and still seals, but the seal records
  `dirty: true`; `bastion attest` refuses to attest such a run.
  `attest/` is split by concern: `mod.rs` is the `bastion attest` flow
  (verifies the seal, re-derives the repository state, signs a bundle with the
  author's SSH key, writes it as a git note under `refs/notes/bastion`),
  `bundle.rs` the bundle and note envelope, `sign.rs` SSH signing and
  signing-key resolution, and `replay.rs` the CI-side verify-and-replay
  planner (`plan`, `AttestationOutcome`) plus note lookup, which
  `commands::review` calls when the registry sets `attestations: true` and the
  run carries a GitHub source, short-circuiting to a fallback (every reviewer
  fresh, no note lookup) when the CI checkout is dirty.
- `src/backend/`: the agent execution boundary. `mod.rs` defines the `Backend`
  trait, the deterministic `MockBackend`, `dispatch`, and the shared prompt helpers
  (including the fenced-YAML `SCHEMA_INSTRUCTION`/`extract_verdict` that the Codex
  and Pi backends share); `command.rs` is the injectable `CommandRunner` subprocess
  seam; `claude_code.rs`, `codex.rs`, and `pi.rs` are the three real backends, each
  driven against a fake executable in tests. `container/` is the container runner,
  split by concern (`plan.rs` resolves the `ExecutionPlan` and builds/resolves the
  image, `runner.rs` is the `ContainerRunner` decorator and its env-file forwarding,
  `credentials.rs` the provider-credential passthrough, `teardown.rs` the timeout
  force-removal guard): `dispatch` resolves an `ExecutionPlan` (the single place an
  unprovisioned capability tier fails closed), then a reviewer with a `runner` block
  and `capabilities.network: true` runs its backend inside a built/named image via a
  `ContainerRunner` decorator over
  the `CommandRunner` seam (the backend code is untouched; the named container is
  force-removed on a timeout). `network: true` grants a containerized reviewer general
  (unscoped) egress; a container with the default `network: false` fails closed because
  provider-only scoped egress is unbuilt, so a containerized reviewer must opt into
  `network: true`. `mcp`/`skills` still fail closed. All three backends
  (`claude-code`, `codex`, `pi`; `any` maps to Claude Code) are wired and execute
  reviewers for real.
- `src/github/`: the GitHub adapter (the CI surface). `codeowners.rs` generates
  the governance block (pure text, no network); `client.rs` is the REST seam,
  modeled on the backend's `CommandRunner`: a proof-carrying `ApiRequest`, a
  `GitHubApi` trait, the real `reqwest`-backed `RestClient`, and a recording double
  for tests; `signing.rs` fetches a user's registered SSH signing keys
  (`GET /users/{username}/ssh_signing_keys`) over the same seam, for attestation
  verification; `context.rs` is the GitHub *producer* of the review context (it gathers a
  PR's body and discussion over the same REST seam, maps `author_association` to the
  generic `Standing`, filters out Bastion's own marker-tagged comments, and resolves a
  finding-thread reply back to its `FindingId` when one carries the marker, so the core
  never sees a GitHub notion). Two caveats matter here: the reply-routing path is wired and
  resolves a reply whose thread root carries a finding marker, but the reporter posts one
  sticky comment and check runs (not per-finding threads), so PR comments arrive as general
  discussion; and prior-findings memory on GitHub needs the workflow to persist and restore
  the run store across runs (a fresh runner starts empty; `bastion.yml` does this with an
  upload/download of the `bastion-run` artifact). Intent and discussion are gathered fresh
  each run and need neither. `report.rs` distills a finished run's event stream into a sticky PR
  comment and check-run payloads (all pure and unit-tested) and posts them. `bastion
  github report` reads a persisted run and posts it: the sticky comment (with every
  finding, optional ones included), a check run per reviewer, and the always-present
  aggregate `bastion` check. It also folds a skills-freshness advisory into the comment
  (a `[!WARNING]` callout) when the checked-out repo's bundled skills are missing or
  drifted from the reporting binary, computed via `skills::assess`; it is advisory only
  and never touches a check-run conclusion. The local `bastion review` mirrors it to
  stderr, but only when the repository has adopted Bastion (a repo-level registry is
  present); a user-level-only local review stays silent (see the `src/skills.rs` entry).
  When one or more reviewers replayed from a verified attestation, `report.rs` adds a
  `[!NOTE]` callout naming them, the attesting key, and when it was signed, plus a line
  on each replayed reviewer's own check-run summary; a fallback (attestation attempted
  but not honored) surfaces as a line naming why. See `src/seal.rs` / `src/attest/`
  below and `docs/developer-guide/attestation.md`.
  Check runs need a GitHub App installation token, so this
  runs under one (the default Actions `GITHUB_TOKEN` qualifies; a classic PAT does
  not). API-created check runs carry no check-suite id, so under the shared
  `github-actions` identity GitHub buckets them into a sibling workflow's suite (they
  render as `Security / <reviewer>`); posting under a dedicated per-adopter app gives
  them their own named suite. `.github/workflows/bastion.yml` mints that app token via
  `actions/create-github-app-token` when the `BASTION_APP_ID`/`BASTION_APP_PRIVATE_KEY`
  secrets exist and falls back to `GITHUB_TOKEN` otherwise; the hosted walkthrough at
  `bastion.jessica.black/github-app` (`site/src/pages/github-app.astro`) walks adopters
  through creating that app by hand in GitHub's UI (GitHub's app-manifest flow is not
  used: completing it needs a backend to exchange the temporary code for credentials,
  and Bastion deliberately custodies none). `report` decides whether to nudge toward a
  dedicated app on its own (no workflow flag): it reads the `app.slug` GitHub stamps
  on the check runs it creates, and when that is the shared `github-actions` identity
  it closes the sticky comment with a note linking to that walkthrough. This is why
  it posts check runs before the comment. See `docs/developer-guide/github-adapter.md`
  (Check-run grouping).
- `src/skills.rs` / `skills/`: the agent skills bundled into the binary. Each
  `skills/<slug>/SKILL.md` is embedded with `include_str!`; `bastion skills
  install` writes it into a consuming repo's `.claude/skills/` and `.agents/skills/`,
  `bastion skills check` fails closed when a checked-in copy has drifted from the
  embedded source (a deterministic, version-independent lint wired into
  `.github/workflows/ci.yml`), and `bastion skills list` shows what is bundled.
  `skills::assess` reuses that same check to build an advisory `DriftWarning` (missing
  and drifted files, with `plain`/`markdown` renderings) that both review surfaces emit
  when a repo's skills are stale: `bastion review` to stderr, `bastion github report`
  into the sticky comment. It is advisory and never gates. The local surface is gated
  on the repository having adopted Bastion: `bastion review` emits it only when a
  repo-level registry is present (a review running on the author's user-level
  reviewers alone stays silent, since nudging skills into a project that has not
  configured Bastion would be misdirected). CI always has a repo registry, so the
  report path is unaffected.
  This repo dogfoods the `using-bastion` skill: its agents work *on* Bastion and
  *with* it. Distinct from it are the repo-local skills that guide agents working
  on Bastion: the Rust skills and the `stop-slop` prose skill, which are not
  bundled into the binary and so sit outside `bastion skills install`/`check`. All
  of these live under both `.agents/skills/` (the agent-neutral convention) and
  `.claude/skills/` (Claude Code's native path), kept as exact copies so every
  skill is available through either surface; `tests/skills_mirror.rs` fails the
  build if the two trees drift.
- `tests/integration/`: the end-to-end suite (one `integration` test target).
  `main.rs` holds the scenarios; the reusable support is split into sibling modules
  (`fakes.rs` for the `rustc`-compiled fake agent and fake container engine,
  `fixtures.rs` for the throwaway `git` repo / `Reviewer` / registry / run-event
  helpers, `github.rs` for the in-process fake GitHub). It drives the *real compiled
  `bastion` binary* (`CARGO_BIN_EXE_bastion`), each scenario in its own throwaway
  `git` repo and private `BASTION_DATA_DIR`, against the fake agent wired in via
  `BASTION_CLAUDE_BIN`/`BASTION_CODEX_BIN`/`BASTION_PI_BIN` (and the fake engine via
  `BASTION_CONTAINER_ENGINE` for the container scenarios). The fake reads per-reviewer
  `env` (which Bastion propagates into the child) to stage passes, blocks, malformed
  output, crashes, and hangs, so the suite exercises the full subprocess path,
  fail-closed/fail-open aggregation, concurrency, persistence, and the read-back
  commands at scale. One scenario also drives `bastion github report` against the
  in-process fake GitHub (the binary's `GITHUB_API_URL` is pointed at it), asserting
  the real comment and check-run requests with no network. It detect-and-skips when
  `rustc`/`git` are absent. Sibling structural test targets:
  `tests/skills_mirror.rs` (the two skill trees stay byte-identical),
  `tests/script_safety.rs` (installer fail-closed pins), and
  `tests/user_guide_integrity.rs` (user-guide frontmatter, unique `order`
  values, and relative-link resolution, the invariants the site build under
  `site/src/content.config.ts` depends on).
- `scripts/install.sh` / `scripts/install.ps1`: the public install scripts
  (`curl | bash` and `irm | iex`). They detect the platform, download the matching
  release archive plus `checksums.txt`, verify the SHA-256, and place `bastion` on
  the user's `PATH`. They fail closed on any checksum problem; `tests/script_safety.rs`
  pins that. `.github/workflows/installers.yml` smoke-tests them against published
  releases on a schedule (not in PR CI, since it depends on release state).

## Development rules

- Do not preserve backwards compatibility by default. Mention breakage plainly.
- Weigh breakage by who actually consumes the thing. The artifact downstream users
  depend on is the `bastion` binary and its surfaces: the CLI, the verdict/event
  schema, the install scripts, and the bundled skills. A change that could wedge or
  break *those* is a real risk to weigh and call out. This repo's *own* CI is not
  one of those surfaces: users run `bastion`, not our workflows, and they do not copy
  `.github/workflows/*` verbatim (the docs show an illustrative example, but each
  team writes its own). So a change that might wedge Bastion's *own* self-review gate
  (for example `.github/workflows/bastion.yml`, which dogfoods the adapter) is only a
  minor inconvenience: the maintainer can admin-merge past a stuck gate. Do not
  contort a design to avoid self-wedging our CI, and do not add break-glass machinery
  for it. In practice, changes to our GitHub Actions workflows are nearly always safe
  to make boldly; reserve the caution for changes to the binary and its surfaces.
- Keep the local surface and the GitHub adapter as mirror images for the
  repository's reviewers: the same reviewers, verdicts, and findings, presented
  through whatever each transport makes natural. A schema change touches both
  surfaces and `docs/`. The user-level registry is the deliberate exception. A purely
  local `bastion review` also merges in an author's personal reviewers from the
  platform config dir, which the GitHub adapter and any `--repo`/`--pr` run never see,
  so a personal reviewer cannot gate someone else's PR.
- Reviewers are declarative and static. Do not add code paths that generate
  reviewers on the fly; that would break the stable trigger set and the
  governance story.
- When you fix an issue, consider whether the class of issue is one a Bastion
  reviewer could catch in future changesets (a recurring bug pattern, a convention
  that keeps getting violated, a footgun in the schema or CLI surface). If so,
  suggest adding or extending a reviewer in `.bastion.yaml` and say what
  its concern and trigger would be. Do not add the reviewer yourself: reviewers are
  governed policy, so leave the decision to the user.
- Gates fail closed. A gate that cannot produce a valid verdict is a block, never
  a silent pass. Advisors fail open.
- Do not use mocks for collaborators; prefer real pure functions and real
  filesystem/git fixtures (`tempfile`, throwaway `git init` repos), as the
  existing tests do. `MockBackend` is a deliberate deterministic test/dev double
  for the agent boundary, not a general mocking pattern.
- Follow the repo-local Rust skills (under `.agents/skills/`, mirrored to
  `.claude/skills/`): parse-don't-validate at boundaries, newtypes over
  stringly-typed data, and the clippy lint groups in `Cargo.toml`.
- Keep user-facing prose (the marketing site, the guides, the README) free of
  AI-register slop: state mechanisms, not the product's character. Follow the
  `stop-slop` skill (under `.claude/skills/stop-slop/`, mirrored to
  `.agents/skills/`), which catches the structural tells. The `prose-anti-slop`
  gate in `.bastion.yaml` blocks the merge on slop in changed prose.
- Use plain ASCII quotes in docs, comments, and generated text. No em dashes or
  en dashes, and no literal `--` used as a dash in prose; recast with a comma, a
  colon, or parentheses.

## Releases

Bastion ships as a binary on GitHub Releases. It is not published to crates.io, so
a release is just a tag, never a crates.io publish.

- To cut a release, push a tag in the shape `vX.Y.Z` to the remote (for example
  `git tag v0.2.0 && git push origin v0.2.0`). The
  [release workflow](.github/workflows/release.yml) fires on `v*` tags, generates
  one per-release run-seal secret and threads it through `BASTION_SEAL_SECRET`
  into every platform build (so the release's binaries verify each other's run
  seals), builds the platform matrix, and publishes a GitHub Release (created as
  a draft so its notes and assets are complete, then flipped to published in a
  final step).
- Do not bump the crate version in `Cargo.toml`. Leave `version = "0.0.0"` as it is:
  it is a deliberate placeholder, not a real version. The released binary's
  `--version` comes from the git tag (CI passes the tag through `BASTION_VERSION`;
  locally `build.rs` runs `git describe`). The `Cargo.toml` version is only a
  build-time fallback, not the source of truth, and is never published.
- A tag with a pre-release suffix (`v0.2.0-rc.1`) ships as a prerelease.

The full release runbook (the build matrix and version derivation) lives in
`CONTRIBUTING.md`. There is no self-review pin to bump: the
`.github/workflows/bastion.yml` gate always runs the latest published release, so it
adopts a new engine automatically once the release is published.

## Verification expectations

Run the core checks for ordinary changes:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
nudge check
```

`nudge check` enforces the mechanical conventions in `.nudge.yaml` (today: no
Unicode dashes in authored text). It runs in CI and as an agent-time hook, and
gates the same way locally; the `prose-anti-slop` gate in
`.bastion.yaml` covers the prose-voice judgment a regex cannot.

Also run targeted checks when relevant:

- Versioning changes: run `bastion --version`.
- Schema changes: update `.bastion.yaml` and the docs under `docs/`.
- Public scaffolding changes: keep `README.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `NOTICE`, and the GitHub workflows in sync.
- Rule changes: validate `.nudge.yaml` with `nudge validate` and confirm
  `nudge check` is clean.
