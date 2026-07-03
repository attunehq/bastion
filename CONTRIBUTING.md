# Contributing

Bastion is an application, not a Rust library intended for crates.io.
Contributions should improve the actual review experience, the reviewer schema,
the runner and backends, the CI adapter, or the maintainability of the system.

## Local setup

Install Rust stable, Git, [`nudge`](https://github.com/attunehq/nudge) (for the
mechanical-convention checks), and (optionally)
[`just`](https://github.com/casey/just).

```sh
curl -sSfL https://raw.githubusercontent.com/attunehq/nudge/main/scripts/install.sh | bash
just check        # or: cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings && nudge check
```

There are no external services to stand up: the test suite is hermetic and uses
`tempfile` for filesystem fixtures and throwaway git repositories.

## Working style

- Fix root causes when they are in scope.
- Do not preserve backwards compatibility by default; if the clean solution means
  changing schemas, renaming concepts, or rewriting call sites, do it and mention
  the breakage plainly.
- Keep the reviewer schema, the verdict/event schema, and the docs under `docs/`
  in sync when behavior changes: for the repository's reviewers the local and GitHub
  surfaces are meant to be mirror images and must not drift. The user-level registry
  is a deliberate local-only exception, so a purely local review can run an author's
  personal reviewers that CI never does.
- Update the example `.bastion.yaml` when the schema changes.
- Run `just check` before opening a PR.
- Use plain ASCII quotes in docs, comments, and generated text.

## AI-assisted contributions

AI-assisted PRs are welcome. The human submitter is responsible for the change:
understand the code, review the generated output, test it, and explain the
intent. Do not submit a raw dump of generated code that you cannot defend or
maintain. Maintainers may ask for simplification, tests, or clearer rationale
before merging.

## Pull requests

PRs should explain why the change exists, what behavior changed, any impact on
the reviewer/verdict schemas or the governance model, and the verification
performed (especially the core Rust checks).

## Releases

Bastion is a binary on GitHub Releases, not a crates.io package, so a release is
just a tag: there is no crates.io publish and no crate-version bump. GitHub Releases
double as the changelog, so there is no hand-maintained changelog file either.
Release builds derive their reported version from the git tag through
`git describe --tags` (and in CI the tag is passed through directly, so the binary's
`--version` is exact regardless of clone depth).

To cut a release:

1. Make sure `main` is green and points at the commit you want to ship.
2. Tag it in the shape `vX.Y.Z` and push the tag. Do not edit `version` in
   `Cargo.toml`: it stays `0.0.0` as a placeholder and build-time fallback only, and
   the tag is the source of truth for the released version.

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The [release workflow](.github/workflows/release.yml) generates one run-seal
   secret in its prepare job and passes it to every matrix build as
   `BASTION_SEAL_SECRET`, so all platform binaries of the release seal and
   verify run attestations identically (see
   `docs/developer-guide/attestation.md`, "The run seal"). It then builds the
   binary for
   every supported target -- Linux x86_64/aarch64 (glibc and musl), macOS
   x86_64/aarch64, and Windows x86_64 -- packages each as a `.tar.gz` alongside
   `README.md`, `LICENSE`, and `NOTICE`, generates SHA-256 `checksums.txt`, and
   publishes a GitHub Release whose notes are generated from the pull requests
   merged since the previous tag (`--generate-notes`). The release is created as a
   draft (so its notes and assets are complete before it is visible), then flipped
   to published in a final step.
4. Bastion's own review gate adopts the new engine automatically; there is no version
   pin to bump. The [`bastion.yml`](.github/workflows/bastion.yml) workflow downloads
   the *latest* published release rather than building from each PR's sources, so the
   engine that judges a PR is never the engine the PR edits, yet it tracks releases
   without a manual pin. Once you publish a release that includes `bastion github report`, the
   `Report the review to the pull request` step (which self-skips on older engines)
   starts posting the PR comment and per-reviewer checks on the next run.

Run the workflow via `workflow_dispatch` with `dry_run: true` to build and package
the whole matrix without creating a release. A tag with a pre-release suffix
(`v0.2.0-rc.1`) is published as a prerelease. macOS binaries currently ship
unsigned; code signing and notarization are a future addition that needs an Apple
Developer account.
