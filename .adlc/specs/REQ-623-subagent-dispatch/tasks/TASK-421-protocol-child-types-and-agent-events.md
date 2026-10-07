---
id: TASK-421
title: "Protocol: child types, seven agent events, and child ids on the four scoped payloads"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: []
repo: teton-code
---

## Description

The wire vocabulary every other task reads. Adds `ChildId`, `ChildTask`, `ChildBounds`,
`ChildStatus` (eight variants), `ChildResult`, `AgentRefusal`; the seven `agent_*` `Event`
variants; and optional `child_id`/`parent_turn_id` on `ToolStarted`, `ToolFinished`,
`ContextPressure`, `CostRecorded`, `PermissionRequest`. Everything is additive and
serde-defaulted so an old CLI ignores the new fields and a new CLI reads `None` from an old
stream (architecture "Data model changes").

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — new `Event` variants with payload structs; `Event::name()` arms; the five optional field pairs
- `crates/teton-protocol/src/agent.rs` — new: `ChildId`, `ChildTask`, `ChildBounds`, `ChildStatus`, `ChildResult`, `AgentRefusal`, with serde round-trip tests
- `crates/teton-protocol/src/lib.rs` — export the new module
- `crates/teton/src/client.rs` — a decode test proving a post-REQ `tool_started` with `child_id` and a pre-REQ one without both deserialize

## Acceptance Criteria

- [x] Every `agent_*` event round-trips through serde with its wire name as the spec's event table spells it
- [x] `ChildStatus` serializes to the eight lowercase strings in the spec's entity table, and nothing else parses
- [x] A `tool_started` JSON with no `child_id` key decodes to `None`; one with it decodes to `Some`; serializing `None` omits the key
- [x] The exhaustive `Event::name()` test (if one exists) covers the seven new variants
- [x] `cargo test -p teton-protocol -p teton` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-10 | test-case | `crates/teton-protocol/src/agent.rs::tests::child_status_eight_variants_round_trip` | no |
| BR-13 | test-case | `crates/teton-protocol/src/events.rs::tests::child_ids_are_optional_and_omitted_when_none` | yes |
| AC-1 | test-case | `crates/teton-protocol/src/agent.rs::tests::child_task_schema_shape` | no |

## Technical Notes

- Fields go on **payloads, not `EventEnvelope`** (ADR-3) — the transcript tap reserves the envelope keys.
- `ChildId` is `"<call_id>/<name>"`; keep it an opaque newtype with `Display`, no parsing API.
- Match the existing `#[serde(rename_all = "snake_case")]` and internal-tag conventions in `events.rs`; look at `ToolCallRepeated` (REQ-617) as the most recent added variant.
- REQ-588's forward-compatible vocabulary rule: an unknown event kind must still be skippable by the CLI — verify the decoder's unknown-variant fallback still compiles with the new variants.
