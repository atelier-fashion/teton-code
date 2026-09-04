---
id: LESSON-648
title: "When one side renames a list entry and the other deletes one, neither side is the answer"
component: "adlc/rebase"
domain: "adlc"
stack: ["git", "rust"]
concerns: ["reliability", "process"]
tags: ["merge-conflict", "semantic-merge", "allowlist", "rebase", "union", "sprint"]
req: REQ-614
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

Two conflicts in REQ-614's rebase were lists, and both looked like the kind of
thing a union resolves.

`runtime_visibility.rs`'s `CRATE_WIDE` allowlist was not. REQ-616 had
**renamed** `LOCAL_ENGINE_N_CTX` to `LOCAL_ENGINE_N_CTX_DEFAULT`; REQ-614 had
**deleted** `TAINT_BY_CONTEXT`, folding three constants into one vocabulary.
Taking either side whole was wrong, and so was keeping both: the union
re-introduces a constant that no longer exists, in a test whose entire job is
to assert that the crate-visible surface is exactly this list. The correct
result — the new name, without the deleted entry — appears verbatim on neither
side.

The `session_ui.rs` `use` list was a true union, but only when computed rather
than read. REQ-614's side was the older, shorter list; taking it would have
silently dropped five symbols main had gained (`ContextCompacted`,
`ShellDutySkipped`, `SkillRefusedNoRoom`, `ToolCallRepeated`,
`TurnRefusedAnchorsExceedBudget`). Diffing the two sides programmatically and
asserting the main-only members survived is what made that visible; by eye,
across twenty-odd alphabetised identifiers, it is not.

## Lesson

Before resolving a conflicted list, ask what each side *did* to it, not what
each side *shows*. Addition against addition is a union. Rename against
deletion is neither side and neither concatenation — resolve it against the
live tree: grep the symbol, confirm the survivor's spelling, and let the tree
decide, because the tree is what the guard is about.

And compute list unions mechanically. Print the members only one side has,
both directions, and check that set against what you intended to keep. A
dropped identifier in an import list is a compile error and therefore cheap; a
dropped entry in an *allowlist* is a silently widened surface and is not.

## Why It Matters

An allowlist resolved by keep-both fails loudly here, which is the good case.
Resolved the other way — a member the union quietly drops — the test passes
while asserting less than it did, and the narrowing is invisible in a diff
that reviewers read as a merge artifact. That is the same class as a test
harness that stops being enumerated: it does not fail, it ceases to check.

## Applies When

Resolving a conflicted list, table, allowlist, match arm set, or import block
where the two sides made *different kinds* of edit — particularly after a
rename, an extraction, or a constant fold landed on one side.
