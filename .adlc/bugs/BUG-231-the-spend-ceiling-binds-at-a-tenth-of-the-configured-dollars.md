---
id: BUG-231
title: "The per-prompt spend ceiling binds at a tenth of the configured dollars — config micro-cents vs recorded usd_micros"
status: resolved
severity: high
created: 2026-10-07
updated: 2026-10-08
resolved: 2026-10-08
component: "daemon/cost-ledger"
domain: "cost"
stack: ["rust", "daemon"]
concerns: ["cost", "reliability"]
tags: ["spend-ceiling", "units", "micro-cents", "usd-micros", "prompt-ceiling", "req-588", "req-623"]
introduced_by: ["REQ-588"]
attribution: derived
---

## Description

`CostConfig::ceiling_micro_cents()` converts `[cost] prompt_ceiling_usd` at
100,000 units per USD, but every recorded call cost is `usd_micros`
(1,000,000 per USD) and is added to `PromptSpend` unconverted. The ceiling
therefore trips at roughly a tenth of the figure the user wrote. Found by
TASK-424 while wiring `spent_by_child`, and confirmed in a probe during
TASK-430: a $0.44 call was reported as "spent $4.40" against a $0.05 ceiling.

REQ-623's `SharePool` deliberately reuses the same `spend_units` helper so
child shares and the ceiling check agree with each other; fixing the unit at
that one seam fixes both.

## Reproduction Steps

1. Set `[cost] prompt_ceiling_usd = 1.0` with a priced remote provider.
2. Make calls totalling about $0.11.
3. The next call is refused `SpendCeilingReached`, and the refusal sentence
   reports the spend as ~$1.10.

## Expected Behavior

The ceiling binds at $1.00 of recorded spend, and the refusal reports the real
figure.

## Actual Behavior

It binds at ~$0.10, and the reported spend is 10× the real one.

## Environment

- Platform: all
- Version: since REQ-588 (v0.1.3x); still present at REQ-623's merge (919bccc)

## Root Cause

Two units met at one seam with no conversion. `CostConfig::ceiling_micro_cents`
(`crates/teton-core/src/config.rs:298-305`, REQ-588) converts
`prompt_ceiling_usd` at 100,000 per USD (micro-cents, 1e-5 USD), and
`cost_ceiling::usd` renders the accumulator on the same assumption
(`micro_cents / 1_000` = cents). The ledger prices every call in `usd_micros`
(1e-6 USD, `prices.rs` `MICROS_PER_USD`), and `MeteredBody::finalize` fed that
price to `PromptSpend::add` as-is through `spend_units`
(`crates/tetond/src/cost/ledger.rs:1286`, REQ-588; relocated into the helper by
REQ-623, arithmetic unchanged). So every recorded call counted ten times over:
a $1.00 ceiling bound at ~$0.10 and the refusal reported 10× the real spend.
REQ-623's `SharePool` inherited the unit through the same helper, so child
shares were wrong by the same factor and consistent with each other.

Attribution: `git blame` on both ranges names REQ-588 (the unconverted add) and
REQ-623 (the relocation); the operator recorded REQ-588 alone, since REQ-623
did not change the arithmetic.

## Resolution

One conversion at the one seam: `spend_units` now divides the row's
`usd_micros` by `teton_core::cost_ceiling::USD_MICROS_PER_MICRO_CENT` (10), a
named constant beside the accumulator it feeds. The live accumulator
(`MeteredBody::finalize`), `CostLedger::spent_by_child`, and REQ-623's
`ChildSpend`/`SharePool` all read prices through that helper, so the ceiling
check, the refusal sentence, the stamped child shares and the per-child ledger
query agree in micro-cents. Wire fields keep their units: `CostRecord.usd_micros`
is still the exact price (the CLI's `/cost` divides by 1,000,000 and was always
right); `*_micro_cents` fields on `ChildBounds`/`ChildResult`/the agent events
are now genuinely micro-cents.

A regression test derives a ceiling of 1.5× a bundled price through the real
config edge and asserts the call leaves it unreached and renders its real
dollars (`share::tests::a_priced_call_just_under_the_ceiling_does_not_reach_it`);
restoring the identity (`.map(i64::unsigned_abs)`) reddens six tests: the
regression test, `spend_units_counts_a_price_and_refuses_to_count_nonsense`,
`a_childs_spend_reaches_its_own_and_the_parents_accumulator_in_ledger_units`,
`agent_dispatch::spend::two_child_split_and_three_child_release`,
`agent_dispatch::statuses::spend_exhausted` and
`cost_attribution::parent_total_is_own_plus_children` (run and reverted). Tests that
had pinned the 1:1 unit (`spent_by_child_is_exactly_that_childs_spend`, the
share accumulator test, the e2e `units()` helper and `cost_attribution`'s
per-child equality) now convert per row, the way the daemon does.

## Deployment

- Merged to `main` as `53a82af` (#338) on 2026-10-08; squash of `fix/bug-231-spend-ceiling-units`.
- No deploy-on-merge for the daemon — ships with the next release (the release runbook); until then `main` carries the fix.
- Lesson: LESSON-665.

## Files Changed

- `crates/teton-core/src/cost_ceiling.rs` — `USD_MICROS_PER_MICRO_CENT`, the one named conversion
- `crates/tetond/src/cost/ledger.rs` — `spend_units` converts; unit test corrected
- `crates/tetond/src/cost/share.rs` — regression test; accumulator test corrected
- `crates/tetond/tests/agent_dispatch.rs` — e2e `units()` helper converts per row
- `crates/tetond/tests/cost_attribution.rs` — per-child ledger equality converts
- `.adlc/knowledge/lessons/LESSON-665-two-sub-dollar-units-meeting-at-one-seam.md` — the lesson
