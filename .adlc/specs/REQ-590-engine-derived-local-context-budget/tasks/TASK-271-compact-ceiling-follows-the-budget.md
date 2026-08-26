---
id: TASK-271
title: "COMPACT_OUTPUT_MAX_BYTES follows the budget it repairs to"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-270]
---

## Description

ADR-5, and BR-9 — a required fix, not an observation. `compact.rs:134` defines
`COMPACT_OUTPUT_MAX_BYTES = LOCAL_BUDGET_BYTES` (32,768). That constant does **not** move
(ADR-4), so with the local budget now 30,720 the compact duty may return more than the budget it
is repairing to — breaking the invariant stated in its own doc.

The failure is silent: a compaction candidate landing in the 2,048-byte gap is rejected at
`context.rs:1492`, `CompactionOutcome::degraded` is returned, and the turn falls back to
deterministic oldest-first eviction — on the route that most needed the model's judgement.

## Files to Create/Modify

- `crates/tetond/src/harness/compact.rs` — `COMPACT_OUTPUT_MAX_BYTES` (line 134) derives from the
  local route's byte budget rather than from `LOCAL_BUDGET_BYTES`; its doc states the chain
- `crates/tetond/src/harness/context.rs` — only if the rejection site needs to read the new source

## Acceptance Criteria

- [x] AC-8: `COMPACT_OUTPUT_MAX_BYTES ≤` the local byte budget, asserted **as a relation between
      the two**, not as two literals that happen to agree. LESSON-491: derive each number from
      its neighbour
- [x] A test drives a compaction whose candidate lands in what *was* the gap (30,721–32,768) and
      shows it is accepted, not degraded. Mutation: pin the ceiling back at `LOCAL_BUDGET_BYTES`;
      this must redden
- [x] `COMPACT_DUTY`'s `max_tokens` still derives from the ceiling, and the relation survives
- [x] Existing compact tests green

## Technical Notes

`COMPACT_PROMPT_BUDGET_BYTES` (`compact.rs:169`) derives from `LOCAL_ENGINE_N_CTX`, which this
REQ does not touch. Leave it alone — an exploration pass claimed it shrinks; it does not.

The chain to write down once (LESSON-491): `engine window → route budget → compaction output
ceiling`. The bug was the third link pinned to the first link's old value.
