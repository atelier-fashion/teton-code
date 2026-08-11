---
id: REQ-568
title: "Session-scoped event delivery and bounded request frames"
status: approved
deployable: true
created: 2026-08-10
updated: 2026-08-11
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon", "json-rpc", "cli"]
concerns: ["security", "privacy"]
tags: ["session-scoping", "event-bus", "socket-auth", "local-only", "broadcast", "frame-length", "dos", "forward-events"]
---

## Description

Two findings from the REQ-567 security re-verify (2026-08-10), both in the
daemon's client-facing socket layer.

**Finding 1 — every client receives every session's events.**
`forward_events` (crates/tetond/src/server.rs) relays every EventBus envelope
to every handshaked connection with no session filter, and the CLI renders
`AgentMessageChunk` unconditionally (crates/teton/src/session_ui.rs). Socket
auth is uid-only (0600 socket + kernel peer-uid check, auth.rs), so any
same-UID process — including tool children and MCP subprocesses the daemon
itself spawns — can connect, handshake, and passively read every session's
streamed model output. On a session pinned to the local tier by `local-only`
content, that streamed output *is* the boundary content: the BR-1 privacy
promise is enforced at egress but leaks sideways at the client socket.
REQ-567's conversation carry raises the stakes — sessions now accumulate
transcripts across prompts, and a `session/prompt` issued against another
client's `session_id` is served from that carried transcript, so the write
path doubles as a read path (informed by REQ-567, LESSON-501).

**Finding 2 — unbounded request frames.** The per-connection reader
(`reader.read_line`, server.rs ~264) buffers a request line of arbitrary
length before parsing. A same-UID peer can hold daemon memory hostage with a
single unterminated frame.

The fix direction: make event delivery session-scoped by default — filter in
`forward_events` on the connection's attached session set (envelopes already
carry `Option<SessionId>`, broadcast.rs) — with an explicit opt-in for
monitor-style clients; require attachment for session-mutating methods; and
cap inbound frame length with a deterministic refusal.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| Connection | attached_sessions | set of SessionId | starts empty; grows via `session/create` (creator auto-attaches) and `session/attach`; connection-lifetime, never persisted |
| Connection | monitor | boolean | default false; settable only by explicit declaration at handshake |
| EventEnvelope | session_id | Option\<SessionId\> | existing field, unchanged; `None` means daemon-scoped |
| Daemon | max_frame_bytes | integer (bytes) | single daemon-wide constant; large enough for the largest legitimate `session/prompt` |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| receive session-scoped events (envelope `session_id = Some(s)`) | connection with `s` in attached_sessions; any monitor connection |
| receive daemon-scoped events (envelope `session_id = None`) | any handshaked connection |
| session/prompt, session/clear | connection attached to the target session |
| session/create, session/list, session/attach | any handshaked same-UID connection (unchanged) |
| declare monitor | any handshaked same-UID connection, explicitly at handshake |

## Business Rules

- [ ] BR-1: A session-scoped envelope is delivered to a connection only if that connection is attached to the envelope's session or has declared monitor. Default delivery is session-scoped; ambient receipt of other sessions' events is impossible without the explicit monitor declaration (informed by LESSON-495).
- [ ] BR-2: Daemon-scoped envelopes (`session_id = None` — model download/benchmark progress, lifecycle) are delivered to every handshaked connection, unchanged.
- [ ] BR-3: The filter is enforced in the daemon at the forwarding seam, not in any client. Client-side rendering choices are presentation, never a privacy control — every current and future client (CLI, extension, ACP shim) crosses the daemon seam (informed by LESSON-484, LESSON-432).
- [ ] BR-4: `session/prompt` and `session/clear` are refused with a distinct, stable error code when the issuing connection is not attached to the target session. Attachment is the single grant seam for session access; mutating methods do not carry an implicit grant (informed by LESSON-495, LESSON-484).
- [ ] BR-5: Monitor declaration is explicit, visible, and never a default: it is stated at handshake, and the daemon records/announces it such that a monitor's existence is observable (at minimum in the daemon log).
- [ ] BR-6: An inbound frame exceeding `max_frame_bytes` is refused with `INVALID_PARAMS` (null id — the frame was never parsed) and the daemon never buffers more than the cap for any connection. Post-refusal behavior is deterministic (resync to next newline or close; decided at architecture, then invariant).
- [ ] BR-7: Filtering must not stall the response fence: an envelope skipped by the filter advances the connection's forwarded watermark exactly as a delivered one does, so no response can wait on an event the connection will never receive.
- [ ] BR-8: Existing single-client flows are behavior-identical: a client that creates a session and prompts it observes the same events, ordering, and responses as today (informed by BUG-152 — state classification lives in the daemon; clients receive codes, not guesses).

## Acceptance Criteria

