---
id: TASK-392
title: "Thread the runtime window into the budget derivation"
status: draft
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-390]
---

## Description

Split `LOCAL_ENGINE_N_CTX`'s two jobs (ADR-616-1): it becomes
`LOCAL_ENGINE_N_CTX_DEFAULT`, the window used when no real engine is loaded, and
`derive`'s local arm reads the loaded window from its inputs instead. Add
`window_tokens` to `RouteBudget` and `RouteDecided` so every surface can report
the window beside the derived pair.

This is the task that makes the local budget follow the engine (BR-1, BR-5).

## Files to Create/Modify

- `crates/tetond/src/runtime/engine.rs` — rename the constant to
  `LOCAL_ENGINE_N_CTX_DEFAULT`; add `LocalEngineWindow` and the daemon-held
  current window
- `crates/tetond/src/harness/budget.rs` — `BudgetInputs::local(window)`; local
  arm reads it; `RouteBudget::window_tokens`
- `crates/tetond/src/router.rs` — pass the live window through `budget_for`
- `crates/teton-protocol/src/events.rs` — `RouteDecided::window_tokens`, an
  additive optional field
- `crates/tetond/tests/runtime_visibility.rs` — update the pinned constant
  spelling the visibility parser asserts

## Acceptance Criteria

- [ ] With no engine loaded the derived pair is unchanged: 21,162 words /
      63,488 bytes, `bound = local_engine`. Every existing assertion on those
      figures passes untouched — that is the ADR-616-1 property
- [ ] At a window of 262,144 the pair is 174,080 words / 522,240 bytes and
      `window_tokens = 262,144` (AC-1)
- [ ] `RouteDecided.window_tokens` is populated on every route, local and
      remote; on `kimi-k3` at `max_context = 1,000,000` it reads 1,000,000 with
      `budget_tokens = 665,984` (AC-2)
- [ ] `window_tokens` is `#[serde(skip_serializing_if = "Option::is_none",
      default)]` — additive, no `PROTOCOL_VERSION` bump (REQ-573 BR-2)
- [ ] The digest fraction is unchanged and still divides by
      `LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES`; both constants are retained
      (BR-5, BR-8)
- [ ] A 500,000-byte / 62,000-word local prompt is served without
      `context_pressure` at 262,144, and the *same* prompt against a stub engine
      at 32,768 **does** emit it — the stated mutation (AC-3)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/harness/budget.rs::local_arm_reads_the_loaded_window` | no |
| BR-5 | test-case | `crates/tetond/src/harness/budget.rs::both_halves_follow_the_window_on_every_route` | no |
| AC-1 | test-case | `crates/tetond/src/harness/budget.rs::local_pair_at_262144` | no |
| AC-2 | test-case | `crates/tetond/src/harness/budget.rs::kimi_window_and_budget_reported` | no |
| AC-3 | test-case | `crates/tetond/tests/skill_over_budget_offer.rs::large_prompt_served_at_262144_pressured_at_32768` | yes |
| AC-7 | test-case | `crates/tetond/tests/token_corpus.rs::reference_workload_no_context_pressure_at_262144` | yes |

## Technical Notes

- **Do not renumber the existing assertions.** The default window stays 32,768
  precisely so they keep passing. If an existing test starts failing, that is a
  signal the split leaked, not a number to update.
- LESSON-599 applies to the rename: bound it to code tokens and diff the prose
  separately (`git diff origin/main..HEAD | grep '^[-+].*//'`) — a word-boundary
  regex reaches doc comments and string literals, which the compiler cannot
  check.
- `runtime_visibility.rs` asserts the literal source text
  `pub(crate) const LOCAL_ENGINE_N_CTX: u32 = 32_768;`. It must be updated in
  lockstep; leaving it stale is a guard that has stopped covering its subject
  (LESSON-598).
