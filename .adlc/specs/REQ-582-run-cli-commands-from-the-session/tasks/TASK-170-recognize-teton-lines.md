---
id: TASK-170
title: "Recognize a `teton …` line typed at the prompt: `Input::{CliLine, CliRefused}`, entry-loop arms, totality tests"
status: draft
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-169]
repo: teton-code
---

## Description

Implement ADR-1: `slash::classify` gains a recognition arm for lines whose
first whitespace token is exactly `teton`, walking clap's tree
(`cli_rows::cli_path`) to the subcommand path. Path names a row → `Input::CliLine
{ name, args }` (`args` via `match_name_words(rest_after_teton, name)`);
path names no row (`uninstall`, bare family) / bare `teton` / `--help` /
`--version` → `Input::CliRefused(text)`; otherwise `Input::Prompt` unchanged.
The entry loop (main.rs ~813) prints one Notice `teton <name> → /<name>`
and calls `slash::dispatch(name, args, …)` for `CliLine`, and one Error line
for `CliRefused` — never a prompt turn, never a subprocess (BR-5).

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `Input::CliLine`, `Input::CliRefused`, recognition arm in `classify`, `UNINSTALL_IS_SHELL_ONLY`, `ALREADY_IN_A_SESSION`, `CLI_FLAGS_ARE_SHELL_ONLY` constants; extend the REQ-555 BR-8 totality tests (ADR-8): every mirrored row reachable from `teton <row>`; `teton uninstall`/`teton`/`teton --version`/`teton -h`/`teton provider` → `CliRefused`; `teton is slow today`, `tetonx provider list`, ` teton provider list` (leading space is trimmed by the loop — classify sees `teton …`), `//teton x` → the expected buckets; `teton provider list please` → `CliLine` (ADR-1 amendment).
- `crates/teton/src/cli_rows.rs` — `cli_path` if not already written in TASK-169; `refusal_for(path_or_flag) -> String` (uninstall sentence; bare family → clap's own error text via `try_parse_from`).
- `crates/teton/src/main.rs` — entry-loop arms; the Notice text constant `fn cli_line_note(name) -> String`.
- `crates/teton/tests/cli_e2e.rs` — AC-5: a piped session line `teton provider list` prints the note then the same lines as `/provider list`, and the scripted engine's reply queue is untouched (the next real prompt gets reply #1); AC-6: `teton uninstall` prints the refusal and no turn runs; `teton is slow today` reaches the model (reply #1 consumed); `teton provider list please` prints clap's `unexpected argument`.

## Acceptance Criteria

- [ ] `classify` remains pure and total; the extended both-directions tests pass; every existing classify test unchanged.
- [ ] `cli_e2e` AC-5/AC-6 tests as listed; assert absence of a model call by the scripted-reply-queue argument (deterministic), not a timer.
- [ ] `//teton provider list` still prompts the model with `/teton provider list` (REQ-555 BR-1b unaffected).
- [ ] The Notice line reads `teton provider list → /provider list` (rendered by the surface as `>> …`).
- [ ] No `std::process::Command` and no second `Connection` anywhere in the recognition path (grep test or review note).

## Technical Notes

- Aliases: `cli_path` uses `find_subcommand`, which honours clap aliases/`visible_alias`; the row name is the canonical subcommand name (`get_name()`), so an aliased spelling still maps to the row.
- Bare family (`teton policy`): clap's `try_parse_from(["teton","policy"])` errors with "requires a subcommand" and lists them — reuse that text (BR-3: same error as the shell).
- OQ-1 (`/teton provider list`) is NOT implemented unless it is a one-line addition in `classify` (`/teton …` → treat as `teton …`); if added, test it; if not, leave OQ-1 open.
