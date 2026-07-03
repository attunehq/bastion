# Local review attestation

> **Implemented.** The run seal is [`src/seal.rs`](../../src/seal.rs), sealed by
> the runner at persist time. `bastion attest` and the CI verify-and-replay
> planner live under [`src/attest/`](../../src/attest/): `mod.rs` is the attest
> flow, `bundle.rs` the bundle and note envelope, `sign.rs` SSH signing, and
> `replay.rs` the CI planner and note lookup. This document describes the
> shipped design and is the reference to read alongside the code.

When a project runs Bastion both locally and in CI, an agent can run `bastion
review` before pushing; CI then runs the same reviewers over the same
changeset again. Every reviewer is an agent invocation, so the project pays
for each review roughly twice. Attestation lets CI reuse the local run: it
signs what it reviewed and concluded, and CI verifies and replays that
record.

An attestation is not a pass token. It is a signed record of the run's
per-reviewer verdicts, including findings and blocks. CI replaying an attested
run that blocked still blocks. Attestation only ever avoids duplicate
execution; it never changes an outcome.

## Trust posture

Attestation uses the existing [threat model](./design.md#threat-model--trust-boundary).
Bastion assumes aligned authors and aims for reasonable reduction proportionate
to effort; CI trusting a signed local run carries the same trust as "we assume
contributors are not disabling lints." Three rules define what is trusted and why:

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
   agent does not control, so it checks the run before signing. The runner
   seals an eligible run on a best-effort basis the moment it finishes (see
   [the run seal](#the-run-seal)): a zero-match run and a run whose bindings
   could not be derived stay unsealed, and an unsealed run cannot be attested.
   `bastion attest` re-derives the repository state and checks the seal before
   signing anything: an edited run store, a bundle describing a run that never
   happened, a repository that moved on since the run, or a run sealed against
   a dirty working tree all fail locally. CI checks the same seal
   independently, so a rebuilt Bastion that skips the local refusal seals with
   the wrong secret and its bundles fail verification there.

## The attestation bundle

A verdict is a judgment about a changeset under a policy, so the bundle binds
to the committed HEAD tree, the merge-base tree, the `base..HEAD` patch-id,
the effective config hash, and the resolved reviewer events, alongside the
seam and dirty flags. CI verifies every binding and falls back to a full run
on any mismatch. The dirty flag carries its own rule: a review that included
uncommitted or untracked work is sealed dirty, and `bastion attest` refuses to
attest it, so only a review over committed content ever reaches CI as an
attestation.

- **The changeset, not the commit.** The merge-base tree and the head tree (with
  a patch-id over the diff). CI recomputes its own merge base against the PR's
  target and replays only on exact match, which catches a local review that
  diffed against a stale base. Binding by content rather than commit id means a
  note that CI *finds* verifies against identical trees regardless of the
  commit id it hangs off (a re-attached note, or a rewrite that happens to keep
  the commit id). The note itself is still looked up by commit id, on HEAD or
  the PR's head SHA (see [Storage](#storage-a-git-note)), so a squash or rebase
  that changes the commit id leaves the note behind on the old, now-orphaned
  commit; CI does not find it there and falls back to a full run. Re-running
  `bastion attest` after a rewrite re-attaches the note to the new HEAD.
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
- **The run itself.** Per-reviewer verdicts and findings, carried as the sealed
  reviewers' `reviewer.resolved` events. The bundle does not carry the rest of
  the run's event stream; CI reconstructs it around the replayed events, so the
  sticky comment and check runs replayed from an attestation carry the same
  detail as a fresh run.

## The run seal

The run store is plain files on the author's machine. The binary makes that
record tamper-evident.

Each release of Bastion embeds a sealing secret generated by the release
workflow and shared by every platform binary of that release; a locally
compiled binary embeds a random per-build secret instead, so a dev build can
seal runs only for itself. When an eligible run finishes, the runner computes
a canonical digest over the committed HEAD tree, the merge-base tree, the
`base..HEAD` patch-id, the effective reviewer-config hash, and the run's
events and verdicts, seals it with a MAC keyed by the embedded secret, and
persists the seal with the run. A keyed MAC rather than an asymmetric
signature is deliberate: the sealer and the verifier are the same binary on
both ends, so a keypair would ship both halves in the same artifact and add
nothing.

The digest also records two flags: whether any of the test seams (the
`BASTION_CLAUDE_BIN`-style backend overrides and the container-engine
override) were active during the run, and whether the working tree was dirty
(uncommitted tracked changes or untracked files) at review time. `bastion
attest` refuses to attest a run that used a test seam or that sealed dirty,
and CI's own planner refuses to replay a bundle whose seal carries either
flag: a run against a stubbed reviewer is a real run of the binary but not a
real review, and a dirty review binds to content that was never committed.

The seal is checked twice. `bastion attest` re-derives every input (the
current HEAD tree, the merge base, the patch-id, the config hash), verifies
the seal, and refuses on any mismatch, so tampering fails on the author's
machine first. CI verifies the seal again with its own embedded secret before
honoring any binding. Only the runner holds the secret and produces seals. A
successful run can be attested; a bundle without that run has no seal to
verify.

One boundary of that claim is worth stating exactly. The secret ships inside a
public binary, so this is tamper evidence, not secrecy: an actor who
deliberately extracts the secret and forges a seal produces bundles CI cannot
distinguish from real ones. That act is the deliberate malice the
[threat model](./design.md#threat-model--trust-boundary) already excludes; the
seal exists to stop the inadvertent version (an agent editing run-store files,
replaying a stale run, or stubbing a reviewer), and those it stops outright.

The seal binds the reviewed content, not commit metadata. It names the content
reviewed, the base it was diffed against, the policy that ran, the engine that
ran it, and what was concluded. The seal also records whether the working tree
was dirty at review time, and `bastion attest` refuses a run sealed dirty. So
an attestable review has to run over committed content: review, commit
nothing further, then attest.

The seal says nothing about what any commit message said or whether a commit
was signed, because none of that affects what the reviewers saw: commit
messages feed the intent context, which is untrusted input excluded from gate
logic. An author who amends a message or signs the commit afterward has
invalidated nothing, since the tree is unchanged.

## Storage: a git note

The bundle and its signature live in a git note under a dedicated ref,
`refs/notes/bastion`, attached to the head commit. Notes attach data to a
commit without changing its hash or its object, so the author's own commit
signature is untouched. The note's text is the envelope: the bundle's compact
JSON, a newline, then the armored SSH signature block verbatim. The note
pushes independently (`git push origin refs/notes/bastion`); `actions/checkout`
does not fetch notes by default, so the workflow adds one fetch line.

The note is *indexed* by commit but *verified* by the content bindings above.
The content bindings verify whatever note CI finds, regardless of the commit
id it hangs off, but the lookup itself is still by commit: CI looks for the
note on HEAD, then the PR's head SHA (see
[Verification and replay in CI](#verification-and-replay-in-ci)). A squash or
rebase that changes the commit id leaves the note behind on the old, now
orphaned commit; CI does not find it there and falls back to a full run.
Re-running `bastion attest` after the rewrite re-attaches the note to the new
HEAD.

## Signing

Signatures are SSH signatures (`ssh-keygen -Y sign` / `-Y verify`), not GPG:
every contributor already has an SSH key, git itself supports SSH commit
signing, and hardware-token and keychain backends exist for presence gating.
Signing and verification both scope to the `bastion` namespace
(`ssh-keygen -Y sign/verify -n bastion`), so a bundle signature cannot be
replayed as, say, a git commit signature by the same key.

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

An empty diff between the merge base and HEAD (a no-op changeset) binds to the
literal patch-id string `"none"` rather than an empty value, so an empty-diff
run and a run whose patch-id genuinely failed to compute are never confused.

## The run store: `seal.json`

The seal persists alongside a run's other files, at `runs/<id>/seal.json`
under the data directory (see [Local surface](./local-surface.md#the-data-directory)).
`bastion attest` reads it to build a bundle. A run has no `seal.json`, and so
cannot be attested, when it was a zero-match run, when its bindings could not
be derived, when it resolved no repository-reviewer event, or when it predates
sealing. Sealing failure never fails the review itself, so the run is
otherwise complete, just unattestable.

## The `bastion attest` flow

1. `bastion review --base <base>` runs, persists to the run store, and seals
   an eligible run as it finishes.
2. `bastion attest [<run>] [--key <path>]` loads that run (the latest by
   default), verifies the seal, and refuses outright if the seal recorded a
   test seam or `dirty: true`. It then re-derives the repository state: HEAD's
   tree, the merge base's tree, the diff's patch-id, and the config hash must
   all still match what the seal recorded. It refuses on any mismatch, since
   the note would otherwise assert the reviewers saw content they did not; the
   dirty refusal is what makes that guarantee hold even for a review that ran
   clean over an already-committed tree but a dirty one at some other point in
   the same working session.
3. It resolves the signing key (`--key`, else `git config user.signingkey`,
   else a refusal naming both options), builds the bundle (the seal, the
   sealed reviewers' `reviewer.resolved` events, the version, and the signer's
   public key), signs it with the author's SSH key (prompting for presence if
   the key demands it), writes the note on HEAD, and prints the resolved
   public key and the push command for the notes ref
   (`git push origin refs/notes/bastion`).

## Verification and replay in CI

`bastion review` in CI attempts a replay only when the repository config sets
`attestations: true` and the run carries a GitHub source (`--repo`/`--pr`); a
purely local review never attempts it. It looks up the note on HEAD first,
falling back to the PR's head SHA when HEAD carries none (CI's checkout can be
a merge commit, so the note the author actually attested may hang off the
PR's own head commit instead). Given a note, it verifies the author's
signature against the PR author's GitHub-registered signing keys
(`GET /users/{username}/ssh_signing_keys`), verifies the run seal with its own
embedded secret, and checks every binding (head tree, merge-base tree,
patch-id, config hash) against its own re-derived values. Then, per routed
reviewer: one covered by the bundle and not opted out (`attestation: never`)
replays; everything else, including a reviewer the bundle does not cover,
executes fresh. Coverage mismatch degrades rather than invalidating the whole
plan.

The merged result flows into the normal report path, so `bastion github report`
posts the same sticky comment and check runs it would for a fresh run, with two
additions for auditability:

- The sticky comment opens with a prominent `[!NOTE]` callout (the same
  mechanism as the skills-drift `[!WARNING]` block): which reviewers were
  replayed from a signed local run, which key attested, and when.
- Each replayed reviewer's check-run summary adds a line stating its verdict
  was replayed from an attested local run rather than executed fresh.

The local `bastion review` mirrors the callout to stderr when it replays, as it
does for the drift advisory. Anyone reading the PR can see that the gate was
satisfied by an attested local run, who attested it, and which note on the head
commit backs it.

Every failure is fail-closed to a full run, never to a silent pass: a missing
or unverifiable note, a key the author has not registered with GitHub, a seal
that does not verify (tampered, produced by a different release, or carrying
an active test seam), a binding mismatch, or a stale base all mean the
reviewers simply execute. The run records why as a `run.attestation-fallback`
event, and the sticky comment surfaces the same reason as a line under the
headline. Replay itself is recorded as a single `run.attested` event covering
every replayed reviewer, and each replayed reviewer's own `reviewer.resolved`
event carries `replayed: true`.

### The adopter's two workflow requirements

CI's checkout must be the PR head commit, not the default merge commit:
attestation binds to the head tree the author attested, and a merge commit's
tree never matches it. `actions/checkout` also does not fetch notes by
default, so the workflow needs an explicit fetch of `refs/notes/bastion`,
tolerant of the ref being absent (most PRs will not carry a note). Both are
one-line additions to an existing `bastion` workflow; see
[the GitHub adapter](./github-adapter.md#verification-and-replay) for the
concrete steps.
