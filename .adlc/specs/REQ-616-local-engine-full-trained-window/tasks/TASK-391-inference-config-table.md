---
id: TASK-391
title: "The [inference] config table and its config/set refusal"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-390]
---

## Description

Add the `[inference]` table so a user can state a window, a KV type, or a
willingness to overcommit — and refuse `n_ctx` above the model's trained window
at the point it is set, naming the trained figure (BR-2, AC-6).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `InferenceConfig { n_ctx, kv_cache_type,
  allow_over_memory }`, `is_unset()`, wired into `Config` as `[inference]`
- `crates/tetond/src/runtime/config_document.rs` — `config/set` handling and the
  refusal message for `n_ctx > n_ctx_train`

## Acceptance Criteria

- [ ] `[inference]` deserializes with all three keys optional;
      `allow_over_memory` defaults to `false` and serializes unconditionally, on
      `LocalModelConfig::auto_accept`'s precedent
- [ ] `config/set` of `inference.n_ctx = 300000` is refused naming
      `n_ctx_train = 262144` (AC-6)
- [ ] A value at or below the trained window is accepted (the benign path)
- [ ] The refusal is a *usability* error at the point of use, not a structural
      one that fails `Config::validate()` and gates daemon startup — per
      `conventions.md`'s "Config validity vs usability" rule
- [ ] Round-trips through the config document without dropping unrelated tables

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/src/runtime/config_document.rs::set_n_ctx_above_trained_is_refused` | yes |
| AC-6 | test-case | `crates/tetond/src/runtime/config_document.rs::set_n_ctx_300000_names_trained_window` | yes |

## Technical Notes

- `n_ctx_train` is only known once a model is known. Where no model is resolved,
  the refusal cannot cite a figure — say that rather than inventing one
  (LESSON-456). Prefer refusing at `config/set` when the selection is known and
  at load otherwise.
- Reuse the existing `[privacy]` / `[local_model]` table patterns for
  serde defaults and `is_unset()`.
