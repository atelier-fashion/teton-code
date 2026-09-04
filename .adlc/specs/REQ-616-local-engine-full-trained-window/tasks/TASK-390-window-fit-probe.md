---
id: TASK-390
title: "The pure window-fit probe: n_ctx_train, KV type, admissible RAM"
status: draft
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

Add `fit_window` — the pure function that decides the local engine's window and
KV cache type from the model's trained window, its weights size, and the
machine's admissible RAM. No hardware detection inside it, on the
`probe::decide` precedent (ADR-616-3), so it is table-testable against simulated
inputs and never depends on the host.

This task lands the arithmetic only. Wiring the loader to it is TASK-394.

## Files to Create/Modify

- `crates/teton-inference/src/window.rs` — new module: `KvCacheType`,
  `WindowFitInputs`, `WindowDecision`, `WindowReason`, `fit_window`,
  `kv_bytes_per_token`, `ADMISSIBLE_RAM_FRACTION`, `admissible_bytes`
- `crates/teton-inference/src/lib.rs` — declare and re-export the module

## Acceptance Criteria

- [ ] `ADMISSIBLE_RAM_FRACTION` is 75 %, with the `[62.5 %, 87.5 %)` derivation
      from AC-5 written at the constant (ADR-616-4)
- [ ] `fit_window` returns `Fits { n_ctx, kv, resident_bytes, reason }` or
      `Refused { shortfall_bytes, … }` carrying the full arithmetic
- [ ] At `n_ctx_train` it tries `f16` first, then `q8_0`, then steps the window
      down by multiples of 4,096 at `q8_0` — `reason` naming which applied
      (BR-3)
- [ ] A result below 65,536 with no explicit `config_n_ctx` is `Refused` (BR-4)
- [ ] `config_n_ctx` waives **only** the quarter-window rule;
      `allow_over_memory` waives **only** the memory check; neither implies the
      other (BR-4, ADR-616-7)
- [ ] `config_n_ctx > n_ctx_train` is rejected — no scaling above the trained
      window (BR-2)
- [ ] `kv_bytes_per_token` from `(n_layer, n_head_kv, head_dim, bytes_per_elem)`
      reproduces the measured 98,304 B/token at f16 for the shipped 30B, pinned
      in both directions (ADR-616-5)
- [ ] Table test covers AC-5's four cases at 48 / 96 / 16 / 32 GiB with the model
      held at the 30B (17.3 GiB weights)
- [ ] Every case asserts the *mutation*: flipping the admissible fraction or the
      KV ratio moves the decision, so the table cannot pass vacuously

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton-inference/src/window.rs::config_n_ctx_above_trained_is_rejected` | yes |
| BR-3 | test-case | `crates/teton-inference/src/window.rs::kv_type_steps_f16_then_q8_then_window` | no |
| BR-4 | test-case | `crates/teton-inference/src/window.rs::shortfall_refuses_and_waivers_are_independent` | yes |
| AC-5 | test-case | `crates/teton-inference/src/window.rs::fit_window_table_at_four_ram_figures` | yes |

## Technical Notes

- `KvCacheType` here is Teton's own enum (`F16` / `Q8_0`), mapped to
  `llama_cpp_2::context::params::KvCacheType` only in the feature-gated loader.
  The pure module must not depend on the `llama` feature — that is what lets
  every CI build test it.
- `q8_0` is half of `f16` (1 byte/elem against 2).
- Resident estimate is `weights + kv_at(n_ctx, kv) + compute_buffers`; state the
  compute-buffer allowance explicitly rather than folding it into a fudge factor.
- Do not read RAM here. `admissible_bytes(physical)` is a pure multiply; the
  caller supplies `physical`.
