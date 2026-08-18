---
id: TASK-169
title: "Ten mirrored slash rows: `Mirror`, `Args::Cli`, `run_mirrored` (typed-input gate → argv → `Cli::try_parse_from`), grouped `/help`"
status: complete
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-168]
repo: teton-code
---

## Description

Add the session rows `provider list`, `provider add`, `boundary list`,
`boundary add`, `policy show`, `policy set-tier`, `policy set-category`,
`model list`, `model status`, `doctor` (BR-1) in a new module
`crates/teton/src/cli_rows.rs`, each a one-line handler naming its shell twin
and calling `run_mirrored(twin, args, conn, ctx)`: write-gate (ADR-4) →
tokens (`twin` words ++ `args.split_whitespace()`, ADR-2) →
`Cli::try_parse_from` → clap error rendered as `LineKind::Error` lines, or
`crate::run_mirrored_command(cli.command, conn, ctx)`. Extend `CommandSpec`
with `mirror: Option<Mirror { shell, writes }>` and `Args` with `Cli`; group
the `/help` listing by family; generalize `model_set_gate` → `write_gate`
(same polarity, same seam).

## Files to Create/Modify

- `crates/teton/src/cli_rows.rs` — new: `Mirror`, `SHELL_ONLY: &[&str] = &["uninstall"]`, `write_gate`, `WriteGate`, `typed_only_line(row, shell)`, `run_mirrored`, the ten `handle_*` fns, `render_clap_error(err, surface)`, `cli_path(tokens) -> Vec<&'static str>` (clap-tree walk via `Cli::command()` + `find_subcommand`, honouring aliases — used by TASK-170; written here so its unit tests sit beside the table), unit tests.
- `crates/teton/src/slash.rs` — `CommandSpec.mirror`, `Args::Cli` (never rejects in `resolve`), ten `CommandSpec` rows placed beside their families (`provider setup`/`provider test` neighbours; `model`/`model set`), `render_help` groups rows by first word with a blank/heading line per family, `model_set_gate` → `cli_rows::write_gate` (keep `MODEL_SET_TYPED_ONLY`), `mod cli_rows;` wiring in `main.rs`.
- `crates/teton/src/main.rs` — `mod cli_rows;`.

## Acceptance Criteria

