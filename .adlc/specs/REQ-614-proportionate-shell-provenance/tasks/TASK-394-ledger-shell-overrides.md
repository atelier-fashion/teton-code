---
id: TASK-394
title: "The shell_overrides ledger table — one append-only row per lift"
status: complete
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

A `shell_overrides` table beside `web_overrides`, with the same append-only
triggers, and the storage-only writer/reader pair. The event stays the RPC
handler's to publish — the ledger records, it does not announce.

## Files to Create/Modify

- `crates/tetond/src/cost/ledger.rs` — `shell_overrides` DDL + triggers in `SCHEMA`, `ShellOverrideRow`, `record_shell_override`, `all_shell_overrides`

## Acceptance Criteria

- [ ] `shell_overrides` carries `id`, `recorded_at_ms`, `session_id`, and the cause that was lifted
- [ ] `no_update` and `no_delete` triggers raise `cost ledger is append-only`, matching `web_overrides`
- [ ] `CREATE TABLE IF NOT EXISTS` in `SCHEMA` means an existing ledger file gains the table on next open — no column migration entry is needed, and a test opens an old-shaped file and asserts the table appears
- [ ] `record_shell_override` is storage only: it publishes no event
- [ ] `all_shell_overrides` returns rows in insertion order

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `crates/tetond/src/cost/ledger.rs::a_shell_override_row_is_append_only` | yes |

## Technical Notes

- Mirror `record_web_override` exactly, including the doc comment's argument
  for why the event belongs to the handler rather than here.
- An update or delete attempt must be asserted to *fail* — a trigger nobody
  tries to violate is a trigger nobody knows works.
