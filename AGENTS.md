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
(`src/runner/`) and all five agent backends (Claude Code, Codex, Pi, Grok Build, and
Muse Code, under `src/backend/`) are implemented and execute reviewers for real over
an injectable subprocess seam. Keep that boundary honest: a backend that cannot produce a valid
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
    local-only exception. A purely local review uses personal reviewers as a
    fallback, or merges them with `--with-user-reviewers`; the GitHub adapter never
    runs them.
  - `docs/developer-guide/attestation.md`: the design for signed local runs that
    CI verifies and replays instead of re-executing reviewers. Implemented: the
    seal in `src/seal.rs`, `bastion attest` and the CI planner in `src/attest/`.
- `.bastion.yaml`: the example reviewer registry at the repository root (the
  `.bastion.yml` spelling is also honored); update it when the schema changes.
- `action.yml`: the composite GitHub Action consumers pin (`attunehq/bastion@v0`).
  Its inputs and outputs are a downstream surface like the CLI; the consumer-facing
  reference is `docs/user-guide/continuous-integration.md`. Sequencing lesson for
  any future pinnable artifact: snapshot discipline (docs describe only the
  shipped release) keeps it out of `docs/` until a published release contains
  it, so ship the artifact with raw-mechanism docs first and flip the docs in a
  follow-up once the release exists.
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
bastion review --base main --include extra-reviewers.yaml
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

When a branch's changeset triggers reviewers, close the review loop locally
before pushing: run `bastion review --base main`, and once it is green, run
`bastion attest` and push the note alongside the branch
(`git push origin refs/notes/bastion`). The CI gate then verifies and replays
the attested run instead of re-executing the reviewers, so the review tokens
are spent once. Three mechanics matter: CI re-derives the merge base from the
PR's base branch and refuses a note sealed against a stale one, so before the
run you attest, fetch and get the branch up to date with `origin/main`, and
review against `origin/main` rather than a local `main` that can lag it;
attestation binds to HEAD's committed tree, so attest after the final commit;
and the seal secret is per-release while CI runs the latest published release,
so keep the local binary current with `bastion update` or CI cannot verify the
seal and falls back to a fresh run. A rebase does not make the re-review
expensive: the carry digest binds the changeset rather than the merge-base
commit, so after syncing, reviewers whose trigger-scoped diff is unchanged
carry their pass forward and only the affected ones re-execute.

Targeted checks when relevant:

- Versioning changes: run `bastion --version`.
- Schema changes: update `.bastion.yaml` and the docs under `docs/`.
- Public scaffolding changes: keep `README.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `NOTICE`, and the GitHub workflows in sync.
- Rule changes: validate `.nudge.yaml` with `nudge validate`, then confirm
  `nudge check` is clean. `nudge` enforces the mechanical conventions in
  `.nudge.yaml` (today: no Unicode dashes in authored text) as a CI gate and an
  agent-time hook; the prose-voice judgment a regex cannot express is
  write-time discipline via the `stop-slop` skill.

## Proof artifacts

- Proof artifacts (screenshots, HTML mocks, logs) go in `scratch/`, which is
  gitignored. Never commit them and never upload them to a host outside GitHub.
- Upload PR images with the `github-image-upload` skill (`gh image`), which produces
  `github.com/user-attachments` URLs that stay private for private repos.
- `bastion attest` requires a clean tree; files in `scratch/` do not dirty it, so
  proof does not cost you an extra paid review.

## Architecture map

The module map and the life of a `bastion review` live in
[`architecture.md`](docs/developer-guide/architecture.md); the backend boundary in
[`backends.md`](docs/developer-guide/backends.md), the GitHub adapter in
[`github-adapter.md`](docs/developer-guide/github-adapter.md), and the seal and
attestation in [`attestation.md`](docs/developer-guide/attestation.md). Start there
before touching a module: those docs are the canonical map, and this file keeps no
second copy to drift against.

## Development rules

- Do not preserve backwards compatibility by default. Mention breakage plainly.
- Weigh breakage by who actually consumes the thing. The artifact downstream users
  depend on is the `bastion` binary and its surfaces: the CLI, the verdict/event
  schema, the install scripts, the GitHub Action (`action.yml` and its
  inputs/outputs), and the bundled skills. A change that could wedge or
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
  local `bastion review` uses an author's personal reviewers when the repository has
  no registry, or merges them when `--with-user-reviewers` is passed. The GitHub
  adapter and any `--repo`/`--pr` run never see them, so a personal reviewer cannot
  gate someone else's PR.
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
  `.agents/skills/`), which catches the structural tells. This write-time pass
  is the only enforcement layer; the registry's review-time prose gate was
  retired to fit the review token budget.
- Use plain ASCII quotes in docs, comments, and generated text. No em dashes or
  en dashes, and no literal `--` used as a dash in prose; recast with a comma, a
  colon, or parentheses.

## Releases

Bastion ships as a binary on GitHub Releases (never a crates.io publish). To cut
one, push a `vX.Y.Z` tag; a `-rc.N` suffix ships as a prerelease. Do not bump the
`Cargo.toml` version: `0.0.0` is a deliberate placeholder, and the released binary's
`--version` comes from the tag. A stable release also auto-advances the floating
major tag (`v0`) that GitHub Action consumers pin; never push a bare major tag by
hand. There is no self-review pin to bump; the `.github/workflows/bastion.yml`
gate always runs the latest published release. The full runbook (the
seal-secret-per-release flow, the build matrix, version derivation) is in
`CONTRIBUTING.md`.
