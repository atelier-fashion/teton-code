---
id: TASK-318
title: "Verify the move and record the before/after measurement with its counting rule"
status: complete
parent: REQ-603
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-317]
---

## Description

Establish that the relocation changed nothing but location, and produce the
AC-6 figures with the counting rule stated beside them.

## Files to Create/Modify

- `.adlc/specs/REQ-603-extract-the-session-lifecycle-slice/architecture.md` — record measured after-figures

## Acceptance Criteria

- [ ] `cargo test --workspace --no-fail-fast` run and the output grepped for `FAILED` (LESSON-533 — a summed pass count is a floor, not a total)
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo fmt --check` clean
- [ ] `runtime_module_map.rs`, `runtime_doc_paths.rs`, `runtime_visibility.rs` all green
- [ ] `mod.rs` production lines reported before and after, with the counting rule
- [ ] AC-1 reported explicitly: the slice's derivation (architecture.md ADR-1) is restated as a method — impl-structure enumeration, field-touch mapping, adjacency check — with the counting rule beside every figure, and with the Assumption's two-part resolution (ADR-2) stated as measured rather than asserted
- [ ] Prose diff reviewed: `git diff origin/main..HEAD | grep '^[-+].*///'` (LESSON-599)
- [ ] The map guard is shown to actually see `session.rs` — mutate the row and confirm red, then revert (LESSON-598: re-run a derived check's mutation after changing program structure)

## Technical Notes

- The mutation in the last criterion is the one that matters: REQ-600 moved a
  line into a helper and an inversion that had gone red went green with nothing
  in 4,000 tests noticing. Adding a module is exactly the structural change that
  can silently take a guard out of contact with its subject.

## Verification

Obligations this task carries, by REQ-603 acceptance-criterion ordinal:

- **AC-1** (slice located by reading the impl structure; counting rule stated
  beside every figure) — `kind: structural-check`. Artifact: architecture.md
  ADR-1/ADR-2 plus the restatement required by this task's ACs.
- **AC-6** (before/after production count, rule stated) — `kind: structural-check`.
  Artifact: the measured figures, derived with the same rule
  `runtime_module_map.rs::production_counts()` applies.
- **AC-7** (suite green grepped for `FAILED`; clippy and `fmt --check` clean) —
  `kind: test-case`. Artifact: `cargo test --workspace --no-fail-fast`,
  `cargo clippy --workspace --all-targets`, `cargo fmt --check`.

**Non-vacuity**: the suite run must report a non-zero executed-case count and
the run is grepped for `FAILED` rather than trusted on a summed pass total
(LESSON-533). The map-guard mutation in this task's ACs is the evidence that
the structural checks are still in contact with their subject after a module
was added (LESSON-598).
