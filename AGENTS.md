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
(`src/runner/`) and all three agent backends (Claude Code, Codex, and Pi, under
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
team code review for aligned contributors, built to be robust against *inadvertent*
gaming and erosion, not a deliberately malicious actor. The bar a reused verdict
must clear is "a real review of this content by this release, not one the agent
fabricated," never "prove a human demonstrably signed off." The seal
(`src/seal.rs`) is an HMAC keyed by a secret embedded in a public binary, so it is
tamper *evidence*, not proof of origin; forging one means extracting that secret,
which is the deliberate malice the threat model already excludes.

That is why the two reuse paths need different machinery. Carry (`src/carry.rs`)
reuses a run from the *same surface*, so the seal plus a content-binding digest is
enough (no signature). Attestation (`src/attest/`) imports the *author's* run
across the local-to-CI boundary, so it adds an SSH signature tying that run to a
forge account CI already trusts. The signature is a presence speed bump, not
evidence a human attested: keys sit unlocked on dev machines.

Guidance that follows, so we stop re-deriving it:

- Do not design against deliberate malice (extracting the embedded secret, forging
  a store, hand-editing a run). It is out of scope, and for the seal it is already
  possible, so contorting a design to prevent it buys nothing and costs clarity.
- Do not promote a proportionate speed bump into a guarantee it does not provide.
  "The agent did not literally fabricate this review" is the standard; "a human was
  demonstrably in the loop" is not, and chasing the latter fights the whole point of
  Bastion.
- The one hard line that holds regardless: gates fail closed, advisors fail open. A
  gate that cannot produce a valid verdict blocks, never a silent pass.

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
bastion update
bastion update --check
```

Targeted checks when relevant:

- Versioning changes: run `bastion --version`.
- Schema changes: update `.bastion.yaml` and the docs under `docs/`.
- Public scaffolding changes: keep `README.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `NOTICE`, and the GitHub workflows in sync.
- Rule changes: validate `.nudge.yaml` with `nudge validate`, then confirm
  `nudge check` is clean. `nudge` enforces the mechanical conventions in
  `.nudge.yaml` (today: no Unicode dashes in authored text) as a CI gate and an
  agent-time hook; the `prose-anti-slop` gate in `.bastion.yaml` covers the
  prose-voice judgment a regex cannot.

## Architecture map

The full module map and the life of a `bastion review` live in
[`architecture.md`](docs/developer-guide/architecture.md); the backend boundary in
[`backends.md`](docs/developer-guide/backends.md), the GitHub adapter in
[`github-adapter.md`](docs/developer-guide/github-adapter.md), and the seal and
attestation in [`attestation.md`](docs/developer-guide/attestation.md). This is the
file-to-purpose map at a glance; go to those docs before duplicating their prose
here.

- `build.rs`: derives `BASTION_VERSION` (from `git describe`, overridable) and bakes
  the rustc target triple as `BASTION_TARGET` and the run-seal secret the binary
  embeds.
- `src/main.rs` / `src/lib.rs` / `src/version.rs`: binary entrypoint, library root
  (installs `color_eyre` + `tracing` and dispatches), and the build-derived version.
- `src/cli.rs`: the clap command tree and dispatch.
- `src/commands/`: one module per subcommand (`review`, `validate`, `read_back`,
  `codeowners`, `attest`, `update`, `github_report`, `skills`); `mod.rs` re-exports
  the CLI surface `cli.rs` calls.
- `src/reviewer.rs` / `src/config.rs`: the declarative reviewer schema and registry
  loading. Discovery walks up for a repository `.bastion.yaml` and merges in a
  user-level one from the platform config dir (overridable with `BASTION_CONFIG_DIR`),
  so a personal reviewer runs locally even against a repo that has not adopted
  Bastion. The merge is a set keyed by name; a same-name collision keeps both with the
  repo side scoped to `REPO_SCOPE_PREFIX` (`repo:`), whose colon `paths.rs` maps to a
  portable run-store path component.
- `src/routing.rs`: compiling trigger globs and matching changed files.
- `src/verdict.rs` / `src/event.rs`: the structured verdict and run-event schemas
  (`Money` carries cents but serializes as dollars).
- `src/context.rs`: the transport-neutral `ReviewContext` a reviewer sees beyond the
  diff (author intent, discussion, prior findings). Everything in it is untrusted and
  read only when rendering the prompt, never in gate logic. The local producer is
  `commands::review`; the GitHub producer is `src/github/context.rs`.
- `src/git.rs`: the git queries the CLI needs (changed files, branch, root,
  `base..HEAD` commit messages).
- `src/paths.rs` / `src/store.rs`: the data-directory layout and run history
  (`store::prior_findings` recalls the last run's findings for a branch).
- `src/render.rs`: human and JSONL output.
- `src/runner/`: the parallel, timeout-bounded runner. `mod.rs` is the orchestration
  core; `verdicts.rs` folds in replayed and carried verdicts, `seal.rs` seals an
  eligible run, `persist.rs` writes the artifacts, `tests.rs` the unit tests. Fails
  closed on error or timeout; a partial run (narrowed by `--reviewer`) is never sealed.
- `src/carry.rs`: incremental re-review on both surfaces. A reviewer's verdict is
  stamped with a trigger-scoped `scope_digest`; on a re-run a prior *pass* with an
  identical digest is carried forward with no backend dispatch, while blocks and
  changed-scope reviewers execute fresh. Full mechanics in
  [`local-surface.md`](docs/developer-guide/local-surface.md#incremental-re-review).
- `src/seal.rs` / `src/attest/`: the run seal (an HMAC over the committed tree, the
  merge-base tree, the `base..HEAD` patch-id, the config hash, the test-seam and
  dirty flags, and the sorted resolved events) and signed attestation, split into
  `attest/{mod,bundle,sign,replay}.rs`. See
  [`attestation.md`](docs/developer-guide/attestation.md).
- `src/backend/`: the agent execution boundary. `mod.rs` defines the `Backend` trait,
  the deterministic `MockBackend`, `dispatch`, and the shared prompt helpers;
  `command.rs` is the injectable subprocess seam; `claude_code.rs`, `codex.rs`, and
  `pi.rs` are the real backends; `container/` runs a backend inside a built image,
  split into `plan`/`runner`/`credentials`/`teardown`. `dispatch` is the single place
  an unprovisioned capability tier fails closed. See
  [`backends.md`](docs/developer-guide/backends.md).
- `src/github/`: the GitHub adapter (the CI surface). `codeowners.rs` generates the
  governance block; `client.rs` is the `reqwest`-backed REST seam; `signing.rs`
  fetches a user's SSH signing keys; `context.rs` produces the review context from a
  PR; `report/` distills a finished run into a sticky comment and check runs, split by
  concern (`comment`, `callouts`, `checks`, `requests`, `post`). Check runs need a
  GitHub App installation token. See
  [`github-adapter.md`](docs/developer-guide/github-adapter.md).
- `src/skills.rs` / `skills/`: the agent skills bundled into the binary with
  `include_str!` and installed by `bastion skills install`/`check`/`list`;
  `skills::assess` builds the advisory drift warning both review surfaces emit.
  Distinct from the bundled `using-bastion` skill are the repo-local skills that guide
  agents working *on* Bastion (the Rust skills and `stop-slop`), which are **not**
  bundled and sit outside `skills install`/`check`. All skills live under both
  `.agents/skills/` (agent-neutral) and `.claude/skills/` (Claude Code's path) as
  exact copies; `tests/skills_mirror.rs` fails the build if the two trees drift.
- `src/update.rs`: the native self-updater behind `bastion update` (resolves the
  latest release, verifies it against `checksums.txt`, swaps the running binary with
  `self-replace`) and the passive out-of-date nag. `BASTION_REPO` and
  `BASTION_BASE_URL` override the release source (tests point them at a local server).
- `tests/integration/`: the end-to-end suite driving the *real compiled binary*
  against a `rustc`-compiled fake agent, each scenario in its own throwaway `git` repo
  and private `BASTION_DATA_DIR`, with the fake wired in via
  `BASTION_CLAUDE_BIN`/`BASTION_CODEX_BIN`/`BASTION_PI_BIN` (and a fake engine via
  `BASTION_CONTAINER_ENGINE`). Scenarios are grouped into per-theme files
  (`aggregation`, `carry`, `container`, `accounting`, `persistence`, `cli_surface`,
  `github_report`, `attestation`) over shared `fakes`/`fixtures`/`github` support.
  Sibling structural targets: `tests/skills_mirror.rs`, `tests/script_safety.rs`, and
  `tests/user_guide_integrity.rs`.
- `scripts/install.sh` / `scripts/install.ps1`: the public install scripts. They
  detect the platform, download the matching archive plus `checksums.txt`, verify the
  SHA-256, and fail closed on any checksum problem; `tests/script_safety.rs` pins that.

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

Bastion ships as a binary on GitHub Releases (never a crates.io publish). To cut
one, push a `vX.Y.Z` tag; a `-rc.N` suffix ships as a prerelease. Do not bump the
`Cargo.toml` version: `0.0.0` is a deliberate placeholder, and the released binary's
`--version` comes from the tag. There is no self-review pin to bump; the
`.github/workflows/bastion.yml` gate always runs the latest published release. The
full runbook (the seal-secret-per-release flow, the build matrix, version
derivation) is in `CONTRIBUTING.md`.
