# Maintenance

> The runbook for routine upkeep: dependencies, the Rust toolchain, incoming
> issues, and maintenance releases. An agent runs it daily; a human can run it by hand the same way.

Routine maintenance is delegated to a scheduled coding agent. It runs once a day on
the maintainer's machine, works from this file, and merges what it can verify. It
hands the maintainer only the decisions a later change cannot cheaply undo. This
file is the policy; the scheduled task's prompt only points here, so change the
policy by changing this file.

## Policy

Stay current aggressively. Take every new stable Rust release, every new major
version of a dependency, and every new major of a GitHub Action as soon as it
ships, rather than letting upgrades pile up into a migration. A small upgrade
taken the week it lands is cheaper than five taken together a year later.

Two rules hold over that default:

- **Gates fail closed.** Nothing merges without the repository's required checks
  green on the final head, and nothing is merged around a failing check.
- **Escalate what cannot be cheaply undone.** The cases are listed under
  [Escalate instead of merging](#escalate-instead-of-merging). Everything else, the
  run decides and merges.

## Each run

Work in order. A later step never blocks an earlier one's merge.

### 1. Sync

Fetch `origin` and branch from `origin/main` for each change
(`jssblck/maint-YYYY-MM-DD`, or a descriptive name for an issue fix). If the
working tree is dirty from an earlier run, look at what is there before discarding
it. Run `bastion update` so the local binary matches the release CI runs (the seal
secret is per release; see `AGENTS.md`).

### 2. Main is green

Check the latest CI run on `main`, the latest scheduled `Security` run, and the
checks on open PRs. Push runs fire only when something merges, so a quiet `main`
can turn red without a push run showing it; a RUSTSEC advisory published since the
last merge shows up only in the weekly scheduled run, or locally with
`git show origin/main:Cargo.lock > <tmp>/Cargo.lock && cargo audit -f <tmp>/Cargo.lock`.

A red `main` (a new stable Rust lint, a fresh RUSTSEC advisory failing
`Cargo audit`, an expired action) blocks every PR, so fixing it comes first, as its
own PR. The exception is a fix that is only the dependency bump step 3 makes
anyway (most advisories): let the consolidated PR carry it, say so at the top of
its description, and land it before anything else. Once the fix merges, update
the branch of each open PR whose checks failed on it, so they re-run against the
fixed `main`.

### 3. Dependencies

Cover all four sources, then land them as one consolidated PR. Give an upgrade its
own PR when it needs more than mechanical code changes, so it can be reverted
alone.

- **Rust toolchain.** `rustup update stable`. CI installs floating `stable`, so a
  new release reaches CI on its own; the work is fixing what it newly warns about.
  Adopt a new edition once it is stable (`cargo fix --edition`, then fix the rest
  by hand).
- **Cargo.** `cargo upgrade --incompatible` (from `cargo-edit`) for majors in
  `Cargo.toml`, then `cargo update` for the lockfile. Fix the code for breaking
  API changes. Keep the comments in `Cargo.toml` true: several record why a crate
  or feature set was chosen (pure Rust, no system libraries, musl-friendly).
- **Site (`site/`).** Upgrade to the latest majors (`npx npm-check-updates -u`),
  `npm install`, `npm audit fix`. The site gate runs `npm ci`, which fails on peer
  conflicts, so hold back a package whose latest release conflicts with a peer and
  say so in the PR.
- **GitHub Actions.** Bump pinned versions and move floating tags to the newest
  major.

Close each Dependabot PR the consolidated PR supersedes, with a comment naming it.
`cargo audit` and `npm audit` must come out clean; an advisory with no fixed
release goes in the PR description and the run report.

Verify before opening the PR: `just check`, the site's `npm ci`, `npm run build`,
and `npm run check`, plus `cargo audit`. A dependency change that could affect the
release targets (a new system library, a raised minimum glibc or macOS, a crate
whose MSRV exceeds stable) needs the release matrix in mind: say in the PR which
targets were reasoned about.

### 4. Issues

Triage open issues that have no linked PR, no `maintainer-decision` label, and no
comment from the maintainer's account since the author's last comment. GitHub's
state is the record of what has been handled, so no separate ledger is kept.

- **Bugs.** Reproduce with a failing test first, fix the root cause, and open a PR
  that says `Fixes #N`.
- **Routine feature requests.** Implement the request when it fits
  [the core design](./design.md) and the threat model, keeps the local and GitHub
  surfaces mirror images, and does not need a product call. Update `docs/` and
  `.bastion.yaml` alongside, as `AGENTS.md` requires.
- **Questions.** Answer from the docs and code, briefly and factually, and link
  the doc section. A question the docs should have answered is a docs fix.
- **Duplicates and already-fixed issues.** Link the original or the fixing PR and
  close.

Comments on issues are posted from the maintainer's GitHub account, so keep them to
facts: what was fixed and where, what was tried when a report cannot be reproduced,
or what information would unblock it. Declining a request is a maintainer decision.

### 5. Review and merge

Every PR the run opens follows the local Bastion loop in `AGENTS.md`: review
against `origin/main`, attest the green run, push `refs/notes/bastion`. A
changeset that triggers no reviewers produces an empty, unsealed run, so there is
nothing to attest; skip the attest and push for it, since CI's gate costs nothing
on the same changeset. Squash-merge once the required checks are green on the
final head; there is no merge queue, so a break between two PRs that were each
green alone shows up as a red `main`, which step 2 of the next run catches.
Dependabot PRs that are not superseded merge the same way when green.

If a PR cannot be made green after three honest attempts, stop, leave it open as a
draft, and escalate with what was tried.

### 6. Release

Merged work reaches users only through a release, so the run cuts maintenance
releases itself. A release has a cost every consumer pays: each one generates a
new seal secret, so every carried or attested run sealed by the previous release
stops verifying and re-executes its reviewers once, and consumers' CI adopts the
new engine immediately (`action.yml` installs the newest stable release in the
pinned major). Hence the batching below, and the release candidate before every
stable tag.

**When.** Only for binary-affecting changes since the last stable tag: `src/`,
`Cargo.toml`, `Cargo.lock`, `build.rs`, `action.yml`, `scripts/`, or `skills/`.
Docs, site, and CI-only changes never trigger a release.

- **Security, same day.** A RUSTSEC advisory fixed on `main` since the last stable
  tag, in a crate compiled into the release binary (`cargo tree -e normal
  --target all -i <crate>` finds it). Check with `cargo audit` against the last
  tag's lockfile, as in step 2.
- **Everything else, weekly.** Monday's run releases whatever binary-affecting
  change has accumulated.

**Version.** A minor bump when any PR merged since the last stable tag adds a
user-visible capability (a command, flag, config key, or action input); otherwise
a patch. A major bump means a breaking change, which was escalated before it
merged, so the run never cuts a major.

**Procedure.** `main` must be green (step 2) first. Then:

1. Tag the release candidate `vX.Y.Z-rc.N` on `origin/main` and push the tag. Wait
   for the `Release` workflow to succeed. A prerelease does not move the floating
   major tag or `releases/latest`, so no consumer adopts it.
2. Verify the candidate: the release is marked prerelease and carries the seven
   platform archives plus `checksums.txt`; the archive for this machine matches its
   checksum; the extracted binary's `--version` prints the candidate tag; and in a
   checkout of the tagged commit, `bastion validate`, `bastion skills check`, and a
   `bastion review --base origin/main` over an empty changeset all succeed.
3. Tag `vX.Y.Z` on the same commit as the candidate (the verified artifacts and
   the shipped ones come from one source) and push it. Wait for the workflow, then
   confirm `releases/latest` redirects to the new tag, the floating major tag
   (`v1`) points at the release commit, the release notes list the PRs since the
   previous stable tag, and `bastion update` installs it locally.

Any failure stops the release and goes to the maintainer. Never delete, move, or
re-push a published tag or edit a published release's assets: a bad candidate is
superseded by `-rc.N+1`, and a bad stable release by a fix-forward patch release.

### 7. Report

End the run with a short report in the scheduled task's thread: what merged (PR and
SHA), what is open and why, what was escalated, any advisory left unresolved, and
any release cut (with its link). A day with nothing to do is one line.

