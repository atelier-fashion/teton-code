---
id: TASK-210
title: "Protocol: the two flags, who invoked, and an acknowledgment subject an old client refuses"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

Every wire element REQ-587 adds, additive, with `PROTOCOL_VERSION` unmoved —
and one variant that is **not** additive in the way the existing test proves,
which is the point of doing it first.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `SkillView` gains `model_invocable` / `user_invocable`; the acknowledgment key's spelling and its `/cd`-expiry predicate, beside `skill_permission_key` and `RESERVED_SKILL_NAMES`
- `crates/teton-protocol/src/events.rs` — `SkillInvoked.invoked_by`, `SkillDynamicContext.invoked_by`, `PermissionSubject::ProjectSkillTrust`

## Acceptance Criteria

- [ ] Every new **field** is `#[serde(default, skip_serializing_if = …)]` and gets the four-leg skew test at `events.rs:~3616` copied whole — including the non-vacuity leg, which is the one that catches a fixture that never carried the key.
- [ ] **`PermissionSubject::ProjectSkillTrust` gets its own variant-skew leg, because the field test does not cover it.** The enum is closed with `#[serde(other)] Unrecognized`, and that arm is a **refusal**, not an ignore. Assert: a REQ-585-vintage reader parsing the new variant lands on `Unrecognized`; the client turns that into `RefusalReason::UnrecognizedSubject`; and the daemon-side consequence is `project_not_acknowledged` with a next step that client can actually perform. A project skill is simply never model-invocable there, and that is a shipped consequence, not a bug — say so in the doc.
- [ ] The acknowledgment key's spelling and `is_project_acknowledgment_key` live **here**, above both crates, for ASSUME-017's reason: a decision with two stores needs one invalidation rule. `/cd` expires it on the daemon *and* in the client's `SessionGrants`.
- [ ] `PROTOCOL_VERSION` unchanged — asserted, not assumed.
- [ ] `SkillInvoked` still never carries the body (pinned at `skill_turn.rs:~1868`); `path_display` stays home-relative and bounded.
- [ ] Mutation: downgrading any `skip_serializing_if` to bare `default` fails the "no key, not null" leg; deleting `#[serde(other)]` fails the `Unrecognized` pin.

## Technical Notes

- Field precedent: `SkillInvoked.name_note` (`Option<String>`) and `SkillSkipped.name` (`String::is_empty`) — pick by whether absent and empty mean the same thing.
- `SkillView`'s two flags are what `/help` renders `(model-only)` from; without them the mark cannot exist (TASK-219).
