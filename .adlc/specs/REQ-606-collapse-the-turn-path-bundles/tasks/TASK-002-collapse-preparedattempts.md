---
id: TASK-002
title: "Collapse PreparedAttempts and delete the refit_system round-trip"
status: complete
parent: REQ-606
created: 2026-09-01
updated: 2026-09-01
dependencies: [TASK-001]
---

## Description

AC-1 and AC-3. `prepare_the_attempts` receives `system: &str`, clones it as
`refit_system`, and returns the clone to a caller that still holds the original
unmutated. The field is transport of a value that never left, and removing it
takes `PreparedAttempts` to two fields — where Rule R makes it a tuple.

Behaviour-identical by construction: `refit_system` is assigned once from
`system.to_owned()` and never rebound, and `system` is never rebound in
`run_prompt_turn`.

## Files to Modify

- `crates/tetond/src/runtime/turn.rs`

## Acceptance Criteria

- [ ] `struct PreparedAttempts` is gone; `prepare_the_attempts` returns
      `(AttemptState, usize)`
- [ ] `AttemptInputs.refit_system` is borrowed from `system` at the call site
- [ ] One fewer `String` allocation per turn
- [ ] `run_prompt_turn`'s body span shrinks; re-measured, not assumed
- [ ] Suite green; clippy 0 under `deny`; fmt clean
