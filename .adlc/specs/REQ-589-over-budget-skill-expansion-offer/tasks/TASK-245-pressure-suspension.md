---
id: TASK-245
title: "Suspend the pressure gate for exactly one iteration"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

BR-12 / ADR-8 / D-3. An accepted over-budget turn must not drop history: consent to send is not consent to lose the conversation. The drop is two calls inside `run_session_turn_with_source`.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — gate `ctx.compact_if_pressured` (941) and `ctx.truncate_to_budget()` (954) behind a per-turn flag
- `crates/tetond/src/harness/mod.rs` or the config struct — carry the flag

## Acceptance Criteria

- [x] Both calls are skipped on the first iteration of an accepted turn and on no other
- [x] The flag clears before the second iteration and cannot leak to the next turn (D-7)
- [x] With history large enough to trigger the gate, no block is dropped; the block list before and after is compared (AC-16)
- [x] A turn that then cannot fit surfaces the typed context outcome rather than a silently shortened conversation
- [x] Deleting the suspension reddens a dedicated seam test (LESSON-508) — end-to-end coverage alone is not sufficient

## Technical Notes

The comment at turn_loop.rs:917 cites REQ-561 ADR-4 and REQ-567 BR-4 — the exact rule this carves a one-turn exception into. Cite it in the code comment and the PR.
