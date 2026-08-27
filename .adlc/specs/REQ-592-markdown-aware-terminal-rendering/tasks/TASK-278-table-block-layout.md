---
id: TASK-278
title: "Table block layout: align when it fits, transpose when it does not"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-277]
---

## Description

Add table measurement and layout to the pure module. Still no terminal bytes. Covers BR-4.

## Files to Create/Modify

- `crates/teton/src/markdown.rs` — table row/separator recognition, column measurement, and the
  two layouts.

## Acceptance Criteria

- [x] AC-4 (aligned): a narrow table renders with its columns lined up vertically and its
      separator row drawn as a rule rather than printed literally.
- [x] AC-4 (transposed): the table in
      `.adlc/specs/REQ-592-markdown-aware-terminal-rendering/fixtures/audit-2026-08-26.md` —
      7 data rows, column 2 measuring 155..243 chars, widest raw row 263 — renders as one labelled
      block per data row at **both 100 and 200 columns**, every value wrapped, and **no emitted row
      exceeding the width in display columns**.
- [x] The fixture is **read from disk by the test**, not transcribed into it.
- [x] A width too small to lay the table out even transposed emits the raw source rows rather
      than clipping cells (ADR-2's degrade-don't-truncate).
- [x] Column measurement ignores inline markers: a cell of `**bold**` measures 4 columns, not 8,
      so alignment is against what is displayed rather than what is typed.

## Technical Notes

**Read the fixture from disk.** A table authored while knowing the layout algorithm tests the
author's assumptions rather than the algorithm — [[LESSON-529]]'s re-enactment corollary, and the
reason the fixture is checked in at all. Resolve the path from `CARGO_MANIFEST_DIR` so the test
works from any working directory. The fixture's header warns against reflowing it; its exact bytes
are the input under test.

Buffering is the caller's job (TASK-279 holds the rows and decides when the run ends). This task
is the pure `(rows, width) -> Vec<String>` decision only.

The transposed layout is the one that actually fixes the reported defect, so give it the harder
cases: a cell containing `|`, a row with a missing trailing cell, a header shorter than its column
name suggests, and a single-column table.
