---
id: LESSON-552
title: "A test that hands the minter its input never exercises the derivation that got it wrong"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["security", "privacy", "reliability"]
tags: ["permissions", "consent", "grant-key", "digest", "skills", "dynamic-context", "vacuity", "fixture"]
req: REQ-587
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

REQ-587 BR-5 gives a remembered dynamic-context grant a digest of the
**substituted** command set, so one set of arguments cannot be answered by an
approval given for another. The minter, `skill_grant_key` in
`harness/permissions.rs`, was well tested — including that a digest-keyed grant
does not answer a different command set.

The predicate that decides *which* key to mint was not tested at all.
`skills/expand.rs`'s `commands_interpolate` asked whether any command in the
**unsubstituted** body named `$ARGUMENTS`/`$N`, while `expand` scans the
**substituted** body for commands. `substitute` splices the caller's bytes
verbatim *before* the scan, so an argument carrying `` !`cmd` `` adds a command
the file never declared — and the predicate still answered `None`, minting the
plain key.

The exploit, with the model as caller: invocation one prompts, the user allows
for the session; invocation two smuggles a command into `args`, the key is
unchanged, the grant settles it, **no prompt is drawn**, and the injected
command runs. `authorize_skill`'s `debug_assert!` does not fire, because it
validates the key against `mint(None)` for the grown list — and `mint(None)` *is*
the plain key.

Every existing test passed the interpolation verdict in **as a literal
parameter** (`skill_consent_matrix.rs`), so the derivation was never run.

## Lesson

When a value is *computed* and then *consumed*, a test that supplies the value
tests the consumer only. Ask which of the two the requirement is actually about:
BR-5's claim is "two argument sets do not share an answer", and that is a claim
about the derivation.

Drive it end to end — expand a real body with real arguments and observe the key
that comes out — and assert the property, not the plumbing: two invocations of
one skill with different arguments must not share a grant.

## Why It Matters

This is the shape a green suite cannot see, and it is LESSON-544's shape one REQ
later: a hand-supplied wire value leaves its producer unguarded. Here the
unguarded producer sat between a consent prompt and a shell command.

## Applies When

Testing anything keyed, hashed, digested, or derived before use — permission
keys, cache keys, provenance ids, signatures; any test that passes an enum or
flag as a literal where production computes it; and any requirement phrased as
"X and Y must not share Z".
