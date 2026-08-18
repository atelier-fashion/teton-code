---
id: TASK-166
title: "Hand-off: guide names /provider test; the surface nudge fires on a connection-question turn"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: []
---

## Description

BR-6 / AC-8a / AC-8b per architecture ADR-4: the resident guide says the
command; the session prints one line when a connection question was answered
by improvising instead.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — after item 3 ("Inspect with …"), one sentence: "To check a provider actually answers: `/provider test <id>` (shell: `teton provider test <id>`) — one consented request, and it reports what came back. Do not probe with shell commands." Keep the guide's byte budget in mind (ASSUME-008: prompt margins are thin — measure the added length and note it in the commit).
- `crates/tetond/src/provider_recipes.rs` (tests) — pin the guide sentence with the existing `drift()` helper in the catalog contract test (or a sibling test in the same module): the guide contains `/provider test <id>`.
- `crates/teton/src/session_ui.rs` — `SessionState` gains `turn_prompt: String` (set by `begin_turn(prompt)` — extend the signature; the entry loop passes `prompt_text`) and `turn_tools: Vec<String>` (append `"<tool>: <command or first arg>"` from the tool-call `session_update` payload the renderer already handles; cleared at `begin_turn`); `fn asks_about_a_connection(prompt, provider_ids) -> bool` (case-insensitive: one of `test|check|verify|working|connected|reach` AND one of `provider|connection|connectivity|api|<a registered provider id>`); `fn improvised_a_probe(plain_reply, turn_tools) -> bool` (reply recites `teton provider|policy|doctor`, or a `shell` tool call names `teton`); `CONNECTION_TEST_LINE: "in this session, /provider test <id> makes one consented call and reports what came back; that is the connection test."`; `hand_off_after_turn` decides: setup line per the existing predicate, else the connection line when `asks_about_a_connection && improvised_a_probe && !reply.contains("/provider test")`; at most one line per turn; TTY only. Provider ids from the cached config snapshot on `SessionState` (fall back to the fixed word list when absent). Tests: predicate table (positive: the screenshot's prompt + a `shell: teton provider list` tool call; negatives: a connection question answered with `/provider test`, an unrelated prompt that runs `teton doctor`, a non-TTY session), at-most-once, and the setup line still wins for a setup-recipe reply.
- `crates/teton/src/main.rs` — pass the prompt to `begin_turn`.
- `docs/manual-verification.md` — a short REQ-581 section: the live A/B for AC-8b (three phrasings of "test the Kimi connection" on the real local model; record whether the line printed and whether the model named `/provider test`) and one real `reached` against Kimi.

## Acceptance Criteria

- [ ] `cargo test -p tetond --lib provider_recipes` pins the guide sentence; `cargo test -p teton --bin teton session_ui` green with the predicate table.
- [ ] The screenshot's turn shape (prompt "Can you test the Kimi connection?", tool calls `shell: teton provider list`, reply without `/provider test`) prints exactly one `CONNECTION_TEST_LINE` on a TTY and nothing on a pipe.
- [ ] A reply that names `/provider test` prints nothing; the setup recipe line's behaviour is unchanged (its tests still pass).
- [ ] The manual-verification section exists and is marked OUTSTANDING.

## Technical Notes

`hand_off_after_turn` already consumes `turn_reply` once per turn — keep that; clear `turn_prompt`/`turn_tools` in the same place. Tool-call payloads: see how `render_session_update` handles `ToolCall`/`ToolCallStatus` — capture the tool name and the `shell` command string there. The word lists are v1 heuristics; say so in doc comments and point at the manual A/B (LESSON-532).
