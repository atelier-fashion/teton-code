---
id: TASK-419
title: "PTY legs for type-ahead, questions, every restore path, multi-byte and unhandled keys; pipe fixtures; the retired REQ-621 legs"
status: complete
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-417", "TASK-418"]
repo: teton-code
---

## Description

The acceptance suite against the real daemon and the real pump (AC-1..9, 12..15).
Harness additions in `tests/common`: send a signal to the child (`libc::kill` on the
pty child's pid), read the pty's terminal settings back by spawning `stty -a` on the
same pty after the client exits and parsing `icanon`/`echo`, write control bytes and
escape sequences. Rewrite `typed_bytes_survive_the_animation` and
`the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer` to the new
behaviour; retire the abandon unit test to the `Failed` fixture (TASK-417).

## Files to Create/Modify

- `crates/teton/tests/pty_e2e.rs` — `a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` (AC-1), `queued_lines_become_the_next_prompts_in_order` (AC-2, incl. `/cost`), `a_question_never_eats_type_ahead` (AC-3), `ctrl_c_restores_the_terminal` (AC-4), `every_exit_restores_the_terminal` (AC-5: normal, RPC error, daemon killed, SIGTERM, panic seam), `the_key_prompt_survives_ctrl_c_with_echo_on` (AC-6), `multi_byte_input_round_trips` (AC-8), `unhandled_keys_are_inert` (AC-9), `a_queued_line_is_announced_on_the_row` (AC-12), `ctrl_d_mid_turn_is_inert` (AC-13), `a_pasted_block_queues_one_prompt_per_line` (AC-14)
- `crates/teton/tests/cli_e2e.rs` — `a_piped_stdin_session_never_enters_raw_mode` (AC-7) plus every existing fixture unchanged
- `crates/teton/tests/common/mod.rs` — `send_signal`, `read_back_termios`, control-byte helpers, tests for the `stty` parser

## Acceptance Criteria

- [x] Every leg polls for state (`wait_for`/`wait_until`), never sleeps; every TTY claim has a pty leg (AC-15)
- [x] AC-4/AC-5 compare `icanon`/`echo` from `stty -a` after exit against a capture taken before the session started
- [x] Mutation: the handler's restore call removed → `ctrl_c_restores_the_terminal` red (rebuild first); recorded in TASK-416's handler test doc and here
- [x] `cargo build -p tetond -p teton` then `cargo test -p teton --test pty_e2e --test cli_e2e` green, twice in a row

## Mutation record

Applied to `crates/teton/src/prompt.rs`, rebuilt, run, and reverted by the same
targeted edit:

| Mutation | Fails |
|---|---|
| the `tcsetattr` call removed from `restore_and_reraise` | **4 red of 999** (2026-09-10): `pty_e2e::ctrl_c_restores_the_terminal`, `pty_e2e::every_exit_restores_the_terminal` (its SIGTERM leg), `pty_e2e::the_key_prompt_survives_ctrl_c_with_echo_on`, and the unit `prompt::tests::the_handler_re_raises_after_restoring`. `cli_e2e` stays green, all 97 |

Only the three legs that end a session with a **signal** redden, which is right:
the guard's `Drop` covers every other exit, so AC-5's normal, RPC-error,
disconnect and panic legs stay green under this mutation.

**The first run of this mutation stayed green, and that is the finding.** The
restore legs spawned the client as the pty's own child, so it was the session
leader — and a BSD kernel revokes a controlling terminal whose session leader
exits, handing the device back with the driver's defaults. The readback after
exit therefore reported `icanon echo` whatever the client had done: an assertion
that could not fail (LESSON-569). The fix is `pty_e2e`'s `Launch::UnderAShell`,
which runs the client inside a shell that holds the pty's session open — the
same shape a user's real terminal has, where the shell owns the session and
`teton` is a child inside it. The counts above are what the mutation says after
that fix.

## The two REQ-621 legs: one folded and deleted, one kept and extended

`pty_e2e::typed_bytes_survive_the_animation` is **deleted**. Its subject was
REQ-621's recorded exception — the abandoned row a submitted line left behind —
and its half 2 asserted that *no repaint arrives past a submitted line*, which
is now the wrong claim: the kernel no longer moves the cursor, so the row must
go on animating. Both halves are folded into
`a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt`, which makes
the opposite claim over the same script and adds what the old leg could not ask
for. **REQ-621's verification table still names it**, so the AC-16 close-out
task has to mark that row retired.

`pty_e2e::the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer`
is **kept and extended**: its four REQ-621 claims all still hold, and two
REQ-622 claims are added on the same fixture — the terminal is still raw while
the question stands (the question reuses the turn's window rather than restoring
to ask, BR-2), and the answer appears once on the question's own row because the
client repainted it there with `ECHO` off.

## Defect found here, fixed, and covered by a leg of its own

A reply **streamed while an unsubmitted pending line is on screen** was broken
one chunk per row in the durable scrollback (`"One" / "two" / "three"` instead of
`"One two three"`); with no pending line the same reply rendered as one row. Each
streamed message withdraws the block and redraws it, and `withdraw_row_above`
flushed what the renderer was holding before it moved the cursor — so a fragment
the stream had not finished was ended as a finished row at every token. That is a
BR-4 violation in TASK-417's two-row block rather than in this task's legs; AC-5's
ordinary-exit leg was written to read the terminal rather than the screen so that
it does not depend on it.

It is **fixed** (commit `83235fd`): the block's rows are drawn with
`Surface::draw_row` and repainted and withdrawn without emitting held text, so a
streamed line stays held across the block's verbs and goes out where the block
was, on the next durable write or at the turn's `end_block`. The leg that pins it
is `pty_e2e::a_reply_streamed_past_a_pending_row_renders_as_it_does_without_one`,
which runs the same reply twice — once with a pending row up and once without —
and asserts the two scrollbacks are the same shape; it is the BR-4 row added to
the table below, beside the submitted-line leg that was already there.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_stdin_session_never_enters_raw_mode` | yes |
| BR-3 | test-case | `crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` | no |
| BR-4 | test-case | `crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` | no |
| BR-4 | test-case | `crates/teton/tests/pty_e2e.rs::a_reply_streamed_past_a_pending_row_renders_as_it_does_without_one` | no |
| BR-5 | test-case | `crates/teton/tests/pty_e2e.rs::a_question_never_eats_type_ahead` | yes |
| BR-6 | test-case | `crates/teton/tests/pty_e2e.rs::queued_lines_become_the_next_prompts_in_order` | no |
| BR-7 | test-case | `crates/teton/tests/pty_e2e.rs::every_exit_restores_the_terminal` | no |
| BR-8 | test-case | `crates/teton/tests/pty_e2e.rs::ctrl_c_restores_the_terminal` | no |
| BR-9 | test-case | `crates/teton/tests/pty_e2e.rs::unhandled_keys_are_inert` | yes |
| BR-14 | test-case | `crates/teton/tests/pty_e2e.rs::a_queued_line_is_announced_on_the_row` | no |
| BR-15 | test-case | `crates/teton/tests/pty_e2e.rs::ctrl_d_mid_turn_is_inert` | yes |
| AC-1 | test-case | `crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` | no |
| AC-2 | test-case | `crates/teton/tests/pty_e2e.rs::queued_lines_become_the_next_prompts_in_order` | no |
| AC-3 | test-case | `crates/teton/tests/pty_e2e.rs::a_question_never_eats_type_ahead` | yes |
| AC-4 | test-case | `crates/teton/tests/pty_e2e.rs::ctrl_c_restores_the_terminal` | no |
| AC-5 | test-case | `crates/teton/tests/pty_e2e.rs::every_exit_restores_the_terminal` | no |
| AC-6 | test-case | `crates/teton/tests/pty_e2e.rs::the_key_prompt_survives_ctrl_c_with_echo_on` | no |
| AC-7 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_stdin_session_never_enters_raw_mode` | yes |
| AC-8 | test-case | `crates/teton/tests/pty_e2e.rs::multi_byte_input_round_trips` | no |
| AC-9 | test-case | `crates/teton/tests/pty_e2e.rs::unhandled_keys_are_inert` | yes |
| AC-12 | test-case | `crates/teton/tests/pty_e2e.rs::a_queued_line_is_announced_on_the_row` | no |
| AC-13 | test-case | `crates/teton/tests/pty_e2e.rs::ctrl_d_mid_turn_is_inert` | yes |
| AC-14 | test-case | `crates/teton/tests/pty_e2e.rs::a_pasted_block_queues_one_prompt_per_line` | no |
| AC-15 | structural-check | `crates/teton/tests/common/mod.rs`: `daemon_bin()` freshness guard on every leg | no |

## Technical Notes

- `stty -a` on the pty after the client exits: spawn `/bin/stty -a` with the pty as its controlling terminal (same `CommandBuilder` path); parse `-icanon`/`icanon`, `-echo`/`echo`. Capture the baseline the same way before spawning teton.
- The `guarded` permission config makes `shell` ask (AC-3); `full` allows it (AC-1/2).
- Delivery of a queued line: assert the daemon received it via the reply the scripted engine returns for that block.
- Rebuild both binaries before every run; the freshness guard reads a stale binary as red.
