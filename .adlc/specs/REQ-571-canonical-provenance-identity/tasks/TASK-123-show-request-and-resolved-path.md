---
id: TASK-123
title: "Show both request and resolved path when they differ"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-11. When a resolved path differs from the request string, `read`
and `edit` output must show both, so a model reading through a symlink or an
absolute path is not told it read something other than what it read.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/read.rs` — output text shows request and resolved form on divergence.
- `crates/tetond/src/harness/tools/edit.rs` — same.
- `crates/tetond/tests/symlink_posture.rs` — add the AC-15 cases.

## Acceptance Criteria

- [ ] AC-15: when request and resolved path differ, `read` output contains both.
- [ ] AC-15: when they match — the overwhelmingly common case — output is byte-identical to today.
- [ ] Display remains separate from provenance: only the `ProvenanceId` governs enforcement, and changing the displayed text cannot alter a boundary verdict.
- [ ] `edit`'s success line carries the same treatment.
- [ ] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Verified safe at spec time: nothing currently depends on the echoed string — no
test asserts on it, and `with_paths` has seven call sites, all within the four
tools and two egress tests. The byte-identical-when-matching criterion is what
keeps that true for the existing suites.

Keep the divergence rendering compact; this text enters model context on every
read, so a verbose form costs tokens on every turn.
