---
id: TASK-276
title: "Reverse D-4: the byte half returns to LOCAL_BUDGET_BYTES"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-26
dependencies: [TASK-270, TASK-271, TASK-272, TASK-273, TASK-275]
---

## Description

Unplanned, and opened **after** TASK-269–275 had all landed. TASK-272's own witness measured the
thing the REQ was opened for and found it still broken: the reported `/analyze` body — 4,097
words, and a byte count the record pins only to [30,500, 31,499] — had gone from *refused by 1
word* to refused on **bytes** across ~78% of that range. The refusal changed currencies instead
of going away.

D-4 — "the byte half falls to 30,720" — was never an owner decision; it was inferred from D-3's
"take the full window". It is reversed. The **word** half stays window-derived at 10,240; the
**byte** half returns to `LOCAL_BUDGET_BYTES`, 32,768.

Recorded as ADR-9 in `architecture.md`. No existing ADR is edited — each was right about the
state it described.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — `derive`'s local arm takes `LOCAL_BUDGET_BYTES` for its
  byte half; the reasoning, with the measurements, lives at that arm. Plus AC-16's bound clause
  (TASK-274's surviving half).
- `crates/tetond/src/harness/compact.rs` — `COMPACT_OUTPUT_MAX_BYTES` **stays** on the engine's
  chain (ADR-5 is unaffected); the ceiling/budget relation becomes an ordering, and
  `a_compaction_that_lands_in_the_old_gap_is_applied_not_degraded` is removed at its own guard's
  request.
- `crates/tetond/tests/token_corpus.rs` — the residual, pinned as a stated equality.
- `crates/tetond/tests/skill_over_budget_offer.rs` — AC-12's witness rewritten and renamed; local
  fixtures sized off `derive` rather than off literals.
- `crates/tetond/tests/compaction_cadence.rs`, `tests/e2e/privacy_fixes.rs`,
  `crates/tetond/src/harness/context.rs`, `crates/teton/tests/pty_e2e.rs` — the pinned pair.
- `requirement.md` (D-4, BR-1, BR-7, BR-9, AC-1, AC-4, AC-7), `architecture.md` (ADR-9 + the
  re-measured AC-11 table), `docs/manual-verification.md` (leg (e)).

## Acceptance Criteria

- [x] `derive(BudgetInputs::local())` is `(10_240, 32_768)`, bound `LocalEngine`
- [x] A 4,097-word local turn at a byte size inside the reported interval **serves**, raising no
      over-budget offer, asserted on a real turn — `the_reported_analyze_measurement_serves_on_both_halves_of_the_local_pair`
- [x] The residual the reversal accepts is asserted as an equality
      (`bytes_claim − words_claim == LOCAL_GENERATION_RESERVATION`), not hidden
- [x] `COMPACT_OUTPUT_MAX_BYTES ≤` the local byte budget, as a relation (AC-8 unchanged)
- [x] AC-11's turn counts re-measured and re-recorded: 15 / 11 / 8 / 4
- [x] No test asserts the 30,720 byte band, which no longer exists

## Technical Notes

The evidence, in one line each:

- **The reported body.** 4,097 w (exact) / [30,500, 31,499] B (`31 KB`, rounded) = 7.44–7.69
  B/word. Under D-4 it is over on bytes across ~78% of that range, and worth between +0.7% and
  −2.4% over all of it. Reversed: it fits on both halves everywhere in the range.
- **The crossover.** A window-derived byte half beats the constant only below 7.5 B/word. Prose
  (≈5) +50%; code (≈8) −6.25%.
- **What the reduction protected.** Nothing measurable. `numeric_grid.txt` is 20,480 B, admitted
  at 30,720 *and* 32,768, and costs 20,480 real tokens against 15,360 usable either way.
