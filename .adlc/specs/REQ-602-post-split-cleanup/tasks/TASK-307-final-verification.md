---
id: TASK-307
title: "Final verification, with every figure carrying its rule"
status: draft
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-301, TASK-302, TASK-303, TASK-304, TASK-305, TASK-306]
---

## Description

AC-9 and AC-10. Run every criterion end to end and record the measured figures.

## Acceptance Criteria

- [ ] `cargo test --workspace --no-fail-fast` green, output captured and
      **grepped for `FAILED`** rather than trusting a summed count.
- [ ] `cargo clippy --workspace --all-targets` clean under `deny`;
      `cargo fmt --check` clean.
- [ ] The traceability sweep and module-map guard pass, with `BASE` and
      `TOUCHED` repointed at this REQ's base.
- [ ] **Every number in the PR body states how it was counted.** This REQ
      produced four different answers to one question; a figure without its rule
      is the thing that cost the most here (LESSON-593).
- [ ] Every AC ticked with evidence or explicitly called out as unmet.
