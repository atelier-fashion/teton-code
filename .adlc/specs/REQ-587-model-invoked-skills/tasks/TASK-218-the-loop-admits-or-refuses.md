---
id: TASK-218
title: "The loop admits or refuses the expansion, and the reroute guard learns there is more than one"
status: complete
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
- `crates/tetond/tests/skill_turn.rs` — the `-32023` bracket pin (`~:2240`), read as a guard that the turn-ending refusals stayed put

## Acceptance Criteria

- [x] The check runs in the loop, against `config.budget`, because the tool cannot: `build_tools` runs before `build_system_prompt`, so at construction there is no system prompt to measure against, and the route can be swapped mid-turn.
- [x] Two stages, same vocabulary: Stage A before the dynamic-context consent is spent, Stage B after the outcomes fold in, and the refusal **says which**.
- [x] The **Stage A/B** refusal is a tool result the model can relay, raised at the fold where a `ToolCall` id is in hand. It is not a fifth `error_code::SKILL_EXPANSION_TOO_LARGE` raise site in `run_prompt_turn`, so `raises.len() == 4` and its 2/2 split **do not move** — read that pin as a guard that the *turn-ending* refusals stayed put, not as something to widen.
- [x] **The reroute arms are the honest exception, and this task scopes them rather than pretending otherwise.** Both `skill_would_not_survive_refit` call sites sit in `run_prompt_turn`'s `'turn` retry loop, *after* `run_session_turn_with_source` returned — there is no `ToolCall` id there and the expansion is already a committed block, so a tool result is not expressible without restructuring the retry, which this REQ does not propose. A model-invoked expansion caught at a reroute therefore **ends the turn** with the existing typed error, and BR-6/BR-9's "never a crash, always relayable" carries a stated exception naming this seam. Do not invent a channel; state the residual and file it.
- [x] **`skill_refit` becomes a list**, and the list's entries are `(name, text, system)` triples for expansions *committed this turn* — the guard re-measures each against the new route and refuses the turn on the first that would not survive. ** It is built from `skill_turn`, which is `Some` only for a user-typed `/name`, so `skill_would_not_survive_refit` returns `None` for every model invocation and `refit_for_reroute` middle-elides the expansion — at the one seam REQ-585 built a guard for. On a boundary-configured machine the privacy pin is the *expected* path, not a corner.
- [x] `the_two_refusals_bracket_the_consent_seam_and_precede_the_seed` stays at `raises.len() == 4` with its 2/2 split. If an implementation makes it 5, that is the signal a model-path refusal was written as a turn-ender — investigate rather than update the number.
- [x] AC-8's digest-bypass leg is **behavioural**, not a source scan: a 2,800-word expansion through the loop on the default-budget route, where the fold would otherwise bite. The existing source pin counts call sites and says nothing about `skill`.
- [x] **Wire `TurnState::note_foreign_tool_completed()`, or BR-6b is wrong in one direction** (moved here from TASK-217, 2026-08-21 — `turn_loop.rs` is your file, and three tasks contending for it is worse than one owning it). TASK-216 shipped the method as an unwired seam: the tool cannot see the loop's other dispatches, so `skill alpha` → `read` → `skill alpha` in one turn is refused `repeated` where BR-6b admits it. **BR-6b's *stated* example is admitted either way** — `/proceed`'s two `/validate` passes separated by `/architect`, where the intervening expansion overwrites the seed — so a test written from the spec's own illustration passes with the seam unwired. Call it from the loop, and pin the case the illustration does not cover.
- [x] **BR-9's "one line per typed refusal" is unpublished on the model path.** TASK-217 wired `publish_invocation` for the *expansion* path only, matching the user path — so a refused model invocation is silent on the session surface, which BR-9 forbids in the same sentence that says a refusal is never silent and never a crash. You own the admit/refuse; publish the refusal line where you raise it, and assert the session prints one.
- [x] Mutation: moving the check into the tool, leaving `skill_refit` a single value, and making the refusal an `RpcError` each fail a named test.

## Technical Notes

- `would_append_fit` (TASK-213) is the measurement; `skill_fit`'s seed model is the wrong question for a mid-loop result.
