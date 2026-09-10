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
| BR-13 | test-case | `crates/teton/src/render.rs::tests::a_surface_that_does_not_override_withdraw_emits_nothing` | yes |

## Technical Notes

- Keep `at_line_start` bookkeeping consistent: after a withdraw the cursor is at column 0 of the cleared row, so set `at_line_start = true`.
- Do not add a `Surface` method that takes the frame text; the pump draws with `line` and repaints with `repaint_row_above` — the withdraw verb is the only new byte sequence.
- The styling entry follows REQ-573: authored in `sgr()`, never by the caller.
