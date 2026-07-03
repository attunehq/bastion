# Bastion on GitHub

> The GitHub adapter: how Bastion runs in Actions, reports to PRs, and gates merges.

The core design ([`design.md`](./design.md)) is deliberately CI-agnostic; it describes reviewers, verdicts, and the merge gate without saying how any of it touches a real forge. This doc is the GitHub adapter: the concrete answer to "where does the workflow live, how does a verdict become a check, and how is the policy layer enforced" when the forge is GitHub. Everything here is one implementation of the plugin-style CI interface the core design refers to; another forge would get its own doc and reuse the same core.

> **What the adapter does.** The self-hosted workflow in
> [`.github/workflows/bastion.yml`](../../.github/workflows/bastion.yml) runs
> `bastion review`, gates on the job's exit code, and then runs `bastion github
> report` to post the results: a sticky PR comment carrying every reviewer's verdict
> and findings (optional ones included), one check run per reviewer, and the
> always-present aggregate `bastion` check. The full run is uploaded as an artifact
> too. Bastion's GitHub helpers are `bastion github codeowners` and `bastion github
> report`. Because `bastion github report` runs after `bastion review` finishes, each
> check run is posted already completed with its conclusion.

The guiding rule is the same as the core: Bastion does not own CI, it plugs into yours. The workflow, the secrets, the preview environments, and the branch protection rules are GitHub's; Bastion reads and writes them through a thin adapter and otherwise stays out of the way.

---

## How it runs

Bastion runs as a GitHub Actions workflow triggered on pull request events: `opened`, `synchronize` (a new push to the PR), and `reopened`. On each event the adapter:

1. Computes the changed file set for the PR.
2. Routes: selects the reviewers whose `trigger` globs match the changed files.
3. Gathers the PR's [review context](./design.md#review-context): `bastion review --repo OWNER/NAME --pr N` reads the PR description and discussion over the REST seam ([`src/github/context.rs`](../../src/github/context.rs)) and hands the reviewers that context alongside their prior findings. Best effort: if the API call fails, the review proceeds on the local context (commit messages and prior findings) alone.
4. Runs each selected reviewer, in parallel, with per-reviewer timeouts (see the core design's _Aggregation & the merge gate_). A reviewer covered by a verified attestation is reconstructed from the attested bundle without dispatching its backend; every other selected reviewer executes through its backend fresh. See [Verification and replay](#verification-and-replay).
5. Reports each verdict back to the PR.

Native reviewers run directly on the Actions runner. A reviewer that declares a container `runner` and `capabilities.network: true` runs its backend inside that container on the Actions runner (the engine is already present on GitHub-hosted runners); see [Containers](./containers.md). None of routing or aggregation is GitHub-specific; only the steps that read the PR and write results go through the adapter.

The adapter is the GitHub *producer* of the review context. It maps GitHub's fields onto the transport-neutral `ReviewContext` and leaves the rest out of the core. A non-empty PR body becomes the author's stated intent; an empty body supplies none, so the local commit-message intent stands. Each non-Bastion comment becomes an untrusted claim carrying the commenter's `Standing` (mapped from `author_association`, so a reviewer can weight a maintainer above an outsider without ever obeying either), and Bastion's own past comments are filtered out by their hidden marker so a reviewer never reads itself. The core never sees an `author_association` or a comment id.

Two parts of the context need state that a single CI run does not have on its own:

- **Prior-findings memory** is recalled from the local run store (`store::prior_findings`), and a fresh Actions runner starts with an empty store. So for a reviewer to recall what it raised on the last push, the workflow must persist the run store between runs and restore the previous run before `bastion review`. The self-hosted example below does this by uploading the run as an artifact and downloading the prior one; without that step, the GitHub surface still gets the PR's intent and discussion (gathered fresh each run), just not cross-run finding memory.
- **Reply routing** (a reply attached to the specific finding it answers) is wired through `FindingId`: a review-comment reply whose thread root carries a Bastion finding marker resolves back to that finding. The reporter posts one sticky comment and check runs, so PR comments reach reviewers as general discussion (visible to every reviewer) rather than routed to a single finding.

---

## Reporting verdicts

A verdict (the core schema: `verdict`, `summary`, `findings`) maps onto two GitHub surfaces.

- **Findings are posted to the PR.** Every finding (blocking and optional) is rendered into a single *sticky* PR comment, and each located finding is also attached to its reviewer's check run as an annotation on its `path` and line range. `kind: blocking` and `kind: optional` are rendered differently so a reader can tell at a glance which findings hold up the merge and which are suggestions; this mirrors how a human reviewer marks some comments blocking and some optional.
- **The verdict becomes a check run.** Each reviewer reports a check run named after itself (`bastion / file-responsibility`), so the PR's checks list shows exactly which reviewers ran and how each landed. A gate that blocks reports a `failure` conclusion; a gate that passes reports `success`; an advisor reports `success` with its findings attached, because advisors comment but never gate.

The summary and the full finding list also go into each check run's output, so everything is visible from the Checks tab even before you scroll the diff.

The sticky comment is the surface the implementing agent is meant to read. A reviewer's actionable feedback is its findings, and an agent fixing the PR gets everything it needs to act from the comment alone, without opening a single check. The check runs carry status and the gate, for humans watching and for the merge logic. An agent should never have to open a check just to learn what to change; the comment already says it.

`bastion github report` reads the run that `bastion review` persisted and renders the recorded outcome: the aggregate `bastion` check carries the recorded `run.completed` verdict (a recorded pass goes green, a recorded block fails, and a run that never completed reads as an incomplete failure). It trusts that recorded run because the runner already decided it: the runner fails a gate closed at write time and clamps advisors to a pass, so the report does not re-derive the merge gate. The one boundary it still checks is gate-verdict consistency. A gate row recorded as a pass that nonetheless carries a blocking finding contradicts itself, so the report fails it closed rather than publishing a green check; the backends already reject such a verdict upstream, so this is a boundary safeguard, not a recomputation of the gate.

The comment also folds in a **skills-freshness advisory** when the checked-out repo's bundled agent skills (`.claude/skills` and `.agents/skills`) are missing or have drifted from the reporting binary's embedded copy. It renders as a GitHub `> [!WARNING]` callout just under the headline, naming each affected file and pointing at `bastion skills install` (or `--force` when a file has drifted). The report computes it by running the same check `bastion skills check` does against the working tree, so it reflects what an agent would actually load. The advisory never touches a check-run conclusion, so a stale skill nudges the maintainer to refresh without failing the gate (advisories fail open). The local surface mirrors it, printing the same notice to stderr so the driving agent sees it, but only when the repository has adopted Bastion (a repo-level reviewer registry is present). `github_report` calls `stale_skills_warning` unconditionally, while the local `warn_on_stale_skills` routes through `local_skills_warning`, which stays silent when no repo registry is found, so a local review running on the author's user-level reviewers alone does not nudge about skills. CI always has a repo registry, so the report path is unaffected.

## Verification and replay

A repository can opt in (`attestations: true` in its registry) to let CI reuse a signed local run instead of re-executing every reviewer; see [Attestation](./attestation.md) for the full design. On the GitHub adapter this happens inside `bastion review` itself, before the runner fans reviewers out, so it is invisible as a separate step, only as which reviewers end up replayed versus executed.

**Fetching the note.** `git notes` are not part of an ordinary checkout, so `bastion review` looks for the note under `refs/notes/bastion` on HEAD first, then falls back to the PR's head SHA (from the gathered GitHub context) when HEAD carries none; CI's checkout can be a merge commit, and the note the author actually attested hangs off their own head commit. Both lookups need the notes ref fetched locally first (see [the two workflow requirements](#the-two-workflow-requirements) below).

**Signature verification.** Given a note, the adapter resolves the PR author's login and fetches their registered SSH signing keys (`GET /users/{username}/ssh_signing_keys`, over the same REST seam `client.rs` already provides) and runs `ssh-keygen -Y verify` against them in the `bastion` namespace. A signature by any other key, including one freshly minted on the author's machine, fails verification.

**Seal verification and binding checks.** The adapter verifies the run seal with its own embedded secret (the same secret every binary of that release shares), then re-derives its own bindings from the checkout (HEAD's tree, the merge base's tree, the diff's patch-id, and the effective config hash) and compares them against what the bundle recorded. Any mismatch, or a seal that shows a test seam was active during the local run, falls back to a full run rather than partially trusting the bundle.

**Per-reviewer replay or execute.** Verification and binding checks pass or fail for the bundle as a whole, but replay is decided per routed reviewer: one the bundle covers and that has not opted out (`attestation: never`) replays; a reviewer CI routed that the bundle does not cover, or one that opted out, executes fresh. A replayed verdict carries the same fail-closed policy a fresh one would, so a replayed block still blocks the gate.

**Reporting.** The merged result (replayed and freshly executed reviewers together) flows through the normal report path, with two additions. The sticky comment opens with a `[!NOTE]` callout naming which reviewers replayed, the attesting key, and when it was signed, right alongside the skills-drift `[!WARNING]` block. Each replayed reviewer's check-run summary adds a line stating its verdict was replayed rather than executed. When attestation was attempted but not honored, the comment instead carries a line naming why (a missing note, an unregistered key, a seal mismatch, a stale binding), taken from the run's `run.attestation-fallback` event.

### The two workflow requirements

Honoring an attestation needs two small additions to an ordinary `bastion` workflow, both already present in this repository's own [`bastion.yml`](../../.github/workflows/bastion.yml):

- **Check out the PR head, not the merge commit.** `actions/checkout`'s default `pull_request` behavior checks out a synthetic merge commit, whose tree never matches the head tree an author attested. Set `ref: ${{ github.event.pull_request.head.sha }}` so CI's HEAD is the commit the note is actually attached to.
- **Fetch the notes ref.** `actions/checkout` does not fetch notes by default, so add `git fetch origin +refs/notes/bastion:refs/notes/bastion`, tolerant of the ref being absent (most PRs will not carry a note; that is the ordinary case, not an error).

A repository that skips either step, or has not set `attestations: true`, never replays: every reviewer executes fresh.

### The aggregate check

There's a wrinkle GitHub forces on us. Branch protection requires you to name the checks that must pass, but Bastion's set of reviewers varies per PR; a docs-only PR and a server PR trigger different reviewers, so there is no fixed list of check names to require.

The fix is a single always-present check, `bastion`, and it is the only one branch protection requires. It always runs, even when zero reviewers match (a trivial pass in that case), so it is a stable required check. Internally it reflects the aggregate: `success` only when every triggered gate passed, and `failure` if any gate blocked, errored, or timed out (fail-closed, per the core design). The per-reviewer check runs stay informational; `bastion` is the gate.

The aggregate check summary and the sticky comment share one headline (the status line): the decision and gate tally, then the run's wall-clock duration and the usage totals summed across reviewers (input and output tokens, cache-read tokens when nonzero, and cost). The token and cache figures are omitted when no backend reported usage, including a mock run or a zero-reviewer run. These are the run-level totals; the per-reviewer breakdown lives on each reviewer's own check (see [Reviewer detail](#reviewer-detail)).

### Reviewer detail

Each reviewer's check run is also where its detail lives; a reader clicks "Details" on that reviewer in the checks list and lands on a page Bastion owns the markdown for. This is for humans and for the occasional surprising decision, not part of the implementing agent's normal loop; the agent already has the feedback in the sticky comment. Two things are presented there.

- **Metadata and decision.** A short header: the reviewer name, its mode (`gate` or `advisor`), the backend it ran on, and how long it took; then the verdict and summary. The check run _title_ carries the one-line decision ("Blocked: `src/foo.ts` concentrates three responsibilities") so it is legible without opening anything.
- **Tokens and cost, when available.** When the backend reports usage, a token line lists the input and output token counts, the cache-read tokens (prompt-cache hits, shown only when nonzero), and the session cost; when the backend reports no usage, the line is omitted rather than shown empty. Usage is per reviewer, so an expensive e2e reviewer and a cheap hermetic one show separate totals.

The full agent session is not embedded in the check output; the run, transcripts included, is uploaded as the workflow artifact, and the sticky comment footer points there. The aggregate `bastion` check renders a plain Markdown table of the triggered reviewers with columns `Reviewer`, `Mode`, `Verdict`, and `Summary`.

A sketch of a reviewer's check output:

```markdown
> - Mode: gate
> - Agent: claude-code
> - Verdict: block
> - Duration: 38s
> - Tokens: 18204 in, 1560 out, 12000 cached ($0.21)

