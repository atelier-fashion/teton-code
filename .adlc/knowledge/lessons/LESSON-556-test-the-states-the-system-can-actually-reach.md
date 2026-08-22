---
id: LESSON-556
title: "A test that crosses every value with every other tests states the system cannot reach — and fails on them"
component: "harness/prompt"
domain: "verification"
stack: ["rust"]
concerns: ["test-quality", "correctness"]
tags: ["combinatorial-test", "reachable-states", "environment-line", "byte-ceiling", "req-584", "req-583", "adr-8"]
req: REQ-584
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

REQ-584 BR-7 puts known project names on the environment line for a
**non-project** root, bounded by the byte length of REQ-583's worst-case
*project* row. The shrink has three steps: names that fit, the bare pointer, no
clause at all.

The first test crossed all three non-project `RootKind`s with seven display
lengths up to the 200-character bound, and asserted the pointer always fits. It
failed — on a 200-character `FilesystemRoot`, whose kind phrase ("the filesystem
root") is the longest of the three.

**That state cannot exist.** A filesystem root's display is always `/`. A home
root's is always `~`. Only `Plain` varies. The failure was real arithmetic about
an unreachable combination, and the fix was a case table pairing each kind with
the display it can actually have — after which the property holds and is worth
asserting.

The near-miss is what makes it a lesson: the obvious response to a red
combinatorial test is to *weaken the assertion* ("the pointer usually fits") or
to add a defensive branch for the impossible case. Both would have shipped a
weaker guarantee to buy silence from a test that was wrong.

## Lesson

**Enumerate the states the system can produce, not the cross-product of its
fields.** A cross-product is easy to write and reads as thorough, but a type
whose fields are correlated — an enum whose variant constrains another field —
has far fewer inhabitants than its shape suggests, and the extras are not merely
uninteresting: they generate *false* failures, and a false failure pressures you
into weakening a true claim.

When a combinatorial test goes red, the first question is **"can that input
happen?"** — before "is my code wrong?" and well before "should I relax the
assertion?".

The reachable-state table also documents the correlation, which the type does
not: `RootKind::FilesystemRoot` implies `display == "/"` is a real invariant of
`session_root::probe`, and it was written down nowhere until a test needed it.

## Why It Matters

The wrong resolutions here are both silent. Weakening the assertion loses the
property (the pointer *does* always fit, and that resolves the spec's A-3
worry). Adding a defensive branch for an unreachable root leaves dead code whose
comment claims to handle a case that cannot occur — which the next reader will
trust.

## Applies When

- Writing a table-driven or property test over a type whose fields constrain
  each other (an enum plus its payload, a kind plus a display, a mode plus its
  optional fields).
- A combinatorial test fails on one cell and the rest pass — suspect the cell
  before the code.
- Tempted to add a `_ => { /* cannot happen */ }` arm to satisfy a test rather
  than to satisfy the compiler.
