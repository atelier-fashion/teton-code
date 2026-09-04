---
id: TASK-005
title: "The environment block dictates the ending at a non-project root"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

BR-3, plus the ceiling re-derivation and the ONE-line test's replacement
(architecture ADR-3). This is the task that changes a pinned invariant, so its
reasoning is in the doc comments, not only here.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — `environment_block_with_projects`,
  `environment_block_ceiling`, `worst_case_session_root`, and the ONE-line test.

## Acceptance Criteria

- [ ] At `Home` / `FilesystemRoot`, the block carries BR-3's dictation verbatim:
      *"This is not a project. Do not create files or directories here. If the
      task needs a project, stop and ask the user to run `/cd <name>`; you cannot
      move the root yourself."*
- [ ] At `Project` and at `Plain`, the block is **byte-identical to today** — no
      dictation, no extra line (BR-9).
- [ ] `environment_block_ceiling()` is the **measured** larger of the worst-case
      project row and the worst-case home row, computed by calling the builder.
      No arithmetic budget.
- [ ] The known-projects shrink loop still reaches step 1 (names that fit) at a
      home root with the dictation present — it does not starve to step 3.
- [ ] A hostile project **name** still cannot add a line: the replacement
      assertion pins `matches('\n').count() <= 2` and pins that the count is the
      same with and without a name containing `\n`.
- [ ] No control or bidi character reaches the line (the existing assertion is
      kept unchanged).
- [ ] `cargo test -p tetond turn_loop` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/src/harness/turn_loop.rs::the_environment_block_dictates_the_ending_at_a_non_project_root` | yes |
| BR-3 | test-case | `crates/tetond/src/harness/turn_loop.rs::known_projects_ride_a_non_project_line_within_the_ceiling` | no |
| BR-9 | test-case | `crates/tetond/src/harness/turn_loop.rs::a_project_block_is_unchanged_by_this_req` | yes |

## Technical Notes

The ONE-line test is being **replaced, not deleted**, and the replacement must
still fail for the reason the original existed. Its purpose was never "one line";
it was "user-controlled data cannot add structure". Assert that directly:
render with a name containing `\n` and with a benign name, and assert both
produce the **same** newline count. That is strictly stronger than the old
count-is-1 assertion and survives the dictation.

Re-measure `environment_block_ceiling` by extending `worst_case_session_root`
into a pair of worst cases and taking the max of the two rendered lengths. Do not
add the dictation's length to the old number — that is the arithmetic derivation
the current doc comment exists to forbid.

This task does **not** touch `RECORDED_PROMPT_MARGIN_BYTES`. TASK-009 measures
the composed artifact after every writer has landed.
