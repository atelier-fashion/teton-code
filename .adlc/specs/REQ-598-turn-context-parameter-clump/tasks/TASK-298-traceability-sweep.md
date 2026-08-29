---
id: TASK-298
title: "Traceability sweep: disappearance, re-association, and orphaning"
status: draft
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-295, TASK-296]
---

## Description

Implement AC-8's three-arm region check with a vacuity floor.

A per-file set diff of REQ/ADR/LESSON/BUG ids is **not sufficient**. The live
evidence: when REQ-597 rebased onto REQ-596, a method was inserted between
`config_snapshot`'s doc comment and its attribute, orphaning the comment from
the item it documents. No id left the file — set identical, count identical,
defect present. That defect happened twice in this file in a single day, before
this REQ's refactor started.

## Files to Create/Modify

- `crates/tetond/tests/traceability_sweep.rs` — the sweep

## Acceptance Criteria

- [ ] **Disappearance arm**: an id present in a touched file before the refactor
      is absent afterwards **anywhere in the workspace**. Workspace-scoped so a
      genuine file-to-file move is not a false positive.
- [ ] **Re-association arm**: for every id, the set of item names (fn / struct /
      impl) whose attached doc-comment block carries it is unchanged, except
      where the PR body names a rename explicitly.
- [ ] **Orphan arm**: a `///` run, or a `//` run immediately preceding an item,
      that is separated from its item by a blank line or an intervening item.
      This is the hazard arm (LESSON-585 — key the sweep on the hazard, not on
      the remedy's shape).
- [ ] **The orphan arm is demonstrated**: reproduce the exact REQ-596/597
      insertion against `config_snapshot` — put a method between its doc comment
      and its attribute — confirm the sweep goes **red**, and revert.
- [ ] **Vacuity floor**: the sweep asserts it saw at least the known number of
      ids and annotated items. A sweep's failure mode is seeing less, and every
      site it misses makes it pass more easily (LESSON-585).
- [ ] The floor is shown to work: narrow the selector so it matches fewer sites
      and confirm the floor fails loudly rather than the sweep passing quietly.

## Technical Notes

Id pattern: `(REQ|ADR|LESSON|BUG|TASK|ASSUME)-\d+`. Include `TASK` and `ASSUME` —
`runtime.rs` carries `REQ-558 TASK-054` and similar pairs, and dropping the task
half loses half the reference.

"Attached doc-comment block" means the contiguous run of `///` or `//` lines
immediately preceding an item, allowing `#[...]` attributes between the comment
and the item — that adjacency is exactly what the orphan arm tests, so the
parser must treat an attribute as part of the item, not as a separator.

Capture the "before" side from `git show <base>:<path>` rather than from a
snapshot file, so the sweep cannot drift out of date with the base commit.

Do not make this sweep a `#[cfg(feature = ...)]` target. LESSON-515: a
feature-gated target is invisible to every refactor, and this one exists to
watch refactors.
