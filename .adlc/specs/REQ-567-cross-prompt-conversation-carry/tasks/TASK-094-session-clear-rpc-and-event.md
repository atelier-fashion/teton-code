---
id: TASK-094
title: "session/clear RPC + context_cleared event + spec erratum"
status: complete
parent: REQ-567
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-092, TASK-093]
repo: teton-code
---

## Description

The daemon-side clear surface (architecture D-2): one new RPC
`session/clear` — `{ session_id } -> { blocks_dropped }` — that empties the
session's conversation and emits the `context_cleared` event. Clears
nothing else (OQ-4 product decision: taint, pasted-URL set, and permission
grants survive). Corrects the spec's "No new RPCs" System Model note in the
same commit.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `SessionClearParams { session_id }`
  / `SessionClearResult { blocks_dropped }` beside the other session-scoped
  methods.
- `crates/teton-protocol/src/events.rs` — `Event::ContextCleared(ContextCleared)`
  with `session_id` riding the envelope (the `SessionTitled` shape),
  payload `blocks_dropped`; wire name `context_cleared` in `Event::name()`.
- `crates/tetond/src/runtime.rs` — handler: `clear_conversation(id)`,
  publish the event; a clear during an in-flight turn is refused with the
  same typed busy error as a concurrent prompt (the turn owns the
  conversation until it commits — clearing under it would make commit
  resurrect the cleared history, violating BR-8's "next prompt starts from
  the system head alone").
- `crates/tetond/src/server.rs` — route the method.
- `.adlc/specs/REQ-567-cross-prompt-conversation-carry/requirement.md` —
  D-2 erratum: System Model note now reads "one new RPC, `session/clear`;
  `session/prompt`'s wire shape is unchanged".

## Acceptance Criteria

- [ ] Wire-shape tests in `events.rs` (`round_trip` + `envelope_wire`
  pattern): `context_cleared` round-trips; envelope carries `session_id`;
  `blocks_dropped` on the wire.
- [ ] Runtime test: clear on a populated session empties it (next
  snapshot is empty), returns the dropped count, emits exactly one
  `context_cleared`; clear on an empty/unknown session returns 0 and still
  succeeds (idempotent).
- [ ] Clear-vs-turn race test: `session/clear` during an in-flight turn is
  refused with the typed busy error; after the turn completes, clear
  succeeds and the next prompt's context contains no prior conversation
  (AC-6 wire half).
- [ ] OQ-4 test: after clear, session taint and remembered permission
  grants are unchanged (a tainted session stays pinned; a granted tool
  stays granted).
- [ ] `cargo test --workspace` green.

## Technical Notes

Event follows the one-variant-with-payload precedent; `session_id` must NOT
be a payload field (internally-tagged flatten collision — see the
`PrefixCache` doc comment in events.rs). The busy refusal reuses TASK-093's
error shape — same claim, same error, one truth (LESSON-456's single-
classifier posture).
