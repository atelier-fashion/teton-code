---
id: TASK-265
title: "Move the piped-invocation e2e test to this branch"
status: complete
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-264]
---

## Description

ADR-5. `cli_e2e::a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget` was broken by the trust gate and repaired by `4be0c34`'s `spawn_scripted_trusting`. It cannot pass on a branch without the trust infrastructure.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — the test and its `spawn_scripted_trusting` fixture

## Acceptance Criteria

- [ ] The test and its fixture are present and passing on THIS branch
- [ ] `an_unattended_session_at_an_unlisted_root_refuses_and_names_the_row` is here too — it is trust-only
- [ ] A note in the test records that it is the split's sharpest signal (see TASK-266's AC)

## Outcome

**Satisfied by TASK-264 without separate work.** `4be0c34` carried both the test and
`spawn_scripted_trusting` in the cherry-pick, so the "move" was already performed. Verified by
running both tests on this branch rather than inferring it from the presence of the fixture.

## Technical Notes

Do not rewrite the test to avoid needing trust. Its dependence on the gate is the fact being preserved.
