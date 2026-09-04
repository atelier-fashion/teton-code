---
id: TASK-395
title: "Record kv_cache_type in model-selection.toml and state both windows in the catalog"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-394]
---

## Description

Persist the KV type the probe chose, and make `teton model list` / `teton model
status` state each model's trained window and the window it will actually be
served at (BR-10, AC-11).

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — `ModelSelection::kv_cache_type`, additive
  and optional
- `crates/tetond/src/selection_store.rs` — persist and read it back
- `crates/teton/src/model_ui.rs` — the `trained 262,144 · served 262,144 (KV
  q8_0)` line
- `crates/teton/src/cli_rows.rs` — row rendering for the catalog listing

## Acceptance Criteria

- [ ] `model-selection.toml` records `kv_cache_type` after a load, and a file
      written before this REQ reads back with `None` rather than failing
- [ ] `teton model list` prints trained and served windows per entry, with the KV
      type, or the fitted figure with its reason
- [ ] `teton model status` prints the same line for the installed model
- [ ] A model whose window was refused says so rather than printing a served
      figure it does not have

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-10 | test-case | `crates/teton/src/model_ui.rs::list_states_trained_and_served_windows` | no |
| AC-11 | test-case | `crates/tetond/src/selection_store.rs::kv_cache_type_round_trips_and_old_files_read_none` | yes |

## Technical Notes

- `ModelSelection` is persisted state written by earlier releases; the new field
  must be `#[serde(default, skip_serializing_if = "Option::is_none")]` so an
  existing `model-selection.toml` still parses (the same additive posture the
  protocol uses).
