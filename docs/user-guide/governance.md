---
title: Governance
summary: "Keeping humans at the policy layer: protecting the registry, the escape-to-improvement loop, and what Bastion deliberately does not guarantee."
order: 7
---

# Governance

> Keeping humans at the policy layer: protecting the registry, the
> escape-to-improvement loop, and what Bastion deliberately does not guarantee.

Bastion relocates the human from reviewing diffs to *governing the reviewers*. That
only works if the reviewer policy itself is protected and continuously improved.
This chapter is the human's operating manual.

## The policy layer

The reviewers, their prompts, and their triggers *are* the review policy. The whole
safety story rests on a simple rule: **any change to that policy is reviewed by a
human before it merges.** Otherwise an aligned-but-mistaken agent could quietly
loosen a trigger or soften a prompt, and the gate would erode without anyone
noticing.

Two native GitHub mechanisms enforce this; neither is exotic.

### CODEOWNERS protects the registry

Bastion can generate a CODEOWNERS block covering the reviewer-policy paths: the
registry, the reviewer definitions, the Bastion workflow, and the CODEOWNERS file
itself:

```sh
bastion github codeowners --owner @your-org/platform
```

Pass `--owner` once per owner (it is repeatable). When run inside the repository,
the command loads the registry and adds an entry for every file it pulls in
(each `include:`d registry file and each `prompt: {file: ...}` prompt file),
since those carry policy exactly like the root file. A pulled-in file that
resolves outside the repository is left out: CODEOWNERS cannot protect a path
outside the tree, so keep policy files in the repository if you want them
governed. Add the generated block to your `CODEOWNERS`, and regenerate it when
you add an include or a prompt file so the new path is covered. With that block in place, any PR that adds, removes, or
edits a reviewer; loosens a trigger; or changes a prompt touches an owned path,
so GitHub requires a human review before merge. You can also write your own
CODEOWNERS instead; the generated block is a correct starting suggestion.

> Why generate it statically rather than have Bastion manage it live? CODEOWNERS
> changes only take effect *after* a PR merges, so the file must be written to
> protect every path Bastion will ever write into, ahead of time, which is what
> the generated, reviewed block provides.

### Branch protection requires the check

Require Bastion's review on your default branch. That is the review job from
[Continuous integration](./continuous-integration.md#the-workflow), which
also posts the always-present aggregate check named `bastion` (with a check run per
reviewer alongside it), so you can require either the job or that `bastion` check.
A PR then cannot merge with the gate switched off, and
because the workflow file and the registry are themselves owned paths, switching it
off is itself a policy change a human sees.

That is the entire enforcement story, and it is intentionally modest. The
contributor Bastion is designed for is an aligned agent that would never quietly
disable CI; the CODEOWNERS trip wire and the required check exist so that *if*
policy changes, a human is in the loop, not so that a determined adversary is
stopped.

## The escape-to-improvement loop

An **escape** is a PR that merged but should have been blocked: a reviewer missed
something. Escapes are inevitable, especially early while reviewers are still being
tuned, and they are the single most valuable signal for improving the system.

Bastion cannot detect escapes itself: if it could, it would have blocked them. This
is a human governance loop:

1. **Notice** an escape (monitoring, a bug report, a production incident).
2. **Triage** it: which reviewer(s) should have caught it, and why did they not?
3. **Improve** the policy: sharpen a prompt, add a new single-concern reviewer for
   the missed property, or fix the reviewer's environment.
4. **Merge** the policy change (through the CODEOWNERS-gated human review above).

This is why Bastion expects reviewers to improve over time. Start with a reviewer
that is good enough and sharpen it from real escapes instead of perfecting it on
paper. Treat escapes as expected feedback rather than failures, and triage them
regularly so the policy keeps improving.

## What Bastion does not guarantee

Govern with these limits in mind; they are deliberate, not gaps to be closed:

- **It is not a correctness proof.** Bastion does not guarantee code is free of
  bugs or vulnerabilities. A reviewer is only as good as its model and prompt;
  it is code review without the human in the small loop, not a verifier.
- **It does not judge whether the right thing is being built.** That is a
  design-time question; by PR time the ship has sailed. Keep humans in the design
  loop.
- **It is not an adversarial security boundary.** Bastion assumes PR authors are
  aligned contributors and treats reviewed code as trusted input; it does not
  defend reviewer agents against prompt injection or exfiltration from the code
  they review. The bar is *reasonable reduction proportionate to effort*: a speed
  bump and good defaults, like lint and CI and human review before it.
  [Attestation](../developer-guide/attestation.md) (`bastion attest`) fits the same
  bar. `bastion attest` signs a local run's verdicts with the author's SSH key,
  and CI verifies that signature against the author's GitHub-registered signing
  keys before replaying anything, so trust is rooted in the forge account, the
  same account repository permissions already trust with merge access. That
  distinguishes an author's enrolled key from one a coding agent minted on the
  spot. It does not defend against a malicious author who deliberately forges a
  seal or signs off on a run they know is wrong: the threat model assumes aligned
  authors throughout, so using signing as an adversarial boundary against a
  malicious author is out of scope, along with an enumerated
  trusted-computing-base and rule storage independent of the forge's own trust.

These limits follow from one assumption: the threat being managed is an
aligned-but-fallible agent, not a determined adversary. Govern accordingly. Bastion
is a control on honest mistakes and drift, layered with the rest of your CI, not a
boundary that holds against someone actively trying to defeat it.

## A governance checklist

For a healthy deployment:

- [ ] `.bastion.yaml` and the Bastion workflow are CODEOWNERS-protected.
- [ ] Bastion's review is required by branch protection on the default branch (the
      review job, or the aggregate `bastion` check that `bastion github report` posts).
- [ ] Reviewer-policy PRs get a real human review, not a rubber stamp.
- [ ] Someone owns escape triage, and escapes feed back into reviewer changes.
- [ ] Billing is configured (per-author secrets or an API-key fallback) so reviews
      are not silently blocked by missing credentials. See
      [Continuous integration](./continuous-integration.md#authentication--billing).

---

That is the guide. If you want to work on Bastion itself rather than use it, the
design notes and contributor docs live in the
[Bastion repository](https://github.com/jssblck/bastion).
