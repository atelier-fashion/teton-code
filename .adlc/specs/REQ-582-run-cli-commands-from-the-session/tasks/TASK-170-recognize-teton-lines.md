---
id: TASK-170
title: "Recognize a `teton …` line typed at the prompt: `Input::{CliLine, CliRefused}`, entry-loop arms, totality tests"
status: complete
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

- [x] `classify` remains pure and total; the extended both-directions tests pass; every existing classify test unchanged.
      — `classify` gained one call (`cli_line`, itself pure and total) and no state; every REQ-555 classify test is byte-unchanged. Ten new unit tests in `slash.rs`: every row whose name is a subcommand path is reachable from `teton <row>` and resolves to the same row a `/` line does; the argument is what the path did not consume (the spec's four examples plus `model set`/`provider test`); `teton uninstall`, bare `teton`, `--help`/`-h`/`--version`/`-V`, `teton provider`/`policy`/`boundary`, `teton policy set …` → `CliRefused`; `teton is slow today`, `tetonx …`, `teton-code …`, `Teton …`, `teton help me read this backtrace` → byte-identical `Prompt`.
- [x] `cli_e2e` AC-5/AC-6 tests as listed; assert absence of a model call by the scripted-reply-queue argument (deterministic), not a timer.
      — `a_typed_teton_line_runs_the_row_it_names_and_costs_no_turn` diffs the two sessions' bodies and calls `assert_no_turn_ran` on both; `a_teton_line_with_no_session_form_is_refused_and_a_question_still_reaches_the_model` drives four lines through one session and pins the reply queue at exactly two turns (replies 1 and 2 present, reply 3 absent).
- [x] `//teton provider list` still prompts the model with `/teton provider list` (REQ-555 BR-1b unaffected).
      — unit (`the_double_slash_escape_still_outranks_recognition`) and e2e (the escaped line is one of the two turns that ran).
- [x] The Notice line reads `teton provider list → /provider list` (rendered by the surface as `>> …`).
      — `main::cli_line_note`; the e2e asserts the rendered `>> teton provider list → /provider list` and that the `/` spelling prints no such line.
- [x] No `std::process::Command` and no second `Connection` anywhere in the recognition path (grep test or review note).
      — review note: recognition ends in `slash::dispatch`, the same table lookup a `/` line reaches, on the session's own `Connection`; the arm in `main.rs` carries the invariant as a comment. `slash.rs` and `cli_rows.rs` name neither `std::process` nor `Connection::connect`.

## Technical Notes

- Aliases: `cli_path` uses `find_subcommand`, which honours clap aliases/`visible_alias`; the row name is the canonical subcommand name (`get_name()`), so an aliased spelling still maps to the row.
- Bare family (`teton policy`): clap's `try_parse_from(["teton","policy"])` errors with "requires a subcommand" and lists them — reuse that text (BR-3: same error as the shell).
- OQ-1 (`/teton provider list`) is NOT implemented unless it is a one-line addition in `classify` (`/teton …` → treat as `teton …`); if added, test it; if not, leave OQ-1 open.

## Deviations from the plan (recorded at implementation, LESSON-533)

1. **A bare family does not get clap's rendered error.** The plan assumed
   `Cli::try_parse_from(["teton","provider"])` produces the short "requires a
   subcommand … [subcommands: …]" error. It does not: clap's derive marks a
   required subcommand `arg_required_else_help`, so the result is
   `DisplayHelpOnMissingArgumentOrSubcommand` — the **whole help page** for that
   family, whose longest line is the global `--yes` description and whose
   `Usage:`/"try '--help'" tail is a shell's instruction. BR-4 and ADR-1 both
   say *one* refusing line, and `Surface::line` owns exactly one row (it
   flattens newlines). So `cli_rows::refusal_for_path` composes one line —
   and composes it from the **table** (`slash::rows_under`) rather than the
   tree: `` `teton provider` is a family rather than a command — in this
   session: /provider setup, /provider test, /provider list, /provider add. ``
   That names the session-only `/provider setup`, which the CLI has no
   subcommand for at all and which is the likeliest thing a user typing
   `teton provider …` wants (BR-4's own note about it), and it keeps BR-3's
   real property: no list maintained twice.
2. **`teton model` is a `CliLine`, not a refusal.** The task listed `model`
   among the bare families, but `model` *is* a row (`/model`, REQ-555's
   one-line current-model answer), so ADR-1's first arm applies and the row
   runs. A shell prints the family's help for those words because a shell has
   no `/model`. Pinned by `a_family_word_that_is_itself_a_row_runs_that_row`.
3. **`teton provider setup` is refused, not prompted.** BR-4 says a session-only
   command "is a plain prompt". Under ADR-1's amended rule the walk stops on the
   real family `provider`, so the line is intercepted — and the refusal names
   `/provider setup`, which is strictly better than sending it to a model whose
   guide tells it to say "run that yourself" (the failure this REQ removes).
4. **The argument is taken by counting the path's words, not by
   `match_name_words(rest, name)`.** `cli_path` honours clap aliases, so an
   aliased spelling resolves to a row whose name does not prefix the typed line;
   matching the row's name would silently drop that line's argument.
   `after_words(rest, path.len())` cannot. No alias exists in the tree today, so
   the two agree on every current input; the unit test pins the mechanism.
5. **OQ-1 implemented** (it was one line) and marked RESOLVED in the
   requirement.
