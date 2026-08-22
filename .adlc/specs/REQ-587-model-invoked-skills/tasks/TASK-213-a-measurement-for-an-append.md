---
id: TASK-213
title: "Would this *append* fit — the measurement `skill_fit` does not make"
status: complete
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

ADR-2's second half. `skill_fit` measures a **seed**; a mid-loop expansion is an
**append** to a conversation that already holds blocks. AC-8's "fits alone but
not with the current context" is a genuinely different question, and nothing
answers it today.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `would_append_fit`, a sibling of `would_seed_fit`
- `crates/tetond/src/harness/budget.rs` — a caller-aware refusal sibling of `skill_refusal`

## Acceptance Criteria

- [x] `would_append_fit(system, request_block, candidate, budget_tokens, budget_bytes) -> Fit` measures the **post-truncation worst case** — system + the turn's request block + the candidate — at `truncated = true`, through the same private estimators the pressure path uses.
- [x] **It does not measure the live block list, and that is the whole point.** `would_seed_fit`'s own doc states the rule: *"an expansion that fits while the assembled conversation does not is ordinary context pressure, and dropping older turns to make room stays permitted"* (BR-8c). `bytes_of` is additive, so append-fits ⟹ seed-fits — measuring the live list is strictly stricter and would **refuse** exactly the case AC-8 requires to *fold*. `latest_request(ctx)` (`turn_loop.rs:844`) hands the request block; take it as a parameter rather than reading `self.blocks`.
- [x] `truncated = true` for `would_seed_fit`'s reason: `bytes_of` adds the 142-byte note only once truncation has happened, so a check that omits it passes and is then clamped.
- [x] The refusal sentence is caller-aware. `skill_refusal` hard-codes `` `/{skill}` `` and *"never shortened into something you did not invoke"* — a model invocation wears neither. Add the caller as a parameter; do **not** copy the composer, and do not mint a second bound table: `BudgetBound::words()` stays the one adjective vocabulary and `bytes_figure`/`thousands` stay the one number vocabulary.
- [x] `SkillFit::TooLarge` must be renderable as a **tool result**, not only as an `RpcError`. Today every one of the four raise sites ends the prompt turn; BR-6 and BR-9 say a model-facing refusal is a typed outcome the model can relay.
- [x] Unit-tested against a conversation with prior blocks, asserting the **right** difference: a candidate that fits by this measurement still fits when history is present (history is droppable), and one that busts system + request + candidate is refused. A test asserting "fits alone, refused with history" would be asserting the bug.
- [x] Mutation: charging `truncated = false`, and measuring `self.blocks` instead of the request block, each fail a named test.

## Technical Notes

- The stage vocabulary (`SkillStage::{Body, WithDynamicContext}`) is reused verbatim — a model invocation has the same two stages for the same reason.
