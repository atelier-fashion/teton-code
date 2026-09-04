---
id: TASK-005
title: "Three new events and one new field on context_pressure"
status: complete
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

The wire half. Three events and one additive field, each with its round-trip
test and its CLI render arm, so the daemon-side wiring in TASK-006/007 has
something to publish.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `ContextCompacted`, `SkillRefusedNoRoom`, `TurnRefusedAnchorsExceedBudget`; `Event` variants and `name()` arms; `ContextPressure.anchors_intact`
- `crates/teton/src/session_ui.rs` — one render arm per event

## Acceptance Criteria

- [x] `context_compacted` carries the `CompactionRecord` fields, the route
      `(provider_id, model)` as `route_decided` already reports it, and `fallback`.
- [x] `skill_refused_no_room` carries `skill`, `body_bytes`, `budget_bytes`,
      `room_fraction`, `route` and the remedy sentence.
- [x] `turn_refused_anchors_exceed_budget` carries `anchor_bytes`, `budget_bytes`
      and the anchor kinds.
- [x] `ContextPressure.anchors_intact` is `#[serde(default)]` so a frame from a
      daemon predating the field means what that daemon could report — the
      posture `bound_floored` already takes.
- [x] Each event round-trips under its wire name and each renders one CLI line;
      the `Event::name()` match stays exhaustive (a missing arm is a compile
      error).
- [x] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `teton_protocol::events::tests::context_compacted_round_trips_under_its_wire_name` | no |
| BR-4 | test-case | `teton_protocol::events::tests::skill_refused_no_room_round_trips` | no |
| BR-1 | test-case | `teton_protocol::events::tests::turn_refused_anchors_exceed_budget_round_trips` | no |

## Technical Notes

Copy `SkillOverBudgetOffered`'s shape: internally-tagged flattened `Event`, so no
`session_id` field on the struct (it would emit the key twice). Follow
`ContextPressureKind`'s `#[serde(other)] Unknown` posture for any new enum that
travels daemon → client.
