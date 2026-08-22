---
id: TASK-227
title: "the scan: bounded, two levels, on demand only"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-225"]
---

## Description

BR-3/BR-4, AC-3/AC-4. A purpose-built two-level scanner (ADR-5) reusing `WalkPolicy`'s name sets and the `WalkBudget` type, with its own `ProjectScanBudget::default()` of 2,000 entries / 2 s.

The dev-folder table is BR-4's single table plus the parent of every `launched` project.

## Files to Create/Modify

- `crates/tetond/src/projects/scan.rs` — the scanner and `DEV_FOLDERS`
- `crates/tetond/src/projects/mod.rs` — wire it

## Acceptance Criteria

- `DEV_FOLDERS` is enumerated **by name** in a test (AC-3/BR-4)
- with an injected table pointing at a fixture: finds at depth 1 and 2, not at depth 3; never enters `Library/`, `node_modules/`, a `.photoslibrary`, or a planted symlinked directory; records finds as `Scanned`; stops at an injected budget and says so
- a `launched` project's parent is scanned even when absent from the table
- a dev folder that does not exist is skipped silently
- **AC-4**: a seam records whether the scanner ran; it does not run at `session/create`, at daemon start, or during a turn that makes no `projects` call
