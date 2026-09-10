---
id: TASK-411
title: "The pump wakes on a timed receive, owns the row, and closes it at the ENDS_TURN seam"
status: complete
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-409", "TASK-410"]
repo: teton-code
---

## Description

Wire ADR-621-1, ADR-621-3, and ADR-621-4. `Connection::recv_timeout` on the
existing `mpsc` receiver; `pump_until_answered` uses it only when
`ctx.surface.has_live_rows()`, otherwise the blocking `recv`. On `Tick`: stamp
`now`, compute the frame, draw (`line`) or repaint. On `Message`: withdraw the
row if visible, dispatch as today, `observe` the event, restore after a
permission answer, redraw. In `call`'s `ENDS_TURN` branch: withdraw if visible,
`finish`, store the summary. The turn arm in `main.rs` prints the BR-16 line on
Ok and Err when `state.verbose`.

## Files to Create/Modify

- `crates/teton/src/client.rs` — `pub(crate) const FRAME_INTERVAL` (moved from main.rs); `Wake` enum; `Connection::recv_timeout`; the pump's tick arm; `RowState { visible: bool }` on the pump; withdraw-before-dispatch / redraw-after; `permission_answered` after `resolve_permission`; `ENDS_TURN` close-out; unit tests with `Connection::scripted*` and `RecordingSurface::with_live_rows()`
- `crates/teton/src/main.rs` — import `FRAME_INTERVAL` from `client`; BR-16 line on both arms of the `session/prompt` match via `session_ui::format_turn_summary`
- `crates/teton/src/session_ui.rs` — `SessionState::observe_activity(&EventEnvelope, now)` thin wrapper (keeps `other_session` in one place)

## Acceptance Criteria

