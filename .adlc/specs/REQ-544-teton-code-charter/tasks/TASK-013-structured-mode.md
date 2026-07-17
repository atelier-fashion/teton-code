---
id: TASK-013
title: "tetond structured mode: phase state machine + artifact gates"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-010]
---

## Description

The generic ADLC extraction (opt-in per BR-3): a phase state machine over the
core Phase enum (D-4) with artifact gates — spec and task artifacts carry
intelligence forward so cheap models can execute the implement phase. Delivers
AC-3 with TASK-010.

## Files to Create/Modify

- `crates/tetond/src/structured/machine.rs` — phase state machine: spec → architect → implement → review; gate checks (artifact exists + minimal validity) before `phase_transition`
- `crates/tetond/src/structured/artifacts.rs` — TaskArtifact storage under `.teton/` in the user's repo: requirement, plan, task files (generic templates — NOT the personal ADLC toolkit's)
- `crates/tetond/src/structured/templates/` — bundled generic templates (requirement, plan, task) answering OQ-5 for fresh repos
- `crates/tetond/tests/structured_flow.rs` — demo requirement flows spec→architect→implement→review with phase-correct routing observable in `route_decided` events (AC-3)

## Acceptance Criteria

- [ ] Full phase flow on a demo requirement with per-phase routing visible in events (AC-3)
- [ ] Freeform sessions never require structured artifacts; entering structured mode is an explicit session option (BR-3)
- [ ] Gate failure (missing/invalid artifact) blocks transition with an actionable message; never auto-generates silently
- [ ] Implement-phase turns carry the task artifact in context (the cheap-model-viability mechanism — assert presence in test)
- [ ] Artifacts work in a repo with no prior `.teton/` (OQ-5: bundled templates path)

## Technical Notes

This is the generic extraction, not Brett's toolkit: no REQ counters, no
global state, no gate scripts — phases, artifacts, gates only. Resist feature
creep from the personal ADLC; anything beyond the four phases is post-MVP.
Freeform = degenerate single-phase case through the same code path (D-4).
