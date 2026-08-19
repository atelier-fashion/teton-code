---
id: TASK-189
title: "Runtime wiring: per-attempt budget, reroute/fallback re-fit + context_pressure, SessionEvents emitter, newest-block notice, carry report, ContextLengthExceeded arm; remote-loop / carry / egress tests"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-185", "TASK-186", "TASK-187"]
repo: teton-code
---

## Description

Wire the pieces into the turn (ADR-2/ADR-3/ADR-8; BR-1, BR-2, BR-7, BR-10):
the loop gates emit `context_pressure`; the reroute arms re-budget and emit
`refit_on_reroute`; the carry commit's report is emitted between turns; a
`ContextLengthExceeded` ends the turn typed with no health change; the
newest-user-block elision is also a turn notice.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — `SessionEvents::context_pressure(&self, report: &PressureReport, kind: ContextPressureKind, budget: &RouteBudget)` (beside `prefix_cache`, L358-414) publishing `Event::ContextPressure`; the three gates (L595, L636, L748) emit when `!report.is_quiet()` (`BlocksDropped` / `BlockElided`); when `newest_user_elided`, also push a one-line notice into the turn's output (reuse the path `capability_dead_end`'s sentence takes); `HarnessError::ContextLengthExceeded { provider_id, assembled_tokens: usize, budget_tokens: usize }`.
- `crates/tetond/src/harness/completion.rs` — `RemoteProviderSource::produce_turn` maps `ProviderError::ContextLengthExceeded` → `HarnessError::ContextLengthExceeded` with the manager's `estimated_tokens()` and the config's budget; it does **not** call `note_failure` (L507-514).
- `crates/tetond/src/runtime.rs` — `run_prompt_turn`: at the two reroute arms (L2961 privacy block → `resolve_local_pin`; L3009-3016 failure → `route = next`), after the new route is chosen: `let report = conversation.ctx_mut().rebudget(route.harness.context_budget_tokens, route.harness.context_budget_bytes);` and emit `context_pressure { RefitOnReroute }` when the pair **changed** (always, even if quiet — the refit is the news) — the Degrade arm keeps the pair and emits nothing (pin both); set the manager's window label from `route.budget` at `CarriedTurn::begin` (L2894) and on rebudget; new arm before `Remote(perr) if attempts < 2` (L2988-3001): `HarnessError::ContextLengthExceeded` → typed `RpcError` (code `CONTEXT_LENGTH_EXCEEDED`, message naming provider, window and assembled size — no body text), **no** `record_health`, **no** `on_provider_failure`; the carry commit's report (`CarriedTurn::commit_now` returns it — change the signature in carry.rs) → emit `context_pressure { BlocksDropped }` with the current route's budget when it dropped (BR-10).
- `crates/tetond/src/carry.rs` — `commit_now` returns `PressureReport` (L225-259); `CarriedTurn::begin` takes the `RouteBudget` (or label) to set on the manager.
- Tests: `crates/tetond/tests/remote_loop.rs` — AC-2: a 128k `HarnessConfig` (via `with_route_budget(derive(..))`) assembles a 20,000-word prompt whole — one request, `transport.requests()[0]` body contains all of it; AC-3: `effort_refusal.rs` shape — a transport answering 400 with `"code":"context_length_exceeded"` yields the typed error, **one** request, no second attempt; runtime-level: no `provider_degraded`, health unchanged; `crates/tetond/tests/e2e/privacy_fixes.rs` `taint_and_reroute` (L73) extended: 128k remote + 60,000-word context → the privacy-block reroute to local emits `context_pressure { refit_on_reroute }` **before** the local `route_decided`, and the turn completes (AC-15a); `tests/e2e/ac_matrix.rs` `ac7_degraded_provider_falls_back_and_completes` (L678) extended for fallback to a smaller-window provider (AC-15b); `tests/routing.rs` `a_malformed_tool_call_degrades_in_place_rather_than_failing` (L534) asserts no refit event and `bound: window` (AC-15c); `crates/tetond/tests/conversation_carry.rs` — `Carry` fixture gains a per-prompt budget override (L441-491) so AC-11 runs: 30,000-word conversation on a 128k pair, next prompt on the default pair → oldest dropped, `context_pressure` emitted, turn completes, retained conversation is what the local turn kept; AC-10 daemon half: three-drop and newest-block-elision emissions pinned and removing either emission fails; `crates/tetond/tests/redact_egress.rs` — AC-6: `[privacy] redact = true` + 128k route: the assembled body fits the scannable bound and the scan forwards (copy `a_context_budget_full_payload_is_scanned_across_windows_and_forwards` L944); `redact = false` + `[web] tier = search` → bound `Window` (router unit).

## Acceptance Criteria

- [ ] AC-2, AC-3 (typed outcome), AC-6, AC-10 (daemon half), AC-11, AC-15 (a/b/c) green in the harnesses named above.
- [ ] `cargo test -p tetond --no-fail-fast` green.
- [ ] A `ContextLengthExceeded` leaves router health unchanged (assert before/after) and emits no `provider_degraded`.

## Technical Notes

- Order inside the reroute arm: choose route → rebudget → emit → `continue` (the event precedes the next `route_decided`). The Degrade arm must be **quiet** — pin it.
- Commit as `feat(daemon): the budget follows the route attempt — refit on reroute, pressure as news, typed context-length outcome [TASK-189]`.
