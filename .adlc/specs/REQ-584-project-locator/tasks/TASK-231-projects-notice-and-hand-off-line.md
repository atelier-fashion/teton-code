---
id: TASK-231
title: "/projects, the launch notice, and the hand-off line"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-228", "TASK-229", "TASK-230"]
---

## Description

BR-9/BR-10/BR-11, AC-10/AC-11/AC-12. The client surfaces: `/projects [query]` through the daemon, N=5 names on the non-project launch notice, and the turn-end hand-off line.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — the `/projects` row
- `crates/teton/src/session_ui.rs` — the hand-off line and the notice clause
- `crates/teton-protocol/src/methods.rs` — the `projects/list` method
- `crates/tetond/src/server.rs` — its handler

## Acceptance Criteria

- **AC-10**: `/projects` renders the same facts the tool returns — a test diffs the content through one renderer; `/projects teton` filters; the budget-stop line appears on an injected budget; nothing scans when `/projects` is not typed
- **AC-11**: the notice lists up to 5 names with `/cd <name>`; an empty registry leaves REQ-583's notice byte-unchanged; piped output stays byte-identical (TTY gate)
- **AC-12**: a turn whose `projects` call matched ends with `→ /cd <name>  (<display>)`; no call or no match prints nothing; **deleting the harness-side append fails the test** (mutation check)
