---
id: TASK-184
title: "egress/redact.rs: REDACT_SCANNABLE_CONTEXT_BYTES derived from the cap constants; REDACT_BODY_OVERHEAD_BYTES promoted; assertions beside the margin test"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: []
repo: teton-code
---

## Description

Extend the redact chain without forking it (ADR-6, BR-4, BR-11): the
scannable bound is an expression over the same constants as
`REDACT_TOTAL_CAP_CHUNKS`; the test-only overhead assumption becomes a
production constant with the same value; the arithmetic doc blocks gain the
bound's derivation; the existing margin test keeps measuring the default
shape and a new assertion pins the bound.

## Files to Create/Modify

- `crates/tetond/src/egress/redact.rs` — drop `#[cfg(test)]` from `REDACT_BODY_OVERHEAD_BYTES` (L284-285; value 10 KiB; doc: "what the body carries beyond the assembled context — system prompt, JSON envelope, escaping"); add `pub(crate) const REDACT_ESCAPING_DIVISOR: usize = 10;` (the `budget/10` term the margin test already uses at L2116-2118 — read it from here, one source) and `pub(crate) const REDACT_SCANNABLE_CONTEXT_BYTES: usize = (REDACT_INPUT_MAX_BYTES - REDACT_BODY_OVERHEAD_BYTES) * REDACT_ESCAPING_DIVISOR / (REDACT_ESCAPING_DIVISOR + 1);` next to `REDACT_INPUT_MAX_BYTES` (L370) with the derivation written out (≈ 89,127; "a floor — cap minus overhead; the 2× margin is not inverted because the bound is derived from the cap and cannot drift"); extend the doc blocks at L305-369 ("REQ-586: the remote budget is bounded by this when the scan applies"); tests: the margin test (L2098-2223) unchanged except it reads `REDACT_ESCAPING_DIVISOR`; new `the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap` asserting `SCANNABLE + SCANNABLE / DIVISOR + OVERHEAD <= REDACT_INPUT_MAX_BYTES` and `SCANNABLE > HarnessConfig::default().context_budget_bytes`, with a comment naming the two single-constant mutations that would fail it.
- `crates/tetond/src/harness/tools/web.rs` — `the_web_tool_docs_clear_the_outbound_body_overhead` (L2264-2370) reads `REDACT_ESCAPING_DIVISOR` instead of a literal 10; otherwise unchanged.

## Acceptance Criteria

- [ ] `cargo test -p tetond egress::redact` and `harness::tools::web` green; `the_total_cap_clears_the_harness_context_budget_with_margin` still measures `for_strong_model()`'s default pair and stays green with no ceiling move.
- [ ] `REDACT_SCANNABLE_CONTEXT_BYTES` is computed, never a literal; `grep -n "89_127\|89127" crates/` finds only comments.
- [ ] The constant is `pub(crate)` and reachable from `harness/budget.rs` (TASK-182).

## Technical Notes

- LESSON-491 ("write the chain down once"), LESSON-456.
- Commit as `feat(egress): the redact-scannable context bound, derived where the cap is [TASK-184]`.
