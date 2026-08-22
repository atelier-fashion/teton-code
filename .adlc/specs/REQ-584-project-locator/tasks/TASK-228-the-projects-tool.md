---
id: TASK-228
title: "the projects tool"
status: pending
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-225", "TASK-227"]
---

## Description

BR-6, AC-6/AC-7. One read-only tool with an optional `query`, cap-exempt with ADR-6's distinct reason, allowed at every permission level (LESSON-524's template).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/projects.rs` — new tool
- `crates/tetond/src/harness/tools/mod.rs` — register cap-exempt; add the `CAP_EXEMPT_TOOLS` row

## Acceptance Criteria

- **AC-2**, the surfacing half: an entry whose directory or marker was removed after it was recorded is absent from the very next `projects` result — the read-time prune observed through the tool rather than through the store
- AC-6 in full, including the empty-machine result naming the dev folders it looked in, and `/cd <path>` recipes when a name is ambiguous
- **AC-7**: allowed at every enumerated `PermissionLevel` including `plan`, with zero pending events; exposed on every profile that exposes `glob`; the degraded-cap headroom assertion still holds
- the `CAP_EXEMPT_TOOLS` cross-check passes in both directions (a row without a registration is red, and vice versa)
- a project name that is a frame label (`User:`) or bidi renders neutralised and bounded (AC-5)
