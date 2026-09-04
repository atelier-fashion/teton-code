---
id: TASK-001
title: "Three new protocol events for the root gates"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

Add the three events the REQ's System Model names, so the gates in later tasks
have something typed to publish. Foundation task: no behaviour changes.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — three `Event` variants plus their
  payload structs and their `name()` arms.

## Acceptance Criteria

- [ ] `Event::WriteRefusedNonProject(WriteRefusedNonProject)` carries `tool`,
      `root_display`, `root_kind`, `remedy`.
- [ ] `Event::SkillRefusedNeedsProject(SkillRefusedNeedsProject)` carries
      `skill`, `source`, `root_display`, `root_kind`, `known_projects`.
- [ ] `Event::SkillPreambleFallback(SkillPreambleFallback)` carries `skill`,
      `command_index`, `root_display` — and **no output field**, so the
      preamble's bytes cannot ride the bus.
- [ ] Each variant's `name()` arm returns its snake_case wire name, matching the
      spec's event table exactly.
- [ ] `cargo test -p teton-protocol` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/teton-protocol/src/events.rs::the_root_gate_events_round_trip_their_wire_names` | no |
| BR-5 | test-case | `crates/teton-protocol/src/events.rs::the_root_gate_events_round_trip_their_wire_names` | no |
| BR-6 | test-case | `crates/teton-protocol/src/events.rs::the_preamble_fallback_event_carries_no_output` | no |

## Technical Notes

Follow the existing `SkillRefused` / `RepoContextState` shape: a payload struct
beside the enum, `#[serde(tag = "event", rename_all = "snake_case")]` on `Event`
does the wire naming, and the `name()` match gains one arm each.

`SkillPreambleFallback` deliberately has no output field. The event is a
*notice* that a fallback fired; the output itself reaches the model on the
expansion, where it is framed. An event carrying it would be a second copy on a
bus whose audience is every attached client and every declared monitor
(REQ-611 BR-4, LESSON-513).
