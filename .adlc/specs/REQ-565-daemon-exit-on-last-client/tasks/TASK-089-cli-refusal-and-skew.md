---
id: TASK-089
title: "CLI: retry a shutting-down daemon, warn on build skew, keep warm-up a notice"
status: pending
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-086]
---

## Description

The client half of BR-3's second arm, plus BR-6's version-skew warning and BR-4's
"warm-up is a notice, not an error" (architecture D-8).

## Files to Create/Modify

- `crates/teton/src/client.rs` — `ensure_connected` / `ensure_connected_session`: a handshake refused with `DAEMON_SHUTTING_DOWN` is **not** a fatal error; fall through to the autostart path (spawn + `poll_for_daemon`) so the client lands on the successor. After a successful handshake, run the build-skew check.
- `crates/teton/src/main.rs` (or the surface module) — render the skew line.

## Acceptance Criteria

- [ ] A handshake refused with `DAEMON_SHUTTING_DOWN` triggers the autostart path
      rather than surfacing an error; the user's command then succeeds against the
      fresh daemon (AC-4's second arm).
- [ ] Any *other* handshake error keeps today's behaviour — in particular
      `UNSUPPORTED_PROTOCOL_VERSION` must still surface its existing diagnosis and
      must **not** be swallowed into a spawn-retry loop.
- [ ] The retry is bounded (one autostart attempt, as today) — a daemon that keeps
      refusing cannot spin the CLI.
- [ ] When `daemon_version != client_version`, exactly **one** line appears naming
      both versions and the remedy (exit all sessions; the next start runs the new
      binary). When they match, **no** line appears (AC-7).
- [ ] The skew check is unit-tested with injected versions — no daemon, no socket
      (AC-7's "unit-testable via injected versions").
- [ ] The model-load window after an on-demand start still renders through the
      existing warming/progress state and is never reported as an engine failure
      (BR-4; regression guard for BUG-146 / BUG-152).

## Technical Notes

- `poll_for_daemon` polls the socket path; a predecessor mid-teardown may still
  have the socket bound for a moment, so the poll must treat a
  `DAEMON_SHUTTING_DOWN` handshake during polling as "keep polling", not as
  success. This is the client-side mirror of TASK-088's `acquire_within`.
- The remedy sentence lives here, not in `teton-protocol` — that crate is
  transport-free and deliberately knows nothing about brew or launchd (see the
  `VersionSkew` doc comment).
- Do not reuse the `VersionSkew` sentence: it says "the running daemon is the
  older build" about *protocol* ranges. Build skew needs its own wording because
  the handshake **succeeded**.
