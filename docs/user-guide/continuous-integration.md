---
title: Continuous integration
summary: "Promoting your reviewers into GitHub Actions: one required check and per-author billing."
order: 6
---

# Continuous integration

> Promoting your reviewers into GitHub Actions: one required check and per-author
> billing.

The local loop gets you to green before you open a PR. CI is the authoritative
confirmation: it executes or replays the reviewers from the repository's
`.bastion.yaml` (replay draws from a verified attestation, when the registry sets
`attestations: true`) and reports one merge gate. Because routing and aggregation are
shared, CI rarely surprises an
author who looped locally. It can differ in two ways: CI adds the PR's description and
discussion that a default local run lacks, and CI runs the repository's reviewers
only, while a local run can also include your personal user-level reviewers (see
[Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)). The user-level
layer is local-only by design, so it can never gate someone else's pull request. This
chapter covers the GitHub adapter, the one forge Bastion targets.

> Bastion does not own CI; it plugs into yours. The workflow, the secrets, the
> preview environments, and the branch-protection rules are GitHub's. Bastion
> reads and writes them through a thin adapter and otherwise stays out of the way.

## How a run maps to GitHub

On each pull-request event (`opened`, `synchronize`, `reopened`) the workflow runs
`bastion review`, which computes the changed files, routes to the matching
reviewers, then executes them in parallel with timeouts or replays those covered by a
verified attestation (a replayed reviewer is reconstructed from the bundle, with no
backend dispatch or timeout), and persists the run. A second step, `bastion github
report`, reads that run and posts it. A verdict reaches two GitHub surfaces:

- **Findings are posted to the PR.** `bastion github report` renders every finding
  (blocking and optional) into a single sticky PR comment, and attaches each located
  finding to its reviewer's check run as an annotation on the finding's `path` and
  line range. The sticky comment is the surface an implementing agent reads; it
  carries everything it needs to act.
- **Each verdict becomes a check run** named after the reviewer
  (`bastion / tenant-isolation`). A blocking gate reports `failure`; a passing gate
  reports `success`; an advisor reports `success` with its findings attached.

`bastion github report` also folds a skills-freshness advisory into the sticky comment
when the checked-out repo's bundled skills (`.claude/skills` and `.agents/skills`) are
missing or have drifted from the reporting binary, the same comparison
`bastion skills check` makes. It renders as a `> [!WARNING]` callout just under the
headline, naming each affected file and pointing at `bastion skills install`. It is
advisory only, so it never changes a check-run conclusion or the `bastion` gate; it
tells you to refresh stale skills without failing the build. The local `bastion review`
prints the same notice to stderr when the repository has adopted Bastion (a repo-level
reviewer registry is present); a review running on user-level reviewers alone stays
silent. In CI the repository always has a registry, so this advisory is unaffected.

The local-to-GitHub mapping is one-to-one for the repository's reviewers: the JSONL
events a CI or `bastion review --repo/--pr` run produces are the same decisions GitHub
renders as checks and a comment. (A purely local run can also include your personal
user-level reviewers, whose events are local-only and have no GitHub twin.) Each
GitHub surface has a local twin:

| GitHub                                                         | Local                               |
| -------------------------------------------------------------- | ----------------------------------- |
| A per-reviewer check run reaching its conclusion               | `reviewer.resolved` event           |
| Findings in the sticky PR comment and as check-run annotations | `findings` in `reviewer.resolved`   |
| Tokens and cost in the check output                            | `usage` in `reviewer.resolved`      |
| The aggregate `bastion` check and the sticky PR comment        | `run.completed` event               |
| Transcript in the uploaded run artifact                        | saved on disk, `bastion transcript` |

The local stream additionally carries `run.started` and `reviewer.started` for an
agent reacting as the run goes; those have no separate GitHub surface, because
`bastion github report` runs after the review finishes and renders the result in one
pass. This mapping is deliberate, so an agent's local loop and the CI gate stay
aligned on what a review means.

## The one required check

Branch protection needs you to name the checks that must pass, but Bastion's set of
reviewers *varies per PR*: a docs-only PR and a server PR trigger different
reviewers, so there is no fixed list of names to require.

