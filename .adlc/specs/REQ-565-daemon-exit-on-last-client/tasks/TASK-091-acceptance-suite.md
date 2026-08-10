---
id: TASK-091
title: "Acceptance suite: lifetime e2e over the real socket"
status: pending
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-087, TASK-088, TASK-089]
---

## Description

Drive the lifetime over the real socket with the real daemon binary — AC-1
through AC-4 and AC-8. BR-9 requires that no behaviour exist only behind
launchd, so these run against a spawned `teton-code`, never a service.

## Files to Create/Modify

- `crates/tetond/tests/e2e/harness.rs` — `DaemonOptions::arg()` (it can only set env today) and `Daemon::wait_for_exit(timeout) -> ExitStatus`; make `Drop` tolerate an already-exited child.
- `crates/tetond/tests/daemon_lifetime.rs` — **new** e2e suite.

## Acceptance Criteria

- [ ] **AC-1**: spawn with no daemon running, handshake one client, disconnect →
      the process exits on its own within the teardown bound; no socket file
      remains; the log carries `daemon_shutdown` with reason `last_client`.
- [ ] **AC-2**: two concurrent clients; disconnecting the first leaves the daemon
      running and reports a live count of 1; disconnecting the second stops it.
- [ ] **AC-3**: disconnect the last client while a scripted turn is in flight →
      the log shows `daemon_shutdown_deferred` with `blocking_activity: turn`,
      the turn completes, **the ledger row for that turn is present and intact**,
      and only then does the process exit.
- [ ] **AC-4**: a client connects while shutdown is armed → either the shutdown is
      cancelled and the handshake succeeds, or the handshake is refused and the
      client's autostart path reaches a fresh daemon. The prompt turn succeeds
      either way, and a flock assertion proves **never two daemons**.
- [ ] **AC-8**: `--shutdown-policy never` survives the last disconnect;
      `--shutdown-policy linger --linger-seconds N` exits N seconds after the last
      disconnect, and a client returning inside the window keeps it alive.
- [ ] The harness's readiness probe (a bare `UnixStream::connect` that never
      handshakes) does **not** count as a client — if it did, every e2e in the
      repo would race the daemon's own exit. Assert this explicitly rather than
      relying on it silently.

## Technical Notes

- Use `TETON_LOCAL_SCRIPT` (the scripted stand-in) so no real model download or
  load runs; otherwise the D-3 deferral makes exit timing depend on multi-GB I/O
  and the suite flakes. `first_run_consent_applies()` is false for a scripted
  engine, which keeps the consent task out of the picture entirely.
- AC-4 is a race: drive it deterministically rather than with sleeps where
  possible — arm the shutdown behind a live activity guard, then connect.
- Assert on the daemon's stderr log (`Daemon::log()`) for the event lines, the
  same way the existing suites assert on startup reports.
- Timing bounds must be generous enough for CI but must still fail if the daemon
  genuinely never exits — a test that passes by timing out is not a test.