- [x] `/provider list`, `/boundary list`, `/policy show`, `/model list`, `/model status`, `/doctor` dispatch to the shared bodies from TASK-168 over the session's connection (unit: `RecordingSurface` + a scripted `Connection` fixture — promote `client::tests::test_connection` to a `pub(crate) #[cfg(test)]` constructor if needed — asserting the RPC method name sent and the rendered lines). — `cli_rows::tests::every_read_row_sends_its_shell_twins_method_on_the_sessions_connection` drives all six over `Connection::scripted` (new `#[cfg(test)] pub(crate)` constructor in `client.rs`, sharing `paired_for_test` with the existing `tests::test_connection`) and asserts the method read off the peer socket; `doctor_reports_the_connection_the_session_already_has` pins BR-7's one differing line.
- [x] `/policy set-tier build kimi --fallback local`, `/policy set-category edit kimi`, `/boundary add src/** --mode local-only`, `/provider add kimi --kind openai-compatible --endpoint https://x/v1/chat/completions --model kimi-k3` parse to the same `Command` value `Cli::try_parse_from(["teton", …])` yields for the shell argv (unit, no connection). — `a_row_parses_its_argument_exactly_as_the_shell_parses_its_argv`, compared over the whole parsed tree's `Debug` rendering rather than a chosen field.
- [x] `/policy set-tier build` and `/policy set-tier summit kimi` render exactly the lines `Cli::try_parse_from` reports for the same argv (AC-7), no RPC; `/policy set-tier --help` renders clap's help for that subcommand. — `a_bad_argument_renders_claps_own_error_and_sends_nothing`, `a_help_request_renders_claps_help_for_that_subcommand`. **Deviation:** clap's own `error: ` lead-in is stripped from the first line, because `LineKind::Error` *is* that prefix on a `PlainSurface` — rendering it verbatim would print `error: error: …`, which is not what the shell prints. Continuation lines (`Usage:`, the argument list, the tail) render as `LineKind::Info`, so the terminal output matches the shell's byte for byte apart from clap's blank lines, which are dropped.
- [x] Write rows on `typed_input == false` without the seam render one line naming the shell twin and send nothing (AC-4 unit); read rows ignore the gate; `write_gate(false, true) == Run` (seam polarity preserved — LESSON test in slash.rs `model_set_runs_only_from_a_terminal_or_under_the_test_seam` still green). — `a_write_row_on_a_pipe_names_its_shell_twin_and_sends_nothing` (all four rows, and no key read), `a_read_row_ignores_the_write_gate`, `the_write_gate_refuses_only_an_unseamed_pipe`; the `/model set` test keeps its name and now calls the generalized `cli_rows::write_gate` (the old `model_set_gate`/`ModelSetGate` are gone — one gate, not two).
- [x] `/help` lists all ten new rows (AC-8), grouped; `every_table_row_is_reachable_from_a_typed_command_line` and `help_lists_every_alias_that_dispatches` still green and now cover the new rows. — `help_lists_every_mirrored_row_grouped_by_family` reads the families back off the rendered listing; grouping is a blank line at each family boundary, where a family is a first word shared by more than one row (so `/model` groups with `/model list` and `/doctor` groups with nothing). `ARGUMENT_FOOTER` precedes `ESCAPE_FOOTER`, which is still last.
- [x] Completeness (ADR-8): a test walks `Cli::command()` recursively and asserts every leaf subcommand path is either a `COMMANDS` row name or in `SHELL_ONLY`; `provider add`/`test`, `policy set` (the hidden retired form) handled — the retired `policy set` is `hide = true`; treat hidden leaves as exempt or list it in `SHELL_ONLY` with a comment. — `every_cli_leaf_is_a_session_row_or_an_explicit_shell_only_exception`, walking via `cli_rows::leaf_command_paths()`. Hidden leaves are **exempt** (a command the CLI does not offer needs no session form; `SHELL_ONLY` means "visible and deliberately shell-only"), and the exemption is asserted narrowly — `hidden == ["policy set"]` — so a second hidden leaf still forces the decision. Both directions pinned: a stale `SHELL_ONLY` entry fails too.
- [x] Row summaries are one line each and name the shell twin nowhere except the typed-only refusal. — `a_mirrored_summary_is_one_line_and_names_no_shell_command`; the one-line half covers every row, the no-twin half covers the mirrored rows only (`/cost`'s summary has named `teton cost` since REQ-555, where that was the point).
- [x] Workspace fmt/clippy clean. — `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --no-fail-fast`: 2856 passed, 0 failed, 1 ignored across 57 targets (`cli_e2e` 39/39, `pty_e2e` 5/5).

## Technical Notes

- Handler signature is fixed: `fn(&mut Connection, &mut UiContext<'_>, &str) -> anyhow::Result<CommandOutcome>` (slash.rs:123). Ten one-liners, not a signature change (ADR-2 rejected alternative).
- clap error rendering: `err.render().to_string()` (plain, no ANSI), one `LineKind::Error` per non-empty line; `err.kind()` `DisplayHelp`/`DisplayVersion` are not errors — render as `LineKind::Info`.
- `cli.yes` from `try_parse_from` is a global flag; mirrored rows ignore it (none of the ten consults `--yes`); do not plumb it.
- Names must equal the subcommand path words joined by one space (`policy set-tier`) — ADR-1's recognition depends on it; add a unit test that every `Mirror.shell` equals `"teton " + row.name`.
- `/help` grouping: keep `ESCAPE_FOOTER` last; add one footer sentence that arguments are whitespace-split (ADR-2/OQ-5).

## Implementation notes (2026-08-18)

- **`run_mirrored` takes a `Mirror`, not `(twin, writes)`.** The sketched shape
  had each handler repeat its row's `writes` literal beside the table's own
  `Mirror { writes }` — two facts about one command that can disagree, and the
  compiler said so (`field writes is never read`). Each row's identity is now a
  single `const Mirror` in `cli_rows` (`POLICY_SET_TIER`, …), referenced by the
  table *and* by its handler, so there is nothing to keep in sync.
- **`run_mirrored_seamed`** takes `seams_allowed` rather than reading
  `test_seams_allowed()`, for the reason `write_gate` does: a unit test of the
  refusal must not depend on how `cargo test` was invoked.
- **A body's `Err` is rendered, not propagated** (one `LineKind::Error` line,
  `Continue`), including a transport failure: a `Connection` carries no typed
  "socket is gone" error to branch on, matching the wording is LESSON-456's
  mistake, and the errors these rows actually raise (a keychain that will not
  store, an endpoint the REQ-578 seam refused) are not session-ending. A real
  transport loss re-surfaces on the next call — the next prompt turn still ends
  the session through the entry loop's `?`.
- `cli_path(tokens: &[&str]) -> Vec<&'static str>` walks a `OnceLock`-memoized
  `Cli::command()`, which is what makes the `'static` names available for
  TASK-170's `Input::CliLine { name }` to borrow rather than allocate.
