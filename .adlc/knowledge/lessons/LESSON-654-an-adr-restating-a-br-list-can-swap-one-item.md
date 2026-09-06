---
id: LESSON-654
title: "An ADR that restates a BR's enumerated list can silently swap one item, and the tests follow the ADR"
component: "adlc/spec"
domain: "testing"
stack: ["rust"]
concerns: ["reliability"]
tags: ["traceability", "adr", "business-rules", "seams", "verification-table", "req-619"]
req: REQ-619
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-619 BR-5 named the seams a new provenance bit must survive: the
dropped-block **absorb**, the context-provenance union, and replay.
ADR-619-2 restated the same idea as "three seams, three tests: the **seed**,
the union and replay". The implementer wrote the three tests the ADR named.
The absorb seam got its `boundary_touch` arm and no test; deleting that arm
left the whole workspace green. The consequence would have been a dropped
boundary-touched expansion shedding its permanence — liftable by
`/shell allow`. The reflector caught it by reading the BR and the ADR side by
side.

## Lesson

When a BR enumerates enforcement points, the ADR quotes the BR's list or
says explicitly what it changed and why, and the task's verification table
keys on the **BR's** list. A restated list is a second source of truth, and
the tests will follow whichever one the implementer read last.

## Why It Matters

Multi-seam invariants are the ones this codebase has been burned by before
(LESSON-501, LESSON-502): a value carried past its creator's scope sheds an
invariant at whichever seam nobody tested. A list that quietly loses an item
is the cheapest way to leave one seam untested while every named test passes.

## Applies When

- Writing an ADR against a BR that lists seams, surfaces, call sites or
  enforcement points.
- Reviewing a verification table: check its rows against the BR's own words,
  not the architecture doc's paraphrase.
