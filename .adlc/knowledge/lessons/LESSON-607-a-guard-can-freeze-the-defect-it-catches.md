---
id: LESSON-607
title: "A guard built from a baseline freezes whatever defect the baseline already contained"
component: "adlc/verification"
domain: "refactoring"
stack: ["rust"]
concerns: ["maintainability", "correctness"]
tags: ["traceability", "baseline", "doc-attachment", "guard-design", "decomposition", "req-596-597-hazard"]
req: REQ-603
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-596/597 established a hazard: an item inserted between a doc comment and the
item that comment explains leaves the file's id set and count identical while
moving the rationale onto the wrong item. `traceability_sweep.rs` arm 2 was
built to catch it, and states the rule as *"if an id annotated item X at the
base and X still exists, the id must still annotate X."*

REQ-603 went to move `store_session_skills` into a new `runtime/session.rs` and
found that the 18-line block describing it — *"Derive `session_id`'s skill
registry … the one derivation"* — sits above `projects()`, with `projects()`'s
own one-line doc appended as the block's last line. `projects()` wears another
function's rationale; `store_session_skills` has no doc at all. That is the
REQ-596/597 hazard, present in the tree.

Correcting it turned arm 2 **red**:

```
AC-3    left `projects` (still on 146 other item(s))
ADR-1   left `projects` (still on 161 other item(s))
BR-1    left `projects` (still on 274 other item(s))
REQ-585 left `projects` (still on 178 other item(s))
```

`git show 17c39ec:crates/tetond/src/runtime.rs` explains it: the wedge is
already present at the sweep's own baseline commit. The baseline therefore
records those four ids as annotating `projects`, and the only available
correction reads to the guard as rationale moving *off* the item it explains.

The guard cannot see this defect, because the defect is older than the guard —
and it actively resists the fix.

## Lesson

**A baseline-anchored guard asserts "unchanged since the baseline", not
"correct". Those diverge for any defect the baseline already contained, and for
those the guard inverts: it turns the fix red and keeps the defect green.**

Two consequences worth acting on:

1. **When adopting a baseline, audit it once for the class you are about to
   pin.** A one-time sweep for the hazard at the baseline commit is cheap
   compared with discovering it from the wrong side, mid-refactor, a year later.
2. **When a guard blocks a fix, check the baseline before rationalizing the
   defect.** The tempting reading — "the guard says this attachment is correct,
   so leave it" — launders a defect into an invariant. `git show <BASE>:<path>`
   settles it in one command.

The correct disposition is a scoped, documented exemption carrying the evidence
— not moving BASE (which discards every other assertion) and not silently
reverting the fix. Loosening the guard is itself a ratchet change and belongs in
a commit where it can be reviewed as one, which is why REQ-603 left
`store_session_skills` behind and filed the untangling separately.

## Why It Matters

The cost is asymmetric and compounds. A frozen defect is invisible from the only
direction anyone looks — the guard is green — and becomes visible only when
someone tries to fix it, at which point the guard argues for the defect with the
full authority of a passing test. REQ-603 spent a phase discovering this and
shipped five of six identified items rather than six.

Worse, the failure is self-concealing in a specific way: the guard's message
("rationale moved off the item it explains") describes the *fixer* as the
offender. Without the baseline check, the natural conclusion is that the fix is
wrong.

## Applies When

- Adopting or reviewing any check anchored to a fixed baseline commit —
  traceability sweeps, snapshot/golden comparisons, API-surface ratchets,
  suppression counts.
- A guard goes red on a change you believe is a correction rather than a
  regression. Check what the baseline actually contains before concluding the
  change is wrong.
- Extracting a module out of a large file, where doc-to-item attachment must
  survive the move (see also LESSON-594: take the plain-`//` comment run, not
  just the `///` block).
