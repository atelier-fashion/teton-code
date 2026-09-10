---
id: TASK-418
title: "Questions read through the editor in raw mode, and queued lines re-enter at the entry poll"
status: draft
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

- [ ] In raw mode `ask` returns the line typed after the question, never bytes pushed before it (unit with a scripted byte source that pre-loads type-ahead)
- [ ] Off raw mode `ask` is byte-identical to today (existing tests untouched)
- [ ] `next_interactive_line` yields queued lines in order, one per call, each drawn once in the frame, then falls through to the poll
- [ ] A queued `/cost` line reaches `slash::classify` as a command (test through the existing classify path)
- [ ] Mutation "queued drain after the poll instead of before" observed red; recorded
- [ ] clippy and fmt clean

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
