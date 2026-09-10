---
id: TASK-415
title: "The pure input editor: keystroke decoding, the pending row, shelve/unshelve, and the queue"
status: draft
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: []
repo: teton-code
---

## Description

Create `crates/teton/src/input_editor.rs` (ADR-622-2): `InputEditor` with `pending`,
`queued`, `shelved`, and a partial-UTF-8 accumulator; `push(&[u8]) -> Vec<Edit>`
decoding printable UTF-8, Backspace (`0x7f`/`0x08`), Enter (`\n`/`\r`), `ESC [` /
`ESC O` sequences dropped as a unit, every other control byte dropped (Ctrl-D
included); `row(width) -> Option<String>` (tail that fits, fixed `> ` marker, fitted
on the defused string like `activity::fit`); `shelve` / `unshelve`; `take_next_queued`;
`queued_len`. Wire `SessionState::input`. No I/O, no terminal.

## Files to Create/Modify

- `crates/teton/src/input_editor.rs` — new module, tests, mutation record
- `crates/teton/src/main.rs` — `mod input_editor;`
- `crates/teton/src/session_ui.rs` — `SessionState::input: InputEditor` (not cleared by `begin_turn`)

## Acceptance Criteria

- [ ] A byte table maps sequences to literal `(pending, queued, echo-row)` triples: ASCII, `é` split across two pushes, CJK, an emoji, Backspace over each, Enter, `\r\n` as one Enter, three lines in one push → three queued, `ESC [ A`, `ESC O P`, `0x04`, `0x03` dropped
- [ ] `shelve` hides `pending`; `push` while shelved edits a fresh buffer the caller reads; `unshelve` restores the original verbatim
- [ ] `row(width)` never exceeds `width-1` columns on the defused string; CJK tail case
- [ ] Mutation "Backspace pops a byte, not a char" applied, observed red on the `é`/CJK rows, reverted, recorded
- [ ] clippy `-D warnings`, fmt clean, no `#[allow]`

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/teton/src/input_editor.rs::tests::the_keystroke_table` | no |
| BR-5 | test-case | `crates/teton/src/input_editor.rs::tests::a_shelved_line_is_untouched_by_an_answer` | yes |
| BR-6 | test-case | `crates/teton/src/input_editor.rs::tests::enter_queues_and_a_pasted_block_queues_one_per_line` | no |
| BR-9 | test-case | `crates/teton/src/input_editor.rs::tests::unhandled_keys_change_nothing` | yes |
| BR-10 | test-case | `crates/teton/src/input_editor.rs::tests::the_keystroke_table` | no |
| BR-15 | test-case | `crates/teton/src/input_editor.rs::tests::unhandled_keys_change_nothing` | yes |
| AC-10 | test-case | `crates/teton/src/input_editor.rs::tests::the_keystroke_table` | no |

## Technical Notes

- Mirror `loading.rs`/`activity.rs` module docs: the two load-bearing properties and the "what breaks which test" table.
- Reuse `render::defused` and `markdown::display_width` for the row; do not add a third width helper.
- `Edit` variants: `Pending` (row changed), `Queued(usize)` (count), `Nothing`; the pump uses them to decide what to repaint.
