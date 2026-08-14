---
id: LESSON-514
title: "An undo must know what it displaced — a fixed-account store is an overwrite"
component: "cli/keychain"
domain: "privacy"
stack: ["rust", "cli", "keychain"]
concerns: ["security", "reliability"]
tags: ["keychain", "credentials", "undo", "rotation", "overwrite", "cleanup"]
req: REQ-572
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

`/web setup` stored the search key under the fixed account
`teton/web-search` and, on a refused commit, deleted the entry — cleanup
keyed on "did *this run* store?". But `set_generic_password` **overwrites in
place**, so for a user re-running setup over a working configuration
(rotating a key, changing backends), the store displaced the old credential
and the cleanup then destroyed it — while the live config still referenced
it. The observable failure was worse than breakage: `search_auth` resolves
to `None` and subsequent searches **still egress the query text**, just
unauthenticated. Two reviewers independently confirmed it as a Major.

## Lesson

Cleanup that assumes "store = create" destroys rotations. Read the prior
state in the same breath as the store — it is the last moment the answer
exists — and make the undo a three-state decision, not a boolean:

- **Absent** → this run created the entry; the undo is delete.
- **Present(bytes)** → this run displaced a credential; the undo is to put
  those exact bytes back. A delete here destroys a working setup the user
  never agreed to give up.
- **Unreadable** → both undos are unsafe; leave the entry and say so.

And when the failure is *ambiguous* (a transport error where the commit may
have landed), mutate nothing: either undo is destructive in one of the two
states the error is consistent with, so the honest move is one notice naming
the account and both possibilities.

## Why It Matters

Rotation is the security-hygiene path — the flow's second run, not its
first, is where users have something to lose. An undo written against the
first-run mental model turns a routine commit failure into silent credential
destruction, and the blast lands later, as unauthenticated egress that reads
like a bad key. The fix (`PriorKey` + `Keychain::read`) is cheap only at
design time; after the store, the displaced bytes are unrecoverable.

## Applies When

Any flow that writes to a fixed-name slot with overwrite semantics
(keychain accounts, config keys, files) and offers cleanup on failure; any
undo/rollback design — ask "what was here before *this* attempt?" and refuse
to guess when the answer is unreadable or the outcome ambiguous. Related:
[[LESSON-501]] (carried state sheds invariants), REQ-572 ADR-3's recorded
residuals.
