---
id: TASK-277
title: "The pure layout module: display width and word wrap"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: []
---

## Description

Create the pure layout module REQ-592 is built on, and take the `unicode-width` dependency
(ADR-5). This task ships **no terminal bytes** — only width-parameterised functions and their
unit tests. Covers BR-3 and BR-10.

## Files to Create/Modify

- `crates/teton/src/markdown.rs` — new. Display width, word wrap, inline-span parsing, and the
  block classifier for the recognized-construct table. No I/O.
- `crates/teton/src/main.rs` — add `mod markdown;`.
- `crates/teton/Cargo.toml` — add `unicode-width`, with a comment recording ADR-5's reasoning
  (a wrong width makes rows exceed the terminal and regresses to the bug this REQ fixes; the two
  prior declines were about a *format-character category* table, a different and cosmetic gap).
- `Cargo.lock` — regenerated.

## Acceptance Criteria

- [x] AC-3: a paragraph wider than the width breaks at spaces and never mid-word; a single token
      longer than the width occupies its own row intact; a list item's continuation rows align
      under its text; `80` is used when no terminal is present.
- [x] Wrapping measures with `UnicodeWidthStr`/`UnicodeWidthChar`, not `chars().count()` — pinned
      by a test with CJK content asserting no emitted row exceeds the width in **display columns**.
- [x] The block classifier returns the right construct for every row of the
      recognized-construct table, and **literal text** for each construct listed under Out of
      Scope (AC-14's unit half): nested list, setext heading, indented code block, nested
      emphasis, `|` inside a code span inside a table cell. No panic, no dropped characters.
- [x] Structural sweep test: `markdown.rs` names no `print!`, `println!`, `write!`, `stdout`, or
      `terminal_width` — the module cannot do I/O or read the width. Modelled on `status.rs:445`.
- [x] Every test in this task runs under plain `cargo test` with no pty and no TTY (BR-10).

## Technical Notes

Follow `crates/teton/src/status.rs` closely — it is the in-repo precedent for pure width-aware
layout: `status_line(level, effort, width) -> Option<String>` takes the width as a parameter, and
structural sweeps (status.rs:445, 479) forbid the module from touching stdout.

Take `status.rs`'s failure posture too: **degrade, don't truncate**. Where a layout cannot fit,
return something complete-but-plain rather than something tidy and clipped.

This module must not know about `Surface`, `PlainSurface`, SGR bytes, or defusing. It returns
plain strings and span descriptions; TASK-279 turns those into bytes. Keeping the split clean is
what lets every test here run with no terminal.
