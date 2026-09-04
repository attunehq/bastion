# Bastion developer guide

> For people working on Bastion itself: the codebase, not the review policy.

This guide is for contributors changing Bastion's own source. If you want to *use*
Bastion on your project, you are in the wrong place: read the
[user guide](../user-guide/README.md) instead.

## Orientation

Bastion is a Rust 2024 application (a single binary, `bastion`), not a library for
crates.io. It runs single-concern reviewers as fitness functions over a changeset,
both locally (the CLI) and in CI. The data and routing layers are real and tested;
the parallel, timeout-bounded runner and all five agent backends (Claude Code,
Codex, Pi, Grok Build, and Muse Code) execute reviewers for real over an injectable
subprocess seam. Keep that boundary honest: a backend that cannot produce a valid verdict must error
rather than fabricate a pass, and gates must fail closed on it.

## Contents

1. **[Architecture](./architecture.md)**: the module-by-module map and the data
   flow of a single `bastion review`. Read this first to find your way around.
2. **[Backends](./backends.md)**: the agent execution boundary: the `Backend`
   trait, the `CommandRunner` subprocess seam, dispatch, `MockBackend`, and how to
   add a new backend.
3. **[Containers](./containers.md)**: how a reviewer with a `runner` block (and
   `capabilities.network: true`) runs its backend inside a container: the
   `ExecutionPlan` parse, image resolution, and the `CommandRunner` decorator that
   wraps a spec into a `docker run`.
4. **[Conventions](./conventions.md)**: the repo-local Rust skills,
   parse-don't-validate at boundaries, newtypes over stringly-typed data, the
   clippy lint groups, fail-closed discipline, and the testing approach (real
   fixtures, not mocks).

### Design references

The authoritative specifications live alongside this guide. They describe intended
behavior and the rationale behind it; the code implements them, and where the two
disagree the design docs are the spec the code should converge to (or the docs
should be updated; they are meant to stay in sync). Read
[`architecture.md`](./architecture.md) first to orient; reach for these references
when a chapter points you into one.

- **[Core design](./design.md)**: reviewers, the verdict contract, the merge
  gate, and the threat model. The authoritative design reference.
- **[GitHub adapter](./github-adapter.md)**: the CI adapter: Actions, checks,
  governance, authentication, and billing.
- **[Local surface](./local-surface.md)**: the local CLI surface this crate
  implements, including the on-disk run store.
- **[Attestation](./attestation.md)**: signed local-run attestation, letting CI
  verify and replay a run instead of re-executing the reviewers.

## Build, test, and run

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
nudge check
```

`just check` runs all four; it is the gate to pass before opening a PR. `nudge
check` enforces the mechanical conventions in `.nudge.yaml` (no Unicode
dashes in authored text); install [`nudge`](https://github.com/attunehq/nudge)
first (see CONTRIBUTING.md). The test suite is hermetic (no external services),
using `tempfile` for filesystem fixtures and throwaway `git init` repositories.

Common commands while developing:

```sh
cargo run -- --version
cargo run -- review
cargo run -- review --format jsonl
just review                 # the same review, via the Justfile
just review-base main       # override automatic base selection
just version
```

## Targeted checks

Run these in addition to the core checks above when relevant:

- **Versioning changes:** `bastion --version` (the string is derived at build time
  by `build.rs` from `git describe --always --tags --dirty=-dirty`, overridable via
  the `BASTION_VERSION` env var, with a `Cargo.toml` fallback).
- **Schema changes:** update [`.bastion.yaml`](../../.bastion.yaml)
  and the affected docs. For the repository's reviewers the local and GitHub surfaces
  are mirror images and must not drift (the user-level registry is a deliberate
  local-only exception); a schema change touches both surfaces, the user guide, and
  the design references.
- **Public scaffolding changes:** keep `README.md`, `CONTRIBUTING.md`,
  `SECURITY.md`, `NOTICE`, and the GitHub workflows in sync.

## Contributing and releases

The contribution workflow, AI-assisted-contribution policy, and the release process
(tagging, the release matrix, and version derivation) live in
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) at the repository root. Agent
guidance (the same rules in the form coding agents consume) is in
[`AGENTS.md`](../../AGENTS.md).
