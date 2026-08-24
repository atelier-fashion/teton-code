---
id: TASK-243
title: "Offer, decline, and accepted sentences — arms on the one composer"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-242]
---

## Description

BR-5. `skill_refusal` (971) is the single composer and stays that way. Add arms; do not fork. The accepted path must NOT emit the refusal's "no provider saw this turn" clause, which becomes false the moment a user proceeds.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — new arms in the `skill_refusal` module; remedy-sentence renderer beside `bound_clause` (1024)

## Acceptance Criteria

- [ ] The offer, the decline refusal, and the accepted record are distinct sentences from one module (BR-5)
- [ ] The accepted path never emits "no provider saw this turn" (AC-11), asserted negatively
- [ ] `ExceedsWindow` states the window will be blown and that proceeding will very likely be rejected; `WindowUnknown` states the daemon cannot promise; `FitsWindow` claims neither (AC-6) — each arm pins its own wording
- [ ] A `RaiseWindow` offer cannot render without BR-7a's risk sentence (AC-7a)
- [ ] A `LocalEngine` offer names both halves and the cost consequence, and never offers a max_context write for the local tier (AC-8)
- [ ] Option labels name the concrete write (`capabilities.max_context = 1000000` for `kimi`), never "raise the limit" (ADR-1)
- [ ] No provider response body can reach any sentence — extend the `a_skill_refusal_carries_no_provider_response_body` pattern to the new arms

## Technical Notes

`the_refusal_names_the_skill_its_size_the_budget_and_the_bound` (budget.rs:2110) is the assertion shape: compute the expected size independently via `approx_tokens` + `SEED_OVERHEAD_BYTES` rather than re-calling the estimator.
