---
id: TASK-003
title: "The compact duty is never offered an anchor, and an answer naming one is rejected whole"
status: draft
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001]
---

## Description

`CompactOffer::droppable` protects exactly one block — the newest — by count.
Anchors are not a prefix, so a count cannot express them. Give the offer a
protected-index set, mark anchored blocks in the rendered prompt the way the
step-in-progress is already marked, and reject any answer that names one.

## Files to Create/Modify

- `crates/tetond/src/harness/compact.rs` — `CompactOffer.protected: BTreeSet<usize>`; anchored-block note in `compact_offer`; `read_compaction` takes the protected set
- `crates/tetond/src/harness/context.rs` — `attempt_compaction` passes the set

## Acceptance Criteria

- [ ] `compact_offer` renders an anchored block with a note in the same shape as
      `PROTECTED_BLOCK_NOTE`, naming why it cannot be forgotten.
- [ ] `read_compaction` rejects an answer naming any protected index, returning a
      degraded reason — never a partial application (REQ-561 BR-4 unchanged).
- [ ] A duty answer naming only droppable indices still applies, so the change
      protects without retiring compaction (benign path).
- [ ] Inversion recorded: drop the protected set from `read_compaction`, confirm
      the rejection test goes red.
- [ ] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `harness::compact::tests::an_answer_naming_an_anchor_is_refused` | yes |
| BR-2 | test-case | `harness::compact::tests::an_anchored_skill_body_is_marked_in_the_offer` | yes |

## Technical Notes

ADR-618-2. The protected block note is already written whether or not the block
was itself offered; keep that property for anchors — a duty shown a prefix must
still know which indices are off limits.
