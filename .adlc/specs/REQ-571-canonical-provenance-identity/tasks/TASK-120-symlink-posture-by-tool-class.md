---
id: TASK-120
title: "Split symlink handling by tool class"
status: complete
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

- [x] AC-3: an in-repo symlink pointing at a boundary-protected file is attributed by `read`/`edit` to the resolved target, the turn is blocked, and no captured payload contains the protected bytes.
- [x] A symlink resolving outside the repo root is refused by `read`/`edit` with a jail error.
- [x] AC-4: `grep`/`glob` skip both an inside-root and an outside-root symlink — neither file's content is surfaced, and neither is reported under an in-jail relative path.
- [x] AC-4: a symlink **cycle** (`a -> b`, `b -> a`) terminates the walk rather than hanging or overflowing the stack. The test has a bounded runtime.
- [x] The walkers test `is_symlink()` explicitly — not inferred from `!is_dir()`.
- [x] AC-9 regression: the six existing egress suites still pass.
- [x] `cargo clippy --all-targets` clean.

## Technical Notes

`std::fs::DirEntry::file_type()` does NOT traverse symlinks (that is the root
cause); `std::fs::metadata` does. Use `file_type().is_symlink()` on the entry.

Create links in tests with `std::os::unix::fs::symlink`. Follow the temp-root
fixture at `crates/tetond/src/harness/tools/read.rs:94-110` (pid + timestamp +
atomic counter) so concurrent tests never share a root.

Skipping in the walkers is deliberate, not an oversight to fix later — two
provenance ids for one file identity is exactly what ADR-A exists to prevent.
Document that in the code so a future contributor does not "fix" it.

## Implementation Notes (as landed)

Recorded for TASK-121/123, which build on these files.

- **`read`/`edit` needed no production change.** TASK-119's `ToolContext::resolve`
  already canonicalizes, and canonicalization *is* the ADR-C posture for the
  explicit-access class: an in-root link resolves to its target, so both halves of
  `Resolved` describe the target and the link's own name never becomes an
  identity; an out-of-root link fails the pre-existing `starts_with(&root)` check.
  So the AC-3 work was to *prove* it end-to-end rather than to build it — the two
  files carry only a comment recording that a symlink is a spelling like any
  other. Unverified-by-construction was the whole of REQ-571's bug, so the tests
  are the deliverable here.
- **The walkers share one predicate, `tools::skip_symlink_entry`,** rather than
  each spelling `file_type().is_symlink()`. Same reasoning as TASK-119 replacing
  the copied `strip_prefix` idiom with `ProvenanceId::from_resolved`: a rule of
  the form "every walker does X" is a convention, and this REQ exists because a
  convention failed. The full rationale (two ids for one identity; cycles;
  ripgrep's default; why `DirEntry::file_type` not traversing is the root cause
  and `!is_dir()` therefore reproduces the bug) lives on that one function's doc,
  which is what a future contributor tempted to follow links will land on.
- **Mutation-checked, both walkers.** Deleting the skip from `grep` and `glob`
  fails three of the seven new tests; the grep failure prints the bug verbatim —
  `outside-link.txt:1: OUTSIDE-ONLY-…`, i.e. content from outside the jail
  surfaced under an in-jail relative path.
- **The cycle was already terminating, for the wrong reason.** Pre-fix, a link
  never satisfied `is_dir()` — that is the bug — so a walk never descended
  through one and `a -> b, b -> a` merely produced two `ELOOP` read failures. The
  cycle test therefore passes before and after; it is a *regression* guard aimed
  at whoever later teaches a walker to follow links without a visited-set, which
  is why it also plants `deep/up -> ..` (a directory link to its own ancestor, the
  shape that actually recurses forever). Grep + glob over the cyclic tree: **940µs
  against a 10s budget**.
- **What AC-4's "neither file's content is surfaced" means for the in-root link.**
  Its target is inside the jail, so the walk reaches that target directly and
  reports it — correctly, under its own name. The harm being excluded is the
  *second* identity, so the assertion is that the id set is exactly the two real
  files and the link name appears nowhere. For the out-of-root link the assertion
  is the stronger one: its bytes appear nowhere at all.
- **For TASK-121.** The one symlink shape this task does not settle is the
  *dangling* link: `canonicalize()` fails, `resolve` falls back to the lexical
  path, and a link to a not-yet-existing file outside the root is therefore
  minted under the link's own in-jail name. Nothing is surfaced today (the
  subsequent `read_to_string` fails), but that fallback is TASK-121's, and this is
  the case it should reason about.
