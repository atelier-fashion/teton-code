---
id: TASK-002
title: "Write the two fixture files with provenance headers"
status: complete
parent: REQ-604
repo: teton-code
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-001]
---

## Files to Create/Modify

- `crates/tetond/tests/fixtures/req604_skill_turn_event_order.txt` (new)
- `crates/tetond/tests/fixtures/req604_consent_turn_event_order.txt` (new)

## Acceptance Criteria

- [x] Each header records the capture commit `17c39ec` (AC-1).
- [x] Each header says ***runtime*** `TurnContext`, not bare `TurnContext`
      (ADR-8) — `ContentClass::TurnContext` existed at the protocol level at
      that commit, so the unqualified claim would be false.
- [x] Each header names which entries are detached and by what discriminator
      they are excluded (AC-4) — never by position.
- [x] The existing `req598_turn_event_order.txt` is **not** modified
      (Out of Scope; REQ-606 AC-4 depends on it replaying unregenerated).
