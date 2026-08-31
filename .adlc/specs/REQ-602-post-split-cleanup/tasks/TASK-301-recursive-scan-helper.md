---
id: TASK-301
title: "One recursive walker for every scan over runtime/"
status: draft
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

- [ ] Every scan over `runtime/` uses one shared recursive helper, not six
      hand-written walkers. `call_sites.rs` and `suppression_ratchet.rs` already
      have `rust_files`; reuse rather than add a seventh.
- [ ] Demonstrated: plant `runtime/nested/mod.rs` carrying a marker each scan
      would report, confirm every scan sees it, then remove it. Record what each
      scan reported before and after.
- [ ] `cargo test --workspace --no-fail-fast` green, grepped for `FAILED`.
