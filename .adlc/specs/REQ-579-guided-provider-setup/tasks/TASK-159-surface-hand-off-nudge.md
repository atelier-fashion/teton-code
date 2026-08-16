---
id: TASK-159
title: "CLI: append the /provider setup hand-off line when the model recites the CLI (ADR-9)"
status: draft
parent: REQ-579
created: 2026-08-16
updated: 2026-08-16
dependencies: ["TASK-155", "TASK-158"]
---

**Covers:** AC-1 (the deterministic half; the model-volunteers half is the recorded live result)

## Description

Three live rounds proved the local model will not volunteer `/provider setup` from the guide (verification.md §1–§24). ADR-9 moves the guarantee to the surface: at the end of a typed-prompt turn on a TTY surface, if that turn's assistant reply contained `teton provider add` or `teton policy set-tier`, print exactly one harness Notice naming the guided command. Deterministic, once per turn, TTY-only, model-output-only.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — accumulate the current turn's assistant text: `SessionState` gains a small `TurnText` (or `current_reply: String`) appended on `SessionUpdatePayload::AgentMessageChunk` and cleared at turn start; a pure `pub(crate) fn recites_provider_cli(reply: &str) -> bool` (matches `teton provider add` / `teton policy set-tier`, case-sensitive, backtick-agnostic); a pure `pub(crate) fn hand_off_line() -> &'static str` returning the one sentence; a `pub(crate) fn hand_off_after_turn(state: &mut SessionState, surface: &mut dyn Surface, tty: bool)` that prints at most once and resets — pure enough to unit-test with the existing recording surface
- `crates/teton/src/main.rs` (~L792, the `Ok(res)` arm after `conn.call(params, &mut ctx)?` in `run_session`) — call `hand_off_after_turn` before the closing blank line, gated on `ctx.typed_input` (the same predicate `web_setup_ui::gate` uses; the non-TTY path already prints the recipe per BR-11 and scripts must get byte-identical output)
- `crates/teton/src/session_ui.rs` (tests) — `a_reply_that_recites_the_cli_earns_exactly_one_hand_off_line`; `a_reply_that_names_the_command_earns_nothing` (model volunteered it → dormant); `a_reply_about_anything_else_earns_nothing`; `the_hand_off_is_once_per_turn_even_when_both_commands_appear`; `the_hand_off_never_prints_on_a_non_tty_surface`; `the_users_own_text_and_help_output_do_not_trigger_it` (only AgentMessageChunk accumulates); `the_hand_off_line_carries_no_ansi_and_names_the_command_verbatim`
- `crates/teton/tests/pty_e2e.rs` — a pty walk ONLY if the harness can drive a scripted model reply (check for a mock-provider or canned-reply seam used by any existing pty test); if it can, one test: model reply contains `teton provider add` → the `>>` hand-off line appears once before the next prompt frame; if it cannot, record why in the completion note and rely on the unit tests
- `crates/teton/tests/cli_e2e.rs` — extend `a_piped_provider_setup_prints_the_recipe_and_asks_nothing` or add a sibling asserting a piped session whose model reply recites the CLI gets NO hand-off line (byte-identical to pre-REQ for scripts) — needs the mock provider the suite already uses for scripted replies, if any; otherwise a unit test on the tty=false path suffices

## Acceptance Criteria

- [ ] In a TTY session, a model reply containing `teton provider add` is followed by exactly one Notice line naming `/provider setup <vendor> [tier]`; a reply that already names `/provider setup` gets nothing; an unrelated reply gets nothing
- [ ] Never printed on a non-TTY surface; never triggered by the user's own input or by `/help`
- [ ] The line is plain text (no ANSI, LESSON-517), stated outright, imperative, no em-dash aside (BUG-168's rules — the model may quote it back)
- [ ] `cargo test -p teton` green; clippy + fmt clean; `cargo test --workspace --no-fail-fast` green

## Technical Notes

The seam is main.rs:792 (`Ok(res)`), the only place a typed prompt's turn ends. Assistant text streams via `render_session_update` → `AgentMessageChunk { text }` → `surface.fragment(text)` (session_ui.rs ~L1159); accumulate there. Reset the accumulator when the next prompt is sent (before `conn.call`), not on turn end, so an interrupted turn cannot leak into the next. Do NOT match on the user's prompt text; do NOT add anything to the daemon; do NOT touch the guide (round 2's wording ships). Wording: `in this session, /provider setup <vendor> [tier] does this without leaving it — no key in chat.` — one sentence, `LineKind::Notice`. If `SessionState` already tracks per-turn text for another purpose (loading indicator, cost meter), reuse it rather than adding a second accumulator.