A new query path reads rows without scoping by tenant id.
```

---

## The merge gate

Merge is GitHub-native. Repository admins should configure branch protection on the default branch to require the `bastion` check and to require review of the reviewer policy (next section).

An author, human or agent, enables GitHub auto-merge on the PR. Once `bastion` goes green and any required policy review is satisfied, GitHub merges; nothing in Bastion presses the button. This is deliberate: the merge mechanics, the queue, the "all required checks pass" logic are GitHub's, and Bastion just supplies one of the required checks.

A push to the PR re-triggers the workflow; the `bastion` check returns to `pending` and resolves again. An agent looping toward green sees the same check transition locally through the CLI and in CI. Cancellation of the old job is also managed by GitHub if configured.

---

## Governance

The core design puts humans at the policy layer; on GitHub that is enforced with two native mechanisms (see the core design's _Threat model & trust boundary_).

**CODEOWNERS.** The Bastion CLI supports generating a CODEOWNERS block covering the reviewer config: the `bastion` review job in GitHub, reviewer definitions, the registry, and the CODEOWNERS file itself. Any PR that adds, removes, or edits a reviewer; loosens a trigger; or changes a prompt touches an owned path, so GitHub requires a human review before merge. Repository maintainers can also obviously provide their own CODEOWNERS instead of using the generated suggestion. The main reason we can't have Bastion automatically manage this is because CODEOWNERS changes only take effect after a PR is merged; as such the CODEOWNERS needs to be written in such a way that it statically protects every path Bastion writes into.

**Branch protection requires the check.** Requiring `bastion` means a PR can't merge with the gate switched off, and because the workflow file and the Bastion config are themselves owned paths, switching it off is itself a policy change that a human sees.

That is the whole enforcement story, and it is intentionally modest. The contributor we are designing for is an aligned agent that would never quietly disable CI; the CODEOWNERS trip wire and the required check exist so that if policy does change a human is in the loop, not so that a determined adversary is stopped. Anything stronger, like signing, external rule storage, or an enumerated trusted-computing-base, is out of scope for the same reason it is in the core design.

---

## Authentication & billing

Backends bill per individual, and coding agent subscriptions tie usage to one person rather than a team. The core design leaves the choice to the user; on GitHub it lands like this.

The PR author is the requester. Bastion runs the reviewers for a PR under credentials mapped to its author, so reviewing Alice's PR is billed to Alice's subscription; that is the ToS-compliant reading, where each contributor's plan powers the review of their own changes. The adapter resolves the author's GitHub login to a secret name and reads that secret from GitHub Actions secrets at run time.

Bastion does not store any credentials; the team stores them as Actions secrets and tells Bastion the mapping. If no subscription is configured for an author, Bastion can fall back to a shared metered API key, so a new contributor is never blocked from review; whether to allow that fallback is the team's call.

This author-mapped flow works by placing the credential in the runner environment that the backend CLI reads (the subscription `auth.json` flow below writes `~/.codex/auth.json` on the runner). A native reviewer reads that host config directly. A containerized reviewer (one with a `runner` and `capabilities.network: true`) does not see the runner's home directory: it receives only the reviewer's literal `env` plus a fixed set of provider-credential variable *names* forwarded into the container, so it authenticates from an env-based provider credential (for example `CODEX_API_KEY` / `ANTHROPIC_API_KEY`) or from auth baked into the image, not from a host `auth.json`. See [Containers](./containers.md).

One operational note carried over from the core design: under heavy volume a subscription's rate limits can throttle reviewers, and because gates fail closed a throttled reviewer reads as a blocked merge. Bastion can optionally support API key fallback for this sort of situation as well, or teams may decide to simply use API billing and keep subscriptions for the local loop. That is a tradeoff to make per org and repo.

### Self-hosted example: Bastion reviewing Bastion

This repository dogfoods the adapter through [`.github/workflows/bastion.yml`](../../.github/workflows/bastion.yml). The job runs a published `bastion` release rather than a binary built from the PR's own sources, so a change can never edit the engine that judges it. It downloads the *latest* published release (resolved with `gh release view`, which excludes prereleases), so engine improvements land without a per-PR pin bump while the engine remains a maintainer-published release rather than the PR's sources. Reviewer policy in [`.bastion.yaml`](../../.bastion.yaml) is still read from the checkout, and both that file and the workflow are CODEOWNERS-protected paths. Every reviewer in `.bastion.yaml` pins `backend: codex`, so each review runs on the Codex CLI billed to the PR author's own ChatGPT subscription. The workflow wires that up by mapping the author's GitHub login to a per-author credential:

1. **Capture the credential once, locally.** Each contributor authenticates Codex against their billed ChatGPT subscription on their own machine with `codex login`. Codex writes an OAuth credential (an access token plus a refresh token) to `~/.codex/auth.json`.
2. **Store it as a per-author secret.** Copy the contents of that `auth.json` into a repository secret named `CODEX_AUTH_<LOGIN>`: the login uppercased, e.g. `CODEX_AUTH_JSSBLCK` for `jssblck`. Bastion never stores credentials; the secret lives in GitHub Actions.
3. **Map the login to the secret.** The `Authenticate Codex as the PR author` step resolves `github.event.pull_request.user.login` to the matching secret through a `case` arm, so reviewing `jssblck`'s PR bills `jssblck`'s subscription. Onboarding a contributor is two reviewed lines: their secret and a `case` arm. Because the mapping lives in the workflow, which is a CODEOWNERS-protected path, changing who may spend a subscription is itself a human-reviewed change.
4. **The job rehydrates it at run time.** Before running `bastion review`, the step writes the resolved credential back to `$HOME/.codex/auth.json`; Codex refreshes the short-lived access token from the stored refresh token on each run, so the secret does not need rotating every time the access token expires. Every reviewer pins `model: gpt-5.5` and `effort: high`, which the Codex backend forwards as `-m` and `model_reasoning_effort`, so the model and effort are selected per review.

An author with no mapped secret fails closed: the step errors and the gate blocks, rather than silently billing someone else's subscription. Two further boundaries keep this safe: GitHub does not expose secrets to workflows triggered by fork pull requests, and the job additionally guards on `head.repo.full_name == github.repository`, so an agentic backend never runs over untrusted code with a live credential; a fork contribution is reviewed by a maintainer re-running it from a trusted branch. The job's pass/fail is the gate (a blocked review exits non-zero); a following `bastion github report` step then posts the sticky comment and the per-reviewer and aggregate check runs, and the full run is also uploaded as an artifact. That report step runs with `if: always()` so the PR is updated even when the review blocked and failed the job; it needs `pull-requests: write` and `checks: write`, and a GitHub App installation token to create check runs (a classic personal access token cannot). This repo configures a dedicated Bastion app for that token so its checks group under their own name rather than a sibling workflow's; see [Check-run grouping and the dedicated app](#check-run-grouping-and-the-dedicated-app). When no such app is configured the step falls back to the default `GITHUB_TOKEN`, which is itself an installation token and still posts.

Dependabot PRs review like any other. They are same-repo (so they clear the `head.repo` guard), and because the job declares an explicit `permissions:` block, the default `GITHUB_TOKEN` on a Dependabot-triggered run carries the `pull-requests: write` / `checks: write` it grants, so `bastion github report` posts the sticky comment and the per-reviewer and aggregate check runs normally. That removes the usual read-only-token obstacle, so the `bastion` check can be required for Dependabot PRs the same as for any other. (As a worked example outside this repo, `Fieldguide/minionforge` runs Bastion on its Dependabot PRs on exactly this shape, an API-key-billed `claude-code` review on a plain `pull_request` trigger, and requires the resulting check.)

Two Dependabot specifics still apply. The first is universal; the second is only for per-author subscription billing.

- **Secrets come from a separate store.** GitHub serves secrets to Dependabot-triggered runs from a *Dependabot* secret store, not the Actions store. Whatever credential the review step reads, an `ANTHROPIC_API_KEY` or a per-author `CODEX_AUTH_<LOGIN>`, must be set there too (`gh secret set <NAME> --app dependabot`), or it arrives empty on a Dependabot PR and the gate fails closed. Every billing model needs this Dependabot-store copy.
- **A bot has no subscription of its own.** Under per-author billing the bot author has no credential, so its `case` arm points at a maintainer who opted in to sponsor its reviews, billing the bot's PRs to that person. The bot login is literally `dependabot[bot]`, and the `[bot]` brackets are a glob character class in a shell `case` pattern, so the arm must quote them (`'dependabot[bot]')`) to match literally. An arm that resolves to an empty secret fails closed with a "mapped but empty" error, usually the sign the Dependabot-store copy above is missing. API-key billing has no per-author arm, so this case does not arise.

For API-key billing instead of a subscription, store the provider's API key as a secret and export it into the review step rather than writing `auth.json`; the same mapping shape applies.

---

## Environments & inputs

Bastion consumes environments, it does not provision them. A reviewer that needs a preview URL, a database, or any other running dependency expects the workflow to have stood it up and exposed it; the reviewer just reads it.

On GitHub that means the workflow owns whatever a reviewer's `env` and `inputs` reference. A typical setup deploys a preview environment for the PR in an earlier job, or a separate workflow, and passes its URL into the Bastion job as an environment variable. How it reaches the agent depends on where the reviewer runs. A native reviewer inherits the job environment, so a variable exported into the Bastion job is visible to the agent, and `env`/`inputs` add literal values on top. A containerized reviewer (one with a `runner` and `capabilities.network: true`) inherits none of the arbitrary job environment; only its literal `env` pairs and the fixed provider-credential set cross into the container, so a per-PR value must be written into the reviewer's `env`, usually by templating the registry before the job runs. A secret a reviewer needs comes from Actions secrets the same way author credentials do.

Standing up a preview environment is a deploy concern, and the deploy system already knows how. Bastion's job starts once the environment exists.

---

## Example workflow

A minimal workflow wiring Bastion into PR review:

```yaml
name: bastion
on:
  pull_request:
    types: [opened, synchronize, reopened]

