---
id: TASK-270
title: "derive's local branch takes the engine window"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-269]
---

## Description

ADR-2 and ADR-3, and the core of the REQ. `derive`'s `is_local` arm stops returning
`default_pair` and instead supplies `window: LOCAL_ENGINE_N_CTX` (16,384) and
`reservation: LOCAL_GENERATION_RESERVATION` (1,024) into the shared arithmetic, while still
stamping `BudgetBound::LocalEngine`.

Result: **10,240 words / 30,720 bytes**, from `(16,384 − 1,024) = 15,360 usable`, then the
existing `× 2/3` words rule and `× 2` bytes rule.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — the `is_local` arm of `derive`; `BudgetInputs::local()`
  gains the real window and reservation in place of its two hardcoded zeros

## Acceptance Criteria

- [x] AC-1: the local pair equals what the remote path yields for a declared window of 16,384
      with the same reservation — one formula, driven from both sides in `derivation_table()`
- [x] AC-2: `HarnessConfig::default().budget.bound` is still `LocalEngine`, asserted **explicitly
      as the bound**, not via the numbers. Mutation: delete the arm so it falls to the
      `window == 0` path; this must redden. The existing `turn_loop.rs:465-468` pin compares only
      numbers and will stay green — that is why this AC exists
- [x] AC-3: a test constructs `HarnessConfig::default()` **and** calls `derive` on the local path
      and terminates. Satisfied by the test existing and returning, plus a note at
      `LOCAL_GENERATION_RESERVATION` explaining why it is not `generation_reservation()`
- [x] AC-4: property over the derivation —
      `budget_bytes / DUTY_REQUEST_BYTES_PER_TOKEN ≤ LOCAL_ENGINE_N_CTX − reservation`.
      **This assertion fails against today's constants**; confirm it does before making it pass
- [x] AC-5: `max_context = 0` still yields `(4096, 32768)` with `DefaultUnknown`, on a fixture
      that is **not** the local route
- [x] AC-15: `derive` with a synthetic small window (4,096) does not produce a byte half that,
      at 2 B/token, exceeds that window. Paired with a large window on the same fixture so it
      cannot pass by the floor never applying
- [x] Full suite: expect breakage in TASK-272's list; do not fix those here

## Technical Notes

`LOCAL_ENGINE_N_CTX` is at `runtime.rs:11999` and is **not** feature-gated — verified: `redact.rs`
has zero `cfg(feature)` gates and already imports it. Do not add a gate or a fallback.

Seams: `derivation_table()` (`budget.rs:2621-2780`) is the table test; `remote(window, cap,
redact_scan)` at `:2607` is the `#[cfg(test)]` builder — construct `BudgetInputs` directly for
the synthetic-window row.

Do **not** change `LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES`. They stay as the no-better-fact
default for `max_context = 0` and the unresolvable route (ADR-4). Two exploration agents assumed
otherwise; the architecture doc records why they were wrong.

## Implementation notes (TASK-270)

**`BudgetInputs::local()` keeps its two zeros; the arm reads the constants instead.** The
Files-to-Modify line above said the opposite, and it costs two live guards: `derive`'s local arm
must ignore `inputs.window` for `precedence_is_pinned_pairwise` (a local route with
`window: 200_000` still derives the engine's pair) and for `proposed_window`'s `is_local: false`,
whose deletion is mutation-tested by
`a_local_route_is_offered_the_window_of_the_provider_it_would_bind_to` — a caller-supplied window
would make that mutation pass. It also keeps AC-2's mutation landing exactly where ADR-2 says it
does, in the `window == 0` path. The rationale is written at `BudgetInputs::local`.

**`HarnessConfig::default()` now derives all five budget-bearing fields** (`turn_loop.rs:465`).
It set `context_budget_*` and `summarize_threshold_*` from the `LOCAL_*` constants while `budget`
carried `derive(local)`; those agreed by construction only because the local arm returned the
constants. After this task they are different numbers, and the default config would have been the
one config in the crate whose pair contradicts its own `RouteBudget` — the split
`with_route_budget` exists to prevent (`router.rs:1105`, REQ-586 BR-8). `remote_loop.rs`'s
`a_128k_route_assembles_a_20000_word_prompt_whole_and_the_default_pair_clamps_it` saw it directly:
the pressure event carried 10,240 while the config's word field still said 4,096.

**Left red for TASK-271/TASK-272** (12 tests, all pinning the old local pair or a fixture sized
against it): `router::tests::the_route_budget_is_derived_from_the_routes_own_window`,
`router::tests::budget_for_is_byte_identical_on_every_bound`,
`harness::context::tests::the_default_routes_digest_thresholds_are_byte_identical_to_today`,
`harness::compact::tests::the_compact_ceiling_is_the_loosest_of_the_five` (**TASK-271's** — it is
ADR-5's defect, visible now instead of silent), the three
`runtime::tests::the_over_budget_offer::*`, `pty_e2e`'s two over-budget cases,
`egress_capture::an_accepted_over_budget_expansion_still_answers_to_the_boundary`,
`skill_over_budget_offer::the_reported_analyze_failure_is_offered_and_accepting_dispatches_it`,
and `e2e::privacy_fixes::a_128k_turn_blocked_by_privacy_is_refitted_before_the_local_pin_serves_it`.

**For TASK-272: `turn_loop.rs:3365-3367` did not redden, and cannot.** Its `4_097 / 4_096` comes
from `ScriptedTurn::WindowRefusal`, a hardcoded `HarnessError::LocalContextLengthExceeded` at
`turn_loop.rs:3261-3265` — the fixture never derives a budget, so inverting it means making the
turn measure a real one, not editing the numbers.
