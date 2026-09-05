---
id: TASK-400
title: "`Provenance::User` carries `boundary_touch` through the three seams, and one fold turns identity plus verdicts into a provenance"
status: complete
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: [TASK-399]
---

## Description

ADR-619-2 and ADR-619-4. `harness::context::Provenance::User` gains
`boundary_touch: bool`, carried by the seed (`push_user_from`), the union
(`context_provenance`) and replay. A new `skills::provenance::fold_expansion`
maps `(identity, &[PreambleRun])` to `ExpansionProvenance { sources, unknown,
boundary_touch }` per ADR-619-4's table, and `runtime::expansion_provenance`
renders the three fields into egress `Provenance`.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `Provenance::User { sources, unknown, boundary_touch }`; `push_user_from` takes the third field; replay arm carries it; `ProvenanceClass::of` treats `boundary_touch` as `Unknown`-class (matching ADR-614-3's egress mapping); tests at the seed and replay seams
- `crates/tetond/src/harness/completion.rs` — `context_provenance` folds `boundary_touch` through `tool_result_provenance(&ToolProvenance::BoundaryTouch)`; test
- `crates/tetond/src/skills/provenance.rs` — new: `ExpansionProvenance`, `fold_expansion`; table-driven tests
- `crates/tetond/src/skills/mod.rs` — `pub mod provenance;`
- `crates/tetond/src/runtime/mod.rs` — `expansion_provenance(sources, unknown, boundary_touch)`; `SkillTurn` gains `boundary_touch`
- `crates/tetond/src/carry.rs`, `crates/tetond/src/sessions.rs` — every `Provenance::User { .. }` construction/destructure names the new field (compile-driven sweep)
- `crates/tetond/src/harness/compact.rs` — the "compaction inherits provenance" test gains the `boundary_touch` case

## Acceptance Criteria

- [ ] A `User` block seeded with `boundary_touch: true` is refused at egress against `<boundary-touch>` (not `<unknown-provenance>`), after a compaction, and after a replay
- [ ] `fold_expansion` implements ADR-619-4's table row for row; a `NotRun` command with any verdict contributes nothing; an in-root `BoundaryTouch` contributes its sources and not the bit
- [ ] `cargo test -p tetond` green including `compaction_cadence`, `conversation_carry`, `egress_capture`

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/src/skills/provenance.rs::tests::the_fold_follows_the_adr_table` | yes |
| BR-2 | test-case | `crates/tetond/src/skills/provenance.rs::tests::an_unrun_command_contributes_nothing_whatever_its_verdict` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/context.rs::tests::a_boundary_touch_on_a_user_block_survives_seed_union_and_replay` | no |
| BR-5 | test-case | `crates/tetond/src/harness/completion.rs::tests::context_provenance_carries_a_user_blocks_boundary_touch` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/compact.rs::tests::a_compaction_of_a_boundary_touched_expansion_stays_boundary_touched` | no |

## Technical Notes

- Three seams, three tests — do not let one test stand in for the others (LESSON-501, LESSON-502).
- The exit-code side channel (REQ-585 verify, AC-6) is closed by the fold: a `BoundaryTouch` verdict on a command that ran and exited 2 sets sources / the bit exactly as one that exited 0. Write that as a named case.
- `fold_expansion` is pure — no I/O — so the model-invoked and typed paths can share it and its tests need no daemon.