- [x] With a live-row surface and a scripted connection that answers after three ticks, the recording shows `Line(Activity)` then two `Repaint(1, Activity)` then `Withdraw(1)` before the response is returned
- [x] With a plain surface the same script records no `Activity` line, no repaint, no withdraw, and the pump used the blocking receive (assert via a counter on the scripted connection: zero timeouts observed)
- [x] An event arriving while the row is visible records `Withdraw(1)`, the event's own line, then a fresh `Line(Activity)`
- [x] A permission outcome: withdraw, prompt, answer, then the prior phase is restored and redrawn
- [x] Every exit of `call` for an `ENDS_TURN` method — Ok, `RpcError`, and a scripted transport drop — leaves `RowState.visible == false` and `state.last_turn_summary.is_some()`
- [x] A non-turn method (`ConfigGetParams`) pumping through a live-row surface never draws a row
- [x] The BR-16 line prints only when `state.verbose`, on both Ok and Err arms, and reads the summary from `finish`
- [x] `cargo test -p teton` green; clippy and fmt clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/src/client.rs::tests::the_row_is_present_in_a_silent_phase_and_withdrawn_while_streaming` | yes |
| BR-4 | test-case | `crates/teton/src/client.rs::tests::the_pump_ticks_while_the_daemon_is_silent` | no |
| BR-4 | test-case | `crates/teton/src/client.rs::tests::a_plain_surface_never_enters_the_tick_arm` | yes |
| BR-5 | test-case | `crates/teton/src/client.rs::tests::a_durable_line_prints_where_the_row_was` | no |
| BR-6 | test-case | `crates/teton/src/client.rs::tests::a_plain_surface_never_enters_the_tick_arm` | yes |
| BR-9 | test-case | `crates/teton/src/client.rs::tests::the_tick_arm_adds_no_latency_to_a_queued_message` | yes |
| BR-9 | test-case | `crates/teton/src/client.rs::tests::a_submitted_line_abandons_the_row_and_never_paints_over_it` (added at verify: the "a repaint never blanks characters the user has echoed" clause had no test, and could not have held — see the note below) | no |
| BR-13 | test-case | `crates/teton/src/client.rs::tests::a_failed_paint_hides_the_row_and_says_so_in_verbose` (added at verify: the row verbs swallowed their write errors, so the pump could not act on one) | no |
| BR-10 | test-case | `crates/teton/src/client.rs::tests::a_durable_line_prints_where_the_row_was` | no |
| BR-12 | test-case | `crates/teton/src/client.rs::tests::every_ends_turn_exit_withdraws_the_row` | no |
| BR-12 | test-case | `crates/teton/src/client.rs::tests::a_non_turn_method_never_draws_or_withdraws` | yes |
| BR-14 | structural-check | `crates/teton-protocol/src/lib.rs::tests::this_build_advertises_only_the_version_its_types_can_read` (unchanged, asserted green — the row's original name, `protocol_version_is_pinned`, is not a test that exists; this is the pin on the version bound, and no protocol source was touched) | no |
| BR-16 | test-case | `crates/teton/src/main.rs::tests::the_verbose_summary_prints_on_both_arms` | yes |
| AC-5 | test-case | `crates/teton/src/client.rs::tests::phases_follow_events_through_the_real_dispatch` (rebuilt at verify: each envelope is now `serde_json::to_value(EventEnvelope::new(seq, session, Event::X(typed payload)))` — the daemon's own construction and serialization — where it had been a `json!` literal, which is the LESSON-544 shape the row this line sits on was supposed to rule out) | no |
| AC-5 | test-case | `crates/teton/src/client.rs::tests::the_rows_close_out_straddles_the_ends_turn_branch` (added at verify: ADR-621-4's two-part close-out, pinned by source region) | no |

## Verify-pass corrections (2026-09-10)

Five findings landed against this task's code, each with a test and a recorded
mutation:

1. **BR-9's second clause was not implemented.** `ECHO` stays on for the length
   of a turn, so a line the user *submits* mid-turn moves the cursor a row down
   and the next `repaint_row_above(1)` overwrites the line holding their echoed
   characters. The pump now abandons the row — `RowState::abandon`, no withdraw
   — the moment `prompt::stdin_ready(ZERO)` reports a line waiting and
   `typed_input` says stdin is a terminal. ADR-621-3's typing bullet and BR-5
   and BR-9 in the spec are amended to the real behaviour.
2. **BR-13 was implemented as zero.** `Surface::repaint_row_above` and
   `withdraw_row_above` now return whether their bytes were written and
   flushed; the pump answers `false` by hiding the row for the rest of the turn
   and, under `/verbose`, printing one `LineKind::Info` line.
3. **AC-5 was overclaimed.** See the table above.
4. **ADR-621-4 had drifted** from the code (the withdraw is hoisted above the
   `ENDS_TURN` branch, and the ADR said it sat on it). The ADR is amended and
   the two-part close-out is region-checked.
5. **The width was read once per call**, so a turn that outlived a resize fitted
   every later row to a window that no longer existed. It is re-read when a row
   is *drawn* and not on a repaint (`RowState::width` says why those differ).

## Technical Notes

- `recv_timeout` maps `RecvTimeoutError::Disconnected` to the same "connection to the daemon closed" error `recv` produces, so the transport path is unchanged.
- Keep the withdraw/redraw pair adjacent to `dispatch_event` inside the pump, not inside `dispatch_event` — `drain_events` (idle path) must not draw; the activity is `Idle` there anyway, but the ownership rule is the pump loop's (ADR-621-3).
- `repaint_row_above(1, ..)` measures from the cursor row; after `line()` draws the row the cursor is on the next line, so the row is one up. After a durable `line` prints, the row is redrawn with `line`, not repaint.
- BR-9: check for a queued message with `try_recv` before entering `recv_timeout`? Not needed — `recv_timeout` returns immediately when a message is queued. Assert it in the latency test with a pre-filled channel.
- Move `FRAME_INTERVAL` without changing its value; `main.rs` has a doc comment and a test region that references it — update the region, keep the arithmetic.
