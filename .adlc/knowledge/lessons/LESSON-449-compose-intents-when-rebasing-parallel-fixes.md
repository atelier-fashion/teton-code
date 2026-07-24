---
id: LESSON-449
title: "A clean-compiling rebase can revert a parallel PR's invariant — compose intents, then run both PRs' tests"
component: "daemon/harness"
domain: "adlc"
stack: ["rust", "git"]
concerns: ["reliability", "process"]
tags: ["rebase", "merge-conflict", "parallel-sessions", "semantic-merge", "invariant", "sprint"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

Two sessions resolved adjacent REQ-547 deferred items in parallel, both
rewriting `summarize_if_large`: PR #5 changed its *contract* (byte-bounded
input, `SummarizeOutcome`, loud mechanical-truncation fallback — "never fold
raw"), while PR #4 changed its *execution* (async, `spawn_blocking`, plus a
new failure mode: a lost blocking task). Main advanced twice during PR #4's
merge window — the branch went `CONFLICTING` minutes after CI passed — and the
rebase produced conflict hunks where either side alone compiled. Taking PR #4's
error arm verbatim (`Err(_) => text.to_owned()`) would have compiled cleanly
and silently reverted PR #5's core invariant: the raw, unbounded result folded
into context with nobody told, on exactly the pathological inputs PR #5 fixed.

## Lesson

Resolving a conflict between two intentional changes is composition, not
selection: read the other PR's diff first (its docs and tests state the
invariant), then map **your** new states into **their** new taxonomy — here,
the blocking-task-lost case had to become a surfaced mechanical-truncation
outcome, not the old silent raw fold. Then run *both* PRs' tests on the merged
result; the other PR's tests are the only automated defense against your
resolution undoing their fix. Re-check mergeability immediately before every
merge in a multi-session window (`pr_view --json mergeable` after CI, not
before), and expect to repeat the cycle.

## Why It Matters

The failure mode is invisible at every gate that normally protects you: the
merge compiles, your own tests pass, and the regression lands in the exact
function a just-merged PR hardened. In a sprint with parallel sessions on one
component this is the *default* collision shape — deferred items from one
close-out cluster in the same functions.

## Applies When

- Rebasing a branch after any parallel PR touched the same functions —
  especially when both PRs originate from the same deferred-items list
  (check `/manifest` for overlap before starting; see also [[LESSON-447]] for
  the specific invariant nearly reverted here, and [[LESSON-441]] for fix
  passes introducing regressions).
- Any `CONFLICTING`/`DIRTY` PR state that appears between CI green and merge.