# The report step writes the PR comment and the check runs, so the job needs more
# than read access. The aggregate `bastion` check it reports is what branch
# protection requires.
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
          fetch-depth: 0          # full history; reviewers compare base vs head

      # Install a published bastion release and authenticate your backend CLI,
      # billed to the PR author (see Authentication & billing above). Then stand up
      # whatever your reviewers consume; Bastion does not do this.
      - id: preview
        run: ./scripts/deploy-preview.sh   # exports the preview URL

      - name: Review
        env:
          BASTION_DATA_DIR: ${{ github.workspace }}/.bastion
          PREVIEW_URL: ${{ steps.preview.outputs.url }}
          # Lets the review gather the PR's description and discussion as context
          # (read-only, best effort). Needs `pull-requests: read` or higher.
          GITHUB_TOKEN: ${{ github.token }}
        # Non-zero exit on a blocked gate fails the job; that is the merge gate.
        # `--repo`/`--pr` feed the reviewers the PR's stated intent and discussion
        # alongside their prior findings; omit them for a context-free review.
        run: |
          bastion review --base "origin/${{ github.base_ref }}" \
            --repo "${{ github.repository }}" \
            --pr "${{ github.event.pull_request.number }}"

      # Optional: mint a token for a dedicated Bastion app so the report's check
      # runs get their own named check suite (see "Check-run grouping" below).
      # Skipped when the app is not configured; reporting then falls back to the
      # default GITHUB_TOKEN.
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