The fix is a single always-present check, **`bastion`**, and it is the only one
branch protection requires. It runs even when zero reviewers match (a trivial pass)
so it is always there to require. Internally it reflects the aggregate: `success`
only when every triggered gate passed, `failure` if any gate blocked, errored, or
timed out (fail-closed). The per-reviewer checks stay informational; `bastion` is
the gate.

## The workflow

The adapter is a self-hosted workflow that installs a published `bastion`
release plus your backend CLI, authenticates the backend, runs `bastion review`, and
then runs `bastion github report` to post the results to the PR. The CLI exits
non-zero if any gate blocks, so the job's pass/fail *is* your merge gate; the report
step adds the sticky comment and the per-reviewer and aggregate check runs. That host
backend CLI and its auth cover **native** reviewers (the default). A reviewer with a
[`runner`](./authoring-reviewers.md#runner-and-capabilities) runs its backend
*inside a container* instead (and must declare `capabilities.network: true`; without it
the reviewer is rejected before it runs, so a gate blocks and an advisor is skipped), so
for those the job needs a container engine on the runner (`docker` by default, or
whatever `BASTION_CONTAINER_ENGINE` names) and the backend executable plus its auth
inside the image, not on the host. The fixed provider
credential variables are forwarded from the job into the container by name, so the host
auth still reaches a containerized reviewer's provider even though the CLI itself lives
in the image:

```yaml
name: bastion
on:
  pull_request:
    types: [opened, synchronize, reopened]

# The report step writes the PR comment and the check runs, so the job needs more
# than read access.
permissions:
  contents: read
  pull-requests: write
  checks: write

jobs:
  review:
    runs-on: ubuntu-latest
    # True only when both dedicated-app secrets are set (the id and key are one
    # credential), so a half-configured repo falls back instead of failing the mint
    # step. Computed here because the `if:` below can read `env` but not `secrets`.
    env:
      HAS_BASTION_APP: ${{ secrets.BASTION_APP_ID != '' && secrets.BASTION_APP_PRIVATE_KEY != '' }}
    # Agentic backends run over the PR's code with live credentials, so restrict to
    # same-repo PRs; a maintainer re-runs a fork PR from a trusted branch.
    if: github.event.pull_request.head.repo.full_name == github.repository
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0          # full history; reviewers diff against the base
          # The PR head, not the default merge commit: attestation replay binds
          # to the head tree the author attested, which a merge commit never matches.
          ref: ${{ github.event.pull_request.head.sha }}

      # actions/checkout does not fetch notes by default. Tolerant of the ref
      # being absent: attestation is optional, so most PRs will not carry a note.
      - name: Fetch the attestation notes ref
        run: git fetch origin +refs/notes/bastion:refs/notes/bastion || true

      # 1. Install a published bastion release (not built from the PR).
      # 2. For native reviewers: install your backend CLI (claude, codex, or pi) on
      #    the runner and authenticate it as the PR author. The concrete per-author
      #    auth step is in "Authentication & billing" below; drop it in here. For
      #    reviewers with a `runner`: ensure a container engine is on the runner
      #    (docker by default, or set BASTION_CONTAINER_ENGINE) and that the backend
      #    CLI and its auth live inside the image; the provider credential variables
      #    are forwarded in by name.
      # 3. Stand up anything your reviewers consume (a preview env, a database).

      - name: Review
        env:
          BASTION_DATA_DIR: ${{ github.workspace }}/.bastion
          # Lets the reviewers read the PR's description and discussion as context
          # (read-only, best effort; gathering reads the first 100 conversation comments
          # and first 100 review comments, no pagination). Omit the --repo/--pr flags
          # below to review the diff and local context without PR discussion.
          GITHUB_TOKEN: ${{ github.token }}
        # Non-zero exit on a blocked gate fails the job; that is the merge gate.
        # --repo/--pr feed the reviewers the PR's stated intent and discussion alongside
        # the diff. Cross-run prior-findings memory needs the run store persisted between
        # runs (upload and restore .bastion/runs); a fresh runner starts without it.
        run: |
          bastion review --base "origin/${{ github.base_ref }}" \
            --repo "${{ github.repository }}" \
            --pr "${{ github.event.pull_request.number }}"

      # Optional: mint a token for a dedicated Bastion app so the check runs get
      # their own check suite and render under the app's name. Skipped (and the
      # report falls back to the default GITHUB_TOKEN) when the app is not set up.
      # See "Grouping the checks under their own app" below.
      - id: app-token
        if: ${{ always() && env.HAS_BASTION_APP == 'true' }}
        uses: actions/create-github-app-token@v2
        with:
          app-id: ${{ secrets.BASTION_APP_ID }}
          private-key: ${{ secrets.BASTION_APP_PRIVATE_KEY }}

      - name: Report to the PR
        # Runs even when the review blocked and failed the job, so the comment and
        # checks always land. Creating check runs needs a GitHub App installation
        # token (a classic PAT cannot); both the dedicated-app token and the default
        # GITHUB_TOKEN qualify, so use the dedicated one when present and fall back.
        if: always()
        env:
          GITHUB_TOKEN: ${{ steps.app-token.outputs.token || github.token }}
          BASTION_DATA_DIR: ${{ github.workspace }}/.bastion
        run: |
          set -euo pipefail
          bastion github report \
            --repo "${{ github.repository }}" \
            --pr "${{ github.event.pull_request.number }}" \
            --sha "${{ github.event.pull_request.head.sha }}"
```

### `bastion github report`

The report step reads the run that `bastion review` just persisted (under
`BASTION_DATA_DIR`) and posts it to the pull request. Its full surface:

```
bastion github report --repo <OWNER/NAME> --pr <N> --sha <SHA> [RUN]
```

- `--repo <OWNER/NAME>`: the repository to post to. Defaults to the
  `GITHUB_REPOSITORY` environment variable that Actions sets, so you can usually
  omit it.
- `--pr <N>`: the pull request number (required).
- `--sha <SHA>`: the head commit the check runs attach to (required); pass the
  PR's `head.sha`, not the merge commit.
- `RUN`: an optional positional run id to report; defaults to the latest recorded
  run, which is what you want right after `bastion review`.

It needs a token with `pull-requests: write` and `checks: write` in `GITHUB_TOKEN`,
and reads `GITHUB_API_URL` (Actions sets it; also the hook for GitHub Enterprise).
Creating check runs requires a GitHub App installation token; both the default
Actions `GITHUB_TOKEN` and a dedicated-app token (see below) are installation
tokens and qualify, while a classic personal access token does not. If the run
cannot be found (an earlier failure persisted nothing), it prints a notice and
exits 0 rather than failing the step a second time. The command is CI-facing and
has no local mirror: locally you read findings straight from
`bastion review --format jsonl`.

### Grouping the checks under their own app

In the PR checks list, the name before the `/` is not the workflow that created a
check; it is the **check suite** the check belongs to, and a check suite is keyed by
`(GitHub App, commit)`. Every GitHub Actions workflow runs under the one shared
`github-actions` app, so a commit that triggers several workflows has several
`github-actions` suites. The check runs `bastion github report` creates through the
REST API carry no suite id (the API does not accept one), so GitHub attaches them to
one of those suites of its own choosing, often a sibling workflow's. The result is
check runs that read like `Security / fail-closed-gates` instead of grouping on
their own.

A check run lands in its own named suite only when a **distinct GitHub App**
creates it. So the fix is to post the report under a small app of your own rather
than the shared Actions identity:

1. Create the app. Go to
   [bastion.jessica.black/github-app](https://bastion.jessica.black/github-app) and
   follow the walkthrough; it shows how to create a GitHub App by hand in GitHub's UI
   with exactly the permissions the report step needs (`checks: write`,
   `pull_requests: write`, `contents: read`, no webhook). The app's **name** is what
   the checks group under, for example `YourOrg's Bastion`.
2. Generate the app's private key, note its numeric App ID, and install the app on
   the repositories that run Bastion.
3. Store `BASTION_APP_ID` (the App ID) and `BASTION_APP_PRIVATE_KEY` (the `.pem`
   contents) as Actions secrets. For Dependabot-triggered runs, set them in the
   Dependabot secret store too.

The workflow above mints a token from those secrets with
[`actions/create-github-app-token`](https://github.com/actions/create-github-app-token)
and hands it to the report step; the per-reviewer and aggregate checks then render
under the app's name. The step is fully optional: with the secrets unset it is
skipped and reporting falls back to the default `GITHUB_TOKEN`, which still posts
the comment and checks, only grouped under whichever suite GitHub picks. When that
happens, `bastion github report` notices (it reads back the app that GitHub stamped
on the check runs it just created) and closes the PR comment with a short note
linking here; once a dedicated app is configured the note disappears. Because the
report reads GitHub's response, the workflow does not pass a flag.

For a complete, working example (latest-release install, per-author backend
credentials, and fork-PR safety), see Bastion's own
[`.github/workflows/bastion.yml`](https://github.com/jssblck/bastion/blob/main/.github/workflows/bastion.yml).
It wires up the per-author auth recipe in [Authentication & billing](#authentication--billing)
below, on the Codex backend.

Configure branch protection on your default branch to require this job (and to
require review of the reviewer-policy paths; see [Governance](./governance.md)).
Merging stays GitHub-native: an author enables auto-merge, and once the required
job is green GitHub merges. A push re-triggers the workflow and it resolves again.

## Attesting a run so CI can replay it

Every reviewer is an agent invocation, so a PR that ran clean locally pays for
each reviewer again when CI confirms it. If you would rather CI trust a signed
local run than re-execute every reviewer, opt in with one registry field:

```yaml
attestations: true

reviewers:
  # ...
```

This works only for a review over committed content. To use attestation, commit the
final change, run `bastion review`, then run `bastion attest` (see [The local
workflow](./local-workflow.md#attesting-a-run-for-ci)). A review over a dirty
working tree still runs and still seals, but the seal records that the tree
was dirty, and `bastion attest` refuses to sign it; attest the clean,
committed run instead. Once an author pushes the resulting note, CI can replay
the covered reviewers instead of re-running them. `bastion review` in CI
verifies the note's signature against the PR author's GitHub-registered SSH
signing keys, checks that the attested run reviewed the exact same content CI
is looking at (the same trees, the same diff, the same effective reviewer
config), and only then replays. A replayed block still blocks the merge,
exactly as a fresh one would; attestation skips duplicate execution, not the
gate.

Two workflow additions are required for this to work, both already present in
Bastion's own self-hosted example below: checking out the PR's head commit
(`ref: ${{ github.event.pull_request.head.sha }}`), since attestation binds to
that exact tree and the default merge-commit checkout will never match it, and
fetching the notes ref (`git fetch origin +refs/notes/bastion:refs/notes/bastion
|| true`), since `actions/checkout` does not fetch notes by default.

When attestation replaces execution, the sticky comment opens with a callout
naming which reviewers replayed, the key that attested, and when; each
replayed reviewer's check-run summary says so too. When it does not (a missing
note, an unregistered key, a stale base, or any other mismatch), CI simply
executes every reviewer as usual and the comment includes the fallback reason.

A reviewer can opt out of ever being replayed with `attestation: never` on that
reviewer, for a gate your team wants CI to execute unconditionally regardless
of what was attested locally.

Whether the SSH key an author attests with is a plain file or a presence-gated
one (a hardware token or an OS keychain entry that prompts per signature) is
worth deciding deliberately. A coding agent running on the author's machine can
use a plain file key without their involvement, so enrolling one means
accepting that an agent on that machine could sign an attestation on its own,
the same trust already extended to that machine through commit access. Bastion
cannot tell the two kinds of key apart from the signature alone, so this is a
call for the author (or your team's policy) to make, not something the tool
enforces. See the [attestation design](https://github.com/jssblck/bastion/blob/main/docs/developer-guide/attestation.md#trust-posture)
for the full reasoning.

## Authentication & billing

Coding-agent subscriptions tie usage to an individual, not a team, so Bastion bills
a PR's reviews to the *PR author*. Reviewing Alice's PR is billed to Alice's
subscription, which is the ToS-compliant reading: each contributor's plan powers the
review of their own changes. Bastion never stores credentials. The team stores each
author's credential as an Actions secret, and the workflow maps the PR author's
GitHub login to the matching secret at run time.

Bastion just runs your backend CLI, and the backend reads whatever auth it finds on
the runner. Your job in CI is to place the right author's credential where that CLI
looks before `bastion review` runs. The pattern is the same for every backend:

1. **Capture the credential once, locally.** Each contributor signs in to the
   backend on their own machine. The CLI writes a credential file:

   | Backend       | Sign-in            | Credential file the CLI reads                  |
   | ------------- | ------------------ | ---------------------------------------------- |
   | `codex`       | `codex login`      | `~/.codex/auth.json` (relocatable: `CODEX_HOME`) |
   | `pi`          | `pi` auth flow     | `~/.pi/agent/auth.json`                         |
   | `claude-code` | `claude` sign-in   | `~/.claude` (OAuth token)                       |

   For a ChatGPT or Claude **subscription**, this file holds an OAuth credential (an
   access token plus a refresh token); the CLI refreshes the short-lived access
   token from the stored refresh token on each run, so the secret does not need
   rotating every time the access token expires. A Codex `auth.json` from a ChatGPT
   sign-in carries `"auth_mode": "chatgpt"`, and the native `backend: codex` reads it
   directly: you do **not** need Pi to spend a ChatGPT subscription (see
   [Spending a subscription in CI](#spending-a-subscription-in-ci) below).

2. **Store it as a per-author secret.** Copy the file's contents into a repository
   secret named `<BACKEND>_AUTH_<LOGIN>`: the backend, then the GitHub login
   uppercased. For the `codex` backend and the login `jssblck`, that is
   `CODEX_AUTH_JSSBLCK`; for `pi`, `PI_AUTH_JSSBLCK`. The name is a convention you
   pick and reference in the workflow, not something Bastion parses.

3. **Map the login to the secret in the workflow.** Resolve
   `github.event.pull_request.user.login` to the matching secret through a `case`
   arm, then write it back to the path the CLI reads:

   ```yaml
   - name: Authenticate Codex as the PR author
     env:
       AUTHOR: ${{ github.event.pull_request.user.login }}
       CODEX_AUTH_JSSBLCK: ${{ secrets.CODEX_AUTH_JSSBLCK }}
     run: |
       set -euo pipefail
       author="$(printf '%s' "$AUTHOR" | tr '[:upper:]' '[:lower:]')"
       case "$author" in
         jssblck) cred="$CODEX_AUTH_JSSBLCK" ;;
         *)
           echo "::error::No Codex credential mapped for PR author '$AUTHOR'. Add a CODEX_AUTH_<LOGIN> secret and a case arm." >&2
           exit 1 ;;
       esac
       if [ -z "$cred" ]; then
         echo "::error::Codex credential for '$AUTHOR' is mapped but its secret is empty." >&2
         exit 1
       fi
       mkdir -p "$HOME/.codex"
       printf '%s' "$cred" > "$HOME/.codex/auth.json"
       chmod 600 "$HOME/.codex/auth.json"
   ```

   Onboarding a contributor is then two reviewed lines: their secret and a `case`
   arm. Because the mapping lives in the workflow, which is a CODEOWNERS-protected
   path (see [Governance](./governance.md)), changing who may spend a subscription is
   itself a human-reviewed change.

An author with no mapped secret **fails closed**: the step errors and the gate
blocks, rather than silently billing someone else's subscription. If you would
rather a new contributor never be blocked, point the `*)` arm at a shared metered
**API key** instead of erroring: store the provider's API key as a secret and export
it (for example `CODEX_API_KEY` / `ANTHROPIC_API_KEY`) into the review step rather
than writing an `auth.json`. The same login-to-secret shape applies. Under heavy
volume a subscription's rate limits can throttle reviewers, and because gates fail
closed a throttled reviewer reads as a blocked merge, so some teams use API billing
in CI and keep subscriptions for the local loop.

### Spending a subscription in CI

A ChatGPT or Claude subscription works in CI the same way it does locally: the
backend CLI reads its OAuth `auth.json` and refreshes the token itself. Use the
backend that matches the subscription you have:

- **`backend: codex` with a ChatGPT subscription.** Sign in with `codex login`
  (ChatGPT), store `~/.codex/auth.json` as `CODEX_AUTH_<LOGIN>`, and rehydrate it to
  `$HOME/.codex/auth.json` as shown above. This is the direct path; no Pi involved.
- **`backend: claude-code` with a Claude subscription.** Same shape against the
  `claude` CLI's auth.
- **`backend: pi` with the `openai-codex` provider.** Pi can also spend a ChatGPT
  subscription, through its `openai-codex` provider (`model: openai-codex/gpt-5.5`).
  Reach for this only when you specifically want Pi's multi-provider routing; for
  plain Codex-on-ChatGPT, the native `codex` backend is simpler.

> **The two `auth.json` files are different.** `~/.codex/auth.json` (Codex CLI) and
> `~/.pi/agent/auth.json` (Pi CLI) are distinct file formats backed by the same
> ChatGPT account. The secret you store must match the backend you pin: a Codex
> `auth.json` rehydrated where Pi looks (or the reverse) will not authenticate. Pick
> the backend first, then capture that CLI's file.

### Dependabot and bot authors

Dependabot opens **same-repo** PRs, so they clear the fork guard and Bastion reviews
them like any other PR. With the `permissions:` block the example workflow declares,
the default `GITHUB_TOKEN` posts the `bastion` check on a Dependabot PR, so you can
require it for those PRs too. There is no read-only-token deadlock to work around.
Dependabot has one required difference for everyone and one extra step that applies
only to per-author billing:

- **Secrets come from a separate store (applies to everyone).** GitHub serves
  secrets to Dependabot-triggered runs from a *Dependabot* secret store, not the
  Actions store. Whatever credential your review step reads, an `ANTHROPIC_API_KEY`
  or a per-author `<BACKEND>_AUTH_<LOGIN>`, must be set in that store as well
  (`gh secret set <NAME> --app dependabot`), or it arrives empty on a Dependabot PR
  and the gate fails closed.
- **A bot has no subscription of its own (per-author billing only).** If you map
  per-author credentials, the bot author needs a `case` arm pointing at a maintainer
  who sponsors its reviews, and the bracketed login must be quoted, since `[bot]` is
  a glob character class in a shell `case` pattern:
  `'dependabot[bot]') cred="$CODEX_AUTH_JSSBLCK" ;;`. An arm that maps to an empty
  secret fails closed with a "mapped but empty" error, usually the sign the
  Dependabot-store copy is missing. Billing with a shared API key instead of
  per-author secrets avoids this entirely: there is no per-author arm to maintain.

### Fork-PR safety

GitHub does not expose secrets to workflows triggered by **fork** pull requests, and
an agentic backend should never run over untrusted code with a live credential
anyway. The example workflow guards on
`github.event.pull_request.head.repo.full_name == github.repository`, so it runs for
same-repo PRs only. A fork contribution is reviewed by a maintainer re-running it
from a trusted branch in the repo.

## Environments & inputs

Bastion consumes environments; it does not provision them. A reviewer that needs a
preview URL, a database, or any running dependency expects the workflow to have
stood it up and exposed it. Typically an earlier job deploys a preview environment
for the PR and passes its URL into the Bastion job as an environment variable. How
that variable reaches the agent depends on where the reviewer runs. A **native**
reviewer inherits the job environment, so the agent can see it directly. A
**containerized** reviewer (one with a
[`runner`](./authoring-reviewers.md#runner-and-capabilities) and
`capabilities.network: true`) runs in a container and does *not* inherit the arbitrary
job environment. Only the reviewer's literal `env`
pairs cross that boundary (plus a fixed provider-credential set, except that a
credential name set in the reviewer's own `env` wins and is not also forwarded from the
job environment), so a per-PR value reaches a containerized reviewer only if you write
its value into the registry,
typically by templating `.bastion.yaml` before the Bastion job runs. A reviewer's
`env` and `inputs` values are literal (Bastion does not shell-expand them), so to put
a dynamic value into the prompt itself you template the registry or have the prompt
read the variable. Standing up the environment is a deploy concern; Bastion's job
starts once it exists. (See
[Authoring reviewers](./authoring-reviewers.md#env) for the reviewer side.)

## Self-hosting note

Bastion dogfoods the adapter through
[`.github/workflows/bastion.yml`](https://github.com/jssblck/bastion/blob/main/.github/workflows/bastion.yml),
running the latest published `bastion` release rather than a binary built from the
PR's own sources, so a change can never edit the engine that judges it. That workflow
is a concrete, self-hosted instance of everything this chapter describes.

---

Next: [Governance](./governance.md). Keeping humans at the policy layer with
CODEOWNERS and branch protection, and the escape-to-improvement loop.
