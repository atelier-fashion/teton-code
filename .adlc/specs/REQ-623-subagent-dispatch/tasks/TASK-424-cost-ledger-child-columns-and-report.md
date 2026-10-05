---
id: TASK-424
title: "Cost ledger: child_id/parent_turn_id columns, per-child headroom query, nested /cost report"
status: draft
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-421"]
repo: teton-code
---

## Description

BR-8's attribution half. Two nullable columns on `cost_records` through the existing
`ALTER TABLE … ADD COLUMN` migration list; `CostLedger::record` takes the pair from the
`CostRecord` payload (TASK-421); a `spent_by_child(child_id)` query; `report.rs` groups by
`parent_turn_id` and nests child rows so a parent turn's total is its own calls plus its
children's. The `/cost` RPC view gains the nested rows; the CLI render is TASK-429.

## Files to Create/Modify

- `crates/tetond/src/cost/ledger.rs` — migration entries, record/query, append-only test extended
- `crates/tetond/src/cost/report.rs` — nested per-child aggregation, totals test
- `crates/tetond/src/cost/mod.rs` — re-exports
- `crates/tetond/tests/cost_attribution.rs` — a child-attributed record appears under its parent turn with both ids; a pre-migration database opens and reads `NULL` as `None`

## Acceptance Criteria

- [ ] Opening a database created before this task migrates without data loss; old rows read `child_id = None`
- [ ] A parent turn with two children reports `total = own + child_a + child_b` and one row per child
- [ ] `spent_by_child` returns exactly that child's micro-cents
- [ ] `cargo test -p tetond cost` green, including the existing append-only invariant

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/tests/cost_attribution.rs::child_records_nest_under_parent_turn` | no |
| AC-12 | test-case | `crates/tetond/tests/cost_attribution.rs::parent_total_is_own_plus_children` | no |

## Technical Notes

- Migration pattern: `ledger.rs:150-175`; the doc comment at `:134-142` explains why a column needs an entry and a table does not.
- The accumulator the ceiling is checked against lives in egress (`egress/mod.rs:534-537`), not the ledger — this task is persistence and reporting only; the live share logic is TASK-427.
