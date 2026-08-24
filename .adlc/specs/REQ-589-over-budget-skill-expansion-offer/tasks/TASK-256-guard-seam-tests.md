---
id: TASK-256
title: "Seam tests for the redundant guards, and the one-home rule"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-245, TASK-246]
---

## Description

LESSON-508 + LESSON-546. Guards whose deletion is silent need their own tests; a one-home rule needs a resident test, not a grep in a task file.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — unit test for the suspension seam
- `crates/tetond/src/harness/permissions.rs` — unit test for non-persistence
- `crates/tetond/tests/` — the one-home test for the recipe window literal

## Acceptance Criteria

- [x] Deleting ADR-8's suspension reddens a seam-level unit test, not merely an end-to-end one (LESSON-508)
- [x] Deleting BR-10's non-persistence guard reddens a test. **Scope: the CONSENT's
  non-persistence in `harness/permissions.rs`.** TASK-246 wrote a separate seam test for the
  *memo's* non-persistence in `runtime.rs` — two different facts, and they stay distinct. Do
  not assume either covers the other
- [x] The recipe window literal appears exactly once outside `#[cfg(test)]` (LESSON-546)
- [x] Each test's doc comment states WHY it exists — that the guard's removal would otherwise be silent

## Technical Notes

> **Inherited from TASK-245 — read before writing the AC-16 leg.** "No history block is
> dropped" is exact only for the **prompt** and the **refusal** path. A suspended turn that
> *succeeds* still passes through the un-suspended `EndTurn` gate after the model's answer is
> appended, which can trim context to bound what the NEXT turn carries — that is D-7 working,
> not a BR-12 violation. Drive the **refusal** case (which returns via `?` before any
> post-gate runs, leaving the block list genuinely untouched), or assert against the
> **assembled prompt** rather than the post-turn block list. Asserting the post-turn block
> list on a succeeding turn will read as a BR-12 breach when nothing is wrong.

LESSON-508's point is that these tests look redundant and are not: the paths that would catch the regression cannot currently reach the case.

## Implementation notes (2026-08-24)

Every guard was mutated against the whole `tetond` suite before a line of test was
written, and again after. Three of the eight mutations **survived** the suite as it
stood; those three are what this task added tests for. The rest are recorded so the
next reader knows which guards were already pinned and by what.

| # | mutation | reddened, before | reddened, after |
|---|---|---|---|
| M1 | delete the `pressure.enforces_this_iteration()` guard (the gate always runs) | `an_accepted_over_budget_turn_keeps_every_block_and_refuses_visibly`, `the_suspension_is_spent_by_the_first_iteration` | + `the_accepted_turns_exit_gates_still_bound_what_the_next_turn_carries` |
| M2 | widen the suspension to the **`EndTurn`** exit | **nothing — 2,343 passed** | `the_accepted_turns_exit_gates_still_bound_what_the_next_turn_carries` (only) |
| M2b | widen the suspension to the **`max_turns`** exit | **nothing — 2,343 passed** | `the_accepted_turns_exit_gates_still_bound_what_the_next_turn_carries` (only) |
| M3 | `run_session_turn_with_source` delegates `SuspendedForAcceptedTurn` | 8 tests across `context_pressure`, `conversation_carry`, `remote_loop` and the lib | unchanged |
| M4 | `enforces_this_iteration` never clears | `a_pressure_suspension_can_be_spent_only_once`, `the_suspension_is_spent_by_the_first_iteration` | unchanged |
| M5 | `consults_grants()` → `true` for the offer | `a_remembered_skill_grant_cannot_settle_an_over_budget_offer` | unchanged |
| M6 | remember the accept arm under the offer's own key | `no_over_budget_answer_is_remembered_and_accepting_twice_asks_twice`, `a_remembered_skill_grant_cannot_settle_an_over_budget_offer` | + `no_over_budget_answer_reaches_the_grant_map_under_any_spelling_of_the_key` |
| M7 | remember the accept arm under the **digest spelling** of the same key | **nothing — 2,343 passed** | `no_over_budget_answer_reaches_the_grant_map_under_any_spelling_of_the_key` (only) |
| M8 | copy a recipe window into a third production file | TASK-240's in-module sweep **stays green** | `every_recipe_window_has_one_home_across_the_whole_daemon` |

**AC-1 — ADR-8's suspension.** TASK-245's four tests hold (M1, M3, M4 all redden). What
they did not cover is ADR-8's *edge*: the paragraph naming the `max_turns` and `EndTurn`
exits as deliberately **not** suspended was prose, and both widening mutations passed the
entire suite. Widening either is exactly the silent class LESSON-508 is about — the
accepted turn still goes out whole, still answers, raises no error and drops no block,
and the **next** turn inherits a conversation nothing bounded.
`the_accepted_turns_exit_gates_still_bound_what_the_next_turn_carries` drives both exits
on a *succeeding* accepted turn and, per the inherited warning, asserts BR-12 against the
**assembled prompt** and D-7 against the context the turn leaves behind — never the
post-turn block list. ADR-8's `max_turns == 0` unreachability, which the exception's
soundness rests on, is pinned by
`no_harness_config_this_module_builds_admits_a_zero_turn_ceiling`.

**AC-2 — BR-10's consent non-persistence.** Both of TASK-244's halves hold (M5, M6). The
hole was that `remembered(&key)` is a lookup at **one spelling**, and a skill's grant has
two (`skill_grant_key`: plain, and plain + a digest of its substituted commands). The
offer is always asked under the plain one; `authorize_skill` asks under whichever the
expansion minted. So an answer recorded under the digest was invisible to every existing
assertion — the offer consults no grants, so accepting twice still asked twice — and
fully visible to the door that *does* consult them, which would then run that skill's
commands for the rest of the session with no prompt on any screen. M7 passed 2,343 tests.
`no_over_budget_answer_reaches_the_grant_map_under_any_spelling_of_the_key` states BR-10
about the **map** rather than about a key, via a new `#[cfg(test)] grant_keys()` accessor,
and opens with a non-vacuity leg so an accessor that always answered "empty" cannot make
it decorative.

**AC-3 — the one-home rule.** TASK-240's narrowing to `provider_recipes.rs` +
`harness/budget.rs` is honest about the *collisions* (`1_000_000` really is micro-USD per
USD; `4_096` really is `LOCAL_BUDGET_TOKENS`) but not about the *conclusion*: a sweep can
be daemon-wide if it **names** the unrelated homes instead of retreating from them. The
narrowing already mattered — REQ-589 grew a proposal path through `harness/permissions.rs`
and `runtime.rs`, neither of which the narrowed sweep looks at.
`crates/tetond/tests/recipe_window_one_home.rs` sweeps all 85 production sources, pins
each window's `(file, count)` map against a `KNOWN_UNRELATED_HOMES` table that says what
each collision actually is, and carries an anti-inert partner —
`the_sweep_flags_a_second_home_in_every_spelling_a_window_could_wear` — because the
failure mode of a sweep is a matcher that never matches (TASK-259's `budget::derive(`
scan, twice over). It tolerates integer suffixes for the same reason: `4_096usize` would
otherwise be a home that lives forever. TASK-240's test is left in place; it is the
narrow one with the "read `recipe_for_model`" message at the composition site.

**Not mine, flagged:** `crates/teton-protocol/src/events.rs` grew a required `sentence`
field on `PermissionSubject::SkillOverBudget` (TASK-247) mid-task, which broke this
crate's `over_budget_subject` fixture. The fixture is in `harness/permissions.rs`, so it
is repaired here; if that field moves again the stand-in string moves with it.

