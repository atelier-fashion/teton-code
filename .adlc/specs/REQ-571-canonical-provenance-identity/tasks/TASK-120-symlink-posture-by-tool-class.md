---
id: TASK-120
title: "Split symlink handling by tool class"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-5 (ADR-C): `read`/`edit` resolve links and attribute to the target;
`grep`/`glob` skip symlink entries entirely. Closes the walker jail escape where
`DirEntry::file_type()` reports a link as not-a-directory, dropping it into the
file branch whose read *does* follow it.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/mod.rs` — link-resolution helper shared by the explicit-access tools.
- `crates/tetond/src/harness/tools/read.rs` — attribute in-root links to target; refuse out-of-root.
- `crates/tetond/src/harness/tools/edit.rs` — same.
- `crates/tetond/src/harness/tools/grep.rs` — skip entries where `file_type().is_symlink()`.
- `crates/tetond/src/harness/tools/glob.rs` — same.
- `crates/tetond/tests/symlink_posture.rs` — new. AC-3 and AC-4.

## Acceptance Criteria

- [ ] AC-3: an in-repo symlink pointing at a boundary-protected file is attributed by `read`/`edit` to the resolved target, the turn is blocked, and no captured payload contains the protected bytes.
- [ ] A symlink resolving outside the repo root is refused by `read`/`edit` with a jail error.
- [ ] AC-4: `grep`/`glob` skip both an inside-root and an outside-root symlink — neither file's content is surfaced, and neither is reported under an in-jail relative path.
- [ ] AC-4: a symlink **cycle** (`a -> b`, `b -> a`) terminates the walk rather than hanging or overflowing the stack. The test has a bounded runtime.
- [ ] The walkers test `is_symlink()` explicitly — not inferred from `!is_dir()`.
- [ ] AC-9 regression: the six existing egress suites still pass.
- [ ] `cargo clippy --all-targets` clean.

## Technical Notes

`std::fs::DirEntry::file_type()` does NOT traverse symlinks (that is the root
cause); `std::fs::metadata` does. Use `file_type().is_symlink()` on the entry.

Create links in tests with `std::os::unix::fs::symlink`. Follow the temp-root
fixture at `crates/tetond/src/harness/tools/read.rs:94-110` (pid + timestamp +
atomic counter) so concurrent tests never share a root.

Skipping in the walkers is deliberate, not an oversight to fix later — two
provenance ids for one file identity is exactly what ADR-A exists to prevent.
Document that in the code so a future contributor does not "fix" it.