## Escalate instead of merging

Open the work as a draft PR (or comment on the issue when there is no code yet),
add the `maintainer-decision` label so later runs skip it, and message the
maintainer with what the decision is, the recommendation, and the link. Then carry
on with the rest of the run.

Escalate when the change:

- **Touches the trust boundary.** The seal, carry, attestation, or signature
  verification (`src/seal.rs`, `src/carry.rs`, `src/attest/`), or a major upgrade
  of a crate they rest on (`hmac`, `sha2`, `getrandom`). A behavior change here can
  make CI reject every existing attestation, or accept one it should not.
- **Breaks a downstream surface.** A removed or renamed CLI flag, a changed
  verdict, event, or reviewer schema, a changed `action.yml` input or output, a
  changed install script, or a changed bundled skill. Additive changes within the
  existing design are routine.
- **Changes behavior users would notice through a dependency.** Swapping the TLS,
  YAML, or HTTP stack, a parser whose accepted input changes, dropping a release
  target, adding a mandatory system library, or raising the minimum OS.
- **Adds or drops a product capability** beyond what an issue asked for, changes
  the governance model, or conflicts with [the core design](./design.md).
- **Declines a request**, or would close an issue as won't-fix.
- **Involves a suspected vulnerability.** Do not discuss it in public issues or PRs;
  `SECURITY.md` routes reports privately.
- **Is an outside contributor's PR.** Review it, but merging someone else's
  work is the maintainer's call.
- **Would cut a major release**, or a release outside the triggers in
  [step 6](#6-release).

## Running it on a schedule

The maintainer's T3 environment runs this daily at 06:00 Pacific, two hours after
Dependabot's 04:00 batch, with a prompt that points at this file. The run posts its
report to one T3 thread and messages the maintainer on Slack (through `tuna`) only
for escalations.
