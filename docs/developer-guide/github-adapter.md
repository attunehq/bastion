# Bastion on GitHub

> The GitHub adapter: how Bastion runs in Actions, reports to PRs, and gates merges.

The core design ([`design.md`](./design.md)) is deliberately CI-agnostic; it describes reviewers, verdicts, and the merge gate without saying how any of it touches a real forge. This doc is the GitHub adapter: the concrete answer to "where does the workflow live, how does a verdict become a check, and how is the policy layer enforced" when the forge is GitHub. Everything here is one implementation of the plugin-style CI interface the core design refers to; another forge would get its own doc and reuse the same core.

> **What the adapter does.** The adapter runs `bastion review`, gates on its
> exit code, and then runs `bastion github report` to post the results: a sticky
> PR comment carrying every reviewer's verdict and findings (optional ones
> included), one check run per reviewer, and the always-present aggregate
> `bastion` check. The full run is uploaded as an artifact too. Bastion's GitHub
> helpers are `bastion github codeowners` and `bastion github report`. Because
> `bastion github report` runs after `bastion review` finishes, each check run is
> posted already completed with its conclusion. The packaged form is the GitHub
> Action ([`action.yml`](../../action.yml) at the repository root, pinned as
> `attunehq/bastion@v0`); this repository dogfoods it through
> [`.github/workflows/bastion.yml`](../../.github/workflows/bastion.yml).

The guiding rule is the same as the core: Bastion does not own CI, it plugs into yours. The workflow, the secrets, the preview environments, and the branch protection rules are GitHub's; Bastion reads and writes them through a thin adapter and otherwise stays out of the way.

---

## How it runs

Bastion runs as a GitHub Actions workflow triggered on pull request events: `opened`, `synchronize` (a new push to the PR), and `reopened`. On each event the adapter:

1. Computes the changed file set from the merge base with the PR's direct base.
   The packaged action passes that base explicitly from the workflow event. A
   local run without `--base` discovers the same value through `gh pr view`.
   For a native stack A <- B <- C, each review covers one layer.
2. Routes candidates: applies path triggers and agent-trigger path prefilters to the changed files.
3. Gathers the PR's [review context](./design.md#review-context): `bastion review --repo OWNER/NAME --pr N` reads the PR identity and description through `gh`, with the Actions REST client as a compatibility fallback, then hands the reviewers that context alongside their prior findings. Discussion comes from the REST seam ([`src/github/context.rs`](../../src/github/context.rs)) when a token is available and remains best effort.
4. Resolves each selected candidate in parallel (see the core design's _Aggregation & the merge gate_). An agent trigger may record a skip before the full reviewer runs. A candidate covered by a verified attestation is reconstructed from the attested bundle without dispatching its backend; a reviewer whose trigger-scoped content is unchanged since the newest prior run on the branch that resolved that reviewer carries that prior pass, also without a backend; every other selected reviewer executes through its backend fresh. See [Verification and replay](#verification-and-replay).
5. Reports each verdict back to the PR.

Native reviewers run directly on the Actions runner. A reviewer that declares a container `runner` and `capabilities.network: true` runs its backend inside that container on the Actions runner (the engine is already present on GitHub-hosted runners); see [Containers](./containers.md). None of routing or aggregation is GitHub-specific; only the steps that read the PR and write results go through the adapter.

### The packaged action

The adapter ships as a composite GitHub Action, the
[`action.yml`](../../action.yml) at the repository root, so an adopter writes
`uses: attunehq/bastion@v0` instead of hand-rolling the steps. Its contract is
deliberately the same split as the local CLI: the consumer owns the checkout
(full history, PR head SHA) and the backend CLI plus its credential,
authenticated in a prior step exactly as a contributor authenticates locally;
the action owns the engine and the review. It installs a checksum-verified
release through the bundled `scripts/install.sh`, fetches the attestation notes
ref, restores the branch's prior run artifact (prior-finding recall and
incremental carry), runs `bastion review` with the PR context flags, uploads
the run, runs `bastion github report`, and only then fails on a blocked review,
so the PR surfaces land even when the gate blocks.

