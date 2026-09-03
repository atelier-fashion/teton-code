---
id: TASK-361
title: "Protocol: transcript_state, session/transcript, and ConfigUpdate::SetTranscriptEnabled"
status: draft
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

- [ ] `transcript_state` round-trips through serde with the flat envelope shape
      `{ "session_id": …, "seq": …, "event": "transcript_state", "enabled": …, "reason": … }`.
- [ ] BR-15: the `TranscriptState` struct has no `path` field; a test deserializes a payload that
      carries a stray `path` key and asserts it is dropped (or refused, matching the crate's
      posture for unknown keys — copy whichever sibling events use).
- [ ] `TranscriptStateReason` and `TranscriptAction` are closed enums: an unknown string is a
      deserialization error, never a default.
- [ ] `session/transcript` round-trips each action; `ENDS_TURN` is `false` and the ends-turn
      table test passes with the new row.
- [ ] `ConfigUpdate::SetTranscriptEnabled(true)` round-trips and appears in
      `config_set_round_trips_each_update_variant` (line ~4459).
- [ ] `cargo test -p teton-protocol --no-fail-fast` is green; the layering test (no crate above
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
