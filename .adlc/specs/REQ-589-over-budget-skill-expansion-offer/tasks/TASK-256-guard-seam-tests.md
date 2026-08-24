---
id: TASK-256
title: "Seam tests for the redundant guards, and the one-home rule"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-245, TASK-246]
---

## Description

LESSON-508 + LESSON-546. Guards whose deletion is silent need their own tests; a one-home rule needs a resident test, not a grep in a task file.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — unit test for the suspension seam
- `crates/tetond/src/harness/permissions.rs` — unit test for non-persistence
- `crates/tetond/tests/` — the one-home test for the recipe window literal

## Acceptance Criteria

- [ ] Deleting ADR-8's suspension reddens a seam-level unit test, not merely an end-to-end one (LESSON-508)
- [ ] Deleting BR-10's non-persistence guard reddens a test
- [ ] The recipe window literal appears exactly once outside `#[cfg(test)]` (LESSON-546)
- [ ] Each test's doc comment states WHY it exists — that the guard's removal would otherwise be silent

## Technical Notes

LESSON-508's point is that these tests look redundant and are not: the paths that would catch the regression cannot currently reach the case.