The release workflow advances a floating major tag (`v0`) on every stable
release, and the action resolves its own ref to an engine: `@vX.Y.Z` pins that
exact engine, `@v0` tracks the newest stable release in the major, and a
`version` input overrides both. The action's inputs and outputs are a
downstream surface like the CLI's, so a breaking change there is weighed and
called out the same way. The consumer-facing
reference (the full workflow, the input table, what stays the consumer's) is
the user guide's
[continuous integration chapter](../user-guide/continuous-integration.md).

The adapter is the GitHub *producer* of the review context. It maps GitHub's fields onto the transport-neutral `ReviewContext` and leaves the rest out of the core. A non-empty PR body becomes the author's stated intent; an empty body supplies none, so the local commit-message intent stands. Each non-Bastion comment becomes an untrusted claim carrying the commenter's `Standing` (mapped from `author_association`, so a reviewer can weight a maintainer above an outsider without ever obeying either), and Bastion's own past comments are filtered out by their hidden marker so a reviewer never reads itself. The core never sees an `author_association` or a comment id.

Two parts of the context need state that a single CI run does not have on its own:

- **Prior-findings memory** is recalled from the local run store (`store::findings_from_events`, over the branch's latest run), and a fresh Actions runner starts with an empty store. So for a reviewer to recall what it raised on the last push, the workflow must persist the run store between runs and restore the previous run before `bastion review`. The self-hosted example below does this by uploading the run as an artifact and downloading the prior one; without that step, the GitHub surface still gets the PR's intent and discussion (gathered fresh each run), just not cross-run finding memory.
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

A repository can opt in (`attestations: true` in its registry) to let CI reuse a signed local run instead of re-executing every reviewer; see [Attestation](./attestation.md) for the full design. On the GitHub adapter this happens inside `bastion review` itself, before the runner fans reviewers out, so it is invisible as a separate step, only as which reviewers end up replayed, carried, or executed.

**Base selection.** An explicit `--base` always wins. Without one, Bastion uses the direct base commit returned by `gh pr view`. If the command finds no PR or the lookup fails, Bastion uses `main`; a failed lookup also prints a warning. This automatic selection applies to native GitHub stacks, whose PRs target the branch immediately below them.

**Dirty-checkout fallback.** Before any note lookup, `bastion review` checks whether the CI working tree is dirty (uncommitted tracked changes or untracked files). A dirty checkout skips note lookup entirely: it records a `run.attestation-fallback` event with the reason, and its reviewers then resolve through the ordinary carry-or-execute path, since a dirty tree's reviewers see content no attestation's committed bindings name.

**Fetching the note.** Given a clean checkout, `git notes` are not part of an ordinary checkout, so `bastion review` looks for the note under `refs/notes/bastion` on HEAD first, then falls back to the PR's head SHA (from the gathered GitHub context) when HEAD carries none; CI's checkout can be a merge commit, and the note the author actually attested hangs off their own head commit. Both lookups need the notes ref fetched locally first (see [the two workflow requirements](#the-two-workflow-requirements) below). When neither lookup finds a note, no attestation was offered: the run resolves to `NotAttested`, records no event, continues through ordinary carry-or-execute planning (an unchanged prior pass may still carry, the rest execute), and says nothing about attestation. An un-attested PR is the ordinary case, so it draws no notice; only an attestation that was offered and *refused* is surfaced.

**Signature verification.** Given a note, the adapter resolves the PR author's login and fetches their registered SSH signing keys (`GET /users/{username}/ssh_signing_keys`, over the same REST seam `client.rs` already provides) and runs `ssh-keygen -Y verify` against them in the `bastion` namespace. A signature by any other key, including one freshly minted on the author's machine, fails verification.

**Seal verification and binding checks.** The adapter verifies the run seal with its own embedded secret (the same secret every binary of that release shares), then re-derives its own bindings from the checkout (HEAD's tree, the merge base's tree, the diff's patch-id, and the effective config hash) and compares them against what the bundle recorded. Any mismatch, or a seal that shows a test seam was active during the local run, skips replay and sends the reviewers through ordinary carry planning and execution rather than partially trusting the bundle.

**Per-reviewer replay or execute.** Verification and binding checks pass or fail for the bundle as a whole, but replay is decided per routed reviewer. CI replays covered reviewers that have not opted out; a reviewer the bundle does not cover then goes through carry planning, carrying an eligible prior pass or executing fresh, while an `attestation: never` reviewer always executes fresh (it opts out of both replay and carry). A replayed verdict carries the same fail-closed policy a fresh one would, so a replayed block still blocks the gate.

**Reporting.** The merged result (replayed, carried, and freshly executed reviewers together) flows through the normal report path, with a few additions. The sticky comment opens with a `[!NOTE]` callout naming which reviewers replayed, the attesting key, and when it was signed, right alongside the skills-drift `[!WARNING]` block, and each replayed reviewer's check-run summary adds a line stating its verdict was replayed rather than executed. Carried reviewers get the parallel treatment: a `[!NOTE]` callout names the ones whose verdict carried from the newest prior run on the branch that resolved that reviewer (an unchanged trigger-scoped diff, and no signature, since carry is not attestation), and each carried reviewer's check-run summary flags it the same way, mirroring the local CLI's `carried` marker. When an attestation was offered but *refused* (an unreadable or unverifiable note, an unregistered key, a seal mismatch, a stale binding, a dirty checkout), the comment carries a `[!WARNING]` block naming why, taken from the run's `run.attestation-fallback` event. A commit that carried no note at all is not a refusal and draws no such block: it is silent.

### The two workflow requirements

Honoring an attestation needs two things from the workflow. The GitHub Action covers the second on its own; a hand-rolled workflow needs both (this repository's [`bastion.yml`](../../.github/workflows/bastion.yml) shows the first, and consumes the action for the second):

- **Check out the PR head, not the merge commit.** `actions/checkout`'s default `pull_request` behavior checks out a synthetic merge commit, whose tree never matches the head tree an author attested. Set `ref: ${{ github.event.pull_request.head.sha }}` so CI's HEAD is the commit the note is actually attached to. The checkout is the consumer's step, so the action cannot do this for them.
- **Fetch the notes ref.** `actions/checkout` does not fetch notes by default. The action fetches `refs/notes/bastion` before reviewing; rolling your own, add `git fetch origin +refs/notes/bastion:refs/notes/bastion`, tolerant of the ref being absent (most PRs will not carry a note; that is the ordinary case, not an error).

If a repository skips either step or has not set `attestations: true`, replay is skipped and reviewers resolve the ordinary way: an unchanged reviewer carries its prior pass and the rest execute fresh.

### The aggregate check

There's a wrinkle GitHub forces on us. Branch protection requires you to name the checks that must pass, but Bastion's set of reviewers varies per PR; a docs-only PR and a server PR trigger different reviewers, so there is no fixed list of check names to require.

The fix is a single always-present check, `bastion`, and it is the only one branch protection requires. It always runs, even when zero reviewers match (a trivial pass in that case), so it is a stable required check. Internally it reflects the aggregate: `success` only when every gate that ran passed, and `failure` if any gate blocked, errored, or timed out (fail-closed, per the core design). Those are normally the same thing, with one exception: a *partial* run (`bastion review --reviewer` ran a subset of the triggered reviewers) reports the aggregate of only the reviewers that ran, and the status line carries an explicit partial-run notice rather than the check failing. A partial run is a hand-driven iteration tool; `bastion.yml` always runs the full set, so the status line names it as partial. The per-reviewer check runs stay informational; `bastion` is the gate.

The aggregate check summary and the sticky comment share one headline (the status line): the decision and gate tally, including semantic skips, then the run's wall-clock duration and the usage totals summed across trigger and reviewer calls (input and output tokens, cache-read tokens when nonzero, and cost). The token and cache figures are omitted when no backend reported usage, including a mock run or a zero-reviewer run. These are the run-level totals; the per-reviewer breakdown lives on each reviewer's own check (see [Reviewer detail](#reviewer-detail)).

### Reviewer detail

Each reviewer's check run is also where its detail lives; a reader clicks "Details" on that reviewer in the checks list and lands on a page Bastion owns the markdown for. This is for humans and for the occasional surprising decision, not part of the implementing agent's normal loop; the agent already has the feedback in the sticky comment. Two things are presented there.

- **Metadata and decision.** A short header: the reviewer name, its mode (`gate` or `advisor`), the backend it ran on, and how long it took; then the verdict and summary. The check run _title_ carries the one-line decision ("Blocked: `src/foo.ts` concentrates three responsibilities") so it is legible without opening anything. An agent-trigger skip gets a successful informational check titled `Skipped` with the routing reason; it does not masquerade as a passing review.
- **Tokens and cost, when available.** When the backend reports usage, a token line lists the input and output token counts, the cache-read tokens (prompt-cache hits, shown only when nonzero), and the session cost; when the backend reports no usage, the line is omitted rather than shown empty. Usage is per reviewer, so an expensive e2e reviewer and a cheap hermetic one show separate totals.

The full agent session is not embedded in the check output; the run, transcripts included, is uploaded as the workflow artifact, and the sticky comment footer points there. The aggregate `bastion` check renders a plain Markdown table of the reviewer candidates the run resolved, with columns `Reviewer`, `Mode`, `Verdict`, and `Summary`. A semantic omission appears as `skipped`. For a partial run the table contains only the selected candidates, with the omitted reviewers represented by the partial-run notice in the headline rather than rows.

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

**CODEOWNERS.** The Bastion CLI supports generating a CODEOWNERS block covering the reviewer config: the `bastion` review job in GitHub, reviewer definitions, the registry (including, when the command runs inside the repository, every `include:`d registry file and prompt file the registry pulls in from inside the repository; a path outside the tree cannot be protected by CODEOWNERS and is left out), and the CODEOWNERS file itself. Any PR that adds, removes, or edits a reviewer; loosens a trigger; or changes a prompt touches an owned path, so GitHub requires a human review before merge. Repository maintainers can also obviously provide their own CODEOWNERS instead of using the generated suggestion. The main reason we can't have Bastion automatically manage this is because CODEOWNERS changes only take effect after a PR is merged; as such the CODEOWNERS needs to be written in such a way that it statically protects every path Bastion writes into.

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

This repository dogfoods the adapter through [`.github/workflows/bastion.yml`](../../.github/workflows/bastion.yml), which consumes the GitHub Action from the PR's own checkout (`uses: ./`) so action changes take effect in the same PR. The engine stays outside the PR's reach: a local `uses: ./` carries no release ref, so the action installs the *latest* published stable release rather than a binary built from the PR's own sources, and a change can never edit the engine that judges it. Engine improvements land without a per-PR pin bump while the engine remains a maintainer-published release. Reviewer policy in [`.bastion.yaml`](../../.bastion.yaml) is still read from the checkout, and that file, the workflow, and `action.yml` are all CODEOWNERS-protected paths. Every reviewer in `.bastion.yaml` pins `backend: muse`, so each review runs on Meta's Muse Code CLI, billed to the repository's Muse API key rather than to a contributor: the workflow installs the Muse launcher on `PATH` and exports the `META_AI_API_KEY` Actions secret as `META_API_KEY` on the review step, which the CLI reads ahead of any account login. The registry defaults every reviewer to `model: muse-spark-1.2-contributor` and `effort: high`, which the Muse Code backend forwards as `--model` and `--reasoning-effort`, so the model and effort are selected per review. This is the API-key billing shape from the section above; the earlier per-author subscription flow (a `CODEX_AUTH_<LOGIN>` secret per contributor, rehydrated into `~/.codex/auth.json` by a `case` arm on the PR author's login) remains the pattern for a team that bills reviews to each author's own plan, and the same workflow can carry both when some reviewers pin a subscription-billed backend.

An empty or missing key fails closed: Muse rejects the run and the gate blocks, rather than silently passing. Two further boundaries keep this safe: GitHub does not expose secrets to workflows triggered by fork pull requests, and the job additionally guards on `head.repo.full_name == github.repository`, so an agentic backend never runs over untrusted code with a live credential; a fork contribution is reviewed by a maintainer re-running it from a trusted branch. The job's pass/fail is the gate (a blocked review exits non-zero): the action runs `bastion github report` (the sticky comment and the per-reviewer and aggregate check runs) and uploads the full run artifact *before* it fails on the review's exit code, so the PR is updated even when the review blocked. Reporting needs `pull-requests: write` and `checks: write`, and a GitHub App installation token to create check runs (a classic personal access token cannot). This repo configures a dedicated Bastion app for that token, minted in a prior step and handed to the action's `report-token` input, so its checks group under their own name rather than a sibling workflow's; see [Check-run grouping and the dedicated app](#check-run-grouping-and-the-dedicated-app). When no such app is configured the action falls back to the default `GITHUB_TOKEN`, which is itself an installation token and still posts.

This repository merges through a GitHub merge queue, and the review does not run there. Reviewers are paid agents, so a queue run would spend a second set of tokens on content the pull request's own green review already covered. `bastion.yml` still declares a `merge_group` trigger, because a required status check that never reports leaves the queue waiting forever; the trigger fires, the job's `if:` refuses the event, and the skipped job reports the `bastion review` check. The tradeoff is explicit: a merge group that combines several pull requests merges on the per-PR verdicts rather than on a review of the combined result. That is the same bet a merge queue already makes about every other check, and the queue re-runs the deterministic checks (CI, security, the site build) on the combined tree, so only the agentic review rests on the per-PR verdict.

The deterministic checks take the opposite default: they run on `merge_group`, but each one first consults the [`merge-queue-skip`](../../.github/actions/merge-queue-skip/action.yml) composite action. GitHub enqueues a pull request only once its required checks are green, so when the merge group's tree matches the head tree of the pull request the group was built from, every check has already accepted exactly this content and the jobs skip. The test is on tree content, so it is conservative on its own: a group holding several pull requests, or a base branch that moved under the pull request, produces a tree nobody checked, the comparison fails, and the checks run.

Dependabot PRs review like any other. They are same-repo (so they clear the `head.repo` guard), and because the job declares an explicit `permissions:` block, the default `GITHUB_TOKEN` on a Dependabot-triggered run carries the `pull-requests: write` / `checks: write` it grants, so `bastion github report` posts the sticky comment and the per-reviewer and aggregate check runs normally. That removes the usual read-only-token obstacle, so the `bastion` check can be required for Dependabot PRs the same as for any other. (As a worked example outside this repo, `Fieldguide/minionforge` runs Bastion on its Dependabot PRs on exactly this shape, an API-key-billed `claude-code` review on a plain `pull_request` trigger, and requires the resulting check.)

Two Dependabot specifics still apply. The first is universal; the second is only for per-author subscription billing.

- **Secrets come from a separate store.** GitHub serves secrets to Dependabot-triggered runs from a *Dependabot* secret store, not the Actions store. Whatever credential the review step reads, this repo's `META_AI_API_KEY`, an `ANTHROPIC_API_KEY`, or a per-author `CODEX_AUTH_<LOGIN>`, must be set there too (`gh secret set <NAME> --app dependabot`), or it arrives empty on a Dependabot PR and the gate fails closed. Every billing model needs this Dependabot-store copy.
- **A bot has no subscription of its own.** Under per-author billing the bot author has no credential, so its `case` arm points at a maintainer who opted in to sponsor its reviews, billing the bot's PRs to that person. The bot login is literally `dependabot[bot]`, and the `[bot]` brackets are a glob character class in a shell `case` pattern, so the arm must quote them (`'dependabot[bot]')`) to match literally. An arm that resolves to an empty secret fails closed with a "mapped but empty" error, usually the sign the Dependabot-store copy above is missing. API-key billing has no per-author arm, so this case does not arise.

For API-key billing instead of a subscription, store the provider's API key as a secret and export it into the review step rather than writing `auth.json`; the same mapping shape applies.

---

## Environments & inputs

Bastion consumes environments, it does not provision them. A reviewer that needs a preview URL, a database, or any other running dependency expects the workflow to have stood it up and exposed it; the reviewer just reads it.

On GitHub that means the workflow owns whatever a reviewer's `env` and `inputs` reference. A typical setup deploys a preview environment for the PR in an earlier job, or a separate workflow, and passes its URL into the Bastion job as an environment variable. How it reaches the agent depends on where the reviewer runs. A native reviewer inherits the job environment, so a variable exported into the Bastion job is visible to the agent, and `env`/`inputs` add literal values on top. A containerized reviewer (one with a `runner` and `capabilities.network: true`) inherits none of the arbitrary job environment; only its literal `env` pairs and the fixed provider-credential set cross into the container, so a per-PR value must be written into the reviewer's `env`, usually by templating the registry before the job runs. A secret a reviewer needs comes from Actions secrets the same way author credentials do.

Standing up a preview environment is a deploy concern, and the deploy system already knows how. Bastion's job starts once the environment exists.

---

## Example workflow

A minimal workflow wiring Bastion into PR review, through the packaged action:

```yaml
name: bastion
on:
  pull_request:
    types: [opened, synchronize, reopened]

# The action posts the PR comment and the check runs, so the job needs more than
# read access; `actions: read` lets it restore the branch's prior run artifact.
# The aggregate `bastion` check it reports is what branch protection requires.
permissions:
  contents: read
  pull-requests: write
  checks: write
  actions: read

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
          # The PR head, not the merge commit: attestation replay binds to the
          # head tree the author attested.
          ref: ${{ github.event.pull_request.head.sha }}

      # Install and authenticate your backend CLI, billed to the PR author (see
      # Authentication & billing above). Then stand up whatever your reviewers
      # consume; Bastion does not do this.
      - id: preview
        run: ./scripts/deploy-preview.sh   # exports the preview URL

      # Optional: mint a token for a dedicated Bastion app so the report's check
      # runs get their own named check suite (see "Check-run grouping" below).
      # Skipped when the app is not configured; reporting then falls back to the
      # default GITHUB_TOKEN.
      - id: app-token
        if: env.HAS_BASTION_APP == 'true'
        uses: actions/create-github-app-token@v2
        with:
          app-id: ${{ secrets.BASTION_APP_ID }}
          private-key: ${{ secrets.BASTION_APP_PRIVATE_KEY }}

      # Installs the engine, fetches the notes ref, restores the branch's prior
      # run, reviews, uploads the run, reports, and fails on a blocked gate, in
      # that order. Step env is visible to native reviewers, so this is where a
      # preview URL reaches the agents.
      - uses: attunehq/bastion@v0
        env:
          PREVIEW_URL: ${{ steps.preview.outputs.url }}
        with:
          report-token: ${{ steps.app-token.outputs.token || github.token }}
```

The steps the action packages, and the raw workflow shape for teams that cannot consume actions from github.com, are documented in the user guide's [continuous integration chapter](../user-guide/continuous-integration.md).

Branch protection on the default branch requires the `bastion` check and review of the owned reviewer-config paths; everything else is standard GitHub.

## Check-run grouping and the dedicated app

In the PR checks list, the label before the `/` is not the workflow that created a check run; it is the **check suite** the run belongs to. A check suite is keyed by `(GitHub App, commit)`, not by workflow. Every GitHub Actions workflow runs under the single shared `github-actions` app, so one commit that triggers several workflows has several `github-actions` check suites (one per workflow). The check runs `bastion github report` posts through the REST API (`POST /repos/{owner}/{repo}/check-runs`) carry no suite id, because the Checks API has no parameter to create or choose one: GitHub assigns the run to a suite for that `(app, commit)` pair on its own. With the shared Actions identity that resolves to one of the commit's other suites (empirically the earliest-created), so the bastion-posted runs render under a sibling workflow's name (for example `Security / fail-closed-gates`) rather than grouping together.

There is no payload or naming trick that fixes this while staying on the default `GITHUB_TOKEN`: the collision is inherent to multiple workflows sharing one app identity. A check run gets its own named suite only when a **distinct GitHub App** creates it. So the durable fix is to post the report under a small per-adopter app instead of the shared Actions identity.

This stays inside Bastion's "owns no infrastructure, custodies no credentials" rule: each adopting org creates and holds its own app, exactly as it already holds its own backend-credential secrets. It is deliberately not one shared public Bastion app: acting as a shared app would require a central service holding the app's private key (a key able to write to every adopter's repo) to mint tokens, which is precisely the always-on, credential-custodying infrastructure the adapter avoids.

Setup is a one-time, per-org step:

1. **Create the app.** The hosted walkthrough at [bastion.attune.inc/github-app](https://bastion.attune.inc/github-app) (source: [`site/src/pages/github-app.astro`](../../site/src/pages/github-app.astro)) walks you through creating a GitHub App by hand: open GitHub's new-app form for the personal account or org, set exactly the permissions the report step needs (`checks: write`, `pull_requests: write`, `contents: read`) with no webhook, and create it. The app's name is what the checks group under, for example `YourOrg's Bastion`. The walkthrough does not use GitHub's [app-manifest flow](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest): completing that flow requires a backend to exchange the temporary code for the app's credentials, and Bastion custodies no credentials and serves no such backend.
2. **Capture its credentials.** Generate the app's private key (a downloaded `.pem`), note the numeric App ID, and install the app on the repositories that run Bastion.
3. **Store the secrets.** Set `BASTION_APP_ID` (the App ID) and `BASTION_APP_PRIVATE_KEY` (the `.pem` contents) as Actions secrets, at the repo or org level. Mirror them into the Dependabot secret store as well if Dependabot PRs are reviewed, for the same reason the review credential is mirrored there.

The workflow mints an installation token from those secrets with [`actions/create-github-app-token`](https://github.com/actions/create-github-app-token) and hands it to the action's `report-token` input (or, hand-rolled, to the report step's `GITHUB_TOKEN`); the per-reviewer and aggregate checks then render under the app's name. The mint step guards on both secrets being present (the two are one credential, so a half-configured repo with only one set falls back rather than failing the mint), so it is fully optional: with the secrets unset the step is skipped and the report step falls back to the default `GITHUB_TOKEN`, still posting the comment and checks (just grouped under whichever suite GitHub picks). The minted token also authors the sticky comment, so the comment and the checks present under one identity.

`bastion github report` detects this situation on its own, with no help from the workflow (the workflow is the adopter's, and they write their own). GitHub stamps every created check run with the `app` that posted it, so the report reads that `app.slug` back from the check-run response: when it is `github-actions` (the shared identity, no dedicated app), the sticky comment closes with a one-line note linking to the setup walkthrough; when it is a distinct app's slug, the checks already have their own suite and the note is omitted. Because the report reads GitHub's response, the workflow does not pass a flag.

---

## Known limitations

GitHub-specific limitations, separate from the core design's list.

- Merge queue. The adapter relies on GitHub auto-merge plus a required check; it does not integrate with GitHub merge queues.
- Discussion gathering reads one page. The context gatherer requests the first 100 issue comments and the first 100 review comments and does not follow pagination. GitHub returns both in ascending id order, so the first page holds the oldest comments; on a PR with more discussion than that, the newer comments past the first page are not gathered (and a routed reply whose thread root sits on a later page does not resolve).
- Finding replies arrive as general discussion. Reply routing by `FindingId` is wired end to end and resolves a reply whose thread root carries a finding marker. The reporter posts one sticky comment and check runs, not per-finding comment threads, so PR comments reach reviewers as general discussion.
