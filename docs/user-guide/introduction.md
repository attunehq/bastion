---
title: Introduction
summary: "Why Bastion exists, and the one idea you need to hold in your head."
order: 1
---

# Introduction

> Why Bastion exists, and the one idea you need to hold in your head.

## The problem

Agents write most of the code on a growing number of teams. When they are
fully unlocked, output volume looks more like *engineers x 100* than *x 1*. Two
things stop teams from unlocking that:

- **Human diff review does not scale.** Asking a 5-person team to review their
  agents' output is like asking 5 people in a 500-person org to review the other
  495. You cannot fix that by trying harder.
- **Without review, codebases rot.** Things go fine until they do not, and then
  you have a ball of mud nobody can work in.

The usual shape of agentic review hands the whole diff to one reviewer that checks
everything and writes comments designed for a person to act on. As you ask one
generic reviewer to check more things, its recall on any single one degrades. A
one-item checklist agent works; at ten items it is weaker; at a hundred it fails.

## The core idea

In Bastion, a reviewer is a **focused fitness function** (an automated check that
continuously asserts one property holds as the system evolves), and review is the
**author agent's loop taken to its conclusion**.

An authoring agent already loops against the compiler, the linter, and the tests.
Bastion adds loops whose oracle is *another agent*, one that encodes judgment a
compiler or a test cannot. The whole system follows from five principles:

1. **One concern per reviewer.** Single-responsibility reviewers stay at high
   recall and confidence. The unit of the system is *the reviewer*, not *the
   review*. You cover more ground by adding narrow reviewers, never by broadening
   one. A cross-cutting property like tenant isolation or migration safety is not
   special; it is just another reviewer whose single concern is that property.
2. **Reviewers run in the author's own loop, not only in CI.** The repository's
   reviewers run locally (fast, pre-PR) and in CI (authoritative), so CI usually
   confirms a green local loop. The two can differ when a local run cannot see the
   pull request (no PR, or `gh` is missing or failed), so reviewers miss that
   discussion, and when a purely local run includes your personal
   user-level reviewers with
   `--with-user-reviewers`, which CI never runs (see
   [Authoring reviewers](./authoring-reviewers.md#user-level-reviewers)).
3. **Humans sit at the policy layer.** The goal is not human-out-of-the-loop. It
   is to move the human from reviewing diffs to *authoring, curating, and
   governing reviewers*, plus triaging escapes (bugs that slipped through a review
   that should have caught them). Your interface becomes the reviewer registry, not
   the diff.
4. **Aligned agents can still inadvertently game the system.** Bastion tolerates
   this and makes it *visible* and *easy to correct* by adjusting reviewers,
   rather than trying to make gaming impossible (which would give up the benefits
   of agentic development entirely).
5. **Reviewers converge through use.** Ship a reviewer that is good enough, then
   improve it from the escapes you actually hit, rather than trying to design a
   perfect one up front. The escape-to-improvement loop is where that happens.

## The mental model

Picture the way a good team did code review before agents:

> An author opens a PR. A reviewer reads it, leaves feedback (some blocking, some
> optional) and withholds approval until satisfied. The author addresses the
> blocking items (by changing the code, or by convincing the reviewer the code is
> already right) and requests re-review. Repeat until approved.

Bastion brings *that* process to the agent era. The reviewers play the colleague's
role, their verdicts are the feedback, and the author agent resolves the blocking
items and re-runs. The human is still in charge, but of the reviewers, not of
every line.

## What Bastion is not

Two non-guarantees are deliberate. Keep them in mind before you adopt it:

- **No guarantee of correctness.** Bastion does not prove your code is free of
  bugs or vulnerabilities. It is code review without the human in the small loop;
  a reviewer is only as good as its model and its prompt.
- **No guarantee the right thing is being built.** Catching "this is the wrong
  thing to build" was never review's job. By PR time that ship has sailed; it is a
  design-time question. Keep humans in the design loop.

Bastion is also **not an adversarial security boundary**. It is the agent-era
equivalent of team code review for aligned contributors: a speed bump and a set of
good defaults that keep earnest actors on the rails, not a defense against a
determined malicious one. The practical consequences for you, and how to govern
within these limits, show up in [Governance](./governance.md).

---

Next: [Getting started](./getting-started.md) -> install the CLI and run your
first review.
