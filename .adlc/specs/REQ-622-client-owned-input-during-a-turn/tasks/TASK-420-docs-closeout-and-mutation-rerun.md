---
id: TASK-420
title: "Docs, BUG-225 resolved, REQ-621 amendments retired, the architecture-context pattern, and the mutation re-run"
status: complete
parent: REQ-622
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-419"]
repo: teton-code
---

## Description

AC-16 and the REQ-619 ownership rule: re-run the editor and handler mutations over the
widened suite and rewrite their records; mark BUG-225 resolved naming REQ-622; add dated
"retired by REQ-622" notes to REQ-621's BR-5/BR-9 amendments and ADR-621-3's typing
bullet; README "In the session" describes typing during a turn, queued prompts, and the
queued clause; CHANGELOG; docs/manual-verification.md; the Key Patterns entry from the
REQ-622 architecture.

## Files to Create/Modify

- `README.md`, `CHANGELOG.md`, `docs/manual-verification.md` — user docs
- `.adlc/bugs/BUG-225-a-line-submitted-over-the-activity-row-leaves-its-last-frame.md` — `status: resolved`, `resolved:` date, Resolution naming REQ-622
- `.adlc/specs/REQ-621-live-turn-activity-line/requirement.md`, `.adlc/specs/REQ-621-live-turn-activity-line/architecture.md` — dated retire notes
- `.adlc/context/architecture.md` — Key Patterns entry
- `crates/teton/src/input_editor.rs`, `crates/teton/src/prompt.rs` — mutation records rewritten with the widened counts

## Acceptance Criteria

- [x] `cargo test --workspace --no-fail-fast` — 0 FAILED after `cargo build -p tetond -p teton` (**4722 passed, 0 failed**; `grep -c FAILED` = 0)
- [x] Mutation records name what reddened in unit and in pty legs — Backspace-pops-a-byte: **1 red of 853** unit (`input_editor::tests::the_keystroke_table`), **1 red of 49** pty (`multi_byte_input_round_trips`); `RESTORE.clear()` removed from `RawMode`'s `Drop`: **2 red of 853** unit (`prompt::tests::both_guards_arm_and_clear_the_restore_slot`, plus the order-dependent collateral `main::tests::queued_lines_re_enter_ahead_of_the_poll_in_order`), **9 red of 49** pty
- [x] clippy `-D warnings`, fmt clean — and clean **without** the `pub mod` stopgap: `input_editor` and `session_ui` are back to `mod`, no dead-code notice returned

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-10 | test-case | `crates/teton/src/input_editor.rs::tests::the_keystroke_table` (mutation re-run recorded) | no |
| AC-16 | structural-check | `README.md`, `CHANGELOG.md`, `.adlc/bugs/BUG-225-*`: strings present (grep) | no |

## Technical Notes

- LESSON-652: rewrite counts, do not append. Revert mutations by the same targeted edit.
