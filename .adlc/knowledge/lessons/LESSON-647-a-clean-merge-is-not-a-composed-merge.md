---
id: LESSON-647
title: "A conflict-free merge is not a composed one — the gaps that cost most raised no markers"
component: "adlc/rebase"
domain: "adlc"
stack: ["git", "rust"]
concerns: ["reliability", "process"]
tags: ["rebase", "semantic-merge", "parallel-sessions", "sprint", "integration", "exhaustive-match"]
req: REQ-614
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-614 rebased onto a `main` that had gained REQ-615, 616, 617 and 618 while
it was blocked. Six files conflicted and were composed by hand. Then three
further defects appeared in files git had merged **cleanly, with no markers at
all**:

* REQ-617 added a protocol command roster the model reads, with a guard
  asserting it equals the CLI dispatch table in both directions. REQ-614 added
  `/shell allow` to that table months of commits earlier than the roster
  existed. Neither side conflicted; the guard went red.
* REQ-618 added `ProvenanceClass::of`, an exhaustive match over
  `ToolProvenance`. REQ-614 added a `BoundaryTouch` variant. Two files, no
  overlap, no conflict — and the crate stopped compiling.
* REQ-599's module map has a 10%-drift guard on production line counts. No
  single REQ moved `runtime/mod.rs` past the band; four together did.

Each is a change to one side interacting with a change to the *other* side's
different file. A conflict is textual overlap, and none of these overlapped.

## Lesson

Conflict markers measure where two diffs touched the same lines. They do not
measure where two diffs touched the same *invariant*. A rebase is finished
when the tree builds and the suite passes on the rebased tip — never when git
stops printing markers.

Two shapes are worth looking for by name, because both are silent at merge
time and loud only at build time: a **new enum variant meeting a new
exhaustive match**, and a **new registry entry meeting a new registry guard**.
Both are the type system and the test suite doing their job — provided
somebody runs them on the composed tree rather than on either parent.

## Why It Matters

`git rebase` exiting 0 reads like success and is routinely reported as one. On
this REQ it left a tree that did not compile. A runner that pushed on that
signal would have burned a CI cycle at best; on a repo whose guard was a test
rather than a compile error, it would have shipped a command the model can
invoke and the roster never announces — precisely the defect REQ-617 exists to
close, reintroduced by the merge that was supposed to preserve it.

## Applies When

Rebasing or merging any branch that sat behind while siblings landed —
especially in a `/sprint` batch, and especially in a language with exhaustive
matching or a repo with registry/parity/drift guards.
