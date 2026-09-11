---
id: TASK-417
title: "The pump engages raw mode, reads keystrokes on its tick, owns two rows, and shelves the pending line around questions"
status: complete
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

- [x] With a live-row surface, `typed_input`, and a scripted raw-mode double (a test hook standing in for `read_available` and `RawMode`, in the shape `TimedReceive` already uses), bytes fed on ticks produce `Line(Activity)` then a pending row draw, repaints of both, and `Withdraw(2)` before a durable event line
- [x] Enter: pending row withdrawn, the activity frame carries `· 1 queued`, nothing durable printed
- [x] A permission outcome: both rows withdrawn, `shelved` set before the prompter is called, restored and redrawn after
- [x] `RawMode` is never engaged for a non-`ENDS_TURN` method, a plain surface, or `typed_input == false` (no termios call recorded)
- [x] A `Failed` raw outcome: no rows beyond REQ-621's, `abandon` path still reachable, one verbose notice
- [x] Every `ENDS_TURN` exit — Ok, RpcError, transport drop — withdraws both rows and drops the guard
- [x] Mutation "shelve skipped before the prompter" observed red; recorded
- [x] `cargo test -p teton` (unit) green; clippy and fmt clean

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

## Implementation record (2026-09-10)

**Mutations, applied and observed red, reverted by targeted edit.**

1. *Shelve skipped before the prompter* — dropped `ctx.state.input.shelve()`
   from `around_a_question`. **1 red of 853**,
   `a_question_reads_a_fresh_buffer_and_the_pending_line_comes_back`, on its
   closure leg (`left: Some("> half a thought")` where the prompter must be
   handed nothing). The literal `Rendered` sequence in the same test is
   **byte-identical** with the shelve gone — a `Prompter` is not a `Surface` and
   cannot reach the editor, and a terminal draws a stolen keystroke as willingly
   as a fresh one — so the leg that observes the editor *at the moment the
   prompter is called* is the only thing in the suite that can fail here
   (LESSON-569).
2. *The activity row's repaint offset fixed at 1* — `let rows_up = 1;` in place
   of the arithmetic the block does over its two visibilities. **2 red of 853**,
   `the_pending_row_is_never_painted_over` and
   `a_question_reads_a_fresh_buffer_…`, on `Repaint(1, ..)` where the oracles
   say `Repaint(2, ..)`. That is BUG-225's failure arriving through the fix for
   it, and nothing else in the suite noticed: every other row test runs with no
   pending row, where the offset is 1 either way.

**Deviations from this task's text, all three deliberate.**

- **The pending row is drawn in `LineKind::Activity`**, the one transient class,
  rather than in a class of its own. A dedicated kind belongs to `render.rs`,
  which is outside this task's files; `Activity` is the only existing kind whose
  contract is "withdrawn rather than left behind", which is the load-bearing
  half. It is drawn dim as a consequence — cosmetic, and worth revisiting in
  TASK-419/420 if the pty legs read badly.
- **The cursor rests below the block, one row under the pending row**, not at
  the end of it. That is what makes `repaint_row_above(1)` address the pending
  row at all; parking the terminal's cursor visually at the row's end needs a
  surface verb that does it, which is again `render.rs`.
- **The block is taken down as two `Withdraw(1)`s, not one `Withdraw(2)`**, as
  the Deliverable's "`withdraw_row_above` per row, bottom first" says and as the
  first acceptance criterion's "`Withdraw(2)`" does not: the verb clears *the*
  row at that offset and leaves the cursor on it, so `withdraw_row_above(2)`
  would erase the activity row and leave the pending row on screen with the
  cursor above it. The oracles are literal `Withdraw(1), Withdraw(1)` pairs.
- **`InputHandover` splits the verdict from the guard** where ADR-622-1 has one
  `RawOutcome`. Only `prompt.rs` can construct a `RawMode` (its saved `termios`
  is private), so a test hook standing in for `engage` could not return
  `RawOutcome::Raw(..)`; `engage_raw_mode` is the one place the two spellings
  meet.

**One correctness addition beyond the task's text.** `engage_input` *seeds*
`TurnActivity::set_queued` from `InputEditor::queued_len`, not only on
`Edit::Queued`: the queue outlives a turn by design (ADR-622-5 drains one line
per pass of the entry loop), and `TurnActivity::begin` has just cleared the
count with every other figure a turn must not inherit — so a line still waiting
when the next turn opens would otherwise go unannounced, against BR-14's "with
the exact count". Pinned by the second leg of
`an_enter_queues_the_line_and_the_row_says_so`.

**Dead-code notices retired.** `cargo build -p teton` (no `cfg(test)`) went from
six notices to none: `RawOutcome`, `RawVerdict`, `classify_raw`, `raw_from`,
`RawMode::engage`, `RawMode::arm`. `read_available`, `is_engaged` and `SLOT_RAW`
were already reached by TASK-418's in-flight `prompt.rs`. The editor's
`push`/`row`/`shelve`/`unshelve`/`queued_len` all have production callers in
`client.rs` now; `take_answer` is the only one still without, and it is
TASK-418's (`take_next_queued` is already called from their `main.rs`).
