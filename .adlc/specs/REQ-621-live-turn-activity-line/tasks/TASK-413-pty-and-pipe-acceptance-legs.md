---
id: TASK-413
title: "PTY legs for the silent lead-in, the running tool, the stall, every exit, and typing; pipe fixtures for byte identity and the verbose line"
status: draft
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-411", "TASK-412"]
repo: teton-code
---

## Description

The acceptance suite (AC-1, AC-2, AC-3, AC-6, AC-7, AC-10, AC-13) against the real
daemon and the real pump. Every pty leg runs under `common::daemon_bin()`'s
freshness guard and polls for state (`wait_for` / `wait_until`), never sleeps.

## Files to Create/Modify

- `crates/teton/tests/pty_e2e.rs` — `the_row_appears_before_the_first_byte_and_withdraws_when_text_streams` (AC-1); `a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line` (AC-2); `a_silent_daemon_earns_the_stall_annotation_and_a_long_tool_does_not` (AC-6, both legs); `every_exit_erases_the_row` (AC-7: normal, `METHOD_NOT_FOUND`-shaped RPC error via a scripted refusal, daemon killed mid-delay); `typed_bytes_survive_the_animation` (AC-10)
- `crates/teton/tests/cli_e2e.rs` — `a_piped_turn_emits_no_activity_bytes` driving the AC-1 and AC-2 scripts with stdout piped (AC-3); the verbose fixture asserts exactly one BR-16 line and its shape (AC-13); every existing non-verbose fixture unchanged
- `crates/teton/tests/common/mod.rs` — helper to script a block with `@delay-ms` and to kill the daemon child mid-turn if one does not exist

## Acceptance Criteria

- [ ] AC-1 leg: within one `FRAME_INTERVAL` of Enter the transcript carries the `preparing turn` row; then `waiting on local` naming the scripted model; at least two distinct elapsed values before the first reply byte; no `Activity`-styled bytes after streaming starts until the turn ends
- [ ] AC-2 leg: `shell: sleep 3 [running]` then the row `running shell: sleep 3 · <n>s`, elapsed advancing, cost so far equal to the scripted engine's recorded `usd_micros` for the first call; then `[done]` and a `waiting on` row
- [ ] AC-6 leg A: `@delay-ms 16500` before the first byte → the row shows `no word from the daemon for 1` (or greater) while still saying `waiting on`; spinner glyph constant across two frames; the annotation is gone once text streams. Leg B: `sleep 16` as a tool → no annotation at any point
- [ ] AC-7: after each exit the final transcript, with cursor sequences applied by the test's own minimal interpreter (cursor-up + clear-line), contains no `Activity` text and is byte-equal to the same script run on the pre-feature expectation minus nothing
- [ ] AC-10: bytes typed during the AC-1 delay arrive intact as the next prompt's text (assert the echo of the next turn's `>` line)
- [ ] AC-3: piped stdout for both scripts contains no `\x1b[` sequences introduced by this REQ and equals the fixture recorded from `origin/main`'s binary for the same script
- [ ] AC-13: `--verbose` piped run ends with one `turn <t>s: model <m>s, tools <k>s, cost $x` line; non-verbose has none
- [ ] `cargo test -p teton --test pty_e2e --test cli_e2e` green after `cargo build -p tetond -p teton`

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/tests/pty_e2e.rs::the_row_appears_before_the_first_byte_and_withdraws_when_text_streams` | yes |
| BR-3 | test-case | `crates/teton/tests/pty_e2e.rs::a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line` | no |
| BR-5 | test-case | `crates/teton/tests/pty_e2e.rs::every_exit_erases_the_row` | no |
| BR-6 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_turn_emits_no_activity_bytes` | yes |
| BR-9 | test-case | `crates/teton/tests/pty_e2e.rs::typed_bytes_survive_the_animation` | no |
| BR-11 | test-case | `crates/teton/tests/pty_e2e.rs::a_silent_daemon_earns_the_stall_annotation_and_a_long_tool_does_not` | yes |
| BR-12 | test-case | `crates/teton/tests/pty_e2e.rs::every_exit_erases_the_row` | no |
| BR-16 | test-case | `crates/teton/tests/cli_e2e.rs::a_verbose_turn_ends_with_one_summary_line` | yes |
| AC-1 | test-case | `crates/teton/tests/pty_e2e.rs::the_row_appears_before_the_first_byte_and_withdraws_when_text_streams` | yes |
| AC-2 | test-case | `crates/teton/tests/pty_e2e.rs::a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line` | no |
| AC-3 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_turn_emits_no_activity_bytes` | yes |
| AC-5 | test-case | `crates/teton/tests/pty_e2e.rs::a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line` | no |
| AC-6 | test-case | `crates/teton/tests/pty_e2e.rs::a_silent_daemon_earns_the_stall_annotation_and_a_long_tool_does_not` | yes |
| AC-7 | test-case | `crates/teton/tests/pty_e2e.rs::every_exit_erases_the_row` | no |
| AC-10 | test-case | `crates/teton/tests/pty_e2e.rs::typed_bytes_survive_the_animation` | no |
| AC-11 | structural-check | `crates/teton/tests/common/mod.rs`: `daemon_bin()` freshness guard on every leg above | no |
| AC-13 | test-case | `crates/teton/tests/cli_e2e.rs::a_verbose_turn_ends_with_one_summary_line` | yes |

## Technical Notes

- The `shell` tool needs a permission level that allows it without a prompt in the pty legs — reuse whatever `permission_levels_change_what_a_session_asks_about` sets (`[permissions] default_level = "full"` or `/permissions full` first).
- Cost so far in AC-2: the scripted engine records a cost row per call; read the expected micros from the daemon's own `cost/query` after the run rather than hardcoding a price (LESSON-544: the producer is the oracle).
- The residue check for AC-7 needs a tiny cursor interpreter over the transcript (handle `\x1b[s`, `\x1b[u`, `\x1b[{n}A`, `\r`, `\x1b[K`); keep it in `tests/common`.
- Timing legs assert on state reached with `wait_until`, and on ordering (row before first byte) with positional checks — never on a fixed sleep (LESSON-450).
- Rebuild both binaries before running (`cargo build -p tetond -p teton`); a stale-binary refusal reads as a red.
