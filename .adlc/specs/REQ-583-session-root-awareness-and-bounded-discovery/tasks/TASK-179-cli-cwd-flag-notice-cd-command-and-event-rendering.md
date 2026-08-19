---
id: TASK-179
title: "CLI: --cwd, banner root, the not-a-project notice, /cd, and session_root_changed rendering"
status: complete
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-174", "TASK-175"]
---

## Description

Leg B's client half (BR-5..BR-8, AC-8, AC-9's CLI half, AC-10's rendering,
AC-11, AC-12) per `architecture.md` ADR-4/ADR-5. **File ownership (parallel
tier):** `crates/teton/src/**` and `README.md`. Unit tests only in this task;
the socket-level e2e legs are TASK-180 (they need TASK-178's daemon).

## Files to Create/Modify

- `crates/teton/src/main.rs` — `Cli.cwd: Option<String>` (`#[arg(long, value_name = "PATH")]`, **not** `global`; doc: "Session root for this session — the directory tools are scoped to — instead of the shell's directory"); resolve once in `main()` via `teton_core::session_root::resolve_cwd_argument(raw, &current_dir, home)`; thread to `run_session(...)` and to `run_provider_test` (the two `SessionCreateParams` sites L914, L2160 send `cwd: Some(resolved)` or today's `current_dir()`); banner call passes `display_for(session_root_path, home)` (not `banner::cwd_display()`); a refused `session/create` prints the daemon's message on `LineKind::Error` and returns a non-zero exit (`anyhow::bail!`-shaped at the binary edge, one line, no session output after it); after a successful create and before the ready line, `if interactive { if let Some(n) = banner::root_notice(root) { surface.line(LineKind::Notice, &n) } }` using `SessionCreateResult.root`; store `root` in `SessionState.root`. Parse tests beside `main.rs:3539` for `--cwd`.
- `crates/teton/src/banner.rs` — `pub fn root_notice(root: &SessionRoot) -> Option<String>` (`None` for `Project`; one line otherwise: `Not inside a project — tools are scoped to {display} ({kind phrase}): every search walks all of it, and privacy boundaries declared for a project do not apply here. Run teton from the project, `teton --cwd <path>`, or `/cd <path>` here.`; for `filesystem_root` say "the whole filesystem"); `cwd_display()` becomes a thin wrapper over `teton_core::session_root::display_for(current_dir, HOME)` (one spelling). Tests: AC-8 (`None` for project; each other kind names display, consequence, `--cwd`, `/cd`; the ≤ 60-char banner-line test is unaffected because the notice is not a banner line).
- `crates/teton/src/session_ui.rs` — `SessionState.root: Option<SessionRoot>`; replace TASK-174's placeholder arm: `Event::SessionRootChanged` → this session: `session root is now {display} ({kind phrase})` (+ `format_context_cleared` is already drawn by the `ContextCleared` event that precedes it — do not print it twice), then `banner::root_notice` if any (BR-8); another session: `session root moved in another session ({id})`; update `state.root`. Tests in the L3899-3980 style: this-session line, elsewhere line, notice re-fire for `Home`, no notice for `Project`.
- `crates/teton/src/slash.rs` — `CommandSpec { name: "cd", aliases: &[], summary: "Move this session's root — the directory tools are scoped to; clears the conversation. Bare form prints the current root.", args: Args::Optional, mirror: None, handler: handle_cd }` beside `clear`; `handle_cd`: no arg → print `session root: {display} ({kind phrase})` from `ctx.state.root` (or a "no root known yet" notice); with arg → `resolve_cwd_argument(arg, current_dir, home)` (error → `LineKind::Error`, no RPC) → `SessionSetCwdParams { session_id, cwd }` → on `Ok` print nothing (the events draw the lines); on `Err`: `METHOD_NOT_FOUND` → notice "this daemon build cannot move a session root" (the `CLEAR_UNAVAILABLE` shape), `SESSION_BUSY` → notice, else `LineKind::Error` with the daemon's message (`report_clear_refusal` shape). Add `"cd"` to the promised list in `the_table_carries_every_command_this_req_promises` (L2362-2441); help-render tests re-snapshot if they pin counts. `/cd` must NOT be a `CliLine` mirror.
- `crates/teton/src/client.rs` — only if `SessionState` is not reachable from the event arm (it is, via `ctx.state`) — otherwise untouched.
- `README.md` — slash table row for `/cd`; a line for `--cwd`.
- AC-12: one grammar table (reuse teton-core's `resolve_cwd_argument` test table by name) drives a `main.rs` `--cwd` parse test and a `slash.rs` `/cd` argument test.

## Acceptance Criteria

- [x] `cargo test -p teton` green: AC-8 notice content, `--cwd` parse/resolve legs (`rel`, `~/x`, `/abs`, empty → error), `/cd` bare-form line, `/cd` refusal rendering, `session_root_changed` this-session/elsewhere/notice-refire tests, promised-list test. (`cargo test -p teton --bin teton`: 539 passed.)
- [x] `LEADING_GLOBAL_FLAGS` pin (`slash.rs:4468`) unchanged — `--cwd` is not global (`cwd_flag_is_not_global` + the two-way pin both green).
- [x] Piped/non-TTY output byte-identical to today when no `--cwd` is given (the notice and banner are TTY-gated; existing cli_e2e byte-parity tests green: `cargo test -p teton --test cli_e2e slash_quit` — and the full `cli_e2e` suite, 50 passed, against a daemon built from the tree at that moment).
- [x] The banner's `cwd:` line shows the session root when `--cwd` differs from the shell cwd (`banner::print` is handed `cwd_display(session_root)`, the resolved `--cwd`).

## Technical Notes

- Do not print the `context_cleared` line from the `/cd` handler or the new arm — the `ContextCleared` event already renders it (BR-7's "existing shape").
- `SessionCreateResult.root` may be `None` from an older daemon — the notice and `/cd` bare form must degrade to a one-line notice, not panic.
- Commit as `feat(cli): --cwd, the not-a-project notice, /cd, and session-root lines [TASK-179]`.

## Implementation notes (2026-08-18)

- A refused `session/create` in `run_session` is `anyhow::bail!("could not start a session: …")` — `main` prints it once on stderr (`teton: …`) and exits non-zero (REQ-582 ADR-3's refused-argument shape), rather than a `LineKind::Error` surface line followed by a second print. `run_provider_test`'s refusal path is unchanged (its doc and an e2e pin "reported on the surface, returns Ok").
- `banner::cwd_display` now takes the session root path (`cwd_display(&Path) -> String`) and is a thin wrapper over `teton_core::session_root::display_for(path, HOME)`; `main` passes the resolved `--cwd` or the shell cwd. `banner::root_line(&SessionRoot)` is the one `{display} ({kind phrase})` spelling the notice, the `session_root_changed` line and `/cd`'s bare form share.
- `main::session_root_for(cwd_flag, shell_cwd, home)` is the pure resolver the `--cwd` grammar test drives; `main::home_dir()` is the one `HOME` read (banner, `--cwd`, `/cd`).
- The `/cd` grammar test maps the table's two empty-argument rows to the bare form (a read), because unlike `--cwd` an empty `/cd` has something useful to do; every non-empty row resolves to the table's path through the same `resolve_cwd_argument`.
