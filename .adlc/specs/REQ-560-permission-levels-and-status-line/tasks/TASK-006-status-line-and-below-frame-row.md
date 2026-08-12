---
id: TASK-006
title: "Pure status-line content function and the TTY-gated row below the frame"
status: complete
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-005]
---

## Description

The status row, in two separable pieces (ADR-E): a pure content function with no
terminal and no I/O (BR-8), and a fourth frame row drawn below the bottom rule
that strands nothing in either direction (BR-11).

BR-8 exists because without it this feature has no verification path at all —
`cli_e2e` drives pipes and BR-9 makes the row invisible there (LESSON-481).
Write the pure function first and test it before touching the frame.

## Files to Create/Modify

- `crates/teton/src/status.rs` — new module:
  - `pub fn status_line(level: PermissionLevel, effort: Option<&str>, width: usize) -> Option<String>`
  - `None` when the rendered content exceeds `width` — the sole degradation
    path (BR-13, OQ-5). **Never truncate**: a clipped security label is worse
    than no label, and BR-10 keeps the value readable
  - `effort: None` renders the permission field alone
- `crates/teton/src/prompt.rs`:
  - `FramedStdinPrompter` gains `below: Option<String>` (the row's content) and
    `below_rows: usize` (what `draw` actually emitted)
  - `set_status(&mut self, Option<String>)` — the caller supplies content; the
    prompter owns placement
  - `draw`: emit the status row after the bottom rule and move up
    `2 + below_rows` instead of a hardcoded 2
  - `read_line`: Enter path emits `1 + below_rows` newlines; EOF path emits
    `2 + below_rows`
  - `erase` is **unchanged** — `\x1b[J` already clears from the cursor to the
    end of the screen, which includes everything below the frame
- `crates/teton/src/main.rs` — the entry loop calls `set_status` with
  `status_line(level, None, terminal_width())` before `draw`; the level comes
  from the session's `session/permissions` read
- `crates/teton/src/lib.rs` (or `main.rs` module list) — declare `status`

## Acceptance Criteria

- [ ] **AC-7 (unit)**: `status_line` returns the expected string for each
      (level × effort) pair with no terminal involved, iterating
      `PermissionLevel::ALL`. Includes the `effort: None` rendering this REQ
      ships with, and a "not applicable" effort string so REQ-559 BR-6's local-
      only case has its slot
- [ ] **AC-12 (unit)**: a width too narrow for the content returns `None` — no
      row, no panic, no truncation. Exercise widths from 0 up to exactly one
      byte short of fitting
- [ ] **BR-11 (unit)**: `draw` with a status row moves the cursor up 3 and
      without one up 2; `read_line`'s Enter and EOF paths emit the matching
      newline counts. Assert against captured bytes, not a real terminal
- [ ] `below_rows` is written by `draw` and read by `read_line` — assert that a
      `read_line` after a `draw` that emitted no row behaves byte-identically to
      today
- [ ] **BR-9 / AC-8 (piped)**: with `framed: false`, `draw`/`read_line` emit not
      one status byte; the existing `cli_e2e` whole-output tests and the
      `/quit`-equals-Ctrl-D equivalence tests pass **unmodified** — if any test
      file under `crates/teton/tests/` needs an edit to accommodate status
      bytes, the implementation is wrong, not the test
- [ ] **BR-14 fence**: the effort field renders from the parameter only; no
      `/effort` command is added
- [ ] `cargo test -p teton` green; no clippy warnings

## Technical Notes

The frame geometry, before and after:

```
[web?] [indicator]        ← above-frame rows, counted by `status_rows` (REQ-556)
────────────────────      ← top rule
> _                       ← input row, cursor
────────────────────      ← bottom rule
permissions: guarded      ← NEW, counted by `below_rows`
```

The two counts stay **independent** (BR-11). `status_rows` keeps its documented
above-frame meaning — do not overload it.

The `read_line` change is the stranding hazard: after Enter the cursor lands on
the bottom rule, so today's single newline would put the next output on top of
the status row and leave a partially overwritten row behind. Storing
`below_rows` on the prompter is what keeps `draw` and `read_line` a matched pair
— the count is written once, by the code that drew the rows.

Every write stays inside `if self.framed`. That is BR-9 by construction rather
than by a test that happens to pass.
