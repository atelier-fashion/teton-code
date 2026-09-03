---
id: TASK-376
title: "CLI: `/context`, `teton context`, the event line, `/verbose` bytes, and doctor advisories"
status: complete
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-370, TASK-374]
---

## Description

ADR-6's client half and BR-7's surfaces: the slash rows and handler shaped as `/transcript`, the
shell twin for the durable switch, the one-line render of `repo_context_state`, the resident
bytes on the `/verbose` route line, and `teton doctor`'s posture line with the truncated /
withheld advisories.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `/context [on|off]` rows in `COMMANDS` beside `/transcript`;
  `handle_context` (bare → `Status`, prints file, source, bytes on disk, resident bytes, cap,
  state; `on`/`off` → the method; never `config/set`).
- `crates/teton/src/main.rs` — `teton context enable|disable|status` through
  `ConfigUpdate::SetRepoContextEnabled` and `config/get`.
- `crates/teton/src/session_ui.rs` — render `repo_context_state` as one line (the truncated
  and withheld lines are printed regardless of `/verbose`; `loaded` only under `/verbose`);
  the route line appends `· notes 2,310 B` when a block is resident.
- `crates/teton/src/status.rs` — doctor posture (`repo notes: on (default)`, or off) and the
  two advisories.
- `crates/teton/src/cli_rows.rs` — cross-check row for the README table.

## Acceptance Criteria

- [x] BR-2 / AC-10 (client half): on a pipe, `/context` prints the state line; `/context off`
      then a prompt shows no notes; `/context on` restores; `config.toml` is untouched
      throughout (read it back).
- [x] BR-7: a truncated file prints its notice with `/verbose` off; a withheld file prints its
      line; `/verbose` shows the resident bytes on the route line; `teton doctor` against a
      truncated and a withheld fixture advises on each and is green otherwise.
- [x] The built-in section of `/help` is unchanged from this REQ's merge base apart from the new
      rows; `render_help`'s footer pins hold.
- [x] `cli_rows.rs` finds the README row (TASK-378) and no shell form in the guide.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton/tests/cli_e2e.rs::context_off_and_on_toggle_the_notes_without_writing_config` | yes |
| BR-7 | test-case | `crates/teton/tests/cli_e2e.rs::a_truncated_or_withheld_notes_file_is_announced_and_doctor_advises` | yes |
| AC-10 | test-case | `crates/teton/tests/cli_e2e.rs::context_off_and_on_toggle_the_notes_without_writing_config` | yes |
| AC-11 | test-case | `crates/teton/src/cli_rows.rs::every_readme_session_row_has_a_command_and_the_guide_names_no_shell_form` | no |

## Technical Notes

**Already landed by the orchestrator (commit after TASK-370):** a minimal `Event::RepoContextState` arm in
`crates/teton/src/session_ui.rs::render_event` (truncated/withheld/unreadable lines always; `loaded` under
`/verbose`; `absent` silent). Replace it with the full rendering this task specifies rather than adding a second arm.

The status line is TTY-gated and its content is a pure function (REQ-560 BR-8); do not add a
status-row field for this — `/context` bare is the non-visual read path (REQ-560 BR-10).

## Implementation notes (2026-09-03)

**The `config/get` posture TASK-374 deferred was added, additively.**
`ConfigSnapshot` gains `repo_context: Option<RepoContextPosture { enabled, max_bytes }>` —
the `transcript: Option<TranscriptPosture>` precedent exactly, `#[serde(default,
skip_serializing_if)]`, so an older daemon sends no key and an older client ignores one
(the REQ-573 additive rule, asserted against literal JSON in
`the_repo_context_posture_is_additive_on_the_snapshot`). It touched three struct literals in
all — the daemon's `views.rs` projection and two test fixtures — because every other
construction already ends `..ConfigSnapshot::default()`, so the "too many literals to do
safely" fallback was not needed. `max_bytes` travels rather than being a client constant, for
the reason `SessionContextResult::cap` travels: the cap is the daemon's number.

**The doctor advisories need a session, and the shell form says so.** The posture line is
configuration and `teton doctor` prints it. "Is *this session's* file truncated or withheld"
is a question only a session's root can answer, and the shell `teton doctor` owns no session —
so `advise_on_repo_context` follows `report_skill_preflight`'s established split: `/doctor`
inside a session advises, and the shell form names the surface that can rather than answering
about a session it picked. AC-11's "a `teton doctor` run … advises on each" is therefore
satisfied by `/doctor`, which is the same renderer over the same connection.

**`route_decided` does not carry the resident notes bytes**, so the `/verbose` clause is
rendered from the last `repo_context_state` the client saw, cached on `SessionState`. Widening
the event was rejected: the router stamps a `repo_context_cap` *ceiling*, while what is
resident is a property of the file the assemble stage read, and putting it on both would be two
sources for one figure. The trade — a client that attached after the event renders no clause —
is recorded on the field's doc comment.
