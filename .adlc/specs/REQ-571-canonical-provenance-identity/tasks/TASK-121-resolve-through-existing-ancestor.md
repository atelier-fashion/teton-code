---
id: TASK-121
title: "Decide containment against a resolved ancestor, not a lexical path"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-120]
---

## Description

Implement BR-6. `ToolContext::resolve` currently does
`normalized.canonicalize().unwrap_or(normalized)` — when the target does not
exist it falls back to the lexical form, so a path traversing a symlinked
directory to a non-existent leaf passes `starts_with(root)` and the OS resolves
the link on open.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/mod.rs` — canonicalize the deepest existing ancestor and re-join the remaining components before the containment check.
- `crates/tetond/tests/symlink_posture.rs` — add the AC-6 case.

## Acceptance Criteria

- [ ] AC-6: a not-yet-existing path routed through a symlinked directory (`link/new` where `link` resolves outside the root) is refused.
- [ ] A not-yet-existing path under a genuine in-root directory is still accepted — the fix must not break creating new files in the repo.
- [ ] The lexical `unwrap_or` fallback is gone; no code path decides containment on a path that traversed unresolved components.
- [ ] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Walk up until `canonicalize()` succeeds, then re-join the untraversed tail and
check `starts_with(root)`. Not exploitable for writes today — `edit` requires a
successful `read_to_string` first, which forces canonicalize to have run — but
it becomes a write primitive the moment a `write`/`create` tool is added, which
is why it is closed prospectively rather than deferred.

Sequenced after TASK-120 because both edit `resolve()`; the dependency exists to
avoid a conflicting concurrent edit, not because of a logical ordering need.
