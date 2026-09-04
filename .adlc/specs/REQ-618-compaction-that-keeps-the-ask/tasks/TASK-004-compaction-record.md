---
id: TASK-004
title: "CompactionRecord: what a compaction kept, dropped and summarized, and what it derived from"
status: complete
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-002, TASK-003]
---

## Description

Every compaction becomes a record the call site can publish. The manager builds
it — it is the only thing that knows the byte totals and the dropped blocks'
provenance — and does not publish it, the split `PressureReport` already takes.
The summary's opening line is reworded here too.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `ProvenanceClass`, `CompactionRecord`, `CompactionOutcome.record`; byte totals in `attempt_compaction`; BR-6 line in `compaction_summary`; totals on `PressureReport` for the mechanical path

## Acceptance Criteria

- [x] `ProvenanceClass` is `Unknown` / `Rooted` / `None`, derived from `Provenance`
      alone; the doc names why `boundary` is absent (ADR-618-3).
- [x] `CompactionRecord` carries `kept_bytes`, `dropped_bytes`, `summarized_bytes`,
      `anchor_bytes`, `dropped_blocks: Vec<(BlockRole, ProvenanceClass, usize)>`
      with **no content**, and `fallback: bool`.
- [x] `kept + dropped + summarized == pre-compaction Σ text.len()` and
      `anchor_bytes <= kept_bytes`, both asserted (AC-4) against terms defined as
      in ADR-618-5 — the summary block's own bytes are in none of the three.
- [x] The summary block opens with
      `[summary of <n> earlier blocks, <bytes> bytes; the user's prompts are kept verbatim below]`,
      outside the untrusted envelope (BR-6).
- [x] A compaction over an `unknown`-provenance block yields an `unknown` summary
      and the record's class says so (BR-7); a summary derived from it is refused
      at remote egress with `privacy_block.path == "<unknown-provenance>"` (AC-9).
- [x] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `harness::context::tests::a_compaction_reports_its_byte_totals` | no |
| BR-6 | test-case | `harness::context::tests::the_summary_says_what_it_replaced` | no |
| BR-7 | test-case | `tests/provenance_egress.rs::an_unknown_summary_stays_unknown` | yes |
| AC-4 | test-case | `harness::context::tests::the_record_bytes_account_for_the_whole_context` | no |
| AC-9 | test-case | `tests/provenance_egress.rs::a_summary_of_unknown_provenance_is_refused_remote` | yes |

## Technical Notes

ADR-618-3, ADR-618-5. The record carries counts and kinds, never content — the
transcript's `max_record_bytes` default (65,536) is the spec's stated assumption
and this is what keeps it true. The BR-6 line replaces
`[earlier conversation compacted — n blocks elided]` in the same slot, outside
the envelope; do not move it inside.
