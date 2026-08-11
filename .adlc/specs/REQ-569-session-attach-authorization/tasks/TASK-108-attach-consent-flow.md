---
id: TASK-108
title: "Attach consent: event, attach/consent RPC, bounded timeout, fail-closed"
status: draft
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

- [ ] Granted consent mints exactly one grant of exactly the requested scope and the attach then succeeds (AC-2).
- [ ] Denied and timed-out requests mint nothing — asserted by inspecting the grant registry after, not inferred from the error code (BR-7).
- [ ] Timeout resolves to denied within the bounded window and emits `attach_refused` with the timeout reason code (AC-6).
- [ ] A descendant peer never reaches this flow at all — no consent event is published for it (asserted; TASK-106's gate precedes this).
- [ ] Consent-request ids are minted daemon-wide and `resolve` refuses to overwrite (LESSON-503/BUG-161 shape not reintroduced) — dedicated test.
- [ ] The reader loop stays responsive while a consent is pending (a second request on the same connection is still served), and a pending monitor consent in `do_handshake` does not wedge the accept loop for other connections.
- [ ] **Monitor is reachable again (BR-2/AC-4):** a monitor-scope consent granted by an already-attached connection produces a working monitor; refused/absent-approver leaves it `NOT_GRANTED`. A monitor grant still does not confer attach, and an attach grant still does not confer monitor (the scope-independence test from TASK-106 must stay green).
- [ ] **Un-ignore `e2e::conversation_carry::client_bs_prompt_carries_the_conversation_client_a_left_behind`** (REQ-567 AC-9, which TASK-106 `#[ignore]`d because cross-session attach was shut). Deleting that `#[ignore]` and seeing the test pass through the consent flow IS this task's AC-2/AC-3 evidence — do not write a parallel test and leave the original ignored.
- [ ] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- Deliberately a **separate registry** from `PendingPermissions`, not a reuse: that one is session-scoped by construction and an attach request has no attachment yet (ADR-E).
- The `requester` string is attacker-influenced (client-supplied name) — bound its length and escape it wherever it is logged, exactly as REQ-568's monitor log line does.
- Timeout duration: a named const with a one-line rationale, not a magic number.
- Nothing here is persisted (ADR-C).
