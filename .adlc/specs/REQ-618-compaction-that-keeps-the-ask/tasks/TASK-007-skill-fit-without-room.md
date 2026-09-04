---
id: TASK-007
title: "A skill body that fits the budget but leaves no room is refused with the arithmetic"
status: complete
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001, TASK-005]
---

## Description

Add the third fit verdict at the seam that already owns the first two, compose
its sentence through the one composer, and route it into REQ-589's existing
offer so a user may still proceed once.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — `ROOM_FRACTION_PERCENT`, `SkillFitVerdict`, a `SkillSentence::NoRoom` arm on `skill_refusal`, the anchored-body sum for BR-2
- `crates/tetond/src/runtime/turn.rs` — `fits_without_room` routes into `offer_or_refuse_over_budget`; publish `skill_refused_no_room`
- `crates/tetond/src/harness/turn_loop.rs` — the model path's second-expansion arithmetic

## Acceptance Criteria

- [x] `ROOM_FRACTION_PERCENT = 25`, integer arithmetic, one home; the verdict is
      `fits_without_room` when `body_bytes > room_fraction × budget_bytes` and the
      underlying `Fit` still fits.
- [x] The refusal names the body size, the budget, the fraction and the remedy,
      and composes through the **same** `skill_refusal` composer as the other two
      sentences — so the three cannot quote different numbers for one measurement.
- [x] A body at 30 % of the budget yields `skill_refused_no_room` and REQ-589's
      offer; `proceed once` expands **and anchors** it; `decline` ends the turn
      with no model call (AC-3).
- [x] BR-2's second expansion: measured as
      `Σ anchored_body_bytes + candidate > room_fraction × budget_bytes`, so two
      10 % bodies are both admitted and the second of two 20 % bodies is refused
      with the arithmetic (ADR-618-6).
- [x] Benign path: a body at 20 % expands untouched, with no offer and no event —
      the shipped ADLC bodies on a REQ-616-sized local window are all in this arm.
- [x] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `harness::budget::tests::a_body_over_the_room_fraction_fits_without_room` | yes |
| BR-2 | test-case | `harness::budget::tests::the_second_anchored_body_is_measured_against_the_first` | yes |
| AC-3 | test-case | `tests/skill_over_budget_offer.rs::no_room_offers_and_proceed_once_anchors` | yes |

## Technical Notes

ADR-618-6. `offer_or_refuse_over_budget` hardcodes `SkillCaller::User` in its
composer, which is correct here too: the offer is the typed caller's alone
(REQ-589 BR-2). The model path gets the refusal, never the offer.
