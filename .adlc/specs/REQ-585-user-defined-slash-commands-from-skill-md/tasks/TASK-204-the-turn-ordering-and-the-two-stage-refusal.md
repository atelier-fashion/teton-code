---
id: TASK-204
title: "run_prompt_turn: expand, refuse before consent, seed with provenance — in that order"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-200, TASK-202, TASK-203]
---

## Description

The ordering BR-8 is really about. `CarriedTurn::begin` (`runtime.rs:2935`)
both pushes the user block **and** arms the drop-commit, so every check BR-8
requires has to happen before that line — not somewhere inside the turn loop.

This task lands the non-consent half: accept the invocation, expand it, refuse
it if the body alone does not fit, and seed the turn with the skill file's
provenance. TASK-205 adds the consent and the commands.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `run_prompt_turn`'s skill path
- `crates/tetond/src/server.rs` — `spawn_prompt_turn` / `flatten_prompt` carry the invocation
- `crates/tetond/tests/skill_turn.rs` — the ordering and refusal suite

## Acceptance Criteria

- [ ] `PromptTurnParams` validation: exactly one of a non-empty `prompt` and a `Some(skill)`. Both empty ⇒ `INVALID_PARAMS`; both populated ⇒ `INVALID_PARAMS`. This is what makes a dropped `skill` field a loud failure instead of a raw `/name args` line reaching a model (ADR-3).
- [ ] An unknown or shadowed skill name arriving from a client is refused by the daemon too — the client's snapshot is a convenience, not the authority (LESSON-520's shape: do not let the only check live on the far side of the wire).
- [ ] Order inside the turn: probe root → route + `route.budget` → `expand` → **Stage A refusal** → (TASK-205's consent and commands) → **Stage B refusal** → `CarriedTurn::begin`. Stage A measures the expansion with a `[dynamic context pending]` placeholder in each slot, so a body that cannot fit is refused **before** the user is asked to approve anything (BR-8d).
- [ ] A refused turn emits **no** `context_pressure` event of any kind and no newest-user elision note. Asserted as a drain-and-assert-empty, copying `context_pressure.rs:786 a_report_with_nothing_in_it_is_the_one_that_says_nothing` and `runtime.rs:27432` (BR-8c).
- [ ] A refused turn changes no health, degrades nothing, and does not retry — the four properties of REQ-586's sibling arm, asserted the same way (`runtime.rs:27362`).
- [ ] A typed oversized prompt **still elides** loudly (REQ-586 BR-7), pinned in the same file so the refusal is seen to apply to skill turns only (AC-16).
- [ ] The turn is seeded with `push_user_from(text, sources)` where sources is the skill file's id — or the unpinnable marker for a user skill outside the root (ADR-9, TASK-197).
- [ ] The `digest` duty never touches the expansion: `summarize_if_large`'s only production call site stays the tool-result fold. Pinned, because REQ-586 scaled the digest thresholds with the route budget and a skill body is squarely inside the band that would trigger it (BR-4).
- [ ] Mutation table: moving either refusal after `CarriedTurn::begin`, and moving Stage A after the consent, each fail a named test.

## Technical Notes

- `flatten_prompt` (`server.rs:1974`) runs before the spawn and returns a `String`. The invocation must travel beside it, not through it — a skill turn has no `PromptBlock`s to flatten.
- Everything this task needs is already in hand at `runtime.rs:2910-2935`: `probed`, `tool_ctx`, `route.harness`, `route.budget`, `system`. Do not re-probe and do not re-derive the budget.
