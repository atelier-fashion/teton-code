---
id: LESSON-524
title: "Exposure is not callability — a capability asserted present must be asserted usable at every permission level"
component: "tetond/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "security", "reliability"]
tags: ["permissions", "tool-registry", "callability", "plan-level", "teton-docs", "mutation-check", "lesson-496"]
req: REQ-577
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

REQ-577's `teton_docs` tool shipped with a spec row saying "without a
permission prompt," a BR mandating exposure on every profile, and tests
pinning exactly that — `exposed_names(Some(DEGRADED_MAX_TOOLS))` contains the
tool, the system prompt lists it, offline sessions carry it. All green. The
first live session hit `? permission requested: teton_docs` at the default
level and a hard `Deny` at `plan`: nothing had added `DOCS_TOOL_NAME` to
`READ_ONLY_TOOLS`, so the tool fell to the ask-by-default classification. CI
could not see it because every test asserted the tool was *listed*, none that
a call *executes* without stopping the turn. A comment in `permissions.rs`
even asserted the failure was impossible ("a new read-only tool that nobody
classifies merely asks — a degradation, not a hole") — falsified by the
`plan`-level denial of the daemon's own documentation.

## Lesson

Presence and usability are separate claims with separate tests. The fix's
test class is the reusable part: drive the real `PermissionGate` across
`PermissionLevel::ALL` and assert `Allowed` + `pending_count() == 0` +
nothing published on the event bus — then mutation-check it by removing the
classification and watching it fail. One trap inside that trap: an `ask`
policy's `authorize` awaits a client answer forever, so the naive mutation
check *hangs* instead of failing — the committed test wraps the call in a
timeout whose panic message names the cause. LESSON-496 is the sibling
(a cap silently withholding an exposed tool); this is the permission-layer
recurrence of the same split, and REQ-577 hit both layers in one feature.

## Why It Matters

The exposure/callability split is a class that recurs at every gate a
capability passes through — registration, cap, permission level, consent.
Each layer green-lights its own claim while the composed path stays untested,
and the failure is only visible live, at the moment a user (or the product's
own front door) needs the capability. Worse, `plan` deny is fail-closed: the
level chosen *because* it is read-only denied the one tool that is read-only
by construction.

## Applies When

Adding any tool, RPC, or capability behind layered gates — write one test per
layer plus one through the composed path; asserting "no prompt" or "allowed"
anywhere — mutation-check the classification, with a timeout if the
unclassified path blocks; reading a comment that says a failure mode is
impossible — that sentence is a test waiting to be written (or falsified).

## Related

- [[LESSON-496]] — the cap-layer sibling (silent withholding of an exposed tool).
- [[LESSON-518]] / [[LESSON-519]] — the prove-it-at-the-real-seam family this extends.
