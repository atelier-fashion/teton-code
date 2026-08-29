---
id: LESSON-584
title: "A broader guard placed in front of a narrower one makes the narrower one's mutation test unfalsifiable"
component: "daemon/egress"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["security", "testing"]
tags: ["mutation-testing", "defense-in-depth", "allowlist", "acceptance-criteria"]
req: REQ-596
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-596 added two guards over the `shell` child's environment: a positive
allowlist (BR-2), and unconditional removal of every variable a configured
`auth_ref = "env:<VAR>"` names (BR-1). AC-5 required a mutation test — deleting
the BR-1 removal step must make AC-1 and AC-2 fail.

It does not, and cannot. AC-1's fixture credential is named `DEEPSEEK_AUTH`,
which is not on the allowlist, so BR-2 already withheld it. Two guards stood
between the credential and the child; removing one changed nothing observable.
The mutation test would have passed with the guard deleted — the exact thing
LESSON-550 says is not evidence a guard works.

The fix was to re-site the mutation where BR-1 is the *only* guard: an
**allowlisted** name that the config declares a credential. There, deleting the
removal step fails 3 assertions. The test now says explicitly that the two
named-credential assertions stay green, rather than implying a coverage it does
not have.

## Lesson

When a REQ adds a broader guard *in front of* an existing narrower one, every
mutation test for the narrower guard has to be re-sited to the region where it is
the only thing standing. Otherwise the test silently converts into an assertion
about the *other* guard, and keeps passing while the thing it names is deleted.

Write the acceptance criterion against the overlap, not against the happy path:
"deleting guard X fails test T" is only meaningful when T's fixture lies outside
everything except X.

## Why It Matters

This failure mode is invisible. The suite is green, the AC is checked off, and
the mutation "was run" — it just could not have failed. A later refactor deletes
the narrower guard, every test still passes, and the credential ships. Layered
defenses are exactly where this happens, and they are exactly where the stakes
are highest.

## Applies When

Adding defense-in-depth; writing or reviewing a mutation/negative test for one
layer of a multi-layer guard; inheriting an acceptance criterion written before a
broader guard existed.
