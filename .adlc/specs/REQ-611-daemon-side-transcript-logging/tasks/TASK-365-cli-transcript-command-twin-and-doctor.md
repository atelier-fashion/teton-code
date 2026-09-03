---
id: TASK-365
title: "CLI: /transcript, teton transcript enable|disable|status, the state event, and doctor"
status: complete
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-361, TASK-364]
---

## Description

The user-facing surfaces. `/transcript [on|off]` in the session, its durable shell twin, the
rendered `transcript_state` notice, and one posture line in `teton doctor` / `/doctor`. Covers
the CLI halves of AC-3, AC-4, AC-5 and all of AC-20; keeps BR-3's "user act" true at the client
by registering the command only in `COMMANDS`.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — one `CommandSpec` row `transcript` with an optional argument
  parsed to `TranscriptAction` (`on`, `off`, bare → `Status`), placed beside `verbose` (line
  ~646); handler sends `session/transcript` and renders the result through the `Surface` seam.
  Piped is allowed (spec OQ-2: not an escalation). Help text one line.
- `crates/teton/src/main.rs` — `Transcript { Enable | Disable | Status }` subcommand: enable and
  disable send `config/set SetTranscriptEnabled`; status does one `config/get` and renders
  the durable default, effective dir (via `TranscriptConfig::effective_dir` when `dir` is unset)
  and retention. Piped into a session, `teton transcript …` names the shell command as the other
  shell-first commands do (README §"Two more `/provider` commands…").
- `crates/teton/src/session_ui.rs` — render `transcript_state`: `transcript: on` /
  `transcript: off (write failure — see /transcript)` / `transcript: off (directory refused …)`.
- `crates/teton/src/status.rs` — a pure `transcript_line(enabled_default, effective_dir,
  retain_days, width)` used by `doctor` and `/doctor`.
- `README.md` — `/transcript` row in the session-command table; `teton transcript` in the twin
  list.

## Acceptance Criteria

- [x] AC-5 (CLI half): bare `/transcript` prints enabled state, path, record count, and the
      degraded reason when present; `/help` lists `/transcript`.
- [x] AC-3 / AC-4 (CLI half): `/transcript on` and `/transcript off` each print one line and the
      rendered `transcript_state` notice arrives once, not twice.
- [x] AC-20: `teton doctor` and `/doctor` each contain exactly one line beginning `transcript:`
      naming the durable default, the effective directory, and `retain_days`; snapshot test
      updated.
- [x] `teton transcript enable` followed by `teton transcript status` shows `enabled` and the
      config file on disk carries the key (read back — LESSON-519).
- [x] The session command is dispatched only via the `COMMANDS` table; no other call site
      parses `transcript` (grep in `crates/teton/src`).
- [x] `cargo test -p teton --no-fail-fast` is green, including `cli_e2e.rs` and `pty_e2e.rs`
      `/help` assertions.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-3 | test-case | `crates/teton/tests/pty_e2e.rs::transcript_on_prints_one_line_and_one_state_notice` | no |
| AC-4 | test-case | `crates/teton/tests/pty_e2e.rs::transcript_off_then_on_prints_the_resume` | no |
| AC-5 | test-case | `crates/teton/tests/pty_e2e.rs::bare_transcript_prints_status_with_path_and_degraded_reason` | no |
| AC-20 | test-case | `crates/teton/tests/cli_e2e.rs::doctor_prints_one_transcript_posture_line` | no |

## Technical Notes

Copy `/permissions`' handler shape, not `/verbose`'s: `/verbose` is a client-local bool on
`SessionState` (session_ui.rs ~138) and never reaches the daemon, whereas `/transcript` must.

The doctor line is pure (no I/O, no clock) like `status_line`; compose the sentence where the
facts are (conventions: "compose the sentence where the facts are").

Keep the notice to one line per state change. A second attached client also receives
`transcript_state`; it renders the same line — that is correct, it is news for that session.

## Outcome

- Implemented in-context by the orchestrator after two backend failures
  (HTTP 529) killed the dispatched agents before they wrote anything.
- `/transcript [on|off]` is one `COMMANDS` row (`Args::Optional`, no mirror);
  `on`/`off` print one handler line naming the file (or `stopped`) and the
  `transcript_state` notice is drawn once by `session_ui` on every attached
  client, the issuer included. Bare `/transcript` prints state, path, record
  count and any degraded reason.
- `teton transcript enable|disable|status` is deliberately **shell-only**, not
  a twin: the two switches have different lifetimes, and `cli_rows::SHELL_ONLY`
  gained the three leaves with their own refusal sentence pointing at
  `/transcript on|off`. `RESERVED_SKILL_NAMES` gained `transcript` so a skill
  cannot shadow the command.
- `status::transcript_line(enabled_default, dir, retain_days)` has no `width`
  parameter: `doctor` lines are never width-gated, only the entry status row is.
- The degraded leg of AC-5 is driven by a pre-existing `0o755` transcript
  directory (BR-9 / AC-11), which the daemon refuses at `on`; the AC-10 write
  seam belongs to TASK-366.
- Mutations run and recorded: doctor line removed (cli_e2e red), degraded
  suffix dropped (unit red), `off` printing the recording form (pty red),
  notice render suppressed (pty red). All restored; suite green
  (4159 passed / 0 failed, clippy clean).

