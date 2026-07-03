# Conventions

> The coding rules this crate holds itself to, and how they are enforced.

[<- Containers](./containers.md) | [Developer guide index](./README.md)

---

Bastion's conventions are mostly the repo-local Rust skills under
[`.agents/skills/`](../../.agents/skills/) plus a few invariants specific to a
review gate. This chapter is the working summary; the skills directory is the full
source, with provenance in [`.agents/skills/readme.md`](../../.agents/skills/readme.md).

## The core invariants

These are not style preferences; breaking one is a correctness bug.

- **Gates fail closed; advisors fail open.** A gate that crashes, times out, or
  cannot produce a valid, consistent verdict resolves to `block`, never a silent
  pass. An advisor that does the same is ignored. "All gates pass" means every gate
  *returned* a clean pass. This invariant lives in
  [`runner.rs`](../../src/runner.rs) and is the single most important property to
  preserve; the `fail-closed-gates` reviewer in the registry guards it.
- **A backend that cannot verdict reviews nothing.** All three backends
  (`claude-code`, `codex`, `pi`) are wired, but any of them returns an error rather
  than fabricating a pass when it cannot produce a valid, consistent verdict; the
  runner turns that into a fail-closed block for a gate. See
  [Backends](./backends.md#dispatch).
- **Don't preserve backwards compatibility by default.** If the clean solution
  means changing a schema, renaming a concept, or rewriting call sites, do it and
  mention the breakage plainly. Bastion is an application, not a published library.

## Parse, don't validate

Data crossing a boundary (the YAML registry, CLI arguments, git output,
subprocess output, agent responses) is parsed *once*, at the edge, into a precise
type that makes invalid states unrepresentable, rather than carried around
stringly-typed and re-checked at each use. Examples in the codebase:

- Raw trigger strings on a `Reviewer` are compiled into a glob matcher *once* by
  [`routing.rs`](../../src/routing.rs); the compiled form is a distinct type.
- Durations and the data directory are parsed at the CLI/`paths` boundary, not
  re-parsed downstream.
- A backend parses the agent's envelope into a `Verdict` at the boundary; nothing
  downstream re-validates it.

The `parse-dont-validate` skill under `.agents/skills/` enforces this at write
time.

## Newtypes over stringly-typed data

Prefer a newtype or enum to a bare `String`/`int` when the value has meaning:
`RunId`, `Money` (cents inside, dollars on the wire), `Decision`, `Mode`, and
`Backend` rather than strings and bools floating around. This is the
`names-are-not-type-safety` skill: a descriptive variable name is not a substitute
for a type the compiler checks.

## Error handling

This is an application, so it uses `color_eyre` (an `anyhow`-style error) with
context, not a `thiserror` library-error taxonomy:

- No `.unwrap()` / `.expect()` on recoverable errors in non-test code. Propagate
  with `?` and add context (`.wrap_err(...)`).
- `expect` is for genuine, documented invariants only: "this cannot fail because
  ...", not laziness.
- `clippy::unwrap_used` and `clippy::expect_used` are denied in `Cargo.toml`
  (tests are exempt via `clippy.toml`), so this is a compile-time gate. A
  genuine invariant `expect` carries a statement-level
  `#[expect(clippy::expect_used, reason = "...")]` stating the invariant.

## Documentation

The crate sets `#![warn(missing_docs)]`. Public items carry doc comments, and
public functions returning `Result` document their failure conditions under a
`# Errors` heading; `clippy::missing_errors_doc` is denied, so a missing section
fails the build.

## Testing discipline

The test suite is hermetic and uses real fixtures, not mocking frameworks:

- **Real pure functions, real filesystem and git fixtures.** Use `tempfile` for
  throwaway directories and `git init` for throwaway repositories, as the existing
  tests do. Bastion does not use a mocking framework.
- **The one deliberate double is the backend boundary.** `MockBackend` is a
  deterministic always-pass double for exercising the runner without an agent, and
  the fake `CommandRunner` lets the real backends run against a fake executable.
  These are the agent/subprocess seam specifically, not a general pattern to reach
  for elsewhere.
- **Inline `#[cfg(test)] mod tests`** while the crate is small; the runner,
  backends, routing, config, and reviewer modules all keep their tests beside the
  code. Test names are descriptive sentences (`a_failing_gate_fails_closed`).
- **Async tests** use `#[tokio::test]`, with `start_paused = true` for
  timeout-sensitive cases so they do not sleep in real time.

The conflict decisions behind some of these (integration tests, test-module shape,
doctests, mocking, generics vs. `dyn`) are recorded in
[`.agents/skills/readme.md`](../../.agents/skills/readme.md).

## Lints

`Cargo.toml` keeps explicit Clippy lint groups. Notably:

- `clippy::inline_always` is denied.
- `clippy::unnecessary_wraps` is denied, to catch functions that claim fallibility
  without needing it.
- `clippy::unwrap_used` and `clippy::expect_used` are denied outside tests, and
  `clippy::missing_errors_doc` is denied, so error-handling discipline and
  `# Errors` coverage are compile-time checks rather than review findings.

The enforced checks are:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
nudge check
```

`just check` runs all four. `nudge check` enforces the mechanical conventions in
`.nudge.yaml` (no Unicode dashes in authored text); it runs in CI and as an
agent-time hook, so the rule is a gate rather than a suggestion. `cargo test`
also carries two structural checks: `tests/skills_mirror.rs` (the two skill
trees stay byte-identical) and `tests/user_guide_integrity.rs` (user-guide
frontmatter, `order` uniqueness, and relative-link resolution, the invariants
the site build depends on). What needs judgment rather than mechanics
(parse-don't-validate, newtypes, fail-closed handling) is caught at write time
by the repo-local skills and at review time by a human and Bastion's own
reviewers running over the PR.

## Text and prose

Use plain ASCII quotes in docs, comments, and generated text. Keep the user guide
and the design references in sync when behavior changes: for the repository's
reviewers the local and GitHub surfaces are mirror images (the user-level registry
is a deliberate local-only exception), so a schema or command change touches both
surfaces, the [user guide](../user-guide/README.md), and the design references in
this directory.

---

Back to the [developer guide index](./README.md).
