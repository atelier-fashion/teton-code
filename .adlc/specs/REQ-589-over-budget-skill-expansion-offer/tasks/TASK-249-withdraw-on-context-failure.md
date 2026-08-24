---
id: TASK-249
title: "Withdraw the expansion when an accepted turn fails at the window"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-239, TASK-246]
---

## Description

BR-14.1 / D-8. An approval must not leave the session hitting the same wall. On the typed context failure, withdraw the expansion so the next turn assembles cleanly.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — the turn's error handling calls `withdraw_block`, mirroring `withdraw_model_expansion` (10860) rather than reusing it

## Acceptance Criteria

- [ ] On `context_length_exceeded`, the accepted expansion is withdrawn via `ContextManager::withdraw_block` (context.rs:986)
- [ ] The withdrawn block's provenance is absorbed into `DroppedProvenance` — a `local-only` source must not survive the withdrawal (BUG-188)
- [ ] The NEXT turn in that session assembles without the expansion — driven by a real second turn, not by inspecting the block list (AC-22)
- [ ] Withdrawal fires only on the context failure; other failure classes leave the turn to the ordinary retry machinery

## Technical Notes

`withdraw_block` already absorbs provenance (context.rs:990-991) — that is why BR-14.1 names it rather than inventing a path. Depends on TASK-239: without the typed outcome there is no reliable trigger.
