# Local review attestation

> **Implementation status.** This document is a design target. None of it is
> implemented: there is no `bastion attest` command, no notes ref, and no
> verification path in CI. It is written down so the implementation has a spec
> to converge to.

When a project runs Bastion both locally and in CI, a well-behaved agent runs
`bastion review` before pushing, and then CI runs the same reviewers over the
same changeset again. Every reviewer is an agent invocation, so the project pays
for each review roughly twice. Attestation lets CI reuse a local run instead of
repeating it: the local run signs a record of what it reviewed and what it
concluded, and CI verifies that record and replays it rather than re-executing
the reviewers.

An attestation is not a pass token. It is the run itself, signed: per-reviewer
verdicts, findings included, blocks included. CI replaying an attested run that
blocked still blocks. Attestation only ever avoids duplicate execution; it never
changes an outcome.

## Trust posture

This fits inside the existing [threat model](./design.md#threat-model--trust-boundary)
rather than weakening it. Bastion assumes aligned authors and aims for
reasonable reduction proportionate to effort, not an adversarial boundary; an
attestation trusted by CI carries the same trust as "we assume contributors are
not disabling lints." Three rules define what is trusted and why:

1. **Key trust rides on the forge account.** CI verifies the note's signature
   against the SSH signing keys the PR author has registered with GitHub.
   Repository permissions already decide who is trusted to merge, so the forge
   account is the natural trust root; what verification adds is distinguishing
   the author's enrolled key from a key the coding agent minted on the spot,
   which is what makes the presence speed bump below bind.
2. **Presence is the author's key choice.** The coding agent under review runs
   on the same machine as the signing key, so a key the agent can use silently
   lets the agent attest its own work. The speed bump against that is a
   presence-gated key: a hardware token or an OS keychain entry that prompts
   the human per signature. Bastion cannot verify the property (a
   presence-gated key and a plain file key produce indistinguishable
   signatures), so the choice sits with the author: enrolling a plain file key
   means accepting that an agent on that machine could attest without a human
   touch, the same trust as the author's commit access.
3. **Bastion refuses to attest what it cannot verify.** The binary is code the
   agent does not control, so it is the one local component that can hold the
   line. Every run is sealed by the binary the moment it finishes (see
   [the run seal](#the-run-seal)), and `bastion attest` re-derives the
   repository state and checks that seal before signing anything: an edited
   run store, a bundle describing a run that never happened, or a repository
   that moved on since the run all fail locally. CI checks the same seal
   independently, so a rebuilt Bastion that skips the local refusal seals with
   the wrong secret and its bundles fail verification there.

## The attestation bundle

A verdict is a judgment about a changeset under a policy, so the bundle binds to
everything that determines the judgment. CI verifies every binding and falls
back to a full run on any mismatch:

- **The changeset, not the commit.** The merge-base tree and the head tree (with
  a patch-id over the diff). CI recomputes its own merge base against the PR's
  target and replays only on exact match, which catches a local review that
  diffed against a stale base. Binding by content rather than commit id also
  lets an attestation survive a squash or rebase that produces identical trees.
- **The effective reviewer config.** A hash of the repository registry after
  each file's `defaults` are applied. The user-level registry is excluded:
  personal reviewers never gate anyone else's PR, so they cannot attest anything
  either. A local run's user-level reviewer events are simply absent from the
  bundle.
- **Coverage.** The set of repository reviewers the local run routed and
  executed. CI routes its own diff; a reviewer CI routes that the bundle does
  not cover runs fresh. Coverage mismatch degrades, it does not invalidate: the
  attested reviewers replay, the rest execute.
- **The engine.** Implicit in the run seal rather than checked as a field: each
  release embeds its own sealing secret, so a bundle verifies only under the
  same release that produced it, and a new release (meaning new reviewer
  behavior) invalidates old bundles automatically. The bundle still carries the
  version string in plain text so a mismatch reports as "attested by vX, CI
  runs vY" rather than a bare verification failure.
- **The run itself.** Per-reviewer verdicts and findings, and the run events the
  report is rendered from, so the sticky comment and check runs replayed from an
  attestation carry the same detail as a fresh run.

## The run seal

The bindings above are only as good as the record they come from, and the run
store is plain files on the author's machine. What makes that record
tamper-evident is the binary itself, which the agent does not control.

Each release of Bastion embeds a sealing secret generated by the release
workflow and shared by every platform binary of that release; a locally
compiled binary embeds a random per-build secret instead, so a dev build can
seal runs only for itself. When a run finishes, the runner computes a
canonical digest over the head tree, the merge-base tree, the effective
reviewer-config hash, and the run's events and verdicts, seals it with a MAC
keyed by the embedded secret, and persists the seal with the run. A keyed MAC
rather than an asymmetric signature is deliberate: the sealer and the verifier
are the same binary on both ends, so a keypair would ship both halves in the
same artifact and add nothing.

The digest also records whether any of the test seams (the
`BASTION_CLAUDE_BIN`-style backend overrides and the container-engine
override) were active during the run. `bastion attest` refuses to attest a
run that used them: a run against a stubbed reviewer is a real run of the
binary, but not a real review.

The seal is checked twice. `bastion attest` re-derives every input (the
current HEAD tree, the merge base, the config hash), verifies the seal, and
refuses on any mismatch, so tampering fails on the author's machine first. CI
verifies the seal again with its own embedded secret before honoring any
binding. A run of Bastion that succeeds can therefore become an attestation,
but an attestation cannot be manufactured without a run: the seal does not
exist until the runner produces it, and nothing else holds the secret.

One boundary of that claim is worth stating exactly. The secret ships inside a
public binary, so this is tamper evidence, not secrecy: an actor who
deliberately extracts the secret and forges a seal produces bundles CI cannot
distinguish from real ones. That act is the deliberate malice the
[threat model](./design.md#threat-model--trust-boundary) already excludes; the
seal exists to stop the inadvertent version (an agent editing run-store files,
replaying a stale run, or stubbing a reviewer), and those it stops outright.

The seal binds content, never git ceremony. It names the content reviewed, the
base it was diffed against, the policy that ran, the engine that ran it, and
what was concluded. It says nothing about whether that content was committed
at the time, what any commit message said, or whether a commit was signed,
because none of that affects what the reviewers saw: commit messages feed the
intent context, which is untrusted input excluded from gate logic. An author
who amends a message, commits after reviewing, or signs the commit afterward
has invalidated nothing.

## Storage: a git note

The bundle and its detached signature live in a git note under a dedicated ref,
`refs/notes/bastion`, attached to the head commit. Notes attach data to a
commit without changing its hash or its object, so the author's own commit
signature is untouched. The note pushes independently
(`git push origin refs/notes/bastion`); `actions/checkout` does not fetch notes
by default, so the workflow adds one fetch line.

The note is *indexed* by commit but *verified* by the content bindings above.
Notes keyed purely by commit id would die on every squash merge; because CI
checks trees and patch-ids, a rewritten commit with identical content still
matches.

## Signing

Signatures are SSH signatures (`ssh-keygen -Y sign` / `-Y verify`), not GPG:
every contributor already has an SSH key, git itself supports SSH commit
signing, and hardware-token and keychain backends exist for presence gating.

CI fetches the SSH signing keys the PR author has registered with GitHub
(`GET /users/{username}/ssh_signing_keys`, over the same REST seam the adapter
already uses), assembles them into an ephemeral `allowed_signers` input, and
runs `ssh-keygen -Y verify` against it. Enrolling a signing key with GitHub is
something the coding agent cannot do without the user's own GitHub
credentials, so a signature by any other key, including one freshly minted on
the author's machine, fails verification and falls back to a full run.

Registry config is a single switch: `attestations: true` enables the feature
(default off; CI ignores notes entirely without it), and a reviewer can opt out
of being replayed (`attestation: never` on the reviewer) for a gate the team
wants CI-executed unconditionally.

## The `bastion attest` flow

1. `bastion review --base <base>` runs, persists to the run store as today,
   and seals the run as it finishes.
2. `bastion attest` loads that run, verifies the seal, and re-derives the
   repository state: HEAD's tree must still match the tree the review saw, and
   the config hash must still match. It refuses on any mismatch, since the
   note would otherwise assert the reviewers saw content they did not.
3. It builds the bundle (seal included), signs it with the author's SSH key
   (prompting for presence if the key demands it), writes the note on HEAD,
   and prints the push command for the notes ref.

## Verification and replay in CI

`bastion review` in CI (the same binary, no separate verb) fetches the note for
the head commit, verifies the author's signature against the PR author's
GitHub-registered signing keys, verifies the run seal with its own embedded
secret, and checks every binding. Then, per reviewer:
an attested verdict whose inputs match replays; everything else executes. The
merged result flows into the normal report path, so `bastion github report`
posts the same sticky comment and check runs it would for a fresh run, with two
additions for auditability:

- The sticky comment opens with a prominent callout (the same mechanism as the
  skills-drift `[!WARNING]` block): which reviewers were replayed from a signed
  local run, which key attested, and when.
- Each replayed reviewer's check-run summary states that its verdict was
  replayed from an attested local run rather than executed fresh.

The local `bastion review` mirrors the callout to stderr when it replays, as it
does for the drift advisory. This surfacing is the human breadcrumb: anyone
reading the PR can see at a glance that the gate was satisfied by an attested
local run and by whom, and can trace it back to the note on the head commit.

Every failure is fail-closed to a full run, never to a silent pass: a missing
or unverifiable note, a key the author has not registered with GitHub, a seal
that does not verify (tampered, or produced by a different release), a
binding mismatch, or a stale base all mean the reviewers simply execute, and
the report notes why the attestation was not honored so the author is not left
guessing.
