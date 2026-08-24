---
id: TASK-244
title: "authorize_skill_over_budget on the permission gate"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-241]
---

## Description

BR-10 + BR-4. A new gate method beside `authorize_skill` (1365), asked under a `skill:` key so it cannot be smuggled through another key family (ADR-7 of REQ-587). Silence is never consent.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — `authorize_skill_over_budget`; option set built conditionally like `options_for` (2055)

## Acceptance Criteria

- [ ] The question is asked under a `skill:` key; `is_skill_permission_key` (2019) rejects anything else — a hard return, not a debug_assert
- [ ] No-route, connection-refused, and disconnected-channel paths all resolve to `Unanswerable`, never to proceed (BR-4, AC-4)
- [ ] Nothing is persisted: no grant survives the invocation (BR-10, AC-10)
- [ ] Remedy-bearing options appear only when BR-7 grants that bound a remedy
- [ ] A seam-level unit test asserts the non-persistence guard's removal reddens (LESSON-508)

## Technical Notes

Reuse `settle`/`Addressed` and the existing `SkillConsent` arms (ASSUME-B). The addressee is the connection that submitted the turn (REQ-587 ADR-3).
