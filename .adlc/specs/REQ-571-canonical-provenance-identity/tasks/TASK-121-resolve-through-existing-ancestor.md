---
id: TASK-121
title: "Decide containment against a resolved ancestor, not a lexical path"
status: complete
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

- [x] AC-6: a not-yet-existing path routed through a symlinked directory (`link/new` where `link` resolves outside the root) is refused.
- [x] A not-yet-existing path under a genuine in-root directory is still accepted — the fix must not break creating new files in the repo.
- [x] The lexical `unwrap_or` fallback is gone; no code path decides containment on a path that traversed unresolved components.
- [x] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Walk up until `canonicalize()` succeeds, then re-join the untraversed tail and
check `starts_with(root)`. Not exploitable for writes today — `edit` requires a
successful `read_to_string` first, which forces canonicalize to have run — but
it becomes a write primitive the moment a `write`/`create` tool is added, which
is why it is closed prospectively rather than deferred.

Sequenced after TASK-120 because both edit `resolve()`; the dependency exists to
avoid a conflicting concurrent edit, not because of a logical ordering need.

## Implementation Notes (as landed)

- **One private helper, `canonical_through_existing_ancestor`**, replacing the
  `unwrap_or` in-line. It walks up from the normalized path until `canonicalize`
  succeeds, then re-joins the untraversed tail onto that canonical ancestor;
  `starts_with(root)` then decides on a value whose every existing component was
  actually traversed. An existing path takes the first iteration and costs the
  same one syscall as before, so nothing about the resolved case changed.
- **The walk alone does not close the dangling link** — which is why TASK-120's
  hand-off mattered. `canonicalize` fails for a *broken* link exactly as it does
  for a not-yet-created file, so a plain ancestor walk would pop `notes.txt`,
  canonicalize the root, re-join, and accept it under its own in-jail name: the
  discarded bug, reproduced. The helper therefore refuses when a component
  `symlink_metadata`s successfully but does not canonicalize — the entry exists,
  so its failure is resolution, not absence, and the daemon cannot say where an
  open would land (ADR-B's no-fallback rule). A permission-denied component lands
  here too, which is the right side to fail on.
- **The accept case is what makes the fix the right shape.** Refusing everything
  unresolvable would also close BR-6 and would break creating files in the repo,
  so `src/new.rs`, `src/deep/new.rs` (missing intermediate directory) and
  `link/new.rs` through an *in-root* link all still resolve — the last one to
  `src/new.rs`, target-attributed like every other spelling (ADR-C). Each refusal
  test is paired with its accept case rather than left to a separate file.
- **A new refusal sentence.** `path ... cannot be resolved: it passes through a
  broken symlink` is distinct from the escape message, so a model (and the tests)
  can tell "you asked for something outside" from "this path has no answer".
- **Mutation-checked.** Restoring `canonicalize().unwrap_or(lexical)` fails all
  six new tests (3 unit, 3 integration) and no others; the current code passes
  every pre-existing test in the file and the crate.
- **Not exploitable today, closed anyway.** `read`/`edit` both `read_to_string`
  before doing anything, so the failed open masked this. The `edit` test asserts
  the outside directory gained no file, so the refusal a future `write`/`create`
  tool inherits is already proven rather than assumed.
