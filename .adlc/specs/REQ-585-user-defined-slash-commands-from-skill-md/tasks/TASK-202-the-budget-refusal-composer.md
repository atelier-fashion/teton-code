---
id: TASK-202
title: "The refusal: a spoken bound, a floored clause, and a code that is not the provider's"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-196, TASK-197]
---

## Description

BR-8's message, composed once, beside REQ-586's other budget prose. Nothing
here decides *when* to refuse — that is TASK-204's ordering — only what the
refusal says and how it is measured.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — `SkillFit`, `skill_refusal(...)`, the two-stage classification

## Acceptance Criteria

- [ ] The message names four things: the skill, its size, the budget, and the bound — and says which stage refused (body alone, or body plus dynamic output).
- [ ] The bound is **spoken**: `BudgetBound::words()` (`crates/teton-protocol/src/events.rs:2202`) — `window`, `unknown window`, `redact scan`, `user cap`, `local engine`. Never `wire_name()`. REQ-586 put `words()` in the protocol crate expressly so this refusal could reach it without minting a second adjective table, and its doc names BR-8 by number (BR-8a, LESSON-528).
- [ ] A **floored** route says so. `RouteBudget.floored` is carried beside `bound` precisely because `bound` alone cannot report that a declared ceiling is not in force — an Ollama-shaped route otherwise reads `bound: window` beside a budget larger than the window it declared (BR-8b).
- [ ] Figures are spelled with `thousands()` and `bytes_figure()` — both already imported at `budget.rs:85`. No local number formatting.
- [ ] The `unknown window` message carries the remedy: `set capabilities.max_context for <id>`. `sanitized_provider_id` is used for the id (it is a config-supplied string reaching a message).
- [ ] Measurement is `ContextManager::would_seed_fit` from TASK-197 — the same estimators the pressure path uses. `budget.rs` must **not** re-derive a budget; `Router::budget_for` stays the single `budget::derive` caller.
- [ ] The refusal carries **no provider response body**, pinned negatively as REQ-586 pinned its sibling (`runtime.rs:27418`: `!err.message.contains("Input token length")`).
- [ ] Unit-tested against the four AC-16 route shapes with the real corpus sizes: local (`/status` fits, `/proceed` refused with `bound: local engine`), `max_context = 128000` (`/proceed` fits), `max_context = 0` (`bound: unknown window` + the remedy), `max_context = 4096` (both refused, floored clause present).
- [ ] Mutation table: printing `wire_name()`, dropping the floored clause, and dropping the stage distinction each fail a named test.

## Technical Notes

- `SKILL_EXPANSION_TOO_LARGE = -32023` (TASK-196), not `CONTEXT_LENGTH_EXCEEDED`. Different cause, different remedy: one means a provider refused a turn it received, the other means Teton refused to send it. Collapsing them makes AC-16's "a typed outcome, not a clamped turn" uncheckable.
- Compose beside `big_window_notice` so the budget vocabulary keeps one home.
