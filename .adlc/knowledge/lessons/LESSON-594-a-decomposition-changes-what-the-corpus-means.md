---
id: LESSON-594
title: "A decomposition changes what \"the corpus\" means, and every source-scanning check with it"
component: "adlc/verification"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "maintainability"]
tags: ["source-scanning", "derived-checks", "cfg-test", "vacuity", "refactor", "sweep"]
req: REQ-599
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

This daemon derives many of its guarantees by reading its own source: "this
duty has exactly one call site", "this number has one home", "these ids still
annotate these items". REQ-599 split `runtime.rs` into a directory of modules.

**Step 1 was a pure `git mv` with no content change. It broke nine of those
tests**, every one of which had `runtime.rs` hard-coded.

**Step 2 broke eight more, and far more quietly.** A new
`#[cfg(test)] pub(crate) mod testsupport;` was declared at the *top* of
`mod.rs`. Those scanners compute a file's production half as "everything above
the first `#[cfg(test)]`", the crate's convention being that test items come
last. One attribute near the top cut the visible production half from ~10,000
lines to ~100. The symptom was a "one home" assertion reporting **zero**
occurrences of a constant that `grep` finds three of, six lines apart.

## Lesson

A source-scanning check has an implicit **corpus**, and a refactor is precisely
the event that changes it. Two rules:

- **Make the corpus a directory, not a file**, the moment a file might become
  one. A file-scoped scan does not fail when its subject moves — it sees less
  and passes, which is the direction that does not announce itself. One of these
  counted call sites of a function; scoped to the old file it would have fallen
  to zero and read as "this is unreachable", the exact condition it existed to
  detect, manufactured by the scan rather than the code.
- **`#[cfg(test)]` position is load-bearing where "production half" is computed
  by truncation.** A test-gated `mod` declaration belongs at the foot of the
  file with the other test items, not beside the other `mod` lines at the top.

## Why It Matters

Every one of these seventeen failures was a real defect, and none would have
been caught by reading the diff — the diff for step 1 was a rename.

The step-2 case is the more dangerous shape because it does not look like a test
change at all. It looks like tidy module organisation, and it silently blinded
eight independent guarantees at once. Anything that changes where the first
`#[cfg(test)]` falls is a change to what every truncating scanner can see.

## Applies When

Renaming or splitting a file that other tests scan; adding a `#[cfg(test)] mod`
declaration to a file whose production half is computed by truncation; writing a
new source-scanning check (scope it to a directory and give it a vacuity floor);
or triaging a derived check that reports **zero** of something — treat zero as
"the scan may have moved" before treating it as "the code changed".
