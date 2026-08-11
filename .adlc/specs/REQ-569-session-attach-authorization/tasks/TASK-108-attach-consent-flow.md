---
id: TASK-108
title: "Attach consent: event, attach/consent RPC, bounded timeout, fail-closed"
status: complete
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-106"]
---

## Description

The only way a grant is minted (BR-3/BR-6/BR-7, ADR-E). When a non-descendant
connection asks to attach to a session it did not create, the daemon raises a
consent request; a granted decision mints the grant, a denial or timeout mints
nothing. This is what restores the resume flow that TASK-106 deliberately
closed.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `attach_consent_requested` event: `{ request_id, session_id, scope, requester }` where `requester` is a **short, non-sensitive** description (client kind/name, already length-bounded per REQ-568's monitor-log fix — never a path, never env, never a command line).
- `crates/teton-protocol/src/methods.rs` — `attach/consent` method: `{ request_id, outcome: Granted | Denied }`.
- `crates/tetond/src/grants.rs` (or a sibling `consent.rs`) — a `PendingConsents` registry mirroring the proven `PendingPermissions` *shape* but **its own type** (ADR-E): daemon-wide monotonic id minting (LESSON-503 — mint at the scope that resolves), `oneshot` waiter, `resolve`, refuse-not-overwrite on id collision, and a bounded timeout.
- `crates/tetond/src/server.rs`:
  - `handle_session_attach`, in the `NOT_GRANTED` branch from TASK-106: instead of refusing immediately, raise a consent request and await it. **Routing (BR-6):** publish the event scoped to the target session so any *already-attached* connection renders it; if none is attached, deliver it to the requesting connection itself (permitted only because TASK-106 already refused descendants). State which arm ran in the code comment.
  - On `Granted` → mint the grant, then attach. On `Denied` / timeout / dropped waiter → refuse with the distinct BR-5 code and publish `attach_refused` with a stable reason (`no_grant` / `consent_denied` / `consent_timeout`).
  - The await must not block the reader loop (the `session/prompt` precedent: run it so `attach/consent` can still be read on the same or another connection — otherwise the flow deadlocks awaiting a reply it cannot read).
- `crates/teton-protocol/src/jsonrpc.rs` — `CONSENT_DENIED` and `CONSENT_TIMEOUT` codes (BR-5's third and fourth distinct reasons), doc-commented.
- **Monitor-scope consent (BR-2/AC-4) — added 2026-08-11 after TASK-106 reported that nothing mints a monitor grant.** Without this, REQ-569 ships `monitor` permanently unreachable, which is a regression against REQ-568, not a security posture. `do_handshake`'s monitor branch (currently refusing `NOT_GRANTED` for everyone) raises a `Monitor`-scope consent request. **Routing rule, deliberately narrower than attach's:** monitor consent is offered **only** to connections already attached to some session — a monitor is a whole-daemon read capability, so it is approved by a surface the user demonstrably already owns, never self-rendered by the requester. If no connection is attached anywhere, monitor is refused (`NOT_GRANTED`) rather than self-approved. This does not weaken BR-6, which is about the *resume* flow for attach, not monitor. Ancestry still precedes everything: a descendant is refused `ATTACH_FORBIDDEN` and no consent is raised. Note the handshake now awaits a bounded consent; make sure that await cannot wedge the accept loop for other connections.
- Tests: consent granted → attach succeeds and REQ-568 delivery applies; denied → refused, no grant left behind; timeout → refused within the bounded window, `attach_refused` emitted with the timeout reason, **and the registry has no residual entry** (BR-7/LESSON-501 — the decision travels with the grant; assert the absence, do not assume it).

## Acceptance Criteria

- [x] Granted consent mints exactly one grant of exactly the requested scope and the attach then succeeds (AC-2).
  — `server::tests::a_granted_consent_mints_exactly_one_attach_grant_and_the_attach_succeeds`
  asserts `grants.held_by(newcomer) == [Grant::attach(newcomer, session)]`, `grants.len() == 1`,
  and `!may_monitor(newcomer)`.
- [x] Denied and timed-out requests mint nothing — asserted by inspecting the grant registry after, not inferred from the error code (BR-7).
  — `server::tests::a_denied_or_timed_out_consent_leaves_the_grant_registry_empty` runs both
  endings and asserts `held_by(...).is_empty()` and `grants.is_empty()` after each.
- [x] Timeout resolves to denied within the bounded window and emits `attach_refused` with the timeout reason code (AC-6).
  — same test: elapsed time bounded by the injected window, and exactly one `attach_refused`
  frame carrying `reason: consent_timeout` reaches the requester.
- [x] A descendant peer never reaches this flow at all — no consent event is published for it (asserted; TASK-106's gate precedes this).
  — `server::tests::a_daemon_descendant_is_refused_attach_before_any_session_lookup_or_prompt`,
  with an ordinary connection's prompt on the *same* surface as the positive control.
  Extended beyond the task's ask: a descendant may not **approve** one either
  (`a_daemon_descendant_may_not_approve_a_consent_request_either`) — see the deviation note below.
- [x] Consent-request ids are minted daemon-wide and `resolve` refuses to overwrite (LESSON-503/BUG-161 shape not reintroduced) — dedicated test.
  — `consent::tests::consent_ids_are_minted_daemon_wide_and_never_repeat` and
  `consent::tests::a_colliding_registration_cannot_steal_a_live_consent_request`.
- [x] The reader loop stays responsive while a consent is pending (a second request on the same connection is still served), and a pending monitor consent in `do_handshake` does not wedge the accept loop for other connections.
  — `multi_client::the_reader_loop_keeps_serving_while_a_consent_is_pending` (ordering, not
  liveness) and claim (3) of
  `multi_client::a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor`.
- [x] **Monitor is reachable again (BR-2/AC-4):** a monitor-scope consent granted by an already-attached connection produces a working monitor; refused/absent-approver leaves it `NOT_GRANTED`. A monitor grant still does not confer attach, and an attach grant still does not confer monitor (the scope-independence test from TASK-106 must stay green).
  — `multi_client::a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor`
  (works, and its own attach still has to ask); `a_monitor_declaration_is_refused_without_a_
  monitor_scope_grant` (no approver → `NOT_GRANTED`);
  `grants::tests::attach_and_monitor_are_answered_by_their_own_scope_and_no_other` unchanged.
- [x] **Un-ignore `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_a_left_behind`** (REQ-567 AC-9, which TASK-106 `#[ignore]`d because cross-session attach was shut). Deleting that `#[ignore]` and seeing the test pass through the consent flow IS this task's AC-2/AC-3 evidence — do not write a parallel test and leave the original ignored.
  — `#[ignore]` deleted; the only body change is that client A is built
  `.with_auto_consent()` (the user who says yes). Every assertion is REQ-567's original.
- [x] `cargo test -p tetond --no-fail-fast` green.
  — workspace: 2046 passed, 0 failed, 1 ignored (the pre-existing `--features live` smoke test).

## Deviations

- **`session/attach` no longer answers `NOT_GRANTED`.** Every ungranted attach now raises a
  prompt, so its refusals are `CONSENT_DENIED`/`CONSENT_TIMEOUT`. `NOT_GRANTED` survives on the
  monitor path only (no attached client to ask). Tests that asserted the old code were updated,
  not deleted.
- **Two additions the task did not name but the design needed.** (1) `ConsentSurfaces`, a
  daemon-wide registry of live connections, because BR-6's routing asks a question no single
  connection can answer ("is anyone else attached to this session?"); it shares each connection's
  attachment `Arc` rather than copying it. (2) An ancestry gate on `attach/consent` itself: a tool
  child *can* hold a session it created, which would have made it an eligible **approver** of a
  peer's monitor request — BR-4 holding on the door and failing on the door handle.
- **`Daemon::with_consent_timeout`** is a fixture-only knob (the `with_daemon_process` precedent),
  so a test can assert the timeout arm without waiting out the shipped 30 s window.
- **Client-side consent UI is out of scope and remains so.** The CLI renders
  `attach_consent_requested` as a notice and cannot answer it; nothing in `crates/teton` calls
  `session/attach`, so no shipped flow regressed. BR-6's user-facing half needs client work
  (TASK-109 / follow-up).

## Technical Notes

- Deliberately a **separate registry** from `PendingPermissions`, not a reuse: that one is session-scoped by construction and an attach request has no attachment yet (ADR-E).
- The `requester` string is attacker-influenced (client-supplied name) — bound its length and escape it wherever it is logged, exactly as REQ-568's monitor log line does.
- Timeout duration: a named const with a one-line rationale, not a magic number.
- Nothing here is persisted (ADR-C).
