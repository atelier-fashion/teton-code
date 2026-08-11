---
id: TASK-105
title: "session/list returns a reduced summary to unattached connections"
status: draft
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

- [ ] An unattached connection's `session/list` shows the session's id/mode/phase but **no** `title` and **no** `cwd` — asserted on the raw wire JSON (the keys are absent, not empty strings).
- [ ] A connection attached to that session (and, separately, a monitor) sees the full summary.
- [ ] The creating connection sees its own session in full (it is auto-attached — REQ-568 behavior preserved).
- [ ] Pure reducer has a table test covering visible/not-visible × title-present/absent × cwd-present/absent.
- [ ] Mutation check: forcing `visible = true` unconditionally must fail the unattached test — see it fail, then restore (LESSON-479).
- [ ] `cargo test -p tetond` green.

## Technical Notes

- Do not filter *which sessions are listed* — the id namespace stays open (BR-8/BR-10 are about the payload, not the listing). Removing rows would break `session/list`'s role and is not what the spec asks.
- The CLI renders `title` when present; confirm a reduced summary degrades gracefully there rather than showing an empty column.
