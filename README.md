# Bastion

Agentic code review for a world where agents write all of the code.

## Why Bastion exists

Agents now write most of the code on a growing number of teams, at a volume closer
to _engineers x 100_ than _x 1_. Two things stop teams from fully unlocking that:

- **Human diff review does not scale.** Asking a 5-person team to review their
  agents' output is like asking 5 people in a 500-person org to review the other
  495. You cannot fix that by trying harder.
- **Without review, codebases rot.** Things go great until they don't, and then
  you have a ball of mud nobody can work in.

The usual shape of agentic review hands the whole diff to one reviewer and has it
leave comments for a person to act on. As you ask one generic reviewer to check
more things, its recall on any single concern collapses, and a more capable model
does not change that. A 1-item checklist agent works great; at 10 items it slips;
at 100 it is useless.

## What Bastion does

Bastion runs code review as a set of **single-concern reviewers**: focused agent
prompts, each responsible for exactly one property, run over a changeset. Because
each reviewer owns one concern, it stays at high recall; you cover more ground by
adding narrow reviewers, never by broadening one. The repository's reviewers run
locally (fast, pre-PR) and in CI (authoritative), and their verdicts aggregate into
one merge gate; locally you can also add personal user-level reviewers that CI does
not run. The human moves from reviewing diffs to authoring, curating, and governing
the reviewers.

The `bastion` CLI is the local surface: an authoring agent loops `bastion review`
until green, then opens a PR that CI largely just confirms.

For the full motivation, mental model, and threat model, see
[the core design](docs/developer-guide/design.md#the-problem).

## Install

The install scripts detect your platform, download the matching archive from the
latest [GitHub release](https://github.com/jssblck/bastion/releases), verify its
SHA-256 checksum, and put `bastion` on your `PATH`.

**Linux, macOS, and Windows (Git Bash):**

```sh
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.ps1 | iex
```

The shell installer accepts `-v/--version`, `-b/--bin-dir`, `-t/--tmp-dir`, and
`-l/--libc` (pass them after `bash -s --`); the PowerShell installer reads the
`Version` and `BinDir` environment variables. Run either with `--help` /
`$env:Help="true"` for details. For example, to pin a version and install location:

```sh
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- -v 0.1.0 -b /usr/local/bin
```

On Linux the installer autodetects the C runtime: it picks the statically linked
musl build on musl systems and on any host whose glibc is older than 2.35 (or
undetectable), and the glibc build only when the host glibc is 2.35 or newer
(Ubuntu 22.04, Debian 12, RHEL 9, and later). You can force the choice with
`--libc gnu|musl` (or `BASTION_LIBC=...`); the musl build has no glibc dependency
and runs anywhere:

```sh
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | BASTION_LIBC=musl bash
```

Prefer to do it by hand? Prebuilt binaries for Linux (x86_64 and aarch64, glibc and
musl), macOS (Intel and Apple silicon), and Windows (x86_64) are attached to every
release. Each archive bundles the binary with `README.md`, `LICENSE`, and `NOTICE`,
and the release lists SHA-256 `checksums.txt`:

```sh
# Example: Linux x86_64
curl -sSL https://github.com/jssblck/bastion/releases/latest/download/bastion-x86_64-unknown-linux-gnu.tar.gz | tar -xz
sudo install bastion-x86_64-unknown-linux-gnu/bastion /usr/local/bin/
bastion --version
```

On a system with glibc older than 2.35, swap `gnu` for `musl` in those URLs to get
the static build.

To build from source instead, you need a Rust 2024 toolchain:

```sh
cargo build --release
./target/release/bastion --version
```

## In CI

The GitHub Action runs the repository's reviewers over each pull request and
posts the verdicts back as a sticky comment and check runs; its failure is the
merge gate. You check out the PR head and authenticate your backend CLI, and
the action does the rest:

```yaml
- uses: jssblck/bastion@vX.Y.Z   # pin a bastion release tag
```

The [continuous integration chapter](docs/user-guide/continuous-integration.md)
covers the full workflow, per-author billing, and branch protection.

## Documentation

- **[User guide](docs/user-guide/README.md)**: using Bastion on your project.
  Concepts, writing reviewers, the local loop, CI, and governance. **Start here.**
- **[Developer guide](docs/developer-guide/README.md)**: working on Bastion
  itself. Architecture, the backend boundary, conventions, and the design
  references.

The [getting-started chapter](docs/user-guide/getting-started.md) takes you from
install to your first review in about five minutes.

## License

Bastion follows the repository license split described in [`NOTICE`](NOTICE):
runtime software is AGPL-3.0-or-later, while documentation and creative content are
CC-BY-SA-4.0 unless a file says otherwise. See also the
[security policy](SECURITY.md) and [contributing guide](CONTRIBUTING.md).
