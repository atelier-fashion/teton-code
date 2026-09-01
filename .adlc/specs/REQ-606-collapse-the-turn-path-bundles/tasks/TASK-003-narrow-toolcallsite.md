---
id: TASK-003
title: "Narrow ToolCallSite to the three fields not already inside call"
status: complete
parent: REQ-606
created: 2026-09-01
updated: 2026-09-01
dependencies: [TASK-001]
---

## Description

AC-1. `ToolCallSite` carries `name` and `arguments` beside `call`, whose
`ToolCall` was built from `name.clone()` and `arguments.clone()` two statements
earlier. Neither is rebound in between, so both are reachable through `call`.

`run_the_allowed_tool` re-derives them at the top as `call.name.as_str()` and
`&call.arguments` — **the same types the destructure produced before**, so the
41 use sites in the 956-line body are untouched.

The full collapse (delete the type, pass `&ModelReply`) is **refused and
recorded as a finding** in `architecture.md`: `serve_tool_call` moves `text` out
of the reply, and destructuring through a reference rebinds `name` as `&String`
and `dropped_calls` as `&u32` across that body.

## Files to Modify

- `crates/tetond/src/harness/turn_loop.rs`

## Acceptance Criteria

- [ ] `ToolCallSite` has three fields: `call`, `request`, `dropped_calls`
- [ ] `name` and `arguments` re-derived from `call` with unchanged types
- [ ] No use site inside `run_the_allowed_tool` changed
- [ ] `run_session_turn_with_pressure_policy` brace depth unchanged (AC-3)
- [ ] Suite green; clippy 0 under `deny`; fmt clean
