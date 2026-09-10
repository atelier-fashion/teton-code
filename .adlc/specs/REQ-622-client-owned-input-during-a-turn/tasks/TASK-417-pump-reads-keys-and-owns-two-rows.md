---
id: TASK-417
title: "The pump engages raw mode, reads keystrokes on its tick, owns two rows, and shelves the pending line around questions"
status: draft
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-415", "TASK-416"]
repo: teton-code
---

## Description

Wire ADR-622-1 and ADR-622-4 into `crates/teton/src/client.rs`. `Connection::call`
engages `RawMode` for an `ENDS_TURN` method when `ctx.surface.has_live_rows() &&
ctx.typed_input`, keeps the guard for the pump's lifetime, and drops it at the
close-out (a `Failed` outcome keeps `RowState::abandon` armed and prints one verbose
notice — BR-11). On every tick and message the pump calls `prompt::read_available`
and feeds `ctx.state.input.push`; `RowState` becomes a two-row block (activity row +
pending row) with `paint_rows`/`withdraw_rows`; before dispatching a message it
withdraws both, and around a question outcome it `shelve`s and `unshelve`s. The
`· N queued` clause lands in `activity.rs`. A debug-only panic seam
(`TETON_TEST_SEAMS=1` + `TETON_TEST_PANIC_MID_TURN=1`) panics after the first tick.

## Files to Create/Modify

- `crates/teton/src/client.rs` — engage/drop in `call`; tick reads; two-row `RowState`; shelve/unshelve; the panic seam; tests
- `crates/teton/src/activity.rs` — `queued: usize` fed by the pump, `· N queued` clause in `frame`, test rows

## Acceptance Criteria

- [ ] With a live-row surface, `typed_input`, and a scripted raw-mode double (a test hook standing in for `read_available` and `RawMode`, in the shape `TimedReceive` already uses), bytes fed on ticks produce `Line(Activity)` then a pending row draw, repaints of both, and `Withdraw(2)` before a durable event line
- [ ] Enter: pending row withdrawn, the activity frame carries `· 1 queued`, nothing durable printed
- [ ] A permission outcome: both rows withdrawn, `shelved` set before the prompter is called, restored and redrawn after
- [ ] `RawMode` is never engaged for a non-`ENDS_TURN` method, a plain surface, or `typed_input == false` (no termios call recorded)
- [ ] A `Failed` raw outcome: no rows beyond REQ-621's, `abandon` path still reachable, one verbose notice
- [ ] Every `ENDS_TURN` exit — Ok, RpcError, transport drop — withdraws both rows and drops the guard
- [ ] Mutation "shelve skipped before the prompter" observed red; recorded
- [ ] `cargo test -p teton` (unit) green; clippy and fmt clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/src/client.rs::tests::raw_mode_is_engaged_only_for_a_turn_at_a_terminal` | yes |
| BR-2 | structural-check | `crates/teton/src/client.rs`: no `io::stdin()` outside `prompt.rs` (asserted by `the_pump_reads_only_through_read_available`) | no |
| BR-3 | test-case | `crates/teton/src/client.rs::tests::the_pending_row_is_never_painted_over` | no |
| BR-4 | test-case | `crates/teton/src/client.rs::tests::every_ends_turn_exit_withdraws_both_rows` | no |
| BR-5 | test-case | `crates/teton/src/client.rs::tests::a_question_reads_a_fresh_buffer_and_the_pending_line_comes_back` | yes |
| BR-11 | test-case | `crates/teton/src/client.rs::tests::a_refused_raw_mode_falls_back_to_abandon_and_says_so` | yes |
| BR-13 | test-case | `crates/teton/src/client.rs::tests::the_pending_row_is_never_painted_over` | no |
| BR-14 | test-case | `crates/teton/src/activity.rs::tests::the_frame_table` (queued rows) | no |
| AC-11 | test-case | `crates/teton/src/client.rs::tests::a_refused_raw_mode_falls_back_to_abandon_and_says_so` | yes |

## Technical Notes

- Read bytes **before** computing the frame on a tick so the pending row reflects the keystroke in the same paint.
- The pending row sits below the activity row; `repaint_row_above` offsets are 2 (activity) and 1 (pending) when both exist — keep them in one `RowBlock` method, never at call sites.
- Keep `RowState::abandon` and `line_waiting` only on the `Failed`/canonical path; document that the raw path cannot need them.
- Cost: `read_available` is one `poll` + at most one `read` per tick, replacing the existing `line_waiting` poll.
