---
id: TASK-189
title: "Runtime wiring: per-attempt budget, reroute/fallback re-fit + context_pressure, SessionEvents emitter, newest-block notice, carry report, ContextLengthExceeded arm; remote-loop + routing + carry unit tests"
status: complete
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

- `crates/tetond/src/harness/turn_loop.rs` — `SessionEvents::context_pressure(&self, report: &PressureReport, kind: ContextPressureKind, budget: &RouteBudget)` (carries `budget_tokens` **and** `budget_bytes`) (beside `prefix_cache`, L358-414) publishing `Event::ContextPressure`; the three gates (L595, L636, L748) emit when `!report.is_quiet()` (`BlocksDropped` / `BlockElided`); when `newest_user_elided`, also push a one-line notice into the turn's output (reuse the path `capability_dead_end`'s sentence takes); `HarnessError::ContextLengthExceeded { provider_id, assembled_tokens: usize, budget_tokens: usize }`.
- `crates/tetond/src/harness/completion.rs` — `RemoteProviderSource::produce_turn` maps `ProviderError::ContextLengthExceeded` → `HarnessError::ContextLengthExceeded` with the manager's `estimated_tokens()` and the config's budget; it does **not** call `note_failure` (L507-514).
- `crates/tetond/src/runtime.rs` — `run_prompt_turn`: at the two reroute arms (L2961 privacy block → `resolve_local_pin`; L3009-3016 failure → `route = next`), after the new route is chosen: `let report = conversation.ctx_mut().rebudget(route.harness.context_budget_tokens, route.harness.context_budget_bytes);` and emit `context_pressure { RefitOnReroute }` when the pair **changed** (always, even if quiet — the refit is the news) — the Degrade arm keeps the pair and emits nothing (pin both); set the manager's window label from `route.budget` at `CarriedTurn::begin` (L2894) and on rebudget; new arm before `Remote(perr) if attempts < 2` (L2988-3001): `HarnessError::ContextLengthExceeded` → typed `RpcError` (code `CONTEXT_LENGTH_EXCEEDED`, message naming provider, window and assembled size — no body text), **no** `record_health`, **no** `on_provider_failure`; the carry commit's report (`CarriedTurn::commit_now` returns it — change the signature in carry.rs) → emit `context_pressure { BlocksDropped }` with the current route's budget when it dropped (BR-10).
- `crates/tetond/src/carry.rs` — `commit_now` returns `PressureReport` (L225-259); `CarriedTurn::begin` takes the `RouteBudget` (or label) to set on the manager.
- Tests (this task): `crates/tetond/tests/remote_loop.rs` — AC-2: a 128k `HarnessConfig` (via `with_route_budget(derive(..))`) assembles a 20,000-word prompt whole (build the fixture at ≤ 4 B/word so the byte guard does not bind first) — one request, `transport.requests()[0]` body contains all of it; AC-3: `effort_refusal.rs` shape — a transport answering 400 with `"code":"context_length_exceeded"` yields the typed error, **one** request, no second attempt; runtime-level: no `provider_degraded`, health unchanged; `tests/routing.rs` `a_malformed_tool_call_degrades_in_place_rather_than_failing` (L534) asserts no refit event and `bound: window` (AC-15c); runtime unit: the privacy-block and failure reroute arms call `rebudget` and emit `refit_on_reroute` (the Degrade arm is quiet). The e2e/integration fixtures (AC-15a/b, AC-11, AC-6, AC-10 daemon emissions) are **TASK-193**.

## Acceptance Criteria

- [x] AC-2, AC-3 (typed outcome), AC-15c and the reroute-arm unit pins green in the harnesses named above (AC-6/10/11/15a/b are TASK-193's).
      *(`remote_loop.rs::a_128k_route_assembles_a_20000_word_prompt_whole_and_the_default_pair_clamps_it`
      (AC-2),
      `remote_loop.rs::a_context_length_refusal_ends_the_turn_typed_after_one_request`
      (AC-3), `router.rs::a_degrade_keeps_the_failed_providers_budget`
      (AC-15c), and the reroute-arm unit pins
      `runtime.rs::a_reroute_to_a_smaller_window_refits_the_context_and_publishes_it`
      / `a_degrade_that_keeps_the_window_refits_nothing_and_says_nothing`.)*
- [x] `cargo test -p tetond --no-fail-fast` green.
      *(green inside TASK-192's `cargo test --workspace --no-fail-fast`:
      3,159 passed / 0 failed / 1 ignored across 59 targets, no `FAILED` in
      the log.)*
- [x] A `ContextLengthExceeded` leaves router health unchanged (assert before/after) and emits no `provider_degraded`.
      *(`runtime.rs::a_context_length_refusal_changes_no_health_and_degrades_nothing`
      — a real socket answering the vendor's 400, health snapshotted before
      and after, and no `provider_degraded` on the bus.)*

## Technical Notes

- **From TASK-185's report**: the shipped spellings are OpenAI `"context_length_exceeded"` (quoted token, matches pretty-printed bodies) / `maximum context length` and Anthropic `prompt is too long`; Moonshot/Kimi's own overflow spelling is NOT pinned yet and Kimi is the dogfood provider — verify Kimi's 400 body for a context overflow against the Moonshot API docs (web lookup) and add it as a fourth `const` in `teton-providers/src/lib.rs` + a conformance case in this task (the providers crate is yours to touch here; TASK-185 is done). Also `provider_id` on the variant is a `String` (the crate has no `ProviderId`); convert at the `HarnessError` mapping.

- Order inside the reroute arm: choose route → rebudget → emit → `continue` (the event precedes the next `route_decided`). The Degrade arm must be **quiet** — pin it.
- Commit as `feat(daemon): the budget follows the route attempt — refit on reroute, pressure as news, typed context-length outcome [TASK-189]`.
