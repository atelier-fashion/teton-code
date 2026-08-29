---
id: TASK-297
title: "Ordering guards: the context observes claim-time cwd and post-hold router"
status: complete
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-296]
---

## Description

Two regression guards for the same hazard class (BR-2 / BR-2.1): a context that
captures a fact before that fact's last binding.

Phase 1 established that the **existing** stale-cwd test does not satisfy AC-5.
`a_turn_handed_a_stale_cwd_snapshot_runs_on_the_root_the_registry_holds_at_claim_time`
guards `run_prompt_turn`'s post-claim re-read, which sits upstream of every
point a `TurnContext` can be built — so any context built below it inherits the
fresh value for free and the test cannot fail on a wrongly-built context. That
is a broader guard standing in front of the mutation (LESSON-586's shape).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — two tests in the existing test module

## Acceptance Criteria

- [ ] AC-5 (a): a test asserts on **`TurnContext`'s own view** of the session
      root, not only on the turn's downstream behavior, so it is not satisfiable
      by the upstream re-read alone.
- [ ] AC-5 (b): the mutation is demonstrated — build the context from the
      `session_cwd` **parameter** (the pre-claim snapshot) instead of the
      post-claim re-read, confirm the test goes **red**, revert.
- [ ] AC-5 (c): that mutation and its observed failure are recorded verbatim in
      the test's doc comment.
- [ ] BR-2.1: a second test drives a turn whose local tier is **warming**, so
      the REQ-580 hold fires and rebinds `router`, and asserts the constructed
      `TurnContext` carries the **post-hold** router.
- [ ] The BR-2.1 test's mutation is demonstrated too — construct the context
      before the hold, confirm red, revert — and recorded in its doc comment.
- [ ] Neither test asserts anything a passing pre-refactor build would also
      satisfy. State explicitly in each doc comment what the test would miss.

## Technical Notes

conventions.md is directive here: "Break the thing the test guards and confirm
it goes red; record the mutation in the test's doc comment." And: "never let the
expected value be computed by the subject."

For the warming-tier test, reuse the existing REQ-580 hold fixtures rather than
building a new warming harness — search the test module for `hold_for`,
`await_local_tier`, and `TurnQueued`. If no reusable fixture exists, say so and
build the smallest one that can actually observe the rebind; do not settle for a
test that only proves the hold fired.

Distinguishing the two routers requires them to be observably different. Seed
the tier state so the pre-hold and post-hold routers resolve differently —
if they resolve identically the test cannot fail and does not satisfy this task.
