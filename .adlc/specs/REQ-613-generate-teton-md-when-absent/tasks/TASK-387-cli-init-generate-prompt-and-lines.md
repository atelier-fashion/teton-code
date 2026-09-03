---
id: TASK-387
title: "CLI: `/context init [--force]`, `teton context init|generate`, the prompt, the lines, and the banner clause"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-380, TASK-386]
---

## Description

ADR-7 and the surfaces: the slash row and handler, the one-shot shell session for `init`, the
config write for `generate`, rendering of the new permission subject (both questions), the event
lines and the `/verbose` drafting line, the banner/`/cd` clause announcing the coming offer, and
the `NoTerminal` refusal for the new subject.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `/context init [--force]` row; `handle_context` gains the
  `init` branch.
- `crates/teton/src/main.rs` — `teton context init [--force]` (one-shot session at cwd) and
  `teton context generate ask|always|never`.
- `crates/teton/src/session_ui.rs` — subject rendering (`replace` wording), `refusal_line` arm,
  event lines, drafting line, banner clause.
- `crates/teton/src/cli_rows.rs` — README cross-check rows.

## Acceptance Criteria

- [ ] AC-3: on a pipe at `guarded` the client refuses without reading stdin (the next line is
      still the next prompt), prints one line, no file; with `generate = always` on the same
      pipe the file is written.
- [ ] AC-10: `/context init` with a file refuses naming size and `--force`; `--force` prompts
      with the replace wording and, accepted, replaces; `teton context init` produces the same
      bytes as the session door on the same fixture.
- [ ] The banner prints the announcement clause only when the daemon reports `absent` and
      generation is not suppressed; `/verbose` prints the drafting line with tier, entries and
      tokens.
- [ ] `teton context generate never` writes the key through `config/set`; the doctor posture
      line shows it.
- [ ] `cargo test -p teton --no-fail-fast` green, including the `cli_e2e` legs named below.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| BR-8 | test-case | `crates/teton/tests/cli_e2e.rs::context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes` | yes |
| BR-10 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| AC-3 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| AC-10 | test-case | `crates/teton/tests/cli_e2e.rs::context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes` | yes |

## Technical Notes

The one-shot session uses the same root probe and level as `teton`; on a pipe it inherits the
`NoTerminal` refusal. Never write config from the slash handler (REQ-611 BR-2's split).
