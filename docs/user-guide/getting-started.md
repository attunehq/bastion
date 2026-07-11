---
title: Getting started
summary: "Install Bastion, write one reviewer, and run your first review."
order: 2
---

# Getting started

> Install Bastion, write one reviewer, and run your first review.

This chapter gets you from nothing to a working review loop. It assumes you have a
git repository and one of the supported agent backends installed (the Claude Code,
Codex, or Pi CLI).

A little vocabulary shows up here in passing: *reviewer*, *gate*, *advisor*,
*verdict*, *findings*. The inline definitions are enough to follow along; the next
chapter, [Concepts](./concepts.md), defines each precisely.

## 1. Install the CLI

The quickest path is the install script. It detects your platform, downloads the
matching archive from the latest
[GitHub release](https://github.com/jssblck/bastion/releases), verifies its
SHA-256 checksum, and puts `bastion` on your `PATH`.

On Linux, macOS, or Windows under Git Bash:

```sh
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash
bastion --version
```

On Windows, from PowerShell:

```powershell
irm https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.ps1 | iex
bastion --version
```

On Windows both installers install `bastion.exe` to the same default location
(`$env:LOCALAPPDATA\Programs\bastion`); use whichever matches your shell.

The shell installer takes `-v/--version`, `-b/--bin-dir`, `-t/--tmp-dir`, and
`-l/--libc` (pass them after `bash -s --`); the PowerShell installer reads the
`Version` and `BinDir` environment variables. Pass `--help` (or set
`$env:Help="true"`) to see them all.

On Linux the installer autodetects the C runtime: it picks the statically linked
musl build on musl systems and on any host whose glibc is older than 2.35 (or
undetectable), and the glibc build only when the host glibc is 2.35 or newer
(Ubuntu 22.04, Debian 12, RHEL 9, and later). Force the choice with `--libc
gnu|musl` (or `BASTION_LIBC=...`) when you want to override it, for example to
take the portable musl build everywhere:

```sh
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- --libc musl
# ...or, without the `-s --` dance, via the environment:
curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | BASTION_LIBC=musl bash
```

Prefer to grab the archive yourself? Prebuilt binaries are attached to every
release for Linux (x86_64 and aarch64, glibc and musl), macOS (Intel and Apple
silicon), and Windows (x86_64). Download the one for your platform, extract it, and
put `bastion` on your `PATH`:

```sh
# Example: Linux x86_64
curl -sSL https://github.com/jssblck/bastion/releases/latest/download/bastion-x86_64-unknown-linux-gnu.tar.gz | tar -xz
sudo install bastion-x86_64-unknown-linux-gnu/bastion /usr/local/bin/
bastion --version
```

On a system with glibc older than 2.35, swap `gnu` for `musl` in those URLs to get
the static build.

Prefer to build from source? You need a Rust 2024 toolchain:

```sh
cargo build --release
./target/release/bastion --version
```

`bastion --version` reports a release tag when one is reachable, otherwise the
short commit SHA, with a `-dirty` suffix when the tree has uncommitted changes.

Once installed, `bastion update` upgrades in place: it resolves the latest
release, downloads the archive built for your platform, verifies its SHA-256
against the release checksums, and swaps it over the running binary, no shell or
`curl` needed. It installs the same bits as the install scripts, so a self-update
and a fresh install converge. `bastion update --check` reports whether a newer
release exists without installing it: it exits 0 whenever the release lookup
succeeds (including when an update is available, or when the running binary is a
development build), and non-zero only when the check itself fails, such as when
the network is unreachable. So `--check` is a status report, not a pass/fail gate;
script against its printed output rather than its exit code. `bastion update
--force` reinstalls the latest release even when the running version is already
current.

Bastion also prints a notice on stderr when a release build detects that a newer
version is available: a line naming the available version, followed by the
`bastion update` command to run. It shows only on an interactive terminal, never
in CI or a pipe; set `BASTION_NO_UPDATE_CHECK=1` to silence it entirely.

Two environment variables retarget where updates come from, for a fork or a
private mirror of the releases: `BASTION_REPO` overrides the `owner/name`
repository (default `jssblck/bastion`), and `BASTION_BASE_URL` overrides the base
URL the release archive and `checksums.txt` are fetched from. Leave both unset for
the normal case.

## 2. Make sure the backend is ready

Bastion does not run its own agent loop. It shells out to an existing coding-agent
CLI and reuses whatever you already have configured locally, so your billing and
auth come along for free. Install and sign in to one of:

- **[Claude Code](https://docs.claude.com/en/docs/claude-code)** (`claude`): the
  default when a reviewer does not pin a backend.
- **[Codex](https://github.com/openai/codex)** (`codex`): pin it with
  `backend: codex` on a reviewer.
- **[Pi](https://github.com/earendil-works/pi)** (`pi`): pin it with `backend: pi`.
  Pi runs against whatever provider you have configured it with locally, unless a
  reviewer pins a `model` (Pi's `provider/id` form, which selects the provider too).

A **subscription** is fine; you do not need an API key. Because Bastion just runs
the CLI, whatever you signed in with works: a ChatGPT subscription through `codex`,
a Claude subscription through `claude`, and so on. The CLI reads its own auth file
(`~/.codex/auth.json`, `~/.claude`) and refreshes its token itself. Getting that same
subscription to bill the right person in CI is its own step, covered in
[Continuous integration](./continuous-integration.md#authentication--billing).

Bastion invokes the backend as a plain executable on your `PATH` (`claude`,
`codex`, or `pi`), so confirm the one you intend to use is installed and
authenticated before running a review:

```sh
claude --version    # for the Claude Code backend
codex --version     # for the Codex backend
pi --version        # for the Pi backend
```

If the binary lives elsewhere or you want to point at a wrapper, set
`BASTION_CLAUDE_BIN`, `BASTION_CODEX_BIN`, or `BASTION_PI_BIN` to its path.

That covers the default, **native** path. If you author a reviewer with a
[`runner`](./authoring-reviewers.md#runner-and-capabilities), that reviewer runs its
backend inside a container instead (and must opt into `capabilities.network: true`;
without it the reviewer is rejected before it runs, so a gate blocks and an advisor is
skipped), so it needs a container engine on the host rather than the backend CLI: Bastion shells out to `docker` by default (set
`BASTION_CONTAINER_ENGINE` to use another, for example `podman`), and the backend CLI
(`claude` / `codex` / `pi`) must be present inside the image. A fixed set of provider
credential variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and the like) is forwarded
from your environment into the container by name so the in-container agent can
authenticate; host CLI auth that lives in a file (`~/.claude`, `~/.codex/auth.json`) is
not, so an image that relies on that should bake it in. You only need this once you
start using `runner` reviewers; the quickstart below stays native.

## 3. Write your first reviewer

Reviewers live in a declarative file at your repository root: `.bastion.yaml` (the
`.bastion.yml` spelling is also honored). Bastion discovers it by walking up from
your current directory, so you can run `bastion` from anywhere inside the repo. You
can also keep personal reviewers in a user-level `.bastion.yaml` in your platform
config directory; a local `bastion review` merges them with the repository's, which
lets you run a reviewer locally even in a repo that has not adopted Bastion (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)). Create the
repository file:

```yaml
# .bastion.yaml
reviewers:
  - name: single-responsibility
    trigger: [src/**/*.rs]   # which changed files wake this reviewer
    mode: gate               # gate = blocks the merge; advisor = comments only
    prompt: |
      Review the changeset to determine whether any one file concentrates too
      many unrelated responsibilities. If a file has clearly taken on multiple
      distinct concerns that should be separate modules, block the PR and name
      the file(s) and the concerns; otherwise approve it. A single large but
      cohesive module is not a violation.
```

That is a complete reviewer. Four fields carry the meaning: a unique `name`, the
`trigger` globs over your changed files, the `mode`, and the `prompt`. Everything
else has a sensible default. The next chapter, [Concepts](./concepts.md), explains
each of these; [Authoring reviewers](./authoring-reviewers.md) covers the full
schema.

> Adapt the trigger to your language: `src/**/*.ts`, `app/**/*.py`, and so on. The
> glob matches against the paths git reports as changed.

## 4. Run a review

Make a change in your working tree (you do not need to commit it; Bastion reviews
the working tree, including uncommitted and untracked files), then:

```sh
bastion review --base main
```

Bastion computes the files your branch changed (the diff at the merge base with
`main`, so changes that landed on `main` itself are never counted), selects the
reviewers whose triggers match, runs them in parallel, and renders progress and
verdicts. (A
re-run of the same branch may carry an already-passed reviewer's verdict
forward instead of executing it again; the exact conditions are in
[the local workflow](./local-workflow.md#re-runs-are-incremental).) A blocked
review exits non-zero; a clean one exits zero. That exit code is what lets an agent
(or a shell loop) know whether to keep working:

```sh
while ! bastion review --base main; do
  # ... fix what blocked, then loop ...
done
```

## 5. Read it as a machine stream

An agent driving the loop wants structured events, not rendered text. Ask for
JSONL: one JSON object per line, emitted as each thing happens:

```sh
bastion review --base main --format jsonl
```

You will get one typed event per line as the run progresses, ending in a
`run.completed` that carries the aggregate verdict. The
[local workflow](./local-workflow.md) chapter documents every event type and the
exact contract an agent should follow when consuming them.

## 6. Look at what was saved

Every run is persisted. Inspect history without re-running anything:

```sh
bastion runs                      # list recent runs and their verdicts
bastion show                      # re-print the latest run's findings
bastion transcript <reviewer>     # the full agent session for one reviewer
```

These are the on-demand detail; the common loop never needs them, but they are one
command away when a verdict surprises you. (`show` and `transcript` default to the
latest run; pass a run id for an older one, and the full forms are in
[the local workflow](./local-workflow.md).)

## 7. Teach your agents to use Bastion

You just drove the loop by hand. The point, though, is for your *coding agents* to
drive it themselves: run the review, read the findings, fix what blocks, and reach a
green gate before they ever open a PR. Bastion ships that instruction as a skill you
install into the repo and commit, so every agent picks it up on checkout:

```sh
bastion skills install
```

This writes a `using-bastion` skill into both `.claude/skills/` (Claude Code's
native skill path) and `.agents/skills/` (the agent-neutral convention). Commit the
result:

```sh
git add .claude/skills .agents/skills
git commit -m "Install the bastion onboarding skill"
```

The skill is generated from the binary, so re-running install after you upgrade
Bastion keeps the checked-in copy current. To confirm it has not drifted from the
binary (handy as a CI guard), run:

```sh
bastion skills check        # exits non-zero if a skill is missing or has drifted
```

The rendered file is deterministic (no version stamp or timestamp), so `check`
stays green across upgrades that do not change the skill text and only flags real
drift: a hand edit, or a forgotten re-install after the skill itself changed. When
you do upgrade, re-run `bastion skills install` to refresh, or
`bastion skills install --force` if you have local edits to overwrite. See what is
bundled with `bastion skills list`, and install into a different directory with
`--dir <path>` (repeatable).

## Keeping scratch runs out of your history

While you are experimenting, point Bastion at a throwaway data directory so trial
runs do not pile up in your real run history:

```sh
bastion --data-dir /tmp/bastion-scratch review --base main
```

The same override is available as the `BASTION_DATA_DIR` environment variable.

Note that `bastion review` never fabricates a verdict: a reviewer that executes
runs on a real backend and costs a model call. What a re-run *can* do is skip
execution entirely for a reviewer that already passed and whose inputs are
unchanged, carrying the prior verdict forward at zero token cost (see
[the local workflow](./local-workflow.md#re-runs-are-incremental)), so the loop's
cost concentrates on first runs and on the reviewers your fixes touch. To keep
cost down while iterating, start with one cheap, fast reviewer and a tight
`timeout`.

## When something goes wrong

The most common first-run snags and what they mean:

- **"no reviewer registry found ..."**: there is no `.bastion.yaml` (or
  `.bastion.yml`) in this repo or any ancestor, and no user-level one in your config
  directory either. The command errors only when both are absent and no `--include
  <path>` supplied a registry file, so create a repository registry (step 3), a
  personal one, or pass a file explicitly.
- **A reviewer registry error (malformed YAML, duplicate name, missing field).**
  The registry is validated before any agent runs, so these fail fast with a clear
  message. Run `bastion validate` (no model call) to check the merged set a local
  review would run, or `bastion validate path/to/.bastion.yaml` to check one
  registry (and everything it includes) without the user-level layer; fix it and
  re-run. See [Authoring reviewers](./authoring-reviewers.md).
- **The review blocks immediately with "did not produce a verdict".** A gate failed
  closed, usually because the backend binary is missing or unauthenticated. Re-check
  `claude --version` / `codex --version` / `pi --version` and that you are signed
  in (step 2).
- **No reviewers ran (a trivial pass).** Nothing in your changeset matched any
  reviewer's `trigger`. Confirm you actually changed a file the globs cover, and
  that `--base` points at the right branch.
- **Everything looks unchanged.** Bastion diffs at the merge base with `--base`
  (default `main`); if your base branch has a different name, pass it
  explicitly.

---

You now have a working reviewer and a review loop. Next:
[Concepts](./concepts.md). The vocabulary (triggers, modes, verdicts, the gate)
the rest of the guide builds on.
