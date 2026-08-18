---
id: TASK-161
title: "Ledger: probe column via ADDITIVE_COLUMNS, CostAttribution.probe, probe_calls in the report"
status: complete
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: [TASK-160]
---

## Description

Let one cost row say "this was a connection test": a nullable `probe` column
added through the existing additive migration, a `probe` flag on
`CostAttribution` that the egress cost meter copies onto the `CostRecord`, and
a `probe_calls` count on the report `teton cost` renders.

## Files to Create/Modify

- `crates/tetond/src/cost/ledger.rs` — `SCHEMA` gains `probe INTEGER` (nullable); `ADDITIVE_COLUMNS` gains `("probe", "ALTER TABLE cost_records ADD COLUMN probe INTEGER")`; the INSERT writes `1` when `record.probe`, else NULL; the row→`CostRecord` read maps `NULL/0 → false`, `1 → true`; `report()` computes `probe_calls` (`COUNT(*) WHERE probe = 1`); tests: migration adds the column to a pre-REQ database (open a ledger built from the old schema literal, reopen, assert `has_column`), a probe row round-trips, a pre-existing row reads `probe = false`, `report().probe_calls` counts only probes.
- `crates/tetond/src/cost/mod.rs` — `CostAttribution` gains `probe: bool` + `.probe()` builder (default false); wherever the meter builds a `CostRecord` from an attribution, copy the flag; `CostReport` gains `probe_calls: u64`.
- `crates/tetond/src/runtime.rs` — `cost_report_view()` maps `probe_calls` onto `CostReportView.probe_calls`.

## Acceptance Criteria

- [ ] `cargo test -p tetond --lib cost` green with the new tests above.
- [ ] A ledger file created by the previous schema opens, migrates, and keeps every old row byte-for-byte (`probe` reads false); the append-only trigger still fires on UPDATE.
- [ ] `CostReportView.probe_calls` is `0` for a ledger with no probes and `N` after `N` probe rows.

## Technical Notes

Follow the `cached_tokens` (REQ-564) precedent exactly — nullable, never backfilled, DDL only. `CostReport` is the daemon-internal aggregate `cost_report_view` projects; keep the wire type's field additive (TASK-160 already added it with `serde(default)`). Do not touch `Category`.
