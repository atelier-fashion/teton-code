---
id: TASK-409
title: "The turn-activity projection: phases folded from events, a pure frame, and the turn summary"
status: complete
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: []
repo: teton-code
---

## Description

Create `crates/teton/src/activity.rs` (ADR-621-2): `TurnActivity` with `Phase`,
`observe(&EventEnvelope, own_session: Option<&SessionId>, now: Instant)`,
`frame(&self, now: Instant, tick: u64, width: usize) -> Option<String>`, and
`finish(&mut self, now) -> TurnSummary`. No I/O, no terminal, no clock of its own.
Cost so far is the exact sum of this turn's `cost_recorded.usd_micros`, formatted
with `cost_ui::format_usd`. The stall is an annotation (ADR-621-5) with
`STALL_AFTER = Duration::from_secs(15)`, exempting `ToolRunning`. Wire the field
onto `SessionState` and arm it in `begin_turn`.

## Files to Create/Modify

- `crates/teton/src/activity.rs` — new: `Phase`, `TurnActivity`, `TurnSummary`, `STALL_AFTER`, `SPINNER` frames, `observe` / `frame` / `finish` / `permission_answered`; unit tests and the mutation record
- `crates/teton/src/main.rs` — `mod activity;`
- `crates/teton/src/session_ui.rs` — `SessionState::activity: TurnActivity` and `last_turn_summary: Option<TurnSummary>`; `begin_turn` calls `activity.begin(Instant::now())`; `format_turn_summary(&TurnSummary) -> String` for BR-16
- `crates/teton/src/cost_ui.rs` — `format_usd` made `pub(crate)` if it is not already reachable

## Acceptance Criteria

- [x] `observe` maps every consumed event to the phase in the REQ's Events table; an event whose envelope names another session is ignored; a missing session id counts as ours (same reading as `other_session`)
- [x] `frame` returns `None` for `Idle`, `Streaming` (until stalled), and `AwaitingPermission`; otherwise `<spinner> <sentence> · <phase>s · turn <turn>s[ · $cost]`, truncated to `width` on a char boundary using `unicode_width`
- [x] Before `route_decided` the sentence is `preparing turn`; after it, `waiting on <provider>[ <model>] (<tier>)` with the model omitted when the event carries none
- [x] A stalled frame stops the spinner (fixed glyph) and appends ` · no word from the daemon for <n>s`; never in `ToolRunning`
- [x] `observe(permission_request)` remembers the prior phase; `permission_answered(now)` restores it
- [x] `finish` accrues the last phase and yields total, model, tool durations and cost; `Idle` afterwards
- [x] Unit table of literal expected strings for `(phase, detail, elapsed, cost, stalled)`; the oracle never calls `frame`
- [x] Mutation "`frame` ignores `tick`" applied and observed red; recorded in the test's doc comment
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean — **fmt is clean and clippy reports nothing but `dead_code`**: 11 items, all of them the projection's pump-facing API (`observe`, `frame`, `permission_answered`, `format_turn_summary`, and the constants, fields, variants and helpers only those reach). They have no caller until TASK-411 wires the pump, which its own file lists as the call sites; the `type_complexity` finding this task did own is fixed. No `#[allow(dead_code)]` was added — this crate has none and the conventions forbid adding one.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton/src/activity.rs::tests::detail_comes_only_from_the_event_that_carried_it` | yes |
| BR-3 | test-case | `crates/teton/src/activity.rs::tests::the_frame_table` | no |
| BR-7 | test-case | `crates/teton/src/activity.rs::tests::the_frame_table` | no |
| BR-11 | test-case | `crates/teton/src/activity.rs::tests::a_stall_annotates_the_last_phase_and_a_running_tool_is_exempt` | yes |
| BR-15 | test-case | `crates/teton/src/activity.rs::tests::another_sessions_event_changes_nothing` | yes |
| BR-16 | test-case | `crates/teton/src/session_ui.rs::tests::the_turn_summary_reads_the_same_accumulator_the_frames_read` | no |
| AC-4 | test-case | `crates/teton/src/activity.rs::tests::the_frame_advances_with_the_tick` | no |
| AC-8 | test-case | `crates/teton/src/activity.rs::tests::no_phase_is_invented` | yes |
| AC-9 | test-case | `crates/teton/src/activity.rs::tests::another_sessions_event_changes_nothing` | yes |

## Technical Notes

- Mirror `loading.rs`: module docs state the two load-bearing properties (no I/O, no clock) and the "what breaks which test" table.
- `observe` takes the envelope so it reads `session_id` and `event` together; reuse `session_ui::other_session` rather than a second reading.
- `Streaming` is entered on `agent_message_chunk` and `last_event` is refreshed on every chunk, so a mid-stream stall measures from the last byte.
- `ToolRunning` → `AwaitingModel` on `tool_call_update` (completed or failed); `tool_time` accrues on that exit, `model_time` accrues on exit from `AwaitingModel`/`Streaming`.
- `TurnSummary` fields: `total`, `model`, `tools`, `cost_micros`. `format_turn_summary` renders `turn <t>s: model <m>s, tools <k>s, cost $x` — one line, `LineKind::Info`.
- LESSON-569: the expected strings in the table are literals; do not build them with `format!` from the same inputs.
