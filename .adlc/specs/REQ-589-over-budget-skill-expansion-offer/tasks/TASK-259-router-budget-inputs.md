---
id: TASK-259
title: "Expose the route's declared window to the offer path"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

**Created mid-Phase-4.** TASK-242 found that neither `Route` nor `Router` exposes the
route's declared `capabilities.max_context`: `capability_of` is private and `Route` carries
only `budget`/`provider_id`/`model`. But `window_verdict` and `OverBudgetOffer::new` need
`window: u32`, and TASK-240's already-landed `proposed_window` needs a `BudgetInputs`.
**TASK-247 and TASK-250 are blocked without this.** TASK-242 declined to add it rather than
exceed its stated ownership, which was correct.

## Files to Create/Modify

- `crates/tetond/src/router.rs`

## Acceptance Criteria

- [x] A `Router::budget_inputs_for(provider_id) -> BudgetInputs` accessor exists, and
      `budget_for` is refactored to call it
- [x] **`budget_for` remains the single `derive` caller in the routing layer** — this is
      REQ-586 AC-12 and the whole reason the budget has one home. A test pins it
- [x] The accessor is additive: every existing `budget_for` result is byte-identical before
      and after, pinned by a test over all five bounds
- [x] No caller outside the routing layer gains the ability to re-derive a budget

## Technical Notes

Strictly additive refactor. The risk is subtle: exposing `BudgetInputs` makes it *possible*
for a caller to call `derive` itself, which is exactly the second-source failure REQ-586's
own verify pass caught (`/verbose` naming a budget the turn was not running under). The
accessor hands out the inputs; it must not become an invitation to re-derive.
