---
id: TASK-240
title: "Recipe window lookup, verified_on field, and the clears-the-refusal guard"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

ADR-6 + ADR-7. BR-7c proposes a window value from the vendor recipes. Match on `example_model` (not `id_suggestion`), promote the comment-only verification date to a field, and refuse to propose a value that would not actually clear the measured expansion.

## Files to Create/Modify

- `crates/tetond/src/provider_recipes.rs` — add `verified_on` to `ProviderRecipe` (~100) and populate every catalog entry from its existing comment; add a lookup helper
- `crates/tetond/src/harness/budget.rs` — the proposal call site

## Acceptance Criteria

- [x] Lookup matches the registered provider's `model` against a recipe's `example_model`, imitating `runtime.rs:6746`; matching by id is absent
- [x] Ollama's 4,096 recipe window — smaller than the local pair — is NOT proposed, because it would not clear the measurement; the offer asks for a value instead
- [x] A provider matching no recipe proposes nothing and asks (BR-7c, **AC-21**) — a test asserts the offer cannot render a number the recipe table does not contain
- [x] `verified_on` is a field, not a comment, and every catalog entry carries the date already in its comment
- [x] A resident one-home test asserts the window literal appears once (LESSON-546)

## Technical Notes

ASSUME-016 is the standing warning: every recalled vendor window in REQ-586 was wrong. Do not re-verify the numbers in this task — they were verified 2026-08-19 with cited URLs; carry them across faithfully.