For brevity, this example omits cross-run prior-findings memory. The reviewers still get the PR's intent and discussion, gathered fresh each run. For a reviewer to recall the findings it raised on the previous push, the run store has to survive between runs. Upload the run as an artifact after the report, and restore the previous one before `bastion review`. Bastion's own [self-review workflow](../../.github/workflows/bastion.yml) shows the pattern (an `actions/upload-artifact` of `.bastion/runs` plus a `gh run download` of the most recent prior run for the branch).

Branch protection on the default branch requires the `bastion` check and review of the owned reviewer-config paths; everything else is standard GitHub.

## Check-run grouping and the dedicated app

In the PR checks list, the label before the `/` is not the workflow that created a check run; it is the **check suite** the run belongs to. A check suite is keyed by `(GitHub App, commit)`, not by workflow. Every GitHub Actions workflow runs under the single shared `github-actions` app, so one commit that triggers several workflows has several `github-actions` check suites (one per workflow). The check runs `bastion github report` posts through the REST API (`POST /repos/{owner}/{repo}/check-runs`) carry no suite id, because the Checks API has no parameter to create or choose one: GitHub assigns the run to a suite for that `(app, commit)` pair on its own. With the shared Actions identity that resolves to one of the commit's other suites (empirically the earliest-created), so the bastion-posted runs render under a sibling workflow's name (for example `Security / fail-closed-gates`) rather than grouping together.

