---
id: TASK-003
title: "Add the replay test module at tip"
status: pending
parent: REQ-604
repo: teton-code
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-002]
---

## Files to Create/Modify

- `crates/tetond/src/runtime/mod.rs` — new `mod req604_event_order` nested in
  `mod conversation_carry`, alongside `mod req598_event_order`

## Acceptance Criteria

- [ ] Both fixtures replay against the current tree (AC-2).
- [ ] Detached events excluded by discriminator, not position (AC-4).
- [ ] Non-vacuity per scenario: positive count of `skill_invoked` /
      `permission_request` respectively, plus exactly one non-title
      `route_decided`, plus a non-empty expected sequence (AC-5).
- [ ] A transposition of two adjacent distinct events fails, per scenario
      (AC-6).
- [ ] Every new assertion is shown able to fail — the mutation is run and
      recorded in the test's doc comment (conventions.md; LESSON-569).
