---
id: TASK-186
title: "Router: with_redact_scan, harness_config_for derives RouteBudget, degraded_harness_config(id) keeps the window, Route carries budget, route_decided projects budget/bound"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-181", "TASK-182", "TASK-184"]
repo: teton-code
---

## Description

The router becomes the owner of the per-route budget (ADR-1/ADR-2, BR-1/BR-8):
`harness_config_for(id)` derives `RouteBudget` through `budget::derive` and
stamps it into the `HarnessConfig` via `with_route_budget`; `Route` carries it;
`Route::route_decided()` projects `budget_tokens`/`bound`; the `Degrade` arm
keeps the failed provider's window; `build_router` feeds `privacy.redact`.

## Files to Create/Modify

- `crates/tetond/src/router.rs` — `Router` gains `redact_scan: bool` + `pub fn with_redact_scan(self, bool) -> Self` (the `with_local_available` builder shape); `pub fn budget_for(&self, provider_id: Option<&str>) -> RouteBudget` (the `effort_for` shape, L414-457: local classified via `self.table.local_provider_id`/`is_local_tier` L1020, never from "capabilities == default"; reservation = `HarnessConfig::default().gen_params.max_tokens`; window/cap from `capability_of(id)`; `redact_scan` from the field; the window label built with the id); `harness_config_for` (L830-836) = `from_harness_profile(profile).with_route_budget(self.budget_for(Some(id)))`; `degraded_harness_config()` (L1167-1176) becomes `degraded_harness_config(&self, failed: &str)` deriving from `capability_of(failed)` with `tool_call_tier = Degraded` **and** `with_route_budget(self.budget_for(Some(failed)))` — bound stays `Window`; `resolve_local_pin` no-local arm and `route_from` no-provider arm keep `HarnessConfig::default()` (doc which bound that is); `Route` (L196-247) gains `pub budget: RouteBudget` (copied from `harness.budget`, one source — assert equality in a test); `Route::route_decided()` (L264-280) sets `budget_tokens: Some(budget.budget_tokens as u64), bound: Some(budget.bound)`; `continue_on` (L1130-1148) carries the new harness unchanged. Tests (mod at L1195): AC-1 table — 128k/1,024 → `(84_650, 253_952)`/`Window`; 0 → default/`DefaultUnknown`; local id → default/`LocalEngine`; cap 40k on 200k → `UserCap`; `with_redact_scan(true)` + 128k → `RedactScan` with bytes = scannable; `degraded_provider_yields_the_reduced_harness_profile` (L2132) extended: Degrade on a 128k provider keeps `budget_tokens` and `bound: Window`; `route_decided_*` tests (L1223, L1432) assert the two new fields; AC-12: `route.budget == route.harness.budget == route_decided().bound` in one test.
- `crates/tetond/src/runtime.rs` — `build_router` (L9273-9371): `.with_redact_scan(config.privacy.redact)`.
- `crates/tetond/tests/routing.rs` — `weak_capability_provider_gets_degraded_harness_profile` (L602) and `a_malformed_tool_call_degrades_in_place_rather_than_failing` (L534): assert the budget survives degrade; fixtures `native()`/`degraded()` (L71-84) now yield window budgets — update expectations.

## Acceptance Criteria

- [ ] `cargo test -p tetond router` and `--test routing` green; AC-1 and the Degrade-keeps-budget case pinned; `route_decided` carries `budget_tokens`/`bound` on every route the router builds (duty routes stay `None`).
- [ ] `grep -n "budget::derive(" crates/tetond/src/router.rs` shows exactly one call site (`budget_for`).
- [ ] Every existing router/routing test green or updated with a one-line reason.

## Technical Notes

- Tracer gotchas #1, #2, #9 (architecture.md). `Route.budget` is a copy of `harness.budget` — assert they agree.
- Commit as `feat(router): the budget is a property of the route attempt [TASK-186]`.
