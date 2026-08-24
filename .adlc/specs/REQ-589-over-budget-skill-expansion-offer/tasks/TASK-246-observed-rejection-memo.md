---
id: TASK-246
title: "Session-scoped memo of observed window rejections"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

BR-14.2 / ADR-9. Remember that this skill on this route was actually rejected at the window, so the next offer is better informed. This is an observation, NOT a consent.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — a store shaped exactly like `EffortRefusals` (551), keyed by (SessionId, skill, route)

## Acceptance Criteria

- [ ] Session-scoped and never persisted to disk
- [ ] `mark()` returns the first-time transition so the caller can announce once
- [ ] The record does NOT suppress the next offer and does NOT pre-answer it — two negative assertions (AC-23, BR-10 boundary)
- [ ] The record lives in ONE store, daemon-side; the CLI does not memoize it (ASSUME-017)
- [ ] A resident system-prompt fact states that consents are not persisted and observations are, so the model cannot claim it 'remembers' a consent (LESSON-543)

## Technical Notes

`EffortRefusals`' doc comment already says 'Remembering is not retrying' — mirror that framing, it is the exact distinction BR-10 turns on.
