---
id: TASK-410
title: "Surface verbs for a live row: withdraw, live-row capability, and the Activity line kind"
status: complete
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: []
repo: teton-code
---

## Description

Extend the `Surface` seam (ADR-621-3) with `withdraw_row_above(rows_up)` (default
no-op) and `has_live_rows() -> bool` (default `false`), add `LineKind::Activity`
to the styling table, implement both verbs on `PlainSurface` with `live_rows` set
only by `with_markdown`, and record both on `RecordingSurface`. `Bare` keeps the
defaults and a test pins that a non-overriding surface emits nothing.

## Files to Create/Modify

- `crates/teton/src/render.rs` — `LineKind::Activity` + its `sgr` entry (dim); `Surface::withdraw_row_above`, `Surface::has_live_rows` defaults; `PlainSurface { live_rows }` set in `with_markdown`; `withdraw_row_above` emits `\x1b[{n}A\r\x1b[K` after `emit_pending`; `RecordingSurface::with_live_rows()` and `Rendered::Withdraw(usize)`; tests

## Acceptance Criteria

- [x] `PlainSurface::new` and `with_color` answer `has_live_rows() == false`; `with_markdown` answers `true`
- [x] `withdraw_row_above(1)` on a markdown surface writes exactly `\x1b[1A\r\x1b[K` after flushing held rows; on `new`/`with_color` it writes nothing
- [x] `LineKind::Activity` styles through the sanitizer table only; `defused` applies to its text
- [x] `a_surface_that_does_not_override_withdraw_emits_nothing` pins the default alongside the repaint sibling
- [x] Every `match` over `LineKind` compiles without a wildcard being added
- [x] `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/teton/src/render.rs::tests::only_the_markdown_surface_has_live_rows` | yes |
| BR-8 | test-case | `crates/teton/src/render.rs::tests::withdraw_goes_through_the_seam_after_the_held_rows` | no |
| BR-13 | test-case | `crates/teton/src/render.rs::tests::a_row_verb_reports_whether_its_bytes_landed` | no |
| BR-13 | test-case | `crates/teton/src/client.rs::tests::a_failed_paint_hides_the_row_and_says_so_in_verbose` | no |
| BR-6 | test-case | `crates/teton/src/render.rs::tests::a_surface_that_does_not_override_withdraw_emits_nothing` (the row this line replaces cited this test for BR-13; it is a fine pin on the *silent default* and says nothing about a rendering failure — see the note below) | yes |

## Verify-pass correction (2026-09-10)

**BR-13 was implemented as zero, and this table said otherwise.** Both row
verbs discarded their writes (`let _ = write!(..); let _ = self.out.flush();`),
so a terminal that refused the row's bytes was indistinguishable from one that
took them: the pump went on repainting a row it believed was one above the
cursor, and "recorded in verbose output" had nothing to record. The row cited
`a_surface_that_does_not_override_withdraw_emits_nothing`, which asserts that a
non-overriding surface writes nothing — a real property, and BR-6's, not this
one's.

Both verbs now return `bool` (written **and** flushed; the trait defaults
answer `false`, `PlainSurface` reports the write, `RecordingSurface` reports
`true` unless built by `with_failing_rows`). `main.rs`'s REQ-556 indicator
ignores the report explicitly, with the reason written at the call site.
`withdraw_row_above` also gained a `debug_assert!` that nothing is held when it
moves the cursor — the claim its `emit_pending()` call exists to make, which
until now was only a comment.

## Technical Notes

- Keep `at_line_start` bookkeeping consistent: after a withdraw the cursor is at column 0 of the cleared row, so set `at_line_start = true`.
- Do not add a `Surface` method that takes the frame text; the pump draws with `line` and repaints with `repaint_row_above` — the withdraw verb is the only new byte sequence.
- The styling entry follows REQ-573: authored in `sgr()`, never by the caller.
