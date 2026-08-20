---
id: TASK-207
title: "The client: send the invocation, refuse a consent it cannot answer, and say one line about it"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-205, TASK-206]
---

## Description

BR-11 and BR-12 on the client. The load-bearing part is small and sharp: on a
pipe, a consent that needs a human must be refused **before** `prompter.ask` is
called, because `StdinPrompter::ask` reads a line unconditionally and a pasted
second line must not become a `y`.

## Files to Create/Modify

- `crates/teton/src/main.rs` — the `Input::Skill` arm; the `skills/list` snapshot at launch and on `session_root_changed`
- `crates/teton/src/client.rs` — thread `ctx.typed_input` into `resolve_permission`
- `crates/teton/src/session_ui.rs` — `consent_gate`, the refusal path, the `SkillInvoked` echo line, `/verbose` detail

## Acceptance Criteria

- [ ] The snapshot is fetched after `session/create` and after every `session_root_changed`. `METHOD_NOT_FOUND` yields an **empty** snapshot and no error — an old daemon therefore classifies no skills and never receives `PromptTurnParams.skill` (ADR-2). Asserted.
- [ ] `consent_gate(subject: Option<&PermissionSubject>, typed_input: bool) -> ConsentGate` is a **pure two-input predicate** with a truth-table unit test, in the style of `cli_rows::write_gate` (`:175`). `Answerable` / `RefuseNoTerminal` / `RefuseUnrecognized`.
- [ ] `RefuseNoTerminal` and `RefuseUnrecognized` return a rejection **without calling `prompter.ask`**. The pin is negative and must be written as one: feed `/status\ny\n` on a pipe at `guarded` and assert `y` arrives as the **next prompt line**, not as an answer (`cli_e2e.rs:1830` is the template).
- [ ] `PermissionSubject::Unrecognized` ⇒ refuse. A client that does not understand a subject must not fall through to reading stdin — fail-closed in the direction that can only cost a skill invocation, never a swallowed prompt line (BR-11).
- [ ] The client selects on `PermissionSubject`, never on the key string. `req.tool_name`'s `skill:<source>:<name>` shape is not parsed anywhere in `teton` — asserted by a source-level check in the test suite, in the style of `boundary_coverage.rs`'s source scans.
- [ ] Consent rendering: one `Surface::line` per command, so "three commands listed verbatim" survives `defused`'s newline destruction.
- [ ] At `full` there is nothing to ask: dynamic context runs on a pipe exactly as on a TTY. That is the automation posture (BR-11).
- [ ] BR-12: one echo line rendered from `Event::SkillInvoked` — `/status → skill status (user, 5.3 KB, 4 dynamic commands)`. The body is never printed. `/verbose` adds the home-relative path, the ignored frontmatter keys, and each command's typed outcome.
- [ ] The echo line respects the existing TTY gates: `cli_e2e.rs:863 assert_no_turn_ran` stays true for `/help` and for a shadowed or skipped name, and the pipe-bytes pins at `cli_e2e.rs:5361` / `:5706` are unchanged.
- [ ] Mutation table: calling `prompter.ask` before the gate, treating `Unrecognized` as answerable, and sniffing the key string each fail a named test.

## Technical Notes

- `ctx.typed_input` already exists (`client.rs:68`, set from `IsTerminal` at `main.rs:1060`); it is simply not threaded into `resolve_permission` (`session_ui.rs:2229`) today. Thread it — do not recompute `is_terminal` inside the UI.
- `FramedStdinPrompter::ask` delegates to `StdinPrompter::ask` when `!framed` (`prompt.rs:455`), so the piped path is byte-identical and one gate covers both.
