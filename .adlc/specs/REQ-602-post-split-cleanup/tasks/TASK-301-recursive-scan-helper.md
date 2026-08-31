---
id: TASK-301
title: "One recursive walker for every scan over runtime/"
status: complete
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

Six directory scans read `crates/tetond/src/runtime/` with a flat `read_dir`.
The first `runtime/foo/mod.rs` leaves the corpus silently — LESSON-594's
hazard, pre-armed rather than waited for (BR-4).

`projects/scan.rs` is on this list because this REQ line put it there: that
scan was rewritten one commit earlier to repair a guard that had gone dead
after a rename, and used a flat `read_dir` to do it.

## Files to Create/Modify

- `crates/tetond/src/runtime/mod.rs` (3 reads), `runtime/taint.rs`,
  `src/projects/scan.rs`
- `crates/tetond/tests/skill_turn.rs`, `tests/runtime_module_map.rs`
- `crates/tetond/tests/traceability_sweep.rs` — floor read only; its workspace
  sweep already walks

## Acceptance Criteria

- [x] **Amended: one shared helper for the three in-lib scans; a local copy for
      the two integration tests.** `call_sites::scan::rust_files` is
      `#[cfg(test)]`-gated inside the lib, so an integration test — which links
      the lib compiled *without* that cfg — cannot reach it. Four sibling
      integration tests already carry their own walker for exactly this reason,
      so a shared `tests/common/` module for two more files would create a
      second pattern rather than consolidate the first. The three in-lib scans
      (`runtime/mod.rs`, `runtime/taint.rs`, `projects/scan.rs`) do share the
      canonical walker; `tests/runtime_module_map.rs` and `tests/skill_turn.rs`
      carry a documented local copy naming it.
- [x] **Demonstrated, and it found a second defect.** With
      `runtime/nested/mod.rs` planted, `runtime_module_map` still **passed** —
      the walker was recursive but the *comparison* collapsed
      `runtime/nested/mod.rs` to the basename `mod.rs`, which is documented. A
      whole undocumented subtree would have read as the root module. Fixed to
      compare paths relative to `runtime/`; the fixture then failed with
      `["nested/mod.rs"]` as it should, and passes again with the fixture
      removed. Recursion alone was not enough — what the scan *found* and what
      the assertion could *see* were two different questions.
- [ ] `cargo test --workspace --no-fail-fast` green, grepped for `FAILED`.
