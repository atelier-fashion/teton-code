---
id: TASK-001
title: "Classify all fourteen bundles and write each stated reason into its doc"
status: complete
parent: REQ-606
created: 2026-09-01
updated: 2026-09-01
dependencies: []
---

## Description

AC-1, and the task the other three depend on: the classification decides
whether each of them happens at all. Every one of the fourteen gets one of the
three verdicts, and the verdict is written where a reader meets the type — in
its doc comment — not only in the architecture doc.

The rules are stated in `architecture.md` (Rule A, Rule R, Rule I) and were
fixed before the verdicts were assigned. A type kept under Rule A says so with
its post-collapse argument count, so the next reader can check the arithmetic
rather than trust the adjective.

**AC-1's third category is the one with real work in it.** `route` appears in
four bundles, `probed` in three, `turn_id` in three. That duplication is
deliberate — each occurrence is the same value at a different ownership stage,
which Rust forces — and AC-1 requires the reason to be *in the type's doc*.

## Files to Modify

- `crates/tetond/src/runtime/turn.rs` — docs on the ten bundles declared there
- `crates/tetond/src/harness/turn_loop.rs` — docs on the four declared there

## Acceptance Criteria

- [ ] Each of the fourteen carries its verdict and the rule it was decided under
- [ ] Every Rule A keep states its post-collapse argument count
- [ ] `route`, `probed` and `turn_id` duplication has its reason in each doc
- [ ] No behaviour change; docs only
