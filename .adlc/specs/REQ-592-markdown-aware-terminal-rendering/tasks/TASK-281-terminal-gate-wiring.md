---
id: TASK-281
title: "Gate the renderer on a terminal, and prove the pipe path is inert"
status: draft
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-279]
---

## Description

Wire the opt-in constructor to the session's terminal facts, and assert the two guarantees that
follow: piped output is untransformed, and the accumulator still sees raw text. Covers BR-7 and
BR-9.

## Files to Create/Modify

- `crates/teton/src/main.rs` — at the surface construction (~line 1047), build with markdown only
  when `interactive`; pass `prompt::terminal_width()` and the existing `color` flag.
- `crates/teton/src/session_ui.rs` — AC-11 tests only. **Line 2425–2427 must not change.**
- `crates/teton/tests/cli_e2e.rs` — AC-7.

## Acceptance Criteria

- [ ] AC-7: piped output for a scripted turn carrying a table, bold text, and a code fence equals
      the concatenated raw chunks the daemon sent — the renderer is inert off a terminal.
- [ ] AC-11: the REQ-579 setup hand-off, the REQ-581 connection hand-off, and the REQ-582 command
      hand-off each still fire on a turn whose reply contains the trigger text wrapped and styled.
- [ ] Every existing `cli_e2e` assertion passes unmodified — including the exact occurrence count
      at cli_e2e.rs:5512. If one needs editing, BR-7 is broken; fix the gate, not the test.
- [ ] **Delete the `#[cfg_attr(not(test), expect(dead_code, ...))]` on `PlainSurface::with_markdown`**
      (render.rs ~263). TASK-279 placed it as a self-removing marker: wiring the `main.rs` caller
      turns it into an unfulfilled lint expectation, so `cargo clippy --workspace -- -D warnings`
      fails until it is removed. That failure is the proof the gate is really wired.
- [ ] `terminal_width()` is read **at the construction site** and passed in; `markdown.rs` still
      does not name it (TASK-277's sweep stays green).

## Technical Notes

**BR-9 is preserved by doing nothing.** `session_ui.rs:2425` pushes the raw chunk to
`state.turn_reply`, then 2427 hands it to the surface; because ADR-1 puts the transform *inside*
`PlainSurface`, the accumulator is already upstream of it. AC-11 exists to prove that stayed true,
not to make it true. If a future edit moves rendering ahead of the push, the REQ-579/581/582
hand-off notices die silently — the predicates match substrings of the model's own words, and every
one of them is TTY-gated so `cli_e2e` cannot see the loss ([[LESSON-529]], [[LESSON-481]]).

`terminal_width()` (prompt.rs:515) documents itself as "the one place the width is queried" and has
two callers today (the entry rule and `entry_status`). This adds a third, in the same shape: query
at the call site, pass the number into pure logic.

`color` at main.rs:1045 is `interactive && banner::color_enabled()`, and `color_enabled()` is
already false under `NO_COLOR` or `TERM=dumb` — so the colour leg needs no new plumbing here. Its
test is TASK-283's pty leg.
