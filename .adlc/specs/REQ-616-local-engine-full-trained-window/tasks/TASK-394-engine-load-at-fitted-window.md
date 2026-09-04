---
id: TASK-394
title: "Load the engine at the fitted window and KV type; emit the two window events"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-390, TASK-391]
---

## Description

Wire the loader to `fit_window`: read `n_ctx_train` from the model, choose the
window and KV type, load with them, and publish the decision (or the refusal)
as a lifecycle event carrying the arithmetic (BR-1, BR-3, BR-4).

## Files to Create/Modify

- `crates/teton-inference/src/engine.rs` — accept a KV type alongside `n_ctx` in
  `LlamaEngine::load` and `new_context`; read `model.n_ctx_train()`
- `crates/teton-inference/src/lifecycle.rs` — `LocalWindowDecided` and
  `LocalWindowRefused` variants
- `crates/tetond/src/runtime/engine.rs` — call `fit_window`, pass the result to
  the loader, publish the events, hold the resulting `LocalEngineWindow`

## Acceptance Criteria

- [ ] `LlamaEngine::load` takes the fitted `n_ctx` and KV type and applies them
      via `with_type_k` / `with_type_v` (pinned `llama-cpp-2 =0.1.151` already
      exposes both plus `KvCacheType::Q8_0` — no crate bump)
- [ ] `local_window_decided` carries `n_ctx`, `n_ctx_train`, `kv_cache_type`,
      `resident_bytes_estimate`, `admissible_bytes`, `reason`
- [ ] `local_window_refused` carries the same plus `shortfall_bytes` and names
      the three remedies (`[inference] n_ctx`, `allow_over_memory`, a smaller
      model)
- [ ] A refusal does **not** load, and an unattended session fails the local tier
      closed with that reason (BR-4)
- [ ] The metadata fallback for `kv_bytes_per_token` announces itself in the
      event rather than degrading silently (LESSON-456)
- [ ] The daemon log shows `n_ctx = 262144` and no "full capacity will not be
      utilized" line on an admitting machine (AC-1, engine half)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/runtime/engine.rs::loader_requests_the_fitted_window` | no |
| BR-3 | test-case | `crates/tetond/src/runtime/engine.rs::decided_event_names_both_kv_figures` | no |
| BR-4 | test-case | `crates/tetond/src/runtime/engine.rs::refusal_does_not_load_and_names_remedies` | yes |

## Technical Notes

- `LlamaEngine::load`'s body is behind `--features llama` and never compiles in
  CI. Keep every decision *outside* the feature gate so it is testable: the gate
  should wrap only the FFI call, with `fit_window`'s result computed and asserted
  in an ungated function.
- Confirm `LifecycleEvent` carries REQ-588 BR-4's `Unknown` catch-all before
  adding variants. If it does not, surface that as a finding rather than working
  around it (ADR-616-6).
- The `feature-gated targets compile (all features)` CI job is what proves the
  gated arm still builds; it is a required check (REQ-608).
