---
id: TASK-182
title: "harness/budget.rs: RouteBudget, BudgetInputs, derive(), constants, digest thresholds, window label — pure, table-tested"
status: complete
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-181", "TASK-184"]
repo: teton-code
---

## Description

The one classifier (BR-8, AC-12): a pure module with no router, clock, or I/O
that turns `(window, cap, reservation, is_local, redact_scan)` into
`RouteBudget { budget_tokens, budget_bytes, bound, window_label, digest_threshold_tokens, digest_threshold_bytes }`.
Holds the constants the architecture table names. Mirrors the `effort`
precedent: `teton_core::effort::resolve_effort` is pure and table-tested, the
router only calls it.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` (new) — `pub struct BudgetInputs { window: u32, cap: u32, reservation: u32, is_local: bool, redact_scan: bool }`; `pub struct RouteBudget { pub budget_tokens: usize, pub budget_bytes: usize, pub bound: teton_protocol::events::BudgetBound, pub window_label: WindowLabel, pub digest_threshold_tokens: usize, pub digest_threshold_bytes: usize }`; `pub fn derive(inputs: BudgetInputs) -> RouteBudget` implementing architecture.md "Derivation" exactly — the cap is a **window ceiling** (`window_eff = min(window, cap)`, `bound = UserCap` iff `cap < window`), the redact clamp applies **last** and names the bound when it bites; precedence `LocalEngine > DefaultUnknown > RedactScan(when it bites) > UserCap > Window`; constants with **one home**: `LOCAL_BUDGET_TOKENS = 4_096`, `LOCAL_BUDGET_BYTES = LOCAL_BUDGET_TOKENS * APPROX_BYTES_PER_TOKEN`, `LOCAL_DIGEST_THRESHOLD_TOKENS = 1_500`, `LOCAL_DIGEST_THRESHOLD_BYTES = LOCAL_DIGEST_THRESHOLD_TOKENS * APPROX_BYTES_PER_TOKEN` (the fraction is written as `LOCAL_DIGEST_THRESHOLD_* / LOCAL_BUDGET_*`), `REMOTE_TOKENS_PER_WORD_NUM/DEN = 3/2`, `DIGEST_ABSOLUTE_CEILING_TOKENS = 20_000`, `DIGEST_ABSOLUTE_CEILING_BYTES = 160 * 1024` (rationale per architecture.md constants table); the byte floor reuses `crate::harness::duty::DUTY_REQUEST_BYTES_PER_TOKEN` (duty.rs:438) — do not introduce a third bytes-per-token number; the scannable bound reads `crate::egress::redact::REDACT_SCANNABLE_CONTEXT_BYTES` (TASK-184). `WindowLabel` → `String` used by the elision marker: `"the local context window"`, `"<id>'s context window"` (the router builds it with the id), `"the redact-scannable window"`; **`HarnessConfig::default()` reads `LOCAL_*` from this module** (the literals move here — no recursion into `derive`, no second home; the one-home grep in TASK-192 keys on this). Doc-comment each constant with its rationale and the AC that pins it.
- `crates/tetond/src/harness/mod.rs` — `pub mod budget;` + re-exports.
- `crates/tetond/src/harness/turn_loop.rs` — `HarnessConfig` gains `summarize_threshold_bytes: usize` (default `1_500 * APPROX_BYTES_PER_TOKEN` = 12,000 — byte-identical today), `budget: RouteBudget` (default = `derive(local)`); `Default`/`for_strong_model`/`from_harness_profile` populate them; `pub fn with_route_budget(self, RouteBudget) -> Self` sets the pair + thresholds + budget (the router's entry point, TASK-186). Nothing else in this file changes (the consumers come in TASK-187/189).

## Acceptance Criteria

- [x] Table tests in `budget.rs`: local → default pair/`LocalEngine`; window 0 → default/`DefaultUnknown`; 128,000 − 1,024 → `(84_650, 253_952)`/`Window`; cap 40,000 on 200k → `UserCap` with the pair from `window_eff = 40,000`; `redact_scan` on 128k → bytes = scannable, words window-derived, `RedactScan`; cap 60k + redact on 200k → `RedactScan` (the clamp is last); cap whose bytes stay under the scannable bound + redact → `UserCap`; cap above window → inert (`Window`); reservation ≥ window → default/`DefaultUnknown`; precedence pinned pairwise.
      *(`budget.rs::derivation_table` (every row above, expectations by
      hand) and `budget.rs::precedence_is_pinned_pairwise`.)*
- [x] Digest thresholds: default route (1,500 / 12,000) byte-identical to today; on a 128k route the fraction (≈30,990 words / ≈93 KB) is capped by the ceiling where the ceiling is smaller — assert `min(fraction, ceiling)` on both currencies and that the ceiling binds on 200k.
      *(`budget.rs::digest_thresholds_on_the_default_route_are_todays` and
      `budget.rs::digest_thresholds_scale_with_the_pair_under_the_ceiling` —
      `min(fraction, ceiling)` on both currencies, the words ceiling binding
      on 200k and the bytes ceiling not.)*
- [x] `HarnessConfig::default().budget` equals `derive(local)` and its pair equals `(LOCAL_BUDGET_TOKENS, LOCAL_BUDGET_BYTES)` — one source.
      *(`budget.rs::harness_config_default_reads_this_module`, plus
      `budget.rs::with_route_budget_stamps_pair_thresholds_and_fact` for the
      router's entry point.)*
- [x] `cargo test -p tetond harness::budget` green; no clippy warnings; integer arithmetic only.
      *(green in TASK-192's workspace gate (3,159 passed / 0 failed); `cargo
      clippy --workspace --all-targets -- -D warnings` clean; the derivation
      is integer-only (no float appears in `budget.rs`).)*

## Technical Notes

- "Policy is pure, mechanism is gated" (architecture.md Key Patterns); LESSON-446 (write the currency and worst-case factor at the constant); LESSON-456 (one home per number).
- Commit as `feat(harness): budget.rs — the one route-budget derivation [TASK-182]`.
