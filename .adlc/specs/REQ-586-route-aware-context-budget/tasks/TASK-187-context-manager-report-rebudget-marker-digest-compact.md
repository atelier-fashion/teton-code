---
id: TASK-187
title: "ContextManager: truncate_to_budget reports, rebudget(), window-labelled marker, digest thresholds words+bytes, bounded compact prompt"
status: complete
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-182"]
repo: teton-code
---

## Description

The context manager learns to say what it did and to be re-budgeted
(ADR-3/ADR-4/ADR-5, BR-6/BR-7): `truncate_to_budget` returns a
`PressureReport`; `rebudget(tokens, bytes)` sets both budgets and runs the gate;
the elision marker names the route's window; `summarize_if_large` takes both
thresholds; `compact_prompt` is bounded to the duty's own prompt budget.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `#[must_use] pub struct PressureReport { pub dropped_blocks: usize, pub elided_bytes: usize, pub newest_user_elided: bool }` with `is_quiet()`; `truncate_to_budget(&mut self) -> PressureReport` (L1248-1278; count drops; when the last block is clamped, record `elided_bytes = before − after` and `newest_user_elided = last.role == User`); `pub fn rebudget(&mut self, tokens, bytes) -> PressureReport` (sets `budget_tokens/bytes`, then `truncate_to_budget()`); `window_label: String` + `with_window_label(label)` builder (default "the local context window"); `truncate_middle_with(text, room, marker)` used by the clamp — `truncate_middle` (L1504-1519) unchanged for the six duty callers; the marker is bounded by `room` inside the clamped block (no `TRUNCATION_NOTE_BYTES` change — that charges the system-prompt note, which stays window-neutral); `truncate_middle_with` keeps the `keep < 64` degenerate branch correct for a longer marker (test); `summarize_if_large(route, tool, text, threshold_tokens, threshold_bytes, prov)` (L1613-1702; drop the `× APPROX_BYTES_PER_TOKEN` at L1620); existing callers/tests updated (`whitespace_poor_but_byte_huge_results_trigger_summarization` L1945, `small_tool_results_are_not_summarized` L1924, `large_tool_results_are_summarized_by_the_local_engine` L1932, `a_single_oversized_block_is_clamped_in_place` L2279 marker text, `truncation_drops_oldest_and_marks_it` L1780 reads the report); new tests: AC-10 unit — three drops → `dropped_blocks: 3`; oversized newest user block → `elided_bytes > 0, newest_user_elided: true`; `rebudget` from 100k to 4k drops and reports; the marker names a custom label; a 240 KB minified result is digested on a 128k pair while a 3,000-word prose result is not (thresholds from `budget::derive`).
- `crates/tetond/src/harness/compact.rs` — `compact_prompt(blocks, prompt_budget_bytes)` (L284-307) offers the **oldest** blocks up to the budget (each still capped at `COMPACT_BLOCK_MAX_BYTES`), stops at the first that would overflow, and says "offered blocks 1..N of M" so the answer (block numbers) stays valid; `pub const COMPACT_PROMPT_BUDGET_BYTES` derived as `(LOCAL_ENGINE_N_CTX − compact max_tokens) × duty::DUTY_REQUEST_BYTES_PER_TOKEN − CHATML_DUTY_ENVELOPE_BYTES` (the `REDACT_PROMPT_BUDGET_BYTES` shape, redact.rs:133 — reuse the constants, no literals); `ContextManager::attempt_compaction` (context.rs:1036-1090) passes it; tests: `an_enormous_block_is_bounded_in_the_prompt` (L541), `the_duty_prompt_numbers_every_block_and_protects_the_last` (L528) updated; new: a 200-block context's compact prompt is ≤ `COMPACT_PROMPT_BUDGET_BYTES` and still names the protected last block; `the_compact_ceiling_is_the_loosest_of_the_five` (L679) unchanged (ADR-5); **AC-9 pins**: a 100k pair fires `under_pressure` at exactly `COMPACT_PRESSURE_PERCENT` as a 4k pair does (new row in the `only_a_pressured_context_is_worth_compacting` family, L466), and the REQ-561 failed-compaction → deterministic-truncation fallback is asserted unchanged (name the existing test or add one).
- `crates/tetond/src/harness/turn_loop.rs` — the tool-result fold (L945-953) passes `config.summarize_threshold_tokens, config.summarize_threshold_bytes`; the three `truncate_to_budget()` call sites (L595, L636, L748) bind the report to `let _pressure = …` with `// TASK-189 emits this`.
- `crates/tetond/src/carry.rs` — `commit_now` (L248) binds the report the same way (`// TASK-189 returns this`); `crates/tetond/src/sessions.rs:1348-1385` and `carry.rs:541/569` tests updated for the return type; because `PressureReport` is `#[must_use]`, every bare `ctx.truncate_to_budget();` in tests must bind it or clippy `-D warnings` fails — sweep `context.rs` tests (~18 sites), `tests/duty_egress.rs:443`, `tests/duty_matrix.rs:555`.
- `crates/tetond/src/harness/tools/docs.rs` — `the_topic_ceiling_stays_under_the_summarize_threshold` (L534-550) reads `HarnessConfig::default().summarize_threshold_bytes`.

## Acceptance Criteria

- [x] `cargo test -p tetond harness::context harness::compact harness::turn_loop carry sessions` green; default-route digest behaviour byte-identical (1,500 / 12,000); marker default text unchanged for the duty callers.
- [x] `truncate_to_budget`'s report is bound at all four call sites (no silently dropped report).
- [x] (deferred to TASK-193 — `tests/conversation_carry.rs` is outside this task's parallel-tier file ownership; the unit equivalent `harness::compact::tests::a_two_hundred_block_conversation_still_fits_the_duty_prompt` is green) A 200-block pressured context compacts through a scripted local engine without an over-window refusal (`tests/conversation_carry.rs` `a_session_driven_past_its_budget_compacts_and_keeps_answering` L911 extended or a sibling).
      *(carried out by TASK-193 as the sibling the AC allowed:
      `context_pressure.rs::a_two_hundred_block_conversation_on_a_big_route_compacts_through_the_local_binding`
      drives 200 blocks on a 128k route through a scripted local engine that
      refuses an over-window prompt, so an unbounded offer is red.)*

## Technical Notes

- Tracer gotchas #3, #4, #8; LESSON-447; LESSON-501.
- Commit as `feat(harness): context pressure is reported, the manager can be re-budgeted, digest thresholds carry bytes [TASK-187]`.
