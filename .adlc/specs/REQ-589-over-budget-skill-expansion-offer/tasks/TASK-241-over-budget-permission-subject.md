---
id: TASK-241
title: "Protocol: SkillOverBudget subject, option ids, and the three events"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

ADR-1 + ADR-2. Add the consent subject and the four option ids that express BR-7's four combinations as a single-select, plus the three new events. No `PermissionOutcome` change.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `PermissionSubject::SkillOverBudget` beside `SkillDynamicContext` (~1213); three events in the Events enum + dispatch table
- `crates/teton-protocol/src/methods.rs` — option-id constants

## Acceptance Criteria

- [x] Four option ids: `over_budget_proceed_once`, `over_budget_proceed_and_remedy`, `over_budget_remedy_only`, `over_budget_decline`
- [x] `PermissionOutcome` is UNCHANGED — a test asserts the wire shape did not widen (ASSUME-B)
- [x] The subject carries measured integers, bound, verdict, skill name, sanitized provider id — and no provider response body
- [x] Adding the variant fails compilation anywhere `PermissionSubject` is matched non-exhaustively (this is the intended forcing function)
- [x] Round-trip serde test for the new subject and each event

## Technical Notes

`PermissionSubject` is `#[serde(tag = "kind")]`; the client's `#[serde(other)]` Unrecognized arm means an older client refuses rather than mis-renders — that is BR-4-compatible and must be left intact.
