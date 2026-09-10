---
id: TASK-413
title: "PTY legs for the silent lead-in, the running tool, the stall, every exit, and typing; pipe fixtures for byte identity and the verbose line"
status: complete
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

- [x] AC-1 leg: within one `FRAME_INTERVAL` of Enter the transcript carries the `preparing turn` row; then `waiting on local` naming the scripted model; at least two distinct elapsed values before the first reply byte; no `Activity`-styled bytes after streaming starts until the turn ends
- [x] AC-2 leg: `shell: sleep 3 [running]` then the row `running shell: sleep 3 · <n>s`, elapsed advancing, cost so far equal to the scripted engine's recorded `usd_micros` for the first call; then `[done]` and a `waiting on` row
- [x] AC-6 leg A: `@delay-ms 16500` before the first byte → the row shows `no word from the daemon for 1` (or greater) while still saying `waiting on`; spinner glyph constant across two frames; the annotation is gone once text streams. Leg B: `sleep 16` as a tool → no annotation at any point
- [x] AC-7: after each exit the final transcript, with cursor sequences applied by the test's own minimal interpreter (cursor-up + clear-line), contains no `Activity` text and is byte-equal to the same script run on the pre-feature expectation minus nothing
- [x] AC-10: bytes typed during the AC-1 delay arrive intact as the next prompt's text (assert the echo of the next turn's `>` line)
- [x] AC-3: piped stdout for both scripts contains no `\x1b[` sequences introduced by this REQ and equals the fixture recorded from `origin/main`'s binary for the same script
- [x] AC-13: `--verbose` piped run ends with one `turn <t>s: model <m>s, tools <k>s, cost $x` line; non-verbose has none
- [x] `cargo test -p teton --test pty_e2e --test cli_e2e` green after `cargo build -p tetond -p teton`

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/tests/pty_e2e.rs::the_row_appears_before_the_first_byte_and_withdraws_when_text_streams` | yes |
| BR-1 | test-case | `crates/teton/tests/pty_e2e.rs::the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer` | yes |
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
| AC-7 | test-case | `crates/teton/tests/common/mod.rs::tests` — the cursor interpreter's own literal oracles (withdraw, repaint save/restore, the three erase modes, cursor-up saturation, a plain transcript) | no |
| AC-10 | test-case | `crates/teton/tests/pty_e2e.rs::typed_bytes_survive_the_animation` | no |
| AC-11 | structural-check | `crates/teton/tests/common/mod.rs`: `daemon_bin()` freshness guard on every leg above | no |
| AC-11 | test-case | `crates/teton/tests/pty_e2e.rs::the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer` — the last TTY claim in the list that had only a renderer-unit stand-in (BUG-191) | yes |
| AC-13 | test-case | `crates/teton/tests/cli_e2e.rs::a_verbose_turn_ends_with_one_summary_line` | yes |
| AC-13 | test-case | `crates/teton/tests/cli_e2e.rs::a_verbose_failed_turn_still_ends_with_the_summary_line` — the error arm, over a pipe | yes |

## Technical Notes

- The `shell` tool needs a permission level that allows it without a prompt in the pty legs — reuse whatever `permission_levels_change_what_a_session_asks_about` sets (`[permissions] default_level = "full"` or `/permissions full` first).
- Cost so far in AC-2: the scripted engine records a cost row per call; read the expected micros from the daemon's own `cost/query` after the run rather than hardcoding a price (LESSON-544: the producer is the oracle).
- The residue check for AC-7 needs a tiny cursor interpreter over the transcript (handle `\x1b[s`, `\x1b[u`, `\x1b[{n}A`, `\r`, `\x1b[K`); keep it in `tests/common`.
- Timing legs assert on state reached with `wait_until`, and on ordering (row before first byte) with positional checks — never on a fixed sleep (LESSON-450).
- Rebuild both binaries before running (`cargo build -p tetond -p teton`); a stale-binary refusal reads as a red.

## Implementation notes (2026-09-10)

Green: `cargo test -p teton --test pty_e2e --test cli_e2e` — 28 pty (was 23),
85 pipe (was 83). No existing fixture was edited: the verbose fixtures already
pass with BR-16's line on them (`slash_verbose_toggles_the_route_notice_around_
real_turns` counts `turn ended`, which the summary rides rather than replaces),
and every non-verbose fixture is untouched, which is how AC-3's byte-identity
clause is asserted.

`crates/teton/tests/common/mod.rs` gained `ACTIVITY_GLYPHS` (shared by both
suites, for opposite claims) and `rendered_screen` — the minimal cursor
interpreter AC-7 needs. A withdrawn row is still in the transcript forever, so
the residue claim is made on the replayed *screen*.

**Mutation, applied and observed red:** the pump's `Wake::Tick` arm advances
`row.tick` without calling `paint_row` (one line in `client.rs`). All five new
pty legs redden; none of `cli_e2e`'s 85 does, because the piped pump never
reaches that arm — BR-6 arriving as evidence. Reverted by the inverse edit and
recorded on `the_row_appears_before_the_first_byte_and_withdraws_when_text_streams`.

### Three clauses the legs deliberately do not carry, and where they are instead

- **AC-1's `preparing turn` frame.** The daemon publishes its first
  `route_decided` (the classifier's reflex route) inside one `FRAME_INTERVAL` of
  the prompt going on the wire, so the row's first paint is already an
  `awaiting_model` sentence. Nothing in the fixture can hold the daemon quiet for
  that 120 ms — the only seam that could is `Connection::recv_timeout`'s own,
  which is unit-test-only. The frame is pinned by
  `client.rs::the_pump_ticks_while_the_daemon_is_silent` (literal
  `⠋ preparing turn · 0s · turn 0s`) and by `activity.rs`'s table. What the leg
  asserts instead is AC-1's property: a row before the first reply byte, naming
  a phase and model the daemon reported, with its counter advancing.
- **AC-2's non-zero cost so far.** No scripted tier can be priced: the local
  model is absent from the bundled price table by design (REQ-564 BR-9), so this
  fixture's `cost_recorded` rows carry `usd_micros = 0` and BR-3 correctly shows
  nothing. The leg reads the daemon's own `teton cost` total and branches on it,
  so a build that started pricing local calls turns it red rather than leaving
  it vacuous (LESSON-544). The non-zero rendering is `activity.rs`'s table's.
- **AC-6's mid-stream stall leg (OQ-2).** `@delay-ms` holds a block before its
  first token, not between two of them, so the pty suite cannot stop a stream
  mid-flight. `activity.rs` covers `Streaming` past the bound.

### One finding

A line **submitted** (return included) while the row is animating leaves the
last frame on screen. The echoed newline scrolls the frame, and the pump
measures its row with `\x1b[1A` from wherever the cursor now is; a client not in
raw mode cannot see an echo happen, so the repaints and the final withdraw land
one row low. Delivery is unaffected — which is the property BR-9 states and
ADR-621-3 explicitly scopes AC-10 to ("echoed characters are visually displaced
while the row animates") — so it is a cosmetic residue in a case BR-5's
byte-identical scrollback does not reach. Both halves are in
`typed_bytes_survive_the_animation`, with the second asserting delivery only and
the doc comment saying why. Worth a follow-up if BR-5 is meant to hold while the
user types over the row; closing it needs the client to track the cursor itself.

**Closed at verify (2026-09-10).** "Visually displaced" understated it: the next
repaint after the echoed newline would have *overwritten* the line holding the
characters the user just typed, and the closing withdraw would have erased it.
The pump now abandons the row the moment a submitted line is waiting on stdin,
and AC-10's half 2 asserts what that produces. See the verify-fix pass below.

## Implementation notes — verify-fix pass (2026-09-10)

Green: `cargo test -p teton --test pty_e2e --test cli_e2e` — **35 pty** (was 28)
and **92 pipe** (was 85). The six shared interpreter tests are compiled into
both binaries, which is +6 in each count; the rest is +1 pty leg and +1 pipe leg.
`cargo test -p tetond delay_directive` green; `cargo clippy -- -D warnings` and
`rustfmt` clean over the four files this pass owns.

Seven findings from the Phase-5 test-half review, each with the mutation that
was **applied, run and observed failing** recorded on the test itself:

1. **MAJOR — the AC-7 cursor interpreter had no tests.** `rendered_screen` is
   the only thing that tells "the row was withdrawn" from "the row is still on
   screen", and its users exercise it in the one direction that cannot notice a
   bug: an interpreter that erased too much reports "no residue" forever. Six
   literal-oracle cases now sit beside it in `tests/common/mod.rs`. *Mutation:*
   `erase_line`'s default arm handled as mode 2 (`row.clear()`) → red at
   `left: []` against `right: ["abc"]`, the other five green.
2. **MAJOR — no pty leg for the permission step-aside (BR-1, AC-11).**
   `the_row_steps_aside_for_a_permission_prompt_and_returns_after_the_answer`.
   The daemon publishes `tool_call` **before** the permission gate, so the order
   on screen is the `[running]` line, a `running shell: sleep 2` row, the
   withdraw, then the question — the leg asserts that order as the producer's
   own and opens its negative window at the question's first byte. *Mutation:*
   the window opened at the `[running]` line instead → red on the one row it
   then swallows.
3. **MINOR — AC-1's and AC-2's `distinct_clocks(...).len() >= 2` were evaluated
   once, after the turn's own marker** (AC-2's flaked). Both are now
   `wait_until` polls over the same windows, run *before* the terminal marker is
   awaited, so the polled condition is the asserted condition (LESSON-450).
4. **MINOR — `after_done.iter().all(...)` in AC-2 was fragile and vacuous.** Any
   notice-shaped event in the post-tool window changes the sentence of the
   frames after it, and `all` is true of an empty window — the state that
   matters. Now the emptiness check first, then `any`.
5. **MINOR — AC-13 scripted only a successful turn.**
   `a_verbose_failed_turn_still_ends_with_the_summary_line` drives the RPC-error
   provocation over a pipe (`unreachable_edit_category`: a `[[categories]]` row,
   because the fixture config's tier bindings cannot be duplicated) and asserts
   exactly one summary line, the failed arm's composition, and nothing at all
   outside `--verbose`. *Mutation:* `--verbose` dropped → red at `left: 0`
   against `right: 1`.
6. **AC-10's prose and half 2.** Prose corrected (see the finding above). Half 1
   keeps its claims and gains one: the repaints **continue** while characters
   are typed, because canonical mode makes nothing readable until Enter. Half 2
   asserts delivery, **zero** `\x1b[s\x1b[1A` past the echoed newline, and the
   echo intact on the replayed screen; `assert_no_row_on_screen` is deliberately
   not applied to it, because the abandoned frame is the recorded exception.
   *Mutation:* half 2's counter read the window before the newline → red at
   `left: 2` against `right: 0`, which puts the stop at the newline to the byte.
7. **MINOR (security) — `delay_directive` parsed an unbounded `u64`.** A
   half-billion-year hold on a turn thread, reachable by a fixture typo, from a
   script file the environment hands in. Capped at `MAX_SCRIPT_DELAY_MS`
   (60 s — above the longest leg's 16.5 s, below the pty suite's per-wait
   window); above it the directive is **malformed** and streams verbatim rather
   than being clamped. `the_delay_directive_is_honoured_only_under_the_seam`
   gained leg 4. *Mutation:* the guard deleted → red at
   `left: Some((60.001s, "hello"))` against `right: None`.
