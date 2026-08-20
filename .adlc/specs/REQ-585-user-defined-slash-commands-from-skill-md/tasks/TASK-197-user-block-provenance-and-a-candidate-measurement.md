---
id: TASK-197
title: "Prompt text can carry file provenance, at all three seams — and a candidate turn can be measured before it is committed"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

BR-7's new machinery. Today `Provenance::User` is a unit variant and two
independent places assert in comments that user text carries no file
provenance. That becomes false: a skill expansion must carry the skill file's
identity so a `local-only` boundary pins the turn exactly as a `read` would.

Also lands `would_seed_fit`, the one measurement TASK-202's refusal will use.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `Provenance::User { sources }`, `push_user_from`, `DroppedProvenance::absorb`'s user arm, `replay`'s user arm, `ContextManager::would_seed_fit`
- `crates/tetond/src/harness/completion.rs` — `context_provenance` merges user sources
- `crates/tetond/src/carry.rs` — `CarriedTurn::begin` takes the prompt's sources beside its text
- `crates/tetond/src/runtime.rs` — the three `begin` call sites (`:2935`, `:25506`, `:26834`)
- `crates/tetond/tests/{context_pressure,conversation_carry,provenance_egress}.rs` — the `begin` call sites at `:738`, `:621`, `:648`

## Acceptance Criteria

- [ ] `Provenance::User { sources: BTreeSet<ProvenanceId> }`. `push_user(text)` keeps its exact signature and seeds an empty set, so every existing caller is byte-identical; `push_user_from(text, sources)` is the new entry point.
- [ ] **Seam 1** — `DroppedProvenance::absorb` absorbs user sources. Its current early-return comment ("user and model text carries no file provenance of its own") is re-written, not deleted.
- [ ] **Seam 2** — `completion::context_provenance` merges user sources. The test named `context_provenance_unions_tool_result_paths_only` **is** the claim this breaks: rename and re-assert it; do not delete it.
- [ ] **Seam 3** — `ContextManager::replay`'s `Provenance::User => push_user(text)` arm preserves sources. Extended round-trip pin on `per_block_provenance_survives_the_commit_and_replay_round_trip` (`context.rs:3914`) with a user block carrying a source.
- [ ] A user block whose sources cannot be minted is marked **unpinnable** and makes the turn fail closed whenever any boundary is configured — the same posture `ToolProvenance::Unknown` gets. `ProvenanceId::from_resolved` refuses a path outside the root by design (REQ-571 ADR-B) and must **not** be widened; `ProvenanceId::claimed` must **not** be used (its doc says a first-party path reaching it is a bug). See ADR-9.
- [ ] `ContextManager::would_seed_fit(system, text, budget_tokens, budget_bytes) -> Fit { tokens, bytes, fits }` measures a candidate seed through the **existing** `tokens_of`/`bytes_of`, the same estimators the pressure path uses. No second estimator is introduced (LESSON-546, LESSON-491).
- [ ] Mutation table: deleting any one of the three seam arms fails a *different* named test (LESSON-502). A single test covering all three is not sufficient evidence.

## Technical Notes

- Three seams, three tests, stated because REQ-567 shipped exactly this shape with only the first seam covered and the verify panel found the other two (LESSON-501).
- `would_seed_fit` measures **system + the one candidate block**, not the replayed conversation: BR-8(c)'s parenthetical says an expansion that fits while the assembled conversation does not is ordinary pressure, and `blocks_dropped` on older turns stays permitted.
- The `begin` signature change is mechanical but wide (6 production + 3 test call sites). Change the signature rather than adding an overload — a second entry point is how the sources get dropped on the path nobody remembered to update.
