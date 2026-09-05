---
id: TASK-401
title: "Both call sites take their provenance from the fold; the `spawned` rule and the user-skill `unknown` are retired, and the tests that asserted them flip"
status: complete
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: [TASK-398, TASK-399, TASK-400]
---

## Description

BR-1, BR-2, BR-3, BR-6, BR-8, BR-9, BR-10. The typed path (`runtime/turn.rs`)
builds `Reach` from the turn's `ToolContext`, calls the new `run_all`, and
writes `fold_expansion(identity, &runs)` onto `SkillTurn` — replacing
`skill.unknown |= any(spawned)` and the `provenance_of → None ⇒ unknown`
reading. The model-invoked path (`harness/tools/skill.rs`) does the same
and maps `ExpansionProvenance` to `ToolProvenance` exactly as
`ShellTool::run` maps a verdict. Every test asserting the retired rules flips
to the new claim; none is deleted.

## Files to Create/Modify

- `crates/tetond/src/runtime/turn.rs` — `SkillTurn` construction: identity via `provenance_of` (either scope), `unknown` only when it is `None`; preamble seam: build `Reach` from the turn's `ToolContext`, call `run_all(…, &reach)`, fold, write `sources`/`unknown`/`boundary_touch`; remove the `spawned` OR; `outcome_view` receives the verdict; the naming-duty provenance uses the same three fields
- `crates/tetond/src/harness/tools/skill.rs` — replace the `(source, spawned)` match with `fold_expansion` + the `ShellTool::run` mapping; roster provenance follows the same fold
- `crates/tetond/src/runtime/mod.rs` — `expansion_provenance` call sites pass three fields
- `crates/tetond/tests/skill_turn.rs` — `a_user_skill_outside_the_root_seeds_a_block_that_says_it_cannot_be_pinned` → `…seeds_a_block_with_its_home_scoped_identity`; `an_invocation_that_ran_a_command_seeds_a_block_that_cannot_be_pinned` → splits into a `Rooted` case (pinned to its sources) and an `Unknown` case (unknown)
- `crates/tetond/tests/skill_boundary.rs` — `a_user_skill_is_unknown_and_pins_under_a_boundary_it_never_touched` → `a_user_skill_leaves_under_a_boundary_it_never_touched_and_is_refused_by_one_that_names_it`
- `crates/tetond/tests/egress_capture.rs` — `a_user_skill_outside_the_root_pins_the_turn_wherever_any_boundary_exists` flips to the leave-by-default / refused-by-a-naming-glob pair
- `crates/tetond/tests/provenance_egress.rs` — `a_model_invoked_user_skill_pins_the_turn_wherever_any_boundary_exists` flips; `a_model_invocation_whose_command_failed_still_pins_the_turn` becomes "…whose *opaque* command failed still pins" with a `Rooted`-failed twin that does not
- `crates/tetond/src/harness/tools/skill.rs` tests — the roster/user-skill cases (TASK-398 changed the minting expectation; here the provenance mapping)
- `.adlc/context/architecture.md` — the Key Pattern that states the retired rule is amended per the architecture's "Retired and amended rules"

## Acceptance Criteria

- [ ] With the thirteen builtin boundaries in force, a typed user skill with no preambles seeds a block whose provenance is `{~/…}` and reaches a capturing transport; the same skill under a user glob naming `.claude/skills` is refused naming its file
- [ ] A typed skill with `cat README.md` and `ls -la` preambles seeds `{identity, README.md}` and no `unknown`; with `sh -c 'echo x'` seeds `unknown`; with `cat secrets/prod.env` seeds `secrets/prod.env` and is refused naming it; with a preamble naming `~/.ssh/config` seeds `boundary_touch` and is refused against `<boundary-touch>`
- [ ] The model-invoked `skill` tool produces the same `ToolProvenance` for each of the cases above
- [ ] No production read of `DynamicOutcome::spawned` remains for provenance (a source scan in `skill_turn.rs` or `dynamic.rs` asserts the absence, LESSON-550)
- [ ] `cargo test --workspace --no-fail-fast` green; clippy `-D warnings` clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/tests/skill_turn.rs::a_rooted_preamble_pins_the_expansion_to_its_sources_and_an_opaque_one_marks_it_unknown` | yes |
| BR-2 | test-case | `crates/tetond/tests/skill_turn.rs::a_boundary_naming_preamble_is_refused_whatever_it_exits` | no |
| BR-3 | test-case | `crates/tetond/tests/egress_capture.rs::a_user_skill_leaves_under_the_builtins_and_is_refused_by_a_glob_that_names_it` | yes |
| BR-6 | test-case | `crates/tetond/tests/provenance_egress.rs::a_model_invoked_user_skill_gets_the_same_provenance_as_the_typed_one` | yes |
| BR-6 | test-case | `crates/tetond/tests/provenance_egress.rs::a_model_invocation_whose_opaque_command_failed_still_pins_and_a_rooted_one_does_not` | yes |
| BR-8 | test-case | `crates/tetond/tests/skill_turn.rs::a_pinned_skill_turn_is_announced_through_the_existing_sink` | no |
| BR-9 | test-case | `crates/tetond/tests/skill_boundary.rs::with_no_boundary_configured_a_user_skill_with_an_opaque_preamble_is_sent` | yes |
| BR-10 | test-case | `crates/tetond/tests/skill_boundary.rs::a_project_skill_still_mints_and_still_pins_as_a_read_would` | yes |
| BR-1 | structural-check | `crates/tetond/tests/skill_turn.rs::no_production_provenance_reads_spawned_any_more` (source scan over `runtime/turn.rs` and `harness/tools/skill.rs`) | no |

## Technical Notes

- The typed path's `ToolContext` is built before the preamble seam; if the ordering in `run_prompt_turn` puts the seam first, move the `Reach` derivation (not the context build) — REQ-600's stage order is guarded by source scans.
- `expansion_provenance` also feeds the naming duty (`spawn_title_session`) with the pre-fold values; after the fold it must be recomputed from the post-fold fields, as the existing comment there already requires.
- Keep the `skill_invoked` publish where it is; TASK-402 adds the fields it renders.
- Flip, don't delete: each retired-rule test keeps its file and gains a doc line naming REQ-619 and the rule it now asserts (LESSON-550).
