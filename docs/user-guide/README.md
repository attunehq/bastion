---
title: Bastion user guide
summary: "Agentic code review for a world where agents write all of the code."
order: 0
---

# Bastion user guide

> Agentic code review for a world where agents write all of the code.

This guide teaches you how to use Bastion on your own project: what it is, how to
run it, how to write reviewers, and how to wire it into CI and governance. It is
written for two audiences at once (the human curating the review policy and the
agent looping against it), because Bastion runs the repository's reviewers and merge
gate for both through whatever surface is natural to each (CI can add the PR's
description and discussion to the reviewers' context, and a purely local run can add
an author's personal user-level reviewers, which CI never sees).

This guide is self-contained: everything you need to run Bastion, write reviewers,
and wire it into CI is here, with nothing essential living elsewhere. If you want to
work on Bastion itself rather than use it, the contributor and design docs live in the
[Bastion repository](https://github.com/jssblck/bastion).

> **Reading this as an agent?** The whole guide is also served as a single plain-text
> file at [`bastion.jessica.black/llms-full.txt`](https://bastion.jessica.black/llms-full.txt),
> so you can ingest every chapter in one fetch instead of crawling pages.

## Read in order

The chapters build on each other. If you read them top to bottom you will go from
"what is this" to "running it in CI with a governed policy" without backtracking.

1. **[Introduction](./introduction.md)**: the problem Bastion solves, the core
   idea (reviewers as fitness functions), and the mental model. Start here.
2. **[Getting started](./getting-started.md)**: install the CLI, write your
   first reviewer, and run your first review in about five minutes.
3. **[Concepts](./concepts.md)**: reviewers, triggers, modes, the verdict, and
   the merge gate. The vocabulary the rest of the guide assumes.
4. **[Authoring reviewers](./authoring-reviewers.md)**: the registry schema in
   full, from the four required fields to timeouts, backends, environment, and
   prompt inputs. How to write a reviewer that stays at high recall.
5. **[The local workflow](./local-workflow.md)**: the `bastion review` loop in
   depth: human output vs. the JSONL agent stream, exit codes, and inspecting
   saved runs (`runs`, `show`, `transcript`, `clean`).
6. **[Continuous integration](./continuous-integration.md)**: promoting your
   repository's reviewers into GitHub Actions: checks, the aggregate gate, and
   per-author billing.
7. **[Governance](./governance.md)**: keeping humans at the policy layer with
   CODEOWNERS and branch protection, the escape-to-improvement loop, and what
   Bastion deliberately does not guarantee.

## In a hurry: set up Bastion in CI

If your goal is "get Bastion reviewing pull requests on GitHub," here is the whole
path; each step links to its details:

1. **Install the CLI and pick a backend.** [Getting started](./getting-started.md)
   (a subscription works; no API key required).
2. **Write `.bastion.yaml`** at your repo root with one or two reviewers, and check
   it with `bastion validate`. [Authoring reviewers](./authoring-reviewers.md). To
   pin a model like `gpt-5.5:high`, set `model:` and `effort:` separately under a
   pinned `backend:`.
3. **Add the workflow** and the per-author auth step.
   [Continuous integration](./continuous-integration.md#the-workflow). The complete,
   copy-pasteable auth recipe (the `<BACKEND>_AUTH_<LOGIN>` secret convention, the
   `case`-arm mapping, Dependabot, and fork safety) is in
   [Authentication & billing](./continuous-integration.md#authentication--billing).
4. **Protect the policy and require the check.** [Governance](./governance.md):
   CODEOWNERS over `.bastion.yaml` and the workflow, and branch protection requiring
   the aggregate `bastion` check.

## The one-paragraph version

You declare **reviewers** (focused agent prompts, one concern each) in
`.bastion.yaml`. Each reviewer has a **trigger** (file globs) and a
**mode** (`gate` blocks the merge, `advisor` only comments). `bastion review`
finds the reviewers whose triggers match your working-tree changes; each
executes in parallel, replays from a verified attestation (CI), or carries its
unchanged pass forward from the branch's previous run (local or CI), and their
verdicts aggregate into one decision: all gates must pass.
A local run can also merge in personal reviewers from a user-level `.bastion.yaml`,
so you can run a reviewer locally even where a repo has not adopted Bastion. An
authoring agent loops `bastion review` until it is green, then opens a PR where CI
executes, replays, or carries the repository's reviewers (the user-level ones are local-only).
CI usually confirms the result, and can differ when it adds the PR's description and
discussion to the reviewers' context. Humans stay in the loop by owning the reviewer
registry, not by reading every diff.

## Status

Bastion is experimental and still partial. The routing, runner, verdict
aggregation, and on-disk run store are implemented and tested, and the Claude Code,
Codex, and Pi backends execute reviewers for real, natively or inside a container
when a reviewer declares a `runner` and opts into `capabilities.network: true`. The
remaining capability fields (`mcp` and `skills`)
are accepted but not provisioned, so a reviewer that opts into one fails closed
rather than running without it. `network: true` grants a containerized reviewer
general (unscoped) egress; a container with the default `network: false` is rejected
before it runs, so a gate blocks and an advisor is skipped (provider-only scoping is
unbuilt). A containerized reviewer must opt into `network: true`. These are called out
where they appear in [Authoring reviewers](./authoring-reviewers.md).
