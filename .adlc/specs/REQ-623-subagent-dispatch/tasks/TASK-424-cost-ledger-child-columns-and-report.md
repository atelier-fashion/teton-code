---
id: TASK-424
title: "Cost ledger: child_id/parent_turn_id columns, per-child headroom query, nested /cost report"
status: complete
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

- [x] Opening a database created before this task migrates without data loss; old rows read `child_id = None`
- [x] A parent turn with two children reports `total = own + child_a + child_b` and one row per child
- [x] `spent_by_child` returns exactly that child's micro-cents
- [x] `cargo test -p tetond cost` green, including the existing append-only invariant

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/tests/cost_attribution.rs::child_records_nest_under_parent_turn` | no |
| AC-12 | test-case | `crates/tetond/tests/cost_attribution.rs::parent_total_is_own_plus_children` | no |

## Technical Notes

- Migration pattern: `ledger.rs:150-175`; the doc comment at `:134-142` explains why a column needs an entry and a table does not.
- The accumulator the ceiling is checked against lives in egress (`egress/mod.rs:534-537`), not the ledger — this task is persistence and reporting only; the live share logic is TASK-427.

## Implementation notes

- **Attribution in.** `CostAttribution` gained `parent_turn_id`/`child_id` with two builders: `for_child(child_id, parent_turn_id)` (a child's egress) and `with_turn(turn_id)` (a parent turn's *own* call). `record_call`, `record_local_call` and `MeteredBody::finalize` carry both onto the `LedgerRow`; `to_wire` projects them onto `CostRecord`.
- **"Own" needs the parent's calls stamped.** `parent_turn_id` is the turn a row nests under, so a parent turn's own call is `parent_turn_id = T, child_id = NULL`. Nothing stamps the parent's own calls yet: until TASK-427/428 attaches `.with_turn(turn_id)` where the parent turn's attribution is built (`Router::egress_context`, `harness/completion.rs`), a turn's `own` reads zero and its total is its children's. The `CostRecord.parent_turn_id` doc in `teton-protocol/src/events.rs` ("`None` exactly when [`child_id`] is") must be relaxed in the same change.
- **Query.** `CostLedger::spent_by_child(&SessionId, &ChildId) -> Result<u64, LedgerError>`. Session-scoped because `ChildId` is unique only within a session (provider-minted call ids repeat across scripted sessions). Sums through `spend_units`, the same conversion `MeteredBody` feeds `PromptSpend` with, so the ledger figure and the live accumulator agree on units by construction.
- **Report.** `CostReport::per_turn: Vec<TurnTotals>` (`session_id`, `turn_id`, `own: GroupTotals`, `children: Vec<ChildTotals>`, `total: GroupTotals`); `ChildTotals` = `child_id`, `name` (id after its first `/`), `route` (distinct `provider/model` in call order, joined `" → "`), `calls`, tokens, `usd_micros`, `unpriced_calls`. Keyed `(session, turn)`, ledger order, only turns with at least one child. Every existing roll-up is unchanged (pinned by `ids_add_a_nested_view_and_move_no_existing_roll_up`).
- **Wire.** Additive: `CostReportView::per_turn: Vec<CostTurnView>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`), `CostTurnView`, `CostChildView` in `teton-protocol/src/methods.rs`; projected in `runtime/mod.rs::cost_report_view`; `per_turn: Vec::new()` added to four `CostReportView` literals in `teton/src/cost_ui.rs` tests.
- **Mutations recorded** in the doc comments of `child_records_nest_under_parent_turn`, `parent_total_is_own_plus_children`, `a_turn_nests_each_child_and_totals_own_plus_children`, `a_pre_child_ledger_gains_both_columns_and_reads_null_as_none`, `spent_by_child_is_exactly_that_childs_spend`.
