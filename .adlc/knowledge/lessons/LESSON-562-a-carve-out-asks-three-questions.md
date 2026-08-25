---
id: LESSON-562
title: "A feature carve-out asks three questions; symbol search answers only the first"
component: "daemon/session"
domain: "harness"
stack: ["rust", "git"]
concerns: ["maintainability", "reliability"]
tags: ["carve-out", "feature-split", "git-log-s", "entanglement"]
req: REQ-591
created: 2026-08-25
updated: 2026-08-25
---

## What Happened

REQ-591's trust gate was carved out of REQ-589 — five commits at positions 4, 5, 8, 11 and 22 of
33, interleaved with the feature they had to be separated from.

`git log -S` over every trust symbol across all 33 commits established that no offer commit
carried trust code. That check was correct and it was not enough. Two further entanglements
appeared, each invisible to symbol search:

**The trust commits compiled against offer code.** One constructed a `Question` enum variant and
read an accessor that the *offer* had introduced, and its tests were written through the offer's
test double. Nothing named a trust symbol; the coupling was in `use` statements and match arms.

**Kept tests depended on the dropped feature's runtime behaviour.** The first full suite run
after the rebase was 3,834 pass / 3 fail, and all three failures were *offer* tests added by
*kept* commits. They named the gate in prose, in a string literal, and in fixture builder names
(`declining_trust()`, `Trust::Decline`) — never as a symbol. Only running the suite found them.

The same form reappeared **in reverse** on the merge back: a fixture client that panicked on any
non-offer prompt was provably unreachable on a trust-free branch and reached by every
project-sourced leg once the two halves were recombined.

## Lesson

Ask all three, in order, and treat only the first as automatable:

1. Does A **carry** B's code? — `git log -S` over every symbol.
2. Does A **compile against** B's code? — read the `use` statements, signatures and match arms.
3. Do A's **tests depend on B's runtime behaviour**? — only a full suite run finds this.

Prefer a rebase that drops commits over `git revert`: a revert leaves both the change and its
undo in history, so the branch still *contains* the work a reader is being told is absent.

Design one decisive test for the split. Here it was a piped invocation passing on the rebuilt
branch **byte-identical to the original file** — it can only do that if the gate is genuinely
absent rather than merely disabled.

## Why It Matters

A carve-out whose entanglement check stops at question 1 produces a branch that does not compile,
or one that compiles and fails three tests for reasons nobody predicted, at the exact moment the
old branch is being force-pushed away. Record the pre-rewrite SHA as a tag before any of it.

## Applies When

- Splitting one branch into two, or extracting a feature that was built as a rider on another.
- Any force-push of a shared branch — the reversibility work belongs before the split, not after.
