---
id: TASK-279
title: "Wire the renderer into PlainSurface, opt-in at construction"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-277, TASK-278]
---

## Description

Give `PlainSurface` the pending-line buffer, the table-run buffer, and the inline SGR table —
all reachable only through a new constructor, so every existing caller and test is byte-unchanged.
Covers BR-5, BR-6, and the mid-turn half of BR-8.

## Files to Create/Modify

- `crates/teton/src/render.rs` — `markdown: Option<MarkdownState>` field; `with_markdown(out,
  color, width)` constructor; `fragment()` accumulates and emits through the layout module;
  `line()` and `repaint_row_above()` emit any pending buffer before claiming their row; a fixed
  inline-style SGR table beside `LineKind::sgr()`.
- `crates/teton/src/markdown.rs` — minor additions if the surface needs a state type to hold.

## Acceptance Criteria

- [x] AC-5: a chunk containing `\x1b[2K\x1b[1A` renders as visible spaces, not cursor motion,
      **with the renderer in the path**; mutation-checked by removing the defuse call and watching
      it fail. `**bold**` in the same chunk still emits SGR — proving styling comes from the fixed
      table, not from the input.
- [x] AC-6: fenced code emits verbatim — original line breaks, no wrapping, no SGR — including
      lines longer than the width. Fence markers are not printed.
- [x] AC-8 (unit leg): a surface built with `color = false` at a fixed width wraps its input and
      emits **zero** `\x1b` bytes; the same input at `color = true` emits the SGR AC-5 pins.
- [x] AC-9: a `Notice`/`Tool` line arriving mid-stream emits **after** the pending buffer, not
      through it. Asserted on `RecordingSurface` as an ordered `(kind, text)` sequence.
- [x] AC-14 (surface leg): each Out-of-Scope construct reaches the terminal as literal text.
- [x] **Every existing test in `render.rs` passes unmodified.** If any needs an edit, stop — the
      opt-in constructor is not doing its job.
- [x] **Remove the `#![allow(dead_code)]` at the top of `markdown.rs`.** TASK-277 added it because
      nothing called into the module yet, and named this task as the removal condition. If it
      survives the REQ, either the wiring never landed or a function in there has no caller —
      both are findings, and tetond's ADR-J is against lingering allows. Deleting the attribute
      and getting a clean `cargo clippy --workspace -- -D warnings` is the proof the wiring is real.
- [x] `cargo test -p teton` green; `cargo clippy --workspace -- -D warnings` clean.

## Technical Notes

**The SGR must be authored here, after `defused_multiline`, from a fixed table.** This is
[[LESSON-517]] and it is not negotiable: text handed in with escapes already embedded gets them
replaced by spaces, which is the guard working correctly. `LineKind::sgr()` (render.rs:62) plus its
`debug_assert` at render.rs:281 is the shape to copy — the surface owns the alphabet it destroys.

**Opt-in is what keeps the diff small.** Existing tests build via `PlainSurface::new(&mut buf)` and
`with_color(&mut buf, color)` (render.rs:427–771), including four that pin exact fragment bytes
(`a_streamed_fragment_cannot_redraw_the_prompt_above_it`, `line_bookkeeping_reads_the_defused_text`,
`plain_surface_closes_an_open_fragment_before_a_line`,
`a_repaint_restores_the_cursor_and_leaves_line_bookkeeping_alone`). None opts in, so none changes.

**Do not add an `end_block()` call anywhere in this task.** The verb and its call sites are
TASK-280's, wholly — see ADR-3. This module must never self-flush on a timer or a heuristic.

`at_line_start` must keep reading the *emitted* text, as it does today (render.rs:319), or a
`line()` after a buffered emit will collide with it.
