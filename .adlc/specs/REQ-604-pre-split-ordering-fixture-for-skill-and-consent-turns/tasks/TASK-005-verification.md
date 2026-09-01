---
id: TASK-005
title: "Full verification"
status: pending
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

- [ ] `cargo test --workspace --no-fail-fast`, output grepped for `FAILED`
      (conventions.md: a summed count from a fail-fast run is a floor, not a
      total — LESSON-533).
- [ ] `cargo clippy --workspace --all-targets` clean under `deny`.
- [ ] `cargo fmt --check` clean.
