---
id: TASK-169
title: "Ten mirrored slash rows: `Mirror`, `Args::Cli`, `run_mirrored` (typed-input gate → argv → `Cli::try_parse_from`), grouped `/help`"
status: draft
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

- [ ] `/provider list`, `/boundary list`, `/policy show`, `/model list`, `/model status`, `/doctor` dispatch to the shared bodies from TASK-168 over the session's connection (unit: `RecordingSurface` + a scripted `Connection` fixture — promote `client::tests::test_connection` to a `pub(crate) #[cfg(test)]` constructor if needed — asserting the RPC method name sent and the rendered lines).
- [ ] `/policy set-tier build kimi --fallback local`, `/policy set-category edit kimi`, `/boundary add src/** --mode local-only`, `/provider add kimi --kind openai-compatible --endpoint https://x/v1/chat/completions --model kimi-k3` parse to the same `Command` value `Cli::try_parse_from(["teton", …])` yields for the shell argv (unit, no connection).
- [ ] `/policy set-tier build` and `/policy set-tier summit kimi` render exactly the lines `Cli::try_parse_from` reports for the same argv (AC-7), no RPC; `/policy set-tier --help` renders clap's help for that subcommand.
- [ ] Write rows on `typed_input == false` without the seam render one line naming the shell twin and send nothing (AC-4 unit); read rows ignore the gate; `write_gate(false, true) == Run` (seam polarity preserved — LESSON test in slash.rs `model_set_runs_only_from_a_terminal_or_under_the_test_seam` still green).
- [ ] `/help` lists all ten new rows (AC-8), grouped; `every_table_row_is_reachable_from_a_typed_command_line` and `help_lists_every_alias_that_dispatches` still green and now cover the new rows.
- [ ] Completeness (ADR-8): a test walks `Cli::command()` recursively and asserts every leaf subcommand path is either a `COMMANDS` row name or in `SHELL_ONLY`; `provider add`/`test`, `policy set` (the hidden retired form) handled — the retired `policy set` is `hide = true`; treat hidden leaves as exempt or list it in `SHELL_ONLY` with a comment.
- [ ] Row summaries are one line each and name the shell twin nowhere except the typed-only refusal.
- [ ] Workspace fmt/clippy clean.

## Technical Notes

- Handler signature is fixed: `fn(&mut Connection, &mut UiContext<'_>, &str) -> anyhow::Result<CommandOutcome>` (slash.rs:123). Ten one-liners, not a signature change (ADR-2 rejected alternative).
- clap error rendering: `err.render().to_string()` (plain, no ANSI), one `LineKind::Error` per non-empty line; `err.kind()` `DisplayHelp`/`DisplayVersion` are not errors — render as `LineKind::Info`.
- `cli.yes` from `try_parse_from` is a global flag; mirrored rows ignore it (none of the ten consults `--yes`); do not plumb it.
- Names must equal the subcommand path words joined by one space (`policy set-tier`) — ADR-1's recognition depends on it; add a unit test that every `Mirror.shell` equals `"teton " + row.name`.
- `/help` grouping: keep `ESCAPE_FOOTER` last; add one footer sentence that arguments are whitespace-split (ADR-2/OQ-5).
