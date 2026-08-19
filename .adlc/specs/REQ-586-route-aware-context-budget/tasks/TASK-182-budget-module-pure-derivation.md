---
id: TASK-182
title: "harness/budget.rs: RouteBudget, BudgetInputs, derive(), constants, digest thresholds, window label — pure, table-tested"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: []
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

- `crates/tetond/src/harness/budget.rs` (new) — `pub struct BudgetInputs { window: u32, cap: u32, reservation: u32, is_local: bool, redact_scan: bool }`; `pub struct RouteBudget { pub budget_tokens: usize, pub budget_bytes: usize, pub bound: teton_protocol::events::BudgetBound, pub window_label: WindowLabel, pub digest_threshold_tokens: usize, pub digest_threshold_bytes: usize }`; `pub fn derive(inputs: BudgetInputs) -> RouteBudget` implementing architecture.md "Derivation" exactly, precedence `LocalEngine > DefaultUnknown > UserCap > RedactScan > Window`; constants `REMOTE_TOKENS_PER_WORD_NUM/DEN = 3/2`, `DIGEST_THRESHOLD_FRACTION = (1_500, 4_096)` words and `(12_000, 32_768)` bytes, `DIGEST_ABSOLUTE_CEILING_TOKENS = 20_000`, `DIGEST_ABSOLUTE_CEILING_BYTES = 160 * 1024`; the byte floor reuses `crate::harness::duty::DUTY_REQUEST_BYTES_PER_TOKEN` (duty.rs:438) — do not introduce a third bytes-per-token number; the scannable bound reads `crate::egress::redact::REDACT_SCANNABLE_CONTEXT_BYTES` (TASK-184). `WindowLabel` → `String` used by the elision marker: `"the local context window"`, `"<id>'s context window"` (the router builds it with the id), `"the redact-scannable window"`; the default pair is read from `HarnessConfig::default()` — one source (assert equality in a test, never a second literal). Doc-comment each constant with its rationale and the AC that pins it.
- `crates/tetond/src/harness/mod.rs` — `pub mod budget;` + re-exports.
- `crates/tetond/src/harness/turn_loop.rs` — `HarnessConfig` gains `summarize_threshold_bytes: usize` (default `1_500 * APPROX_BYTES_PER_TOKEN` = 12,000 — byte-identical today), `budget: RouteBudget` (default = `derive(local)`); `Default`/`for_strong_model`/`from_harness_profile` populate them; `pub fn with_route_budget(self, RouteBudget) -> Self` sets the pair + thresholds + budget (the router's entry point, TASK-186). Nothing else in this file changes (the consumers come in TASK-187/189).

## Acceptance Criteria

- [ ] Table tests in `budget.rs`: local → default pair/`LocalEngine`; window 0 → default/`DefaultUnknown`; 128,000 − 1,024 → `(84_650, 253_952)`/`Window`; cap 40,000 on 200k → `UserCap` with the pair from the cap; `redact_scan` on 128k → bytes = scannable, words window-derived, `RedactScan`; cap above window → inert (`Window`); reservation ≥ window → default/`DefaultUnknown`; precedence order pinned by one test per pair of bounds.
- [ ] Digest thresholds: default route (1,500 / 12,000) byte-identical to today; on a 128k route the fraction (≈30,990 words / ≈93 KB) is capped by the ceiling where the ceiling is smaller — assert `min(fraction, ceiling)` on both currencies and that the ceiling binds on 200k.
- [ ] `HarnessConfig::default().budget` equals `derive(local)`; the default pair is asserted equal to `HarnessConfig::default()`'s pair (one source).
- [ ] `cargo test -p tetond harness::budget` green; no clippy warnings; integer arithmetic only.

## Technical Notes

- "Policy is pure, mechanism is gated" (architecture.md Key Patterns); LESSON-446 (write the currency and worst-case factor at the constant); LESSON-456 (one home per number).
- Commit as `feat(harness): budget.rs — the one route-budget derivation [TASK-182]`.
