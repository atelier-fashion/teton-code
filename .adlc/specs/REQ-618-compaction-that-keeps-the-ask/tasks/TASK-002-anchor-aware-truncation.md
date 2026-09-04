---
id: TASK-002
title: "The deterministic drop and the in-place clamp both stop at an anchor"
status: draft
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001]
---

## Description

`truncate_to_budget` drops `blocks[0]` until the estimate fits, and clamps the
last block in place when it alone busts the byte budget. Both reach the user's
ask. Teach the loop to skip anchored blocks and the clamp to refuse an anchored
last block, and add the two readers BR-1's refusal will need.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — anchor-aware drop loop; clamp guard; `anchor_bytes()`, `anchors_fit()`; `PressureReport` gains `anchors_intact`

## Acceptance Criteria

- [ ] The drop loop removes the oldest **non-anchored** block each iteration and
      stops when only anchors remain, leaving `over_budget: true` rather than
      dropping one.
- [ ] The in-place clamp is skipped when the last block is anchored; the context
      is left over budget and says so.
- [ ] `anchor_bytes()` sums the anchored blocks plus the system prompt;
      `anchors_fit()` compares it against both budgets.
- [ ] `PressureReport.anchors_intact` is `true` on every path this method can
      take — by construction, and asserted rather than assumed (BR-1's witness).
- [ ] Inversion recorded: remove the anchor guard from the drop loop, confirm the
      new tests go red, write the count in the test's doc comment.
- [ ] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `harness::context::tests::the_drop_loop_never_takes_an_anchor` | yes |
| BR-1 | test-case | `harness::context::tests::the_clamp_refuses_an_anchored_last_block` | yes |

## Technical Notes

ADR-618-2. This method runs from `Drop` in `CarriedTurn::commit_now`, so it must
keep **returning** rather than refusing — the refusal is TASK-006's, raised at
the gate. Keep the existing `blocks.len() > 1` floor as the outer bound so an
all-anchor context still leaves content.
