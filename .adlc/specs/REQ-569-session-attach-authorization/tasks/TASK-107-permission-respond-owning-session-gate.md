---
id: TASK-107
title: "permission/respond resolves its owning session and requires attachment"
status: complete
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

`permission/respond` is ungated: a connection that never attached — including a
`monitor`, which by design sees every session's `permission_request` — can
answer another session's tool prompt. Close it by storing the owning session
alongside each waiter and requiring attachment (BR-9, AC-9, ADR-F). Unblocked
by BUG-161, which made request ids daemon-unique.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — `PendingPermissions` stores the owning `SessionId` with each waiter (the map value becomes `(SessionId, oneshot::Sender<..>)` or a small struct). Add `fn owner_of(&self, id: &RequestId) -> Option<SessionId>`. `PermissionGate` already knows its `session_id` and passes it at `register`. Keep the BUG-161 refuse-not-overwrite behavior and its tripwire log intact — do not regress it while touching this code.
- `crates/tetond/src/server.rs` — `handle_permission_respond` gains the connection context and gates: resolve the request's owning session via `owner_of`; if absent → existing "no waiter" behavior (unchanged); if present and the connection may not drive that session (`ConnState::may_drive`, REQ-568's write-side predicate — **not** `may_receive`, so a monitor is refused) → refuse `NOT_ATTACHED` **without resolving the waiter**. The prompt must remain pending for its rightful answerer.
- Tests: unit-level in `permissions.rs` for `owner_of`; raw-RPC test in `crates/tetond/tests/multi_client.rs` — an unattached connection and, separately, a monitor-declared connection each try to answer another session's pending request and are refused, the waiter stays pending, and the attached client's own answer then still resolves it.

## Acceptance Criteria

- [x] An unattached connection answering another session's request is refused `NOT_ATTACHED` and the waiter is **still pending** afterward (asserted — a refusal that consumed the waiter would be a denial-of-service on the real user).
- [x] A `monitor` connection — which receives the `permission_request` event — is likewise refused (uses `may_drive`, not `may_receive`).
- [x] The rightful attached connection's answer still resolves normally; no change to the happy path.
- [x] BUG-161's refuse-not-overwrite `register` behavior and its regression test remain green.
- [x] Mutation check: swapping the gate to `may_receive` must fail the monitor test (the exact bug LESSON-502 is about); see it fail, restore.
- [x] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- Gate in the handler, below `dispatch`, so raw-RPC tests exercise the real gate (LESSON-484, BUG-155) — not in `handle_client` and not in the CLI.
- `web/override` is already gated (REQ-568). After this task, audit the dispatch table once more and list in your report any remaining method that names or affects a session without a gate — do not fix them silently; report them.
- Do not change the `permission_request` event shape or who receives it; monitor still *sees* prompts, it just cannot answer them.

## Implementation Notes (as built)

- **The monitor case is asserted at `dispatch`, not over a real socket.** After
  TASK-106, declaring `monitor` at the handshake requires a monitor-scope grant
  that nothing mints until TASK-108, so a monitor connection is not constructible
  end-to-end on this branch (pinned by
  `a_monitor_declaration_is_refused_without_a_monitor_scope_grant`). It is
  covered one layer down by
  `server::tests::a_monitor_may_see_a_permission_prompt_and_may_not_answer_it`,
  which is where the gate lives, and which asserts the thing a socket test could
  not: that the refused connection *did* receive the prompt. The unattached case
  is at the raw RPC surface as specified
  (`multi_client::an_unattached_connection_cannot_answer_another_sessions_permission_prompt`).
  Revisit after TASK-108 mints monitor grants: the monitor half of AC-9 can then
  move to the wire.
- **Dispatch-table audit (required by the technical notes above).** No remaining
  dispatch method *names* a session without a gate — the only params types
  carrying a `session_id` are `session/attach`, `session/clear`,
  `session/prompt` and `web/override`, all gated, and `permission/respond` now
  resolves its session via `PendingPermissions::owner_of`. What remains is a
  different shape and is **reported, not fixed**: `config/set`, `model/set`,
  `model/confirm`, `web/refresh`, `config/get` and `cost/query` take no
  connection at all and affect (or expose) every session daemon-wide.
  `model/confirm` in particular is the same bug as this task's at daemon scope —
  it answers a broadcast proposal by `request_id`, and any handshaked connection
  may answer one it did not raise. `session/create` is also not ancestry-gated,
  so a daemon descendant that may never *attach* may still create and drive a
  session of its own.
