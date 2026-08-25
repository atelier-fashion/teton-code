---
id: TASK-269
title: "Give the generation reservation a constant home"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: []
---

## Description

ADR-1. Hoist the literal `1_024` out of `HarnessConfig::default()`'s `gen_params` into a
constant in `budget.rs`, and make `generation_reservation()` return it directly instead of
constructing a whole `HarnessConfig` to read one field off it.

**This is the task that makes the rest of the REQ safe.** `HarnessConfig::default()` calls
`derive(BudgetInputs::local())`; `generation_reservation()` calls `HarnessConfig::default()`.
Today the cycle is open only because `derive`'s local arm returns before reading anything.
TASK-270 gives that arm a body — so unless this lands first, wiring the reservation in is a
stack overflow in the most-constructed value in the crate.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — add `LOCAL_GENERATION_RESERVATION: u32 = 1_024`;
  rewrite `generation_reservation()` (line ~614) to return it
- `crates/tetond/src/harness/turn_loop.rs` — `HarnessConfig::default()`'s `gen_params.max_tokens`
  (line ~482) reads the constant instead of the literal

## Acceptance Criteria

- [x] `generation_reservation()` does not mention `HarnessConfig` — grep proves it
- [x] `HarnessConfig::default().gen_params.max_tokens == LOCAL_GENERATION_RESERVATION`, asserted
- [x] All six existing callers still compile and get 1,024: `router.rs:630`, `:2733`,
      `server.rs:11541`, `budget.rs:655`, `:3389`, `context.rs:1732`
- [x] Full suite green — this task changes no behaviour, only where a number lives

## Technical Notes

The constant's doc must say **why** it is a constant and not a field read: name the cycle and
name TASK-270 as the reason it matters. A reader who does not know that will "simplify" it back.

This is a pure refactor and must be verifiable as one: the value is 1,024 before and after,
every caller returns 1,024 before and after. If any test's behaviour changes, something else is
wrong — stop and report rather than adjusting the test.
