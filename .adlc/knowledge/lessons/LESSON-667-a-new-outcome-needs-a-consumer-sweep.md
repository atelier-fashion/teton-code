---
id: LESSON-667
title: "Making an outcome reachable needs a sweep of every catch-all that folds it"
component: "daemon/runtime/child_turn"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "testing"]
tags: ["max-tokens", "stop-reason", "catch-all", "exhaustive-match", "mutation-testing", "subagent", "silent-success"]
req: BUG-235
created: 2026-10-07
updated: 2026-10-07
---

## What Happened

BUG-229 made `StopReason::MaxTokens` reachable for the first time, and it fixed
the place where the stop was produced. REQ-623's child dispatch had already
merged a match on the same enum:

```rust
StopReason::MaxTurnRequests => /* turns_exhausted */,
StopReason::Cancelled => Ended::of(ChildStatus::Cancelled),
_ => finished_on_its_own(..),   // → Completed
```

When REQ-623 was written, `_` meant "ended normally", because nothing produced
`MaxTokens`. Once BUG-229 landed, the same `_` also caught a cut-off reply and
reported it as `completed`. The compiler, clippy, and all 4,889 existing tests
had nothing to object to. LESSON-666 (BUG-229) caught the producer side ("grep
for the producer of every outcome variant") and predicted this follow-up. This
lesson is the consumer side of the same seam.

Separately, the first mutation run for the fix's test stayed green. The
mutation added `MaxTokens` to the later `EndTurn | Refusal` arm but left the
earlier `MaxTokens` arm in place, so the mutated pattern could never match and
the fix still ran. Only deleting the arm reproduced the pre-fix behaviour, and
that turned 1 of 4,890 tests red.

## Lesson

1. **When a change makes an enum variant reachable, sweep every `match` on that
   enum that has a `_` arm, in every crate.** Producing a variant for the first
   time changes the meaning of every catch-all written while it was dead. The
   search is `grep -rn "match .*stop_reason"` (or the enum's name) followed by
   reading each `_ =>`. Put the sweep in the producer's fix, not in a later
   bug.
2. **Fold a closed outcome enum exhaustively.** List the "ends normally" arms
   by name (`EndTurn | Refusal`) instead of `_`. The next variant then fails to
   compile at every consumer, which turns this sweep into a compiler error.
3. **A mutation must take the fixed path away, not add a pattern beside it.**
   Rust matches arms top-down. Adding the variant to a later arm, while an
   earlier arm still matches it, is unreachable code: the mutation stays green
   and proves nothing. Delete or reorder the arm, and check that the mutated
   build actually behaves like the old code. `#[allow(unreachable_patterns)]`,
   or a warning you silenced to make the mutation compile, means it is a dud.

## Why It Matters

The failure looks like success. A parent model told `completed` with an
empty report concludes "the child found nothing" and reasons on from there.
No log, transcript line, or event marks anything as wrong. Each consumer layer
that still has a catch-all can reintroduce the same silent success.

## Applies When

- A fix or feature starts constructing an enum variant that nothing produced
  before (`StopReason`, a status, an error class).
- Reviewing a `match` over a protocol enum that has a `_` arm.
- Recording a mutation for a test that guards a `match` arm.
