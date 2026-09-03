---
id: TASK-383
title: "The write: create-new with `O_NOFOLLOW`, the header line, cleanup on failure, and `--force` by rename"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

ADR-5. `write_new` and `replace` on real filesystems (tempdir tests), plus the header composer.
No precedent for `create_new` exists in the tree; this task establishes it with the transcript
writer's `O_NOFOLLOW` discipline.

## Files to Create/Modify

- `crates/tetond/src/repo_context/write.rs` — `write_new(root, body) -> Result<Written,
  WriteFailure>` (`create_new(true)`, `mode(0o644)`, `O_NOFOLLOW`, whole-buffer write, remove on
  error), `replace(root, body)` (temp `create_new` + `rename`), `WriteFailure { AlreadyExists,
  Symlink, Io(kind) }`.
- `crates/tetond/src/repo_context/render.rs` — `generated_header(tier, date, stop, cut) -> String`
  (one line, ≤ 200 bytes, golden).
- `crates/tetond/src/repo_context/mod.rs` — `pub mod write;`.

## Acceptance Criteria

- [ ] BR-6: writing where a file exists returns `AlreadyExists` and changes nothing (bytes and
      mtime); a symlink at the path is refused; a write that fails after create (injected
      short write via a read-only directory or a full-disk seam) leaves no file.
- [ ] `replace` leaves either the old or the new bytes, never a truncated file, when interrupted
      between temp write and rename (simulate by asserting the temp path is used and the rename
      is the last operation).
- [ ] The header golden names the tier, date, and a cut or stop when present; ≤ 200 bytes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/tetond/src/repo_context/write.rs::write_new_refuses_an_existing_file_and_a_symlink_and_leaves_nothing_on_failure` | yes |
| AC-8 | test-case | `crates/tetond/src/repo_context/write.rs::write_new_refuses_an_existing_file_and_a_symlink_and_leaves_nothing_on_failure` | yes |
| AC-9 | test-case | `crates/tetond/src/repo_context/write.rs::a_failed_write_leaves_no_partial_file` | yes |

## Technical Notes

Mode is `0o644` on purpose (a committed file), not the transcript's `0o600`; say so in the doc
comment. Use `std::fs::remove_file` on the error path and ignore `NotFound` there only.
