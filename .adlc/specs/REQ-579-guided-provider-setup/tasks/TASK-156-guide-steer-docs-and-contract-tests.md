---
id: TASK-156
title: "Guide steer line, provider docs, and the catalog↔plan contract test"
status: complete
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-153"]
---

## Description

Teach the model to hand off. Edit `self_config.md` line 2 so the first thing it names for connecting a provider is `/provider setup <vendor> [tier]` (in a session) and the second is `teton provider add` (from a shell); keep the recipe list on line 5 verbatim (it is the BR-11 answer and the vendor spellings the model uses). Update `harness/docs/providers.md` to lead with the same hand-off. Add the contract test that pins the daemon's `plan.catalog` to `recipe_catalog()` (ADR-4). Prove the prompt-size ceiling still clears.

**Covers:** AC-1 (the guide sentence), AC-5 (catalog↔plan contract)

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — line 2: replace "point them at `teton provider add` or `/web setup`, which read it echo-off into the keychain" with wording that names `/provider setup <vendor> [tier]` first, then `teton provider add` from a shell, then `/web setup` for web; byte count must not exceed the current line by more than the ceiling test's margin — aim for neutral or smaller
- `crates/tetond/src/harness/docs/providers.md` — lead with the in-session hand-off; keep the CLI recipes as the scripted path
- `crates/tetond/src/harness/turn_loop.rs` (tests) — update the test that asserts the guide's provider sentence (search `provider add` in the test module ~L2191+) to assert `/provider setup` is named AND `teton provider add` is still named; assert the guide does NOT instruct the model to ask for a key
- `crates/tetond/src/egress/redact.rs` (test) — `the_total_cap_clears_the_harness_context_budget_with_margin` must still pass; if the margin shrinks, record the before/after bytes in the task's completion note
- `crates/tetond/tests/provider_setup_contracts.rs` — new; for every `ProviderRecipe` in `recipe_catalog()`, the corresponding `ProviderRecipeEntry` in `DaemonRuntime::provider_setup_plan().catalog` is field-equal (mirror `web_setup_contracts.rs`); the guide's per-vendor line names each `guide_spelling` (reuse the existing REQ-577 guide gate if it already asserts this — do not duplicate)

## Acceptance Criteria

- [ ] `self_config.md` names `/provider setup` before `teton provider add`; the recipe list is unchanged
- [ ] The prompt ceiling test passes; the guide file's byte size is ≤ the current size + the documented margin
- [ ] The contract test fails if a recipe field or entry is added on one side and not the other
- [ ] `cargo test -p tetond` green

## Technical Notes

ADR-3 explains why this is a guide edit and not a `*_capability_clause`. Do not touch `web_capability_clause`. The turn_loop test at ~L2191 (`the_system_prompt_bundles_tetons_own_provider_setup` or similar) is the one that pins the sentence; keep its name. If the ceiling test fails, shorten line 2 — do not raise the ceiling.
