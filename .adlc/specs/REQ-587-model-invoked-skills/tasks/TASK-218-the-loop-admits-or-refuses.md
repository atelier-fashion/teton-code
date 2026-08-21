---
id: TASK-218
title: "The loop admits or refuses the expansion, and the reroute guard learns there is more than one"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-213, TASK-217]
---

## Description

ADR-2's first half plus B-5's fix. The budget decision belongs to the loop
because the tool cannot make it, and the reroute guard REQ-585 built is blind to
anything the model invoked.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — the admit/refuse between dispatch and the fold
- `crates/tetond/src/runtime.rs` — `skill_refit` becomes a list; `skill_would_not_survive_refit` takes it

## Acceptance Criteria

- [ ] The check runs in the loop, against `config.budget`, because the tool cannot: `build_tools` runs before `build_system_prompt`, so at construction there is no system prompt to measure against, and the route can be swapped mid-turn.
- [ ] Two stages, same vocabulary: Stage A before the dynamic-context consent is spent, Stage B after the outcomes fold in, and the refusal **says which**.
- [ ] The refusal is a **tool result the model can relay**, not an `RpcError` that ends the prompt. All four existing raise sites end the turn; this is the fifth and it is different, deliberately.
- [ ] **`skill_refit` becomes a list.** It is built from `skill_turn`, which is `Some` only for a user-typed `/name`, so `skill_would_not_survive_refit` returns `None` for every model invocation and `refit_for_reroute` middle-elides the expansion — at the one seam REQ-585 built a guard for. On a boundary-configured machine the privacy pin is the *expected* path, not a corner.
- [ ] `the_two_refusals_bracket_the_consent_seam_and_precede_the_seed` pins `raises.len() == 4` with a 2/2 split around `CarriedTurn::begin` and a 400-byte window naming the guard. All three move **deliberately** — widen with the reasoning written down, do not relax.
- [ ] AC-8's digest-bypass leg is **behavioural**, not a source scan: a 2,800-word expansion through the loop on the default-budget route, where the fold would otherwise bite. The existing source pin counts call sites and says nothing about `skill`.
- [ ] Mutation: moving the check into the tool, leaving `skill_refit` a single value, and making the refusal an `RpcError` each fail a named test.

## Technical Notes

- `would_append_fit` (TASK-213) is the measurement; `skill_fit`'s seed model is the wrong question for a mid-loop result.
