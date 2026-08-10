---
id: TASK-086
title: "Protocol: lifetime events, shutting-down error code, build-version skew"
status: complete
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: []
---

## Description

The wire vocabulary REQ-565 needs: the daemon-lifetime event family, the typed
handshake refusal a client must be able to recognize (BR-3), and the pure
build-version skew classifier behind BR-6/AC-7 (architecture D-8).

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — add a `DaemonLifetime(DaemonLifetime)` variant with a `stage` enum folding the spec's five events: `ClientConnected { live_connection_count }`, `ClientDisconnected { live_connection_count }`, `ShutdownArmed { policy, pending }`, `ShutdownDeferred { blocking_activity }`, `Shutdown { reason, uptime_seconds, sessions_closed }`. No `conn_id` on the wire — see notes.
- `crates/teton-protocol/src/jsonrpc.rs` — `error_code::DAEMON_SHUTTING_DOWN`.
- `crates/teton-protocol/src/handshake.rs` — `BuildSkew { daemon_version, client_version }` and `build_skew(client: &str, daemon: &str) -> Option<BuildSkew>`.

## Acceptance Criteria

- [ ] All new event shapes round-trip through serde (match the existing
      `round_trip` test pattern in this crate).
- [ ] `DAEMON_SHUTTING_DOWN` does not collide with any existing code in
      `error_code`, and a test asserts the whole set is distinct.
- [ ] `build_skew` returns `None` for equal versions and `Some` for any
      difference, with both versions carried so the surface can name them
      (AC-7's "does not appear when they match").
- [ ] `build_skew` is **independent of** `VersionSkew`/`VersionMismatch`: a test
      asserts that two builds whose protocol ranges overlap (so `negotiate`
      succeeds) still produce a build skew — that pairing is the exact harm
      REQ-565 cites and the existing protocol check is silent on it.
- [ ] No sentence in this crate mentions brew, launchd, or how a client is
      installed — remedies live at the surfaces (see the `VersionSkew` doc
      comment stating this rule).

## Technical Notes

- Fold the five spec events into one variant family with a stage, exactly as
  REQ-563's D-8 did for the web-lookup vocabulary; do not add five near-identical
  top-level variants.
- `conn_id` stays daemon-internal. The spec's `ClientConnection.conn_id` is
  required to be unique per live connection for the daemon's own bookkeeping, but
  putting it on a broadcast event would tell every attached client about the
  existence and identity of the others for no consumer benefit. The counts are
  what AC-2 asserts on.
- `PROTOCOL_VERSION_MAX` does **not** need to move: this adds an event variant and
  an error code, both of which older clients already tolerate (unknown
  notification methods are ignored — `server.rs:602`).
