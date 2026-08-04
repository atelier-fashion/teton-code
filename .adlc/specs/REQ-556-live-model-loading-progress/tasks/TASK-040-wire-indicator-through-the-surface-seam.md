---
id: TASK-040
title: "Wire the indicator into the entry loop through the Surface seam, TTY-gated"
status: draft
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: [TASK-038, TASK-039]
---

## Description

Join the two halves: the unified loop (TASK-038) ticks the state machine
(TASK-039) on each `recv_timeout` expiry and paints the result on a dedicated
row above the entry frame (ADR-556-4). All output goes through `Surface` (BR-3),
and the non-TTY implementation is a no-op — the second mechanical guarantee
behind BR-2.

## Files to Create/Modify

- `crates/teton/src/render.rs` — add an in-place repaint capability to the `Surface` trait; ANSI implementation in `PlainSurface` (save cursor → move → `\r\x1b[K` → redraw → restore), no-op default so non-TTY surfaces inherit silence; `RecordingSurface` records the calls
- `crates/teton/src/main.rs` — on tick, advance the indicator and repaint; on a rendered lifecycle event, repaint; clear the indicator when `frame` yields `None`

## Acceptance Criteria

- [ ] With the daemon mid-load, an interactive session shows the indicator
      advancing at a steady interval with no input typed (AC-1, verified at unit
      level here via `RecordingSurface`; the pty leg is TASK-041).
- [ ] When a terminal stage arrives, the indicator is cleared and the existing
      `render_lifecycle` line renders — one line, not two (AC-2, BR-10).
- [ ] A partially typed line is never disturbed by a repaint: the repaint saves
      and restores the cursor, and the indicator's row is above the entry frame
      (AC-5, ADR-556-4).
- [ ] Non-TTY: the repaint capability is a no-op and no indicator bytes are
      emitted. Assert against a `RecordingSurface` configured as non-TTY, and
      confirm `cli_e2e`'s byte-equality tests still pass unmodified (AC-4, BR-2).
- [ ] A repaint failure (write error, unusable terminal) does not abort the
      session: the indicator degrades to the existing static notice and the
      session stays usable, with the degradation visible rather than silent
      (BR-9, LESSON-447).
- [ ] No direct `print!`/`println!`/`write!(stdout)` anywhere in the new code —
      every byte goes through `Surface` (BR-3). Grep-assert this in review.

## Technical Notes

- `Surface` is at `crates/teton/src/render.rs:41` with `line`, `fragment`,
  `flush`. `PlainSurface` already tracks `at_line_start` (`render.rs:65`) — the
  repaint must keep that bookkeeping honest or the next `line()` will collide
  with streamed output.
- `LineKind::Notice` renders with the `>> ` prefix (`render.rs:79`). Decide
  deliberately whether the indicator carries that prefix; whichever way, the
  cleared/finished line must match what `render_lifecycle` emits so the
  transition does not visibly jump.
- The entry frame is drawn by `FramedStdinPrompter` (`crates/teton/src/prompt.rs:44`),
  which writes three rows and moves the cursor up two. The indicator's row sits
  above that block.
- Terminal width comes from the existing `terminal_width()` helper in
  `prompt.rs` — reuse it, do not add a second probe (LESSON-433: it is
  platform-specific and CI must exercise both).
- Keep the repaint additive to the trait with a defaulted no-op method so every
  existing `Surface` implementor keeps compiling and silently does nothing —
  that default *is* the BR-2 guarantee for any future surface.
