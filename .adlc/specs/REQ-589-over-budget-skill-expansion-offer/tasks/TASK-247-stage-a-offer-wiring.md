---
id: TASK-247
title: "Wire the offer into Stage A for the typed caller"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-242, TASK-243, TASK-244]
---

## Description

BR-2 + BR-11. Stage A (runtime.rs:3601) becomes offer-capable for `SkillCaller::User` only. The model-invoked path (`skill_append_fit`) keeps refusing and is never offered a choice.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — Stage A Body (3601) and Stage B WithDynamicContext (3681); the reroute guard (10894) stays refusal-only

## Acceptance Criteria

- [ ] A model-invoked over-budget call is refused with the Model arm's sentence and reaches no offer (BR-2, AC-5)
- [ ] Every not-sent path reaches no provider, emits no context_pressure, changes no health, and does not spend the session-naming duty (BR-11, AC-18) — asserted by egress capture, not by inspection
- [ ] The naming duty stays deferred below the gate (runtime.rs:3641)
- [ ] Declining produces byte-identical output to today's refusal under the same -32023 (AC-3)
- [ ] Accepting dispatches the expansion whole, byte-for-byte what skill_fit measured (AC-1, BR-1)

## Technical Notes

`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed` (skill_turn.rs:3357) and `the_budget_check_runs_in_the_loop_and_the_tool_measures_nothing` (:3447) are structural tests that WILL break on any ordering change — update them deliberately, do not delete.