- [ ] AC-1: Two connections, two sessions: connection B receives none of session A's envelopes (asserted at the socket, on raw NDJSON — not via CLI rendering), while both receive daemon-scoped envelopes.
- [ ] AC-2: A handshaked connection that never created or attached a session receives only daemon-scoped envelopes.
- [ ] AC-3: A connection declaring monitor at handshake receives all sessions' envelopes; the declaration is observable in the daemon log.
- [ ] AC-4: `session/prompt` and `session/clear` against a session the connection never attached are refused with the BR-4 code; after `session/attach` the same calls succeed.
- [ ] AC-5: A frame larger than `max_frame_bytes` is refused per BR-6, daemon memory for that connection's read buffer stays ≤ the cap (asserted by construction: the reader is incapable of buffering more), and a fresh connection still serves normally afterward.
- [ ] AC-6: A response gated on the event fence completes when the filter drops events destined for other connections' sessions (no hang, no timeout).
- [ ] AC-7: The full existing e2e suite passes unchanged for single-client attach → prompt → stream flows.
- [ ] AC-8: The CLI renders only envelopes for its own attached session (defense in depth atop BR-3, not a substitute for it).

## External Dependencies

- None. All changes are within the existing workspace (tetond, teton-protocol, teton).

## Assumptions

- `EventEnvelope` already carries `Option<SessionId>` and every session-scoped publish populates it — verified in broadcast.rs at spec time; any publish site passing `None` for what is actually session output would silently broadcast under BR-2 and must be audited at architecture time (informed by LESSON-432).
- The uid boundary remains the outer auth perimeter. This REQ removes *ambient* cross-session exposure within that perimeter; it does not claim to sandbox a deliberately malicious same-UID process (see Open Questions on attach authorization).
- `session/create` auto-attaching the creator matches every existing client flow; no current client prompts a session it did not create or attach.
- The protocol change (monitor declaration in handshake, new error code) is additive; existing clients that never set the new field keep today's semantics minus the leak.

## Open Questions

- [x] OQ-1: RESOLVED 2026-08-11 — filed as REQ-569 (session attach requires a grant). This REQ stays a tight containment fix: attach remains open to same-UID connections here, recorded as an accepted residual until REQ-569 lands. (Original question: session ids are guessable and `session/attach` has no authorization beyond uid, so BR-1's filter stops passive receipt but not deliberate attachment; the daemon-spawned tool/MCP subprocess case is the sharp end.)
- [ ] OQ-2: What is `max_frame_bytes`? `session/prompt` legitimately carries large pasted content; the cap must clear the largest supported prompt with margin. Candidate: single-digit MiB, decided with a measurement at architecture time.
- [ ] OQ-3: Oversized frame: refuse-and-resync (discard until newline, keep the connection) or refuse-and-close? Resync keeps a sloppy client alive; close is simpler and an attacker reconnects either way.
- [ ] OQ-4: Should a monitor declaration require anything beyond the handshake field — e.g., surfacing as a user-visible event so an interactive client can display "a monitor is attached"?
- [ ] OQ-5: Should `session/attach` itself emit an event to the session's existing attachees (visibility of new readers)?

## Out of Scope

- Cross-UID access, socket path/permission changes, or any change to the peer-credential auth layer (auth.rs stays as is).
- Attach authorization and sandboxing daemon-spawned tool/MCP subprocesses away from the socket — filed as REQ-569; not solved here.
- Backpressure/lag policy changes — `SUBSCRIPTION_LAGGED` eviction semantics are untouched.
- The ACP compatibility shim.
- Rate limiting or any DoS surface beyond the single unbounded-frame finding (connection-count caps, event-flood throttling).

## Retrieved Context

- LESSON-501 (lesson, score 9): State carried past its creator's lifetime sheds invariants silently
- LESSON-494 (lesson, score 8): A security gate and the client that executes the request must share one parser
- LESSON-432 (lesson, score 8): Provenance must derive from what a tool touches, not from an argument name
- LESSON-490 (lesson, score 6): A guard that runs on an encoded form is tested against the encoder's output
- LESSON-492 (lesson, score 6): A composite guard's failure path must not discard evidence a completed pass established
- LESSON-495 (lesson, score 5): A remembered grant answers every question its key matches — so the key must encode the whole question
- LESSON-497 (lesson, score 5): A test fixture that looks like a real credential blocks the push that ships it
- BUG-152 (bug, score 4): A prompt typed while the local tier is still loading is reported as an error, not as a wait
- LESSON-443 (lesson, score 4): A guard keyed on a feature's absence disables itself when the feature lands
- LESSON-445 (lesson, score 4): Side effects of a minutes-long operation must be staged, then committed only after re-checking authority
- LESSON-484 (lesson, score 3): Enforce a rule where the decision is made, not where it was convenient to write
- BUG-155 (bug, score 3): REQ-557's deleted provider-id fallback was only relocated, and three other defects it shipped
- REQ-557 (spec, score 3): Provider model identity and an explicit default provider
- LESSON-479 (lesson, score 3): A subset invariant is only tested in the direction your loop iterates — write the equation down, then check which half you wrote
- BUG-151 (bug, score 3): The frame-marker coverage invariant only holds in one direction
