---
id: TASK-361
title: "Protocol: transcript_state, session/transcript, and ConfigUpdate::SetTranscriptEnabled"
status: complete
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The wire vocabulary, landed alone so the daemon and CLI tasks compile against one definition.
One new event, one new session method, one new config update variant (ADR-2, ADR-5, ADR-6).
Covers BR-15's shape rule (the event has no path) and the wire halves of AC-3, AC-5, AC-6.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `Event::TranscriptState(TranscriptState { enabled:
  bool, reason: TranscriptStateReason })`, `TranscriptStateReason { ConfigDefault,
  SessionCommand, WriteFailure, DirRefused }` as a closed snake_case enum; `name()` arm
  `"transcript_state"`; row in `event_names_match_the_spec_events_table` (line ~3915).
- `crates/teton-protocol/src/methods.rs` — `SessionTranscriptParams { session_id, action:
  TranscriptAction }`, `TranscriptAction { On, Off, Status }`, `SessionTranscriptResult {
  enabled: bool, path: Option<String>, records: u64, degraded: Option<String> }`, `impl RpcMethod`
  with `METHOD = "session/transcript"` and `ENDS_TURN = false`; `ConfigUpdate::
  SetTranscriptEnabled(bool)`; the new method's row in the ends-turn table (line ~3418).
- `crates/teton-protocol/src/lib.rs` — re-exports as the crate does for sibling types.

## Acceptance Criteria

- [x] `transcript_state` round-trips through serde with the flat envelope shape
      `{ "session_id": …, "seq": …, "event": "transcript_state", "enabled": …, "reason": … }`.
- [x] BR-15: the `TranscriptState` struct has no `path` field; a test deserializes a payload that
      carries a stray `path` key and asserts it is dropped (or refused, matching the crate's
      posture for unknown keys — copy whichever sibling events use).
- [x] `TranscriptStateReason` and `TranscriptAction` are closed enums: an unknown string is a
      deserialization error, never a default.
- [x] `session/transcript` round-trips each action; `ENDS_TURN` is `false` and the ends-turn
      table test passes with the new row.
- [x] `ConfigUpdate::SetTranscriptEnabled { enabled: true }` round-trips and appears in
      `config_set_round_trips_each_update_variant` (line ~4459).
- [x] `cargo test -p teton-protocol --no-fail-fast` is green; the layering test (no crate above
      protocol becomes a dependency of it) still passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-15 | test-case | `crates/teton-protocol/src/events.rs::transcript_state_carries_enabled_and_reason_and_no_path` | no |
| AC-5 | test-case | `crates/teton-protocol/src/methods.rs::session_transcript_round_trips_each_action` | no |
| AC-6 | test-case | `crates/teton-protocol/src/methods.rs::config_set_round_trips_each_update_variant` | no |

## Technical Notes

Follow the `SessionPermissionsParams` / `SessionPermissionsResult` pair (methods.rs ~2270–2310)
line for line; it is the nearest twin in gate shape and `ENDS_TURN` posture. The reason enum is
closed for the same reason `AttachConsentOutcome` is — there is no safe default to fall back to
when a client cannot read a state change.

Do **not** add the sink-local record kinds here (ADR-2). They are `tetond` types.

## Implementation notes

**Deviation: `SetTranscriptEnabled { enabled: bool }`, not `SetTranscriptEnabled(bool)`.**
`ConfigUpdate` is internally tagged (`#[serde(tag = "op")]`), and serde cannot serialize a
tagged newtype variant whose content is a primitive. The specified newtype spelling compiles
and `cargo check`s clean, then fails at **runtime** with `cannot serialize tagged newtype
variant ConfigUpdate::SetTranscriptEnabled containing a boolean` — a refusal the user would
meet at `teton transcript enable`, not one the author meets at build time (observed, then
fixed). The struct variant gives the flat
`{"op":"set_transcript_enabled","enabled":true}` and names the field after the config key.
`config_set_round_trips_each_update_variant` asserts the wire object explicitly so reverting
to the newtype reds there rather than shipping.

**`ENDS_TURN` is the trait default.** `RpcMethod::ENDS_TURN` defaults to `false` and no impl in
the crate states `false` explicitly (only `PromptTurnParams` states `true`); the twin
`SessionPermissionsParams` omits it. The value is pinned from the outside by the new row in
`only_the_prompt_method_ends_a_turn`.

**`lib.rs` needed no edit.** The crate re-exports no types from `events`/`methods` — the modules
are `pub` and every sibling type is reached as `teton_protocol::events::…` /
`teton_protocol::methods::…`. `TranscriptState`, `TranscriptStateReason`, `TranscriptAction`,
`SessionTranscriptParams`, `SessionTranscriptResult` are reachable the same way.

**Downstream compilation.** Adding an `Event` variant reds `crates/teton/src/session_ui.rs`'s
exhaustive render match, and the new `ConfigUpdate` variant reds `tetond`'s exhaustive matches
in `runtime/mod.rs` and `runtime/turn.rs`. Both are owned by later tasks in this REQ
(architecture component map) and are out of this task's scope; `cargo test -p teton-protocol`
is green.
