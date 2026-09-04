---
id: TASK-004
title: "The edit tool refuses a write under a non-project root"
status: complete
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001, TASK-002]
---

## Description

BR-4's `edit` half — the second of the rule's two enforcement points.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/edit.rs` — the gate, before path resolution.

## Acceptance Criteria

- [ ] At a `Home` / `FilesystemRoot` root, `edit` is refused before the file is
      read or written; the target file is unchanged on disk afterwards
      (inspected, not inferred).
- [ ] The refusal names the root display, the kind and the remedy, and publishes
      `write_refused_non_project` with `tool: "edit"`.
- [ ] At a `Project` root and at a `Plain` root, `edit` behaves exactly as
      today — REQ-613's `TETON.md` write at a plain root keeps working (BR-4's
      carve-out, BR-9).
- [ ] `cargo test -p tetond` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/src/harness/tools/edit.rs::an_edit_at_a_home_root_is_refused_and_writes_nothing` | yes |
| BR-9 | test-case | `crates/tetond/src/harness/tools/edit.rs::an_edit_at_a_plain_root_still_writes` | yes |
| AC-3 | test-case | `crates/tetond/src/harness/tools/edit.rs::an_edit_at_a_home_root_is_refused_and_writes_nothing` | yes |

## Technical Notes

`edit` reaches a `RootKind` the same way `shell` does — `ctx.root_kind()`. The
gate is a call to `root_gate::write_gate`; `edit` is unconditionally a write, so
it passes a marker rather than a command string. Give `write_gate` a sibling
`edit_gate(kind) -> WriteVerdict` in TASK-002's module rather than fabricating a
fake command — one module, two entry points, one table of kinds.
