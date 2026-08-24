---
id: TASK-242
title: "The window-verdict classifier and BR-7 remedy table"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-240, TASK-241]
---

## Description

BR-3 + BR-7. Add `OverBudgetOffer`, `Remedy`, and the `window_verdict` classifier keyed off the stamped `BudgetBound` and the declared window. Nothing is re-derived or re-measured — the figures come from the measurement that produced them and the `RouteBudget` the router stamped.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — new types beside `skill_fit` (871); the bound→remedy table; the verdict classifier
- `crates/tetond/src/router.rs` — provider-enumeration helper for ADR-12 (new)

## Acceptance Criteria

- [ ] All three verdicts produce an offer; none refuses outright (BR-3, AC-6)
- [ ] The BR-7 table is exhaustive over `BudgetBound`; `RedactScan` alone yields no remedy (BR-7b, AC-7)
- [ ] Absence of a remedy is `Remedy::NotOffered` — there is no second `Option` representation (LESSON-545)
- [ ] Only reachable (bound, verdict) pairs are tested, per architecture.md's reachability table; no vacuous cells (LESSON-520)
- [ ] The classifier re-derives no budget and re-runs no estimator — asserted by passing a stamped budget and checking the figures are carried, not recomputed

## Technical Notes

Test shapes: `BudgetInputs::local()` for the local arm and the test-module helper `remote(window, cap, redact_scan)` (budget.rs:1056) for all three remote bounds. `skill_turn.rs`'s Harness cannot build UserCap/RedactScan/local — do not try.
