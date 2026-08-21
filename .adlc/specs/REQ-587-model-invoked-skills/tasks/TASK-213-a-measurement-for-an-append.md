---
id: TASK-213
title: "Would this *append* fit — the measurement `skill_fit` does not make"
status: draft
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

- [ ] `ContextManager::would_append_fit(&self, text, budget_tokens, budget_bytes) -> Fit` measures the **existing** blocks plus the candidate, through the same private `tokens_of`/`bytes_of` the pressure path uses, and charges `truncated = true` for the reason `would_seed_fit` does: `bytes_of` adds the 142-byte note only once truncation has happened, so a check that omits it passes and is then clamped.
- [ ] The refusal sentence is caller-aware. `skill_refusal` hard-codes `` `/{skill}` `` and *"never shortened into something you did not invoke"* — a model invocation wears neither. Add the caller as a parameter; do **not** copy the composer, and do not mint a second bound table: `BudgetBound::words()` stays the one adjective vocabulary and `bytes_figure`/`thousands` stay the one number vocabulary.
- [ ] `SkillFit::TooLarge` must be renderable as a **tool result**, not only as an `RpcError`. Today every one of the four raise sites ends the prompt turn; BR-6 and BR-9 say a model-facing refusal is a typed outcome the model can relay.
- [ ] Unit-tested against a conversation with prior blocks, so the difference from `would_seed_fit` is observable: a candidate that fits as a seed and does not fit as an append.
- [ ] Mutation: charging `truncated = false`, and reusing `would_seed_fit` for the append case, each fail a named test.

## Technical Notes

- The stage vocabulary (`SkillStage::{Body, WithDynamicContext}`) is reused verbatim — a model invocation has the same two stages for the same reason.
