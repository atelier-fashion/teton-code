---
id: TASK-095
title: "CLI /clear slash command + context_cleared rendering"
status: complete
parent: REQ-567
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-094]
repo: teton-code
---

## Description

The user surface: a `/clear` slash command (REQ-555 pattern) that issues
`session/clear` for the current session, and a session-UI rendering of the
`context_cleared` event.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `CommandSpec` row (`name: "clear"`,
  `args: Args::None`, summary per the /help table) + `handle_clear` issuing
  `session/clear` over the existing connection; render the typed busy
  refusal as a notice ("a turn is still running"), not an error.
- `crates/teton/src/session_ui.rs` — render `context_cleared` as a notice
  ("context cleared — N blocks dropped"); always visible (a user-initiated
  state change, not verbose-only chrome).

## Acceptance Criteria

- [ ] CLI e2e beside REQ-555's tests (`cli_e2e.rs` slash suite): `/clear`
  issues no model turn; the notice renders with the dropped count; `/help`
  lists `clear` (the help-table invariant test keeps passing).
- [ ] End-to-end: prompt, `/clear`, prompt about the first exchange — the
  reply demonstrates no carried knowledge (scripted engine: assert the
  second prompt's received context contains no first-exchange blocks)
  (AC-6 flow half).
- [ ] Structural AC-6: a tool-surface test asserting no tool in
  `ToolRegistry::with_builtins()` (nor any MCP wiring path) exposes
  `session/clear` — clear is unreachable by the model by construction.
- [ ] `cargo test --workspace` green.

## Technical Notes

Slash dispatch happens before prompt construction (REQ-555 BR-1), which is
what makes the structural claim honest — the command string never reaches
the model. `/clear` is session-scoped state mutation but needs no
typed-input gate (`/model set` precedent does): clearing is reversible
by... nothing, but it destroys only conversational convenience, never
consent, money, or data (OQ-4) — document that reasoning in the handler
comment.
