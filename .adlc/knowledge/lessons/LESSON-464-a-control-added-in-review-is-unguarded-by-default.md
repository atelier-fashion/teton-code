---
id: LESSON-464
title: "A control added during a fix pass is unguarded by default — new guards need their own known-bads in the same pass"
component: "adlc/review"
domain: "process"
stack: ["ci", "bash", "adlc"]
concerns: ["process", "security", "testing"]
tags: ["fix-pass", "known-bad-fixture", "mutation-testing", "self-asserting-controls", "verify-loop"]
req: REQ-551
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

REQ-551's first verify fix pass added three security controls: an early
keychain-destroy step, an env scrub in the signing step, and a `.stage-meta`
integrity manifest. The Step-D re-verify then proved, by mutation against
the real files, that every one of them was silently deletable: removing the
early-destroy step, neutering its `if:`, moving it below the upload, or
deleting the scrub lines all left the 382-case suite green — while three
documents confidently described the narrowed window those controls created.
This repo's own BR for the REQ ("the ordering is mechanically asserted, not
commented") had been applied to the *original* change but not to the
controls the fix pass itself added. The round-3 pass made every control
self-asserting (in-suite known-bad workflow mutants); the suite grew
323→407 and deleting any control now turns CI red.

## Lesson

A fix pass that adds a guard has, by default, added an *unguarded* guard:
nothing fails when the guard is later removed, weakened, or repositioned —
and the docs written alongside it start lying the moment that happens. The
rule that closes the loop: every control added during verify/review lands
with its own known-bad in the same commit — a mutation case proving the
suite goes red when the control is deleted, moved, or its predicate
neutered. "The reviewer checked it exists" is a point-in-time claim;
only a permanent known-bad makes it durable. Budget the verify loop for
this: reviewing the fix pass as new code (LESSON-441) must include asking
"what asserts THIS?" of every line the fix added.

## Why It Matters

Controls added in review are precisely the ones with the least design
scrutiny and the most confident surrounding prose — the fix was written to
close a finding, so everyone's attention is on the finding, not on the
fix's own durability. An unasserted control decays into LESSON-443's
comment-claiming-a-guard within one refactor.

## Applies When

- Any verify/review fix pass that adds a gate, scrub, ordering constraint,
  or cleanup step (ask per added control: which case goes red if this
  vanishes?).
- Writing docs that describe a security posture — each claim should trace
  to a mechanical assertion, or say it is unasserted.
- Designing suites for CI workflows: workflow-mutant known-bads (scratch
  copies, never the tracked file) are cheap and permanent (see
  [[LESSON-454]], [[LESSON-441]], [[LESSON-443]]).
