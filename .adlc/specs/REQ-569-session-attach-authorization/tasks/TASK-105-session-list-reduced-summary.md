---
id: TASK-105
title: "session/list returns a reduced summary to unattached connections"
status: complete
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

`session/list` currently returns full `SessionSummary` — including `title`
(model-generated from the user's prompt text) and `cwd` (an absolute path) — to
any handshaked connection. Reduce the payload for sessions the connection is
not attached to (BR-10, AC-10, ADR-G). Closes REQ-568's recorded residual.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — the `SessionListParams::METHOD` arm currently returns `daemon.sessions.list()` verbatim. Map each summary through a reducer that keeps `session_id`, `mode`, `phase` always, and retains `title`/`cwd` **only** when the connection may see that session. Reuse REQ-568's existing read predicate (`ConnState::may_receive`, which already folds in monitor) rather than writing a second definition of "may this connection see this session" — one predicate, one answer (LESSON-484).
- The reduction must be a **pure function** (`fn reduce_for(summary, visible: bool) -> SessionSummary`) with its own table test, so the redaction rule is testable without a socket.
- `crates/teton-protocol/src/methods.rs` — no shape change needed: `title` and `cwd` are already `Option` with `skip_serializing_if`, so omission is expressible on the wire. Confirm this and state it; do NOT add a new field or a second summary type.

## Acceptance Criteria

- [x] An unattached connection's `session/list` shows the session's id/mode/phase but **no** `title` and **no** `cwd` — asserted on the raw wire JSON (the keys are absent, not empty strings).
- [x] A connection attached to that session (and, separately, a monitor) sees the full summary.
- [x] The creating connection sees its own session in full (it is auto-attached — REQ-568 behavior preserved).
- [x] Pure reducer has a table test covering visible/not-visible × title-present/absent × cwd-present/absent.
- [x] Mutation check: forcing `visible = true` unconditionally must fail the unattached test — see it fail, then restore (LESSON-479).
- [x] `cargo test -p tetond` green.

## Technical Notes

- Do not filter *which sessions are listed* — the id namespace stays open (BR-8/BR-10 are about the payload, not the listing). Removing rows would break `session/list`'s role and is not what the spec asks.
- The CLI renders `title` when present; confirm a reduced summary degrades gracefully there rather than showing an empty column.

## Implementation Notes

- `reduce_for(summary, visible)` in `crates/tetond/src/server.rs` sits beside
  `should_forward`, the other pure policy function. It takes the visibility
  answer rather than computing one: the `session/list` arm asks
  `ConnState::may_receive` — the same predicate the event forwarder asks — so
  monitor is folded in without this handler knowing what a monitor is
  (LESSON-484).
- No protocol change. `SessionSummary::title` and `::cwd` were already
  `Option` with `#[serde(skip_serializing_if = "Option::is_none", default)]`,
  so omission is expressible on the wire *and* a reduced row still deserializes
  into the typed summary as `None` rather than failing a client.
- The wire test asserts the two things separately: the `title`/`cwd` **keys are
  absent from the JSON object** (not present and empty), and neither the title
  text nor the path appears anywhere in the raw frame.
- Mutation check ran: forcing `visible = true` in the `session/list` arm failed
  both `session_list_omits_title_and_cwd_from_unattached_connections` (wire) and
  `session_list_reduces_only_for_connections_that_may_not_see_the_session`
  (dispatch), with the title and cwd visible in the panic's frame dump. Restored
  and re-run green.
- CLI degradation: nothing to degrade. The CLI never calls `session/list` and
  never reads `SessionSummary::title`/`::cwd`; its one title-bearing surface is
  the `Event::SessionTitled` arm in `session_ui.rs`, which consumes the event
  without rendering it (REQ-561 BR-9a). There is no title column to blank and no
  unwrap to panic.
