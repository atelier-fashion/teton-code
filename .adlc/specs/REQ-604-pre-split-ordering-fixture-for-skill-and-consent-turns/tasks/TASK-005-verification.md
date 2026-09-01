---
id: TASK-005
title: "Full verification"
status: complete
parent: REQ-604
repo: teton-code
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-004]
---

## Files to Create/Modify

None — this task runs commands and records their output. Any fix it provokes
belongs to the task that owns the file.

## Acceptance Criteria

- [x] `cargo test --workspace --no-fail-fast`, output grepped for `FAILED`
      (conventions.md: a summed count from a fail-fast run is a floor, not a
      total — LESSON-533).
- [x] `cargo clippy --workspace --all-targets` clean under `deny`.
- [x] `cargo fmt --check` clean.

## Result

Final tree, run three times across the implementation and the two review-fix
passes; the figures below are the last run.

- `cargo test --workspace --no-fail-fast` — exit 0, **0** occurrences of
  `FAILED`, 74 test targets, **4,078** tests passed.
- `cargo clippy --workspace --all-targets` — clean under `deny`, and **no new
  `#[allow(...)]`** was added (`suppression_ratchet.rs` refuses one by design).
- `cargo fmt --check` — clean.

Two checks beyond the AC, both because this REQ adds code to a file several
source-scanning tests read:

- The new module sits at line 18086, well past the column-0 `#[cfg(test)]` at
  line 7421, so it is outside the corpora those checks cut (LESSON-589).
- `suppression_ratchet`, `runtime_module_map`, `runtime_visibility`,
  `runtime_doc_paths` and `traceability_sweep` were each run individually and
  are green.
