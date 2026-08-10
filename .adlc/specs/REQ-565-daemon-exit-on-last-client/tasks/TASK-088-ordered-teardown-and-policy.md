---
id: TASK-088
title: "Ordered shutdown, flock hand-off, and policy resolution in the daemon binary"
status: pending
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-085, TASK-087]
---

## Description

The daemon-binary half: resolve the shutdown policy from flag/env/config, run the
ordered teardown, and close the autostart race by making a successor wait for the
predecessor's flock (architecture D-6, D-7, D-9).

## Files to Create/Modify

- `crates/tetond/src/main.rs` — parse `--shutdown-policy <mode>` / `--linger-seconds <n>` (alongside the existing `--version`); resolve policy **flag > env (`TETON_SHUTDOWN_POLICY` / `TETON_LINGER_SECONDS`) > config `[lifetime]` > default**; construct the supervisor; run the ordered teardown after `serve()` returns.
- `crates/tetond/src/single_instance.rs` — `SingleInstance::acquire_within(path, window)` with a bounded retry (default 5 s at 25 ms); `main` uses it.
- `crates/tetond/src/runtime.rs` — expose the resolved `LifetimeConfig` from `from_env` (do **not** route these through `test_seams_enabled` — see notes).

## Acceptance Criteria

- [ ] Policy precedence is flag > env > config > default, and the resolved
      `PolicySource` is reported in the startup line so a misconfiguration is
      diagnosable (BR-7).
- [ ] An invalid `--shutdown-policy` value refuses to start and names the three
      valid spellings, rather than silently defaulting.
- [ ] The policy flags/env are honoured in a **release** build — a test asserts
      they are not gated behind `TETON_TEST_SEAMS`, because the shipped formula
      depends on `--shutdown-policy never` working (D-7).
- [ ] Teardown order is exactly D-9: stop accepting → drain client tasks
      (bounded) → close sessions → drop the ledger connection → unlink the socket
      → emit `daemon_shutdown{reason, uptime_seconds, sessions_closed}` → release
      the flock **last**. A test asserts the socket file is gone *before* the lock
      is released (BR-3, BR-8).
- [ ] After a clean exit no socket file remains, so the next autostart binds
      fresh rather than finding a stale path.
- [ ] `acquire_within` returns the lock once a predecessor releases it inside the
      window, and still reports "already running" for a genuinely live daemon
      after the window (D-6).
- [ ] A daemon that never gets a client exits after the startup grace with reason
      `startup_unclaimed`; under `never` it does not.

## Technical Notes

- The lifetime settings are **operator** settings, not test seams. Adding them to
  `test_seams_enabled`'s gate (`runtime.rs:4358`) would make a release build
  *panic* when the formula passes them — the exact opposite of OQ-2's
  requirement. Follow the `TETON_CONFIG` precedent: honoured in release, absent
  from the seam list.
- The flock guard `_instance` must stay alive until after step 5; keep it bound in
  `main` and let it drop at the end of `main`, after `block_on` returns.
- Signal handling (SIGTERM/SIGINT → `ExitReason::Signal`) runs the same ordered
  teardown, so `brew services stop` under the `never` policy still exits cleanly.
- `unlink` failure is logged, never fatal — a missing socket is the desired end
  state either way.
