---
id: TASK-087
title: "LifetimeSupervisor + server integration (admit, guards, no-abort teardown)"
status: complete
parent: REQ-565
repo: teton-code
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-085, TASK-086]
---

## Description

Wrap the pure state machine in the async supervisor that owns the mutex, hands
out RAII guards, emits events, and signals the accept loop — then wire it into
`server.rs` (architecture D-1, D-3, D-4, D-5).

## Files to Create/Modify

- `crates/tetond/src/lifetime.rs` — **new**: `LifetimeSupervisor { state: Mutex<LifetimeState>, shutdown: watch/Notify, events: Arc<EventBus>, started_at: Instant }` with `admit() -> Option<ClientGuard>`, `activity(BlockingActivity) -> ActivityGuard`, `wait_for_shutdown()`, `shutdown_reason()`. `ClientGuard`/`ActivityGuard` release on `Drop`; both apply the returned `LifetimeAction` (emit event, arm/cancel the linger timer, signal shutdown).
- `crates/tetond/src/lib.rs` — export; add the supervisor to `Daemon`.
- `crates/tetond/src/server.rs` —
  - `serve()` selects over `listener.accept()` and `supervisor.wait_for_shutdown()`, returning cleanly on shutdown.
  - `do_handshake`: on successful negotiation call `admit()`; `None` → respond `DAEMON_SHUTTING_DOWN` and leave the client unhandshaked.
  - `spawn_prompt_turn`: the turn owns an `ActivityGuard(Turn)` for its whole execution.
  - `handle_client` teardown: **remove `task.abort()` for prompt tasks** — drop the `ClientGuard` first, then await the outstanding turns (see AC below).
- `crates/tetond/src/model_consent.rs` — expose `any_install_in_flight()` (the existing `installing` set, name-free) so the supervisor can query download/load without a second source of truth.

## Acceptance Criteria

- [ ] The count increments at **handshake completion**, not at `accept` (D-1): a
      test opens a raw `UnixStream`, drops it without handshaking, and asserts
      the daemon neither counts it nor arms shutdown.
- [ ] `admit()` and the flip to `Committed` happen under the **same mutex**; a
      test drives admit-vs-commit concurrently and asserts exactly one of
      {admitted, refused} with no state in which a committed daemon admitted.
- [ ] A client admitted while `Armed` cancels the shutdown and the daemon keeps
      serving (BR-3 first arm).
- [ ] A client arriving after `Committed` gets `DAEMON_SHUTTING_DOWN` rather than
      a hung or half-served session (BR-3 second arm).
- [ ] A disconnect mid-turn no longer aborts the turn: the turn runs to
      completion and its cost row is recorded. Assert on the **ledger row**, not
      on the streamed output (the writer half is gone by then) — this is AC-3's
      real claim and it is false today (`server.rs:297`).
- [ ] Guards release on panic/cancel — a test that panics inside a guarded scope
      asserts the daemon is not wedged alive.
- [ ] `ActivityGuard(ModelDownload|ModelLoad)` reflects the existing
      `ModelConsentGate` claim rather than a parallel flag.
- [ ] Existing suites still pass: `multi_client.rs`, `event_response_ordering.rs`,
      `nonblocking_inference.rs`.

## Technical Notes

- Teardown order in `handle_client` is load-bearing (D-4): drop `ClientGuard`
  (arms) → await prompt tasks (defers on their guards) → last guard drops →
  commit. Awaiting before dropping the client guard would never arm and would
  hide the deferral the event vocabulary must show.
- Bound the teardown await so a wedged turn cannot hold the daemon forever; on
  timeout, abort as today and log why.
- The event fence (`EventFence`, server.rs module docs) is unchanged — lifetime
  events are broadcast, and the ordering rule for responses still applies.
- Do not hold the state mutex across an `.await`.
