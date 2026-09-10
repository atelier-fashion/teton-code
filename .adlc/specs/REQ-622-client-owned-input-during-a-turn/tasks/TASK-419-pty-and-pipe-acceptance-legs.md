---
id: TASK-419
title: "PTY legs for type-ahead, questions, every restore path, multi-byte and unhandled keys; pipe fixtures; the retired REQ-621 legs"
status: draft
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

- [ ] Every leg polls for state (`wait_for`/`wait_until`), never sleeps; every TTY claim has a pty leg (AC-15)
- [ ] AC-4/AC-5 compare `icanon`/`echo` from `stty -a` after exit against a capture taken before the session started
- [ ] Mutation: the handler's restore call removed → `ctrl_c_restores_the_terminal` red (rebuild first); recorded in TASK-416's handler test doc and here
- [ ] `cargo build -p tetond -p teton` then `cargo test -p teton --test pty_e2e --test cli_e2e` green, twice in a row

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_stdin_session_never_enters_raw_mode` | yes |
| BR-3 | test-case | `crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` | no |
| BR-4 | test-case | `crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt` | no |
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
