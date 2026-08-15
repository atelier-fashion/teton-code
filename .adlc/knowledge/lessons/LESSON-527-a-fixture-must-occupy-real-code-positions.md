---
id: LESSON-527
title: "A regression fixture must occupy the position real code occupies — or it passes against the bug it targets"
component: "tests/boundary_coverage"
domain: "testing"
stack: ["rust"]
concerns: ["coverage-accuracy", "false-negatives", "test-fixtures"]
tags: ["mutation-check", "fixture-position", "anchored-matching", "fail-safe-direction", "lesson-441"]
req: BUG-172
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

BUG-172's fix re-anchored `production_half` from `"\n#[cfg(test)]\n"` to
`"\n#[cfg(test)]\nmod "` so a `cfg(test)` *item* above an `impl Tool` block
could no longer truncate the boundary-coverage scan and silently shrink the
tool universe. The first regression fixture put its `#[cfg(test)] const` at
**byte zero** of the fixture string — a position where a newline-anchored
marker can never match — so when the fix was mutation-checked by reverting the
marker, the test **passed against the exact defect it was written to catch**.
Only the mutation check exposed it; the fixture was repaired by opening with a
module-doc line, the position every real file's items actually occupy, with a
comment naming that line load-bearing.

## Lesson

Two duties when writing a regression test for position-sensitive matching:

1. **Place the fixture's trigger where real code puts it**, not where it is
   easiest to write. Anchored patterns (`\n`-prefixed markers, line-start
   matches, indentation-delimited scans) make byte-zero and other boundary
   placements *unmatchable*, which turns the fixture into a no-op.
2. **Mutation-check the test against the original defect before trusting it**
   (LESSON-441's practice, extended to the test itself): revert the fix,
   confirm the new test *fails*, restore. A regression test that has never
   been seen failing proves nothing.

Relatedly: a "fail-safe" claim about a matcher has two failure directions —
the marker going *missing* (scan widens, loud) and the marker matching
*early* (scan shrinks, silent). Argue both before writing the claim into a
doc comment.

## Why It Matters

A regression test that cannot fail is worse than none: it converts "we fixed
it" into a permanent, checkable-looking lie, and the next person who breaks
the invariant gets a green suite. Here the cost of the mutation check was one
rebuild; the cost of skipping it would have been a guard that guards nothing.

## Applies When

Writing regression tests for any string- or position-anchored extraction
(scanners, truncators, marker-delimited parsing); asserting a fail-safe
direction in documentation; and any time a new test's first run is green —
ask whether it has ever been red.
