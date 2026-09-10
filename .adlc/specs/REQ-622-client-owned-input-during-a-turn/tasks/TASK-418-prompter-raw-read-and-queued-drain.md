---
id: TASK-418
title: "Questions read through the editor in raw mode, and queued lines re-enter at the entry poll"
status: complete
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-415", "TASK-416"]
repo: teton-code
---

## Description

ADR-622-2's prompter half and ADR-622-5. `FramedStdinPrompter::ask` (and
`StdinPrompter::ask`) branch on `RawMode::is_engaged()`: in raw mode they loop
`read_available` into a fresh `InputEditor` answer buffer, echoing through the surface
seam, until Enter, and return the line — the same reader, a fresh buffer (BR-2, BR-5).
`next_interactive_line` returns `state.input.take_next_queued()` ahead of its poll,
after drawing the entry frame with the line echoed in its input row, so a queued line
takes the typed-line path unchanged (BR-6).

## Files to Create/Modify

- `crates/teton/src/prompt.rs` — raw read path in `ask`/`read_line`; echo of the answer through the prompter's own writer; tests with a scripted byte source
- `crates/teton/src/main.rs` — queued drain in `next_interactive_line`; frame echo of a queued line via `FramedStdinPrompter::draw` with prefilled input; tests

## Acceptance Criteria

- [x] In raw mode `ask` returns the line typed after the question, never bytes pushed before it (unit with a scripted byte source that pre-loads type-ahead)
- [x] Off raw mode `ask` is byte-identical to today (existing tests untouched)
- [x] `next_interactive_line` yields queued lines in order, one per call, each drawn once in the frame, then falls through to the poll
- [x] A queued `/cost` line reaches `slash::classify` as a command (test through the existing classify path)
- [x] Mutation "queued drain after the poll instead of before" observed red; recorded
- [x] clippy and fmt clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton/src/prompt.rs::tests::a_raw_mode_answer_is_read_through_the_editor` | no |
| BR-5 | test-case | `crates/teton/src/prompt.rs::tests::type_ahead_before_a_question_is_not_its_answer` | yes |
| BR-6 | test-case | `crates/teton/src/main.rs::tests::queued_lines_re_enter_ahead_of_the_poll_in_order` | yes |
| BR-12 | structural-check | `crates/teton-protocol/src/lib.rs::tests::this_build_advertises_only_the_version_its_types_can_read` (unchanged, asserted green) | no |

## Technical Notes

- The answer echo is the prompter's, not the pump's: the pump's rows are withdrawn before the question (TASK-417), so the prompter owns the terminal until it returns.
- `draw` with a prefilled input row: extend `draw_bytes(question)` with an optional prefilled line so the geometry stays assertable without a terminal (REQ-560 BR-11).
- Do not send anything while `RawMode::is_engaged()`; the drain runs between turns only.

## Implementation Record (2026-09-10)

**`prompt.rs`.** Both prompters gained an `answer: InputEditor` field of their own,
and both `ask`s branch on `RawMode::is_engaged()` into `read_answer_raw` — one
free function with `keys`, `wait` and `out` handed in, so the BR-5 behaviour is
driven with no terminal, no keyboard and no clock. It resets and `shelve`s the
buffer at the top of every read, which is what makes "only keystrokes typed after
the question was drawn" true by construction and what makes Enter *hold* the line
for `take_answer` instead of queuing it. It feeds the editor a byte at a time and
stops at the first Enter, so a pasted block answers one question and the rest of
that read is dropped. The echo is `\r\x1b[K` plus the question plus
`InputEditor::answer_so_far()` — composed from the editor's text, never from the
bytes the reader saw, so the decoding has one implementation. Off raw mode both
`ask`s are byte-identical: the same `defused(question)` write and the same
`read_line`.

`draw_bytes` took a `prefill` parameter (one function, not two) and `draw_submitted`
/ `submitted_bytes` compose the frame a **queued** line is shown in, advancing the
cursor with `advance_bytes(true)` — nothing was typed, so the terminal echoed no
newline, exactly as at EOF. `advance_bytes`' parameter was renamed
`no_echoed_newline` for that reason. The defuse-then-tint transform moved into
`styled`, now shared by the frame and the answer row.

**`main.rs`.** `queued_for_entry(state, raw_engaged)` is the drain, called from
`next_interactive_line` **above** its poll with `prompt::RawMode::is_engaged()`;
it returns `None` mid-turn and leaves the line in the queue. One line per call,
drawn once through `draw_submitted`, then returned exactly as a typed line — so
`slash::classify`, the REQ-615 `cd` intercept, skill dispatch and every pre-send
check are met with no second code path.

**Deviations.**

- `input_editor.rs` gained one method, `answer_so_far() -> &str`, outside this
  task's file list. It is the read-only half of `take_answer` and the prompter's
  echo source: without it the writer would have had to re-decode the bytes it
  pushed, which is the second decoder BR-2 exists to forbid.
- `FRAME_INTERVAL` is imported from `client.rs` (`pub(crate)` already, and
  `main.rs` imports it the same way) rather than duplicated with a comment. One
  figure, no drift, and no edit to a file TASK-417 owned.
- The "forced raw-engaged flag" was not needed: `RawMode::arm(current_termios())`
  already arms the slot deterministically with no terminal
  (`both_guards_arm_and_clear_the_restore_slot`), and the wiring from each `ask`
  into `read_answer_raw` is pinned by a region check over this file's production
  half instead — a unit cannot drive `ask`'s other branch without reading the
  real descriptor 0.
- The `cargo test -p teton --bin teton main` gate names no tests: `main.rs`'s
  module is `tests::`, not `main::`. The whole bin suite was run instead —
  **853 passed, 0 failed**.

**Mutation (BR-6, ADR-622-5), applied and observed red.** Moved the drain from
above `entry.draw(entry_prompt)` into the loop below the `stdin_ready` arm.
**1 red of 853** — `queued_lines_re_enter_ahead_of_the_poll_in_order` and nothing
else. It fails on the first assertion: the poll reaches `read_line` on a stdin at
EOF, so the queued line comes back `None` rather than `cargo test`. Its
source-order assertion (`drain < poll`) is red under the same mutant and is the
half that fires whatever descriptor 0 happens to be — the behavioural half would
pass, one frame late, on a machine where `cargo test` inherits a terminal.
Reverted with the same edit; 853 green again.

**Gates.** `cargo test -p teton --bin teton prompt` 49 passed; the two new
`main.rs` tests pass by exact name; the full bin suite 853 passed;
`cargo fmt --all -- --check` clean; `cargo clippy -p teton --all-targets -- -D
warnings` clean with **no dead-code notices remaining** — this wiring retired
`read_available`, `RawMode::is_engaged`, `SLOT_RAW`, `take_answer` and
`take_next_queued`, and TASK-417 retired the pump-facing `engage`, `arm`,
`raw_from`, `classify_raw`, `RawOutcome` and `RawVerdict`. `teton-protocol`'s
`this_build_advertises_only_the_version_its_types_can_read` asserted green,
unchanged (BR-12).
