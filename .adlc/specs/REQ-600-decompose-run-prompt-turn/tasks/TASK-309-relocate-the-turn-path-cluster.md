---
id: TASK-309
title: "Relocate the turn-path cluster into runtime/turn.rs — a move, not a restructure"
status: complete
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-308]
---

## Description

AC-2. `impl DaemonRuntime` is 6,543 production lines and must reach 4,500 or
below. ADR-5's arithmetic says `run_prompt_turn` alone is not enough: moving it
leaves 5,458. The full turn-path cluster — 2,815 lines counted as method spans
including their doc blocks — leaves **3,728**.

This task moves them and changes **no control flow**. Keeping the relocation
separate from the decomposition is what lets a bisect tell "moved" from
"restructured", and is the whole reason REQ-599 deferred this work rather than
bundling it.

## Files to Create/Modify

- `crates/tetond/src/runtime/turn.rs` — new module, the cluster's new home
- `crates/tetond/src/runtime/mod.rs` — the cluster leaves; `mod turn;` added at
  the **foot** of the file if it needs a `#[cfg(test)]` sibling (LESSON-594: a
  `#[cfg(test)] mod` declared at the top truncated the production half for eight
  scanners)

## Acceptance Criteria

- [ ] The sixteen methods named in ADR-5 move to `runtime/turn.rs`, bodies
      unchanged. An inherent `impl DaemonRuntime` block may be split across
      modules of the same crate — no trait, no newtype, no call-site change.
- [ ] `impl DaemonRuntime`'s production line count is re-measured and reported
      **with its rule**; the target is ≤ 4,500.
- [ ] Visibility: anything the move makes cross-module is `pub(super)`, not
      `pub(crate)`, unless a caller outside `runtime/` genuinely exists. Establish
      that by demoting and building, never by grepping for the name — three
      searches gave three wrong answers in REQ-602 (LESSON-596).
- [ ] `crates/tetond/tests/runtime_visibility.rs` stays green without loosening.
      Its corpus is enumerated from disk, so `turn.rs` is scanned automatically.
- [ ] Tests move with their subject, or `turn.rs`'s module header names the ones
      that stayed and says why (REQ-599 BR-7, as enforced from REQ-602 onward).
- [ ] Suite green, grepped for `FAILED`; clippy clean under `deny`; fmt clean.

## Technical Notes

`runtime_module_map.rs` **will fail** until `turn.rs` is added to the module map
table in `.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md`. Verified
empirically by planting a probe module: that guard fires, and
`runtime_visibility`, `runtime_doc_paths` and `traceability_sweep` do not. The
map row format the parser accepts is ``| `name.rs` | <production> | <holds> |``.

Adjacency is not membership (REQ-599): check each item inside the moved ranges
actually belongs to the turn path rather than merely sitting between two things
that do.
