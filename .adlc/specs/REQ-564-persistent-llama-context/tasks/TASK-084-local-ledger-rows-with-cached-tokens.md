---
id: TASK-084
title: "Ledger: local-tier rows and an additive cached_tokens column"
status: draft
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-081, TASK-083]
---

## Description

BR-9 asks local-call ledger records to gain a cached-tokens count. There are no
local-call records today — the ledger is fed only through the remote egress
choke point (architecture D-6). This task creates them and adds the column.

## Files to Create/Modify

- `crates/tetond/src/cost/ledger.rs` — `cached_tokens` in `ADDITIVE_COLUMNS`,
  on `LedgerRow`, on insert and on `all_records`
- `crates/teton-protocol/src/events.rs` — `cached_tokens` on `CostRecord`
- `crates/tetond/src/cost/report.rs` — carry the column through the report shape
- `crates/tetond/src/harness/completion.rs` — record one row per local turn

## Acceptance Criteria

- [ ] `cached_tokens` is added as a nullable `ALTER TABLE … ADD COLUMN`, per the
      `ADDITIVE_COLUMNS` contract; a `cost.db` written before this REQ opens and
      reads back `None` for pre-existing rows
- [ ] Exactly one ledger row per completed local agent turn, with
      `provider_id = "local"` and the engine's model id
- [ ] Local rows are **unpriced** (`usd_micros: None`) — not `Some(0)` — and the
      report's priced/unpriced split reflects that
- [ ] `cached_tokens` is `Some(n)` on local rows, `None` on every remote row
- [ ] `input_tokens` remains the full prompt token count; cached is a
      *component* of it, not a substitute
- [ ] Append-only invariant still holds (the existing no-update trigger test passes)
- [ ] `cargo test --workspace` passes

## Technical Notes

`record_call` already takes an arbitrary `provider_id`; the ledger's own
`ledger_is_append_only` test passes `"local"`, so the shape is proven. Add a
sibling of `record_call` carrying `cached_tokens` rather than widening
`record_call`'s signature for every remote caller.

Do not backfill historical rows — the store is append-only and a migration that
rewrote them would both trip the no-update trigger and invent an attribution
nobody recorded. That reasoning is already written at `ADDITIVE_COLUMNS`; follow
it rather than restating it.

A local row is a **usage** record, not a spend record. Make sure the cost meter
does not start reporting local turns as $0.00 of spend — unpriced is the
distinction that keeps the meter honest.
