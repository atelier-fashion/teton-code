---
id: TASK-004
title: "tetond skeleton: UDS server, sessions, event broadcast"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-002, TASK-003]
---

## Description

The daemon's spine: Unix-domain-socket JSON-RPC server, socket auth, client
attach/detach, session registry, and event broadcast to subscribed clients.
Delivers AC-6 (multi-client, daemon survives client exit).

## Files to Create/Modify

- `crates/tetond/src/main.rs` — startup, single-instance lock, socket path (`$XDG_RUNTIME_DIR` or `~/Library/Application Support/teton/`)
- `crates/tetond/src/server.rs` — tokio UDS listener, per-client task, JSON-RPC dispatch to typed handlers
- `crates/tetond/src/auth.rs` — socket file mode 0600 + SO_PEERCRED/LOCAL_PEERCRED uid check
- `crates/tetond/src/sessions.rs` — session registry; sessions outlive clients; list/create/attach handlers
- `crates/tetond/src/broadcast.rs` — event bus: subscription per client, backpressure policy (bounded channel, slow-client eviction with warning event)
- `crates/tetond/tests/multi_client.rs` — integration: two clients attach, consistent session lists, daemon survives client exit (AC-6)

## Acceptance Criteria

- [ ] Two concurrent clients see consistent session lists; events from a session started by client A reach subscribed client B (AC-6)
- [ ] Daemon keeps sessions alive across client disconnect/reconnect
- [ ] A process running as a different uid cannot connect (peer-cred test, may be cfg(unix) gated)
- [ ] Second daemon instance exits cleanly with "already running" (lock file or socket probe)
- [ ] Slow subscriber cannot block the event bus (bounded-channel test)

## Technical Notes

Backpressure decision deferred from ADR-002 lands here: bounded broadcast
channel; on overflow, evict the subscription and emit a `subscription_lagged`
protocol error to that client — never buffer unboundedly, never block
publishers. Handshake (TASK-002) enforced before any other method.
