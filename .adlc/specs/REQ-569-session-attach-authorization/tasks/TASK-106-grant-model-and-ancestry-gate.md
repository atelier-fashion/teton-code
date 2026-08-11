---
id: TASK-106
title: "Grant model and the ancestry hard gate on attach/monitor"
status: complete
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-103"]
---

## Description

The core security change. Introduce the grant model and apply Layer 1 (ancestry
hard refusal) and Layer 2 (grant required) to `session/attach` and the monitor
declaration (BR-1/BR-2/BR-3/BR-4, ADR-A/ADR-C/ADR-D). Consent minting lands in
TASK-108; until then a grant can only come from having created the session, so
this task alone is deliberately fail-closed for cross-session attach.

## Files to Create/Modify

- `crates/tetond/src/grants.rs` — NEW. `GrantScope { Attach, Monitor }` and a registry keyed by `(ConnectionId, SessionId, GrantScope)`, held in the daemon, entries dropped when the connection ends (ADR-C: daemon-lifetime, in-memory, nothing persisted). `Monitor` never implies `Attach` and vice versa (LESSON-495, and the same split REQ-568 made between `may_receive`/`may_drive`). Pure `fn may_attach(...) -> bool` / `fn may_monitor(...) -> bool` over the registry + creator set, table-tested.
- `crates/tetond/src/server.rs`:
  - `ConnState` gains the peer's `Ancestry` (computed once at handshake from TASK-103) and a connection id. **Ancestry is computed at handshake, not per call** — one kernel read per connection, and the value cannot drift mid-connection.
  - `handle_session_attach`: refuse *before* the registry lookup, in this order — (1) `Ancestry::Descendant` → refuse `ATTACH_FORBIDDEN` (no consent path, ever); (2) not the creator and no attach grant → refuse `NOT_GRANTED`; (3) otherwise attach as today. Ordering matters: the ancestry refusal must not leak session existence, so it precedes `daemon.sessions.get`.
  - Monitor declaration (in `do_handshake`): a `Descendant` peer declaring monitor is refused — the handshake itself fails rather than silently downgrading `monitor` to false (a silent downgrade would be a guard that disables itself).
  - `Ancestry::Indeterminate` policy: **treat as Descendant (refuse)** for attach/monitor. Fail closed, and log the reason distinctly so an operator can tell "refused because descendant" from "refused because we could not tell". Write the rationale in a comment.
- `crates/teton-protocol/src/jsonrpc.rs` — add `ATTACH_FORBIDDEN` and `NOT_GRANTED` to the `application_error_codes!` macro (next free codes after `-32009 NOT_ATTACHED`), each doc-commented with what distinguishes it from the other and from `NOT_ATTACHED` (BR-5: stable codes, clients render from codes not prose — BUG-152).
- Tests in `crates/tetond/tests/multi_client.rs` at the raw RPC surface (BUG-155 pattern): a non-creator connection attaching to another session is refused `NOT_GRANTED`; a creator attaching to its own session still succeeds; monitor without a grant is refused.

## Acceptance Criteria

- [x] A `Descendant` peer is refused attach AND monitor, before any session lookup, with `ATTACH_FORBIDDEN` — and receives no consent request of any kind.
- [x] `Indeterminate` fails closed (refused) and is logged distinguishably from `Descendant`.
- [x] A non-creator, non-granted connection is refused `NOT_GRANTED`; the creator's own attach is unchanged.
- [x] Monitor and attach scopes are independent — an attach grant does not enable monitor (dedicated test, per LESSON-495).
- [x] Refusals are asserted at the raw RPC surface, not through the CLI (BUG-155).
- [x] Mutation checks, each in isolation: inverting the ancestry gate fails the descendant test; inverting the grant check fails the non-creator test; making `may_monitor` read `may_attach` fails the scope-independence test. See each fail, restore.
- [x] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- The error text carries no session id, path, or content (conventions).
- Do not gate `session/list` here — TASK-105 owns its payload; the listing stays open.
- REQ-568's `may_drive` gates (prompt/clear/web-override) are unchanged; this task adds the *attach* seam beneath them.
- Grants are never derived from env, socket path, or filesystem state (BR-3) — the registry is only written by TASK-108's consent path and by session creation.

## Outcome — what TASK-108 inherits

The gate landed as specified and is fail-closed, so three behaviours that
REQ-568 shipped are **deliberately shut on this branch** until the consent path
mints grants. TASK-108 must reopen each, and each is asserted in its closed
state today rather than left untested:

1. **Cross-session attach.** Refused `NOT_GRANTED`. Pinned by
   `multi_client::knowing_a_session_id_does_not_let_another_connection_attach`
   and `e2e::ac_matrix::ac6_two_clients_share_sessions_daemon_survives_exit`.
2. **The resume flow (BR-6).** Same refusal — a fresh client cannot rejoin a
   session it did not create. `e2e::conversation_carry::client_bs_prompt_carries_
   the_conversation_client_a_left_behind` (REQ-567 AC-9) is `#[ignore]`d with
   that reason and its body left intact; deleting the `#[ignore]` is part of
   TASK-108's AC-2/AC-3 evidence.
3. **The `monitor` declaration.** Refused `NOT_GRANTED` at the handshake for
   every connection, because nothing mints a monitor-scope grant. BR-2 and AC-4
   ask for exactly this, but **no task in the current breakdown mints one**, so
   as things stand REQ-569 ships `monitor` permanently unreachable. TASK-108 or
   TASK-109 has to either add a monitor-scope consent path or record the
   unavailability as an intended outcome — it should not be discovered at
   wrap-up.

Also landed, not in the original file list: `DaemonProcess` on `Daemon`, which
names the process tree the ancestry gate excludes. Production constructors set
`Own(std::process::id())`; `Daemon::new()` (the fixture constructor) is
`Embedded`, because an in-process harness shares one pid with its clients and
`Own` would classify every one of them as the daemon itself. Pinned by
`server::tests::the_production_constructors_own_their_process`.