There is no payload or naming trick that fixes this while staying on the default `GITHUB_TOKEN`: the collision is inherent to multiple workflows sharing one app identity. A check run gets its own named suite only when a **distinct GitHub App** creates it. So the durable fix is to post the report under a small per-adopter app instead of the shared Actions identity.

This stays inside Bastion's "owns no infrastructure, custodies no credentials" rule: each adopting org creates and holds its own app, exactly as it already holds its own backend-credential secrets. It is deliberately not one shared public Bastion app: acting as a shared app would require a central service holding the app's private key (a key able to write to every adopter's repo) to mint tokens, which is precisely the always-on, credential-custodying infrastructure the adapter avoids.

Setup is a one-time, per-org step:

1. **Create the app.** The hosted walkthrough at [bastion.jessica.black/github-app](https://bastion.jessica.black/github-app) (source: [`site/src/pages/github-app.astro`](../../site/src/pages/github-app.astro)) walks you through creating a GitHub App by hand: open GitHub's new-app form for the personal account or org, set exactly the permissions the report step needs (`checks: write`, `pull_requests: write`, `contents: read`) with no webhook, and create it. The app's name is what the checks group under, for example `YourOrg's Bastion`. The walkthrough does not use GitHub's [app-manifest flow](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest): completing that flow requires a backend to exchange the temporary code for the app's credentials, and Bastion custodies no credentials and serves no such backend.
2. **Capture its credentials.** Generate the app's private key (a downloaded `.pem`), note the numeric App ID, and install the app on the repositories that run Bastion.
3. **Store the secrets.** Set `BASTION_APP_ID` (the App ID) and `BASTION_APP_PRIVATE_KEY` (the `.pem` contents) as Actions secrets, at the repo or org level. Mirror them into the Dependabot secret store as well if Dependabot PRs are reviewed, for the same reason the `CODEX_AUTH_<LOGIN>` secrets are mirrored there.

The workflow mints an installation token from those secrets with [`actions/create-github-app-token`](https://github.com/actions/create-github-app-token) and hands it to the report step; the per-reviewer and aggregate checks then render under the app's name. The mint step guards on both secrets being present (the two are one credential, so a half-configured repo with only one set falls back rather than failing the mint), so it is fully optional: with the secrets unset the step is skipped and the report step falls back to the default `GITHUB_TOKEN`, still posting the comment and checks (just grouped under whichever suite GitHub picks). The minted token also authors the sticky comment, so the comment and the checks present under one identity.

`bastion github report` detects this situation on its own, with no help from the workflow (the workflow is the adopter's, and they write their own). GitHub stamps every created check run with the `app` that posted it, so the report reads that `app.slug` back from the check-run response: when it is `github-actions` (the shared identity, no dedicated app), the sticky comment closes with a one-line note linking to the setup walkthrough; when it is a distinct app's slug, the checks already have their own suite and the note is omitted. Because the report reads GitHub's response, the workflow does not pass a flag.

---

## Known limitations

GitHub-specific limitations, separate from the core design's list.

- Merge queue. The adapter relies on GitHub auto-merge plus a required check; it does not integrate with GitHub merge queues.
- Discussion gathering reads one page. The context gatherer requests the first 100 issue comments and the first 100 review comments and does not follow pagination. GitHub returns both in ascending id order, so the first page holds the oldest comments; on a PR with more discussion than that, the newer comments past the first page are not gathered (and a routed reply whose thread root sits on a later page does not resolve).
- Finding replies arrive as general discussion. Reply routing by `FindingId` is wired end to end and resolves a reply whose thread root carries a finding marker. The reporter posts one sticky comment and check runs, not per-finding comment threads, so PR comments reach reviewers as general discussion.
