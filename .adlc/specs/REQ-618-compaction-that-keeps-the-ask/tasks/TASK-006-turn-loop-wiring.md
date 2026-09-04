---
id: TASK-006
title: "The turn loop publishes every compaction and refuses a turn its anchors cannot fit"
status: draft
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-004, TASK-005]
---

## Description

Wire the record to the bus at both `compact_if_pressured` call sites, emit the
mechanical-fallback record, anchor a model-invoked expansion, and raise BR-1's
refusal at the last point where "nothing was sent" is still true.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — publish `context_compacted` after the gate; `fallback: true` record on the degraded path; `SkillBody` anchor at the `ResultDisposition::Expansion` admit; BR-1 refusal before `ctx.prepare`
- `crates/tetond/src/runtime/duty.rs` — the second `compact_if_pressured` call site
- `crates/tetond/src/harness/context.rs` — `anchors_intact` onto the published `context_pressure`

## Acceptance Criteria

- [ ] One `context_compacted` per compaction, on both the duty path and the
      mechanical fallback; the fallback's carries `fallback: true` and its
      anchors are still intact (AC-5).
- [ ] A turn whose anchors alone exceed either budget publishes
      `turn_refused_anchors_exceed_budget` naming both figures and ends **before**
      `ctx.prepare` — no provider is reached, asserted by an egress-capture test
      showing zero requests (AC-2).
- [ ] The refusal is a typed outcome with both halves: `failure_class() -> None`
      and its own arm on the turn path, so the retry/fallback machinery does not
      act on it and the user is not told "provider failed unrecoverably".
- [ ] A model-invoked `skill` expansion is pushed with `Anchor::SkillBody`.
- [ ] Benign path: an ordinary pressured turn whose anchors fit is unaffected —
      it compacts, publishes, and reaches the model exactly as today.
- [ ] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `tests/context_pressure.rs::a_turn_whose_anchors_exceed_the_budget_is_refused` | yes |
| BR-2 | test-case | `tests/skill_tool_loop.rs::a_model_expansion_is_anchored_for_its_turn` | yes |
| BR-5 | test-case | `tests/transcript.rs::every_compaction_reaches_the_transcript` | no |
| AC-2 | test-case | `tests/egress_capture.rs::an_anchor_refusal_sends_nothing` | yes |
| AC-5 | test-case | `tests/context_pressure.rs::a_mechanical_fallback_still_records_and_keeps_anchors` | yes |

## Technical Notes

ADR-618-4, ADR-618-5. Respect REQ-589 BR-12 / D-3: the compact-then-truncate pair
is skipped on the first iteration of an accepted over-budget turn. BR-1's refusal
must sit **after** that exception, not inside it — a user who accepted an
oversized expansion has not accepted a turn that cannot hold their own prompt.
