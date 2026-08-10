---
id: TASK-085
title: "Pure lifetime state machine + [lifetime] config"
status: pending
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: []
---

## Description

The arm/disarm/defer/commit decision as a pure state machine in `teton-core` —
no tokio, no socket, no I/O — plus the `[lifetime]` config section that feeds it
(architecture D-2, D-3, D-5, D-7). This is the artifact BR-9/AC-9 require to be
exercisable without a socket, launchd, or a TTY.

## Files to Create/Modify

- `crates/teton-core/src/lifetime.rs` — **new**:
  - `ShutdownPolicy { OnLastDisconnect, Linger { seconds: u64 }, Never }`
  - `PolicySource { Default, Config, Env, Flag }`
  - `BlockingActivity { Turn, ModelDownload, ModelLoad, LedgerFlush }`
  - `LifetimePhase { AwaitingFirstClient, Serving, Armed, Deferred, Committed }`
  - `ExitReason { LastClient, StartupUnclaimed, Signal }`
  - `Admission { Admitted, Refused }`
  - `LifetimeAction` — what the caller must do after a transition (emit armed /
    deferred / shutdown, start or cancel a linger timer, nothing)
  - `LifetimeState` with `new(policy, source)`, `admit()`, `on_disconnect()`,
    `begin_activity(a)` / `end_activity(a)`, `on_linger_elapsed()`,
    `on_startup_grace_elapsed()`, and read-only `phase()`, `client_count()`,
    `blocking()`.
- `crates/teton-core/src/config.rs` — `LifetimeConfig { shutdown: ShutdownPolicyKind, linger_seconds: u64 }` as `[lifetime]`, wired into `Config` with `#[serde(default, skip_serializing_if = ...)]`, following the `PrivacyConfig` placement pattern and its comment style; extend `Config::validate()`.
- `crates/teton-core/src/lib.rs` — export the module.

## Acceptance Criteria

- [ ] Activities are **counted, not flagged**: two overlapping `Turn` claims
      require two `end_activity` calls before the state leaves `Deferred`.
- [ ] `AwaitingFirstClient` never arms on a zero count; only a decrement to zero
      arms (D-2). A state that has never seen a client and whose startup grace
      elapses commits with `ExitReason::StartupUnclaimed`.
- [ ] `admit()` on a `Committed` state returns `Refused`; on any other state it
      returns `Admitted`, increments, and cancels a pending arm/linger (D-5).
- [ ] Under `Never`: no disconnect arms, and the startup grace never expires.
- [ ] Under `Linger { seconds }`: the last disconnect arms and requests a timer;
      `on_linger_elapsed` commits only if the count is still zero and nothing
      blocks; a client admitted before it elapses cancels it (AC-8).
- [ ] Under `OnLastDisconnect` with no blocking work, the last disconnect goes
      straight to `Committed` (OQ-1 resolved grace = 0 s).
- [ ] Blocking work defers: last disconnect with a live activity yields
      `Deferred` naming the blocking activity, and commit happens when the last
      activity ends — not before (BR-2).
- [ ] `validate()` rejects `linger_seconds` set with a non-`linger` mode, and an
      unknown `shutdown` value names the three valid spellings.
- [ ] Unit tests cover every transition above with **no socket, no tokio runtime,
      no launchd, no TTY** (AC-9, BR-9).

## Technical Notes

- Follow `PrivacyConfig` / `WebConfig` (config.rs) for serde attrs and the
  "written-out config states posture" comment style.
- Keep this crate dependency-free of tokio — `teton-core` currently depends only
  on serde, toml, thiserror, globset, and must stay that way.
- The state machine returns *actions*; it must not emit events or touch a clock
  itself. Timers and event emission belong to the supervisor (TASK-087).
