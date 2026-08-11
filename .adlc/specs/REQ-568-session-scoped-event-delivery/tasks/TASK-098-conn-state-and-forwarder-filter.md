---
id: TASK-098
title: "Daemon: per-connection ConnState and session filter in forward_events"
status: draft
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-097"]
---

## Description

Introduce the per-connection `ConnState` (attached-session set + immutable
monitor flag) and make `forward_events` deliver session-scoped envelopes only
to attached or monitor connections (BR-1/BR-2/BR-3), with skipped envelopes
still advancing the fence watermark (BR-7, ADR-A). Rewrite the multi_client
test that currently encodes the leak as a feature.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — (1) `ConnState { attached: Arc<RwLock<HashSet<SessionId>>>, monitor: bool }` created in `handle_client` after `do_handshake` (monitor from `HandshakeParams.monitor`; log line at declaration: client kind/name + "monitor client attached" — BR-5). (2) Pass a clone of `attached` (+ monitor) into `forward_events`; in the loop, decide via a pure fn `should_forward(env_session: Option<&SessionId>, attached: &HashSet<SessionId>, monitor: bool) -> bool` — deliver iff `env_session.is_none() || monitor || attached.contains(sid)`. A skipped envelope still runs `count += 1; forwarded.send(count)` exactly like the existing serialization-failure arm. (3) `handle_session_create` inserts the new session id into `attached` on success (creator auto-attach, BR-1); `handle_session_attach` inserts on success. `session/clear` does NOT remove (attachment is connection-lifetime). (4) Table-driven unit test for `should_forward` covering the six (scoped/daemon × attached/monitor/neither) cells.
- `crates/tetond/tests/multi_client.rs` — rewrite `two_clients_share_sessions_and_daemon_survives_client_exit` per the architecture's "deliberate test-contract change": client B no longer passively receives A's `phase_transition`; B `session/attach`es first (or handshakes with `monitor: true`) and then sees it. The session-list sharing and daemon-survives-exit assertions stay.

## Acceptance Criteria

- [ ] `should_forward` unit test: daemon-scoped → everyone; session-scoped → attached ✓, monitor ✓, neither ✗ (6 cells, table-driven).
- [ ] Rewritten multi_client test: unattached B receives none of A's session envelopes while still listing the session; after attach (or with monitor), B receives them; daemon survives A's exit.
- [ ] Monitor handshake produces a daemon log line (assert via the test harness's log capture if available; otherwise assert the declaration is observable — BR-5).
- [ ] `cargo test -p tetond --test multi_client` and tetond unit tests pass.

## Technical Notes

- `broadcast.rs` is untouched — the bus stays connection-agnostic (ADR-A); dispatch signature changes to carry `&ConnState` land in TASK-099, but if plumbing create/attach mutation requires the parameter now, add it here and keep TASK-099 to the gating logic (call it out in the commit either way).
- The forwarder reads `attached` under a `std::sync::RwLock` (short critical section, no await inside the guard) or `tokio::sync::RwLock` — match the file's existing locking idiom; never hold the guard across `out_tx.send(...).await`.
- Filtered clients observe seq gaps (ADR-A consequence) — do not "fix" seq; TASK-102 pins gap tolerance.
- LESSON-443 guard-shape rule: the filter predicate names the real condition (attached/monitor), never a proxy like "set is empty means legacy client".
