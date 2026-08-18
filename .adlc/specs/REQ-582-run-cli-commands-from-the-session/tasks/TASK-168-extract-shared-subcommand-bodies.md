---
id: TASK-168
title: "Extract each mirrored subcommand's body to a shared `<sub>_on(conn, ctx, …)` the session can call"
status: draft
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: []
repo: teton-code
---

## Description

Split the shell subcommands the session will mirror into (a) the shell
wrapper — `stdout_surface`, `client::ensure_connected`, `passive_ctx` — and
(b) a shared body `<sub>_on(conn: &mut Connection, ctx: &mut UiContext<'_>, …)`
holding the RPC + renderer, so `/provider list` and `teton provider list`
run one function (REQ-582 BR-2, ADR-3). Also add `run_mirrored_command(cmd:
Command, conn, ctx)`, the exhaustive `match` from the ten mirrored `Command`
variants onto those bodies (ADR-2 step 3 calls it), and make the clap tree
(`Cli`, `Command`, `ProviderAction`, `BoundaryAction`, `PolicyAction`,
`ModelAction`, `CliProviderKind`, `CliPrivacyMode`, `CliTier`,
`CliCategory`) `pub(crate)` so `cli_rows.rs` (TASK-169) can name them.

CLI behaviour and **bytes are unchanged** — the existing `cli_e2e` suite is
the regression net; this task adds no rows and no recognition.

## Files to Create/Modify

- `crates/teton/src/main.rs` — extract `provider_list_on`, `provider_add_on`, `boundary_list_on`, `boundary_add_on`, `policy_show_on`, `policy_bind_on`, `model_list_on`, `model_status_on`, `doctor_report_on(paths, conn, ctx, attach: DoctorAttach)`; `run_*` wrappers call them; `read_secret(id, prompter: &mut dyn Prompter)`; `ProviderAddRefusal` enum (remote-without-model, duplicate id, no key) returned by `provider_add_on` and mapped by `run_provider_add` back to the exact `bail!` sentences; `run_mirrored_command`; `pub(crate)` on the clap tree; `DoctorAttach::{Fresh(HandshakeResult), Session}` deciding only the `daemon: running — …` line (ADR-5).
- `crates/teton/src/client.rs` — nothing new expected; if `Connection` needs to expose the protocol version beside `daemon_version()` for the session-doctor line, add the accessor here.

## Acceptance Criteria

- [ ] `teton provider list`, `teton provider add`, `teton boundary list|add`, `teton policy show|set-tier|set-category`, `teton model list|status`, `teton doctor` print byte-identical output before/after (existing `cli_e2e` tests green; run `cargo test -p teton --test cli_e2e` after `cargo build --workspace` — LESSON-510/BUG-164: a targeted run does not rebuild the daemon).
- [ ] Every `<sub>_on` takes `&mut Connection, &mut UiContext<'_>` and creates neither a surface nor a connection.
- [ ] `provider_add_on` returns `Result<(), ProviderAddRefusal>`-shaped outcomes for the three refusals; `run_provider_add` still exits non-zero with the same three sentences (pin with the existing tests that assert them; add one if none does).
- [ ] `read_secret` prompts through the passed prompter; `TETON_PROVIDER_KEY` still short-circuits; unit test with `ScriptedPrompter` proves the session-side path uses `ask_secret` (echo-off), never `ask`.
- [ ] `run_mirrored_command` matches all ten mirrored variants exhaustively; every other `Command` variant renders one Error line ("not a session row") rather than panicking.
- [ ] `doctor_report_on` with `DoctorAttach::Fresh` reproduces `teton doctor`'s bytes; with `DoctorAttach::Session` the only differing line is the `daemon: running — teton-code X (this session's connection)` line (unit test with `RecordingSurface` over both arms).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` clean.

## Technical Notes

- Precedent: `handle_cost` → `crate::query_and_render_cost(conn, ctx)` and `handle_model_set` → `crate::apply_model_set(...)` (slash.rs ~696, ~812) — the same shape, ten more times.
- `run_provider_add` (main.rs ~1373): keep the ORDER — model check → duplicate probe → `settle_registration` (needs `ctx.surface`) → `read_secret` → `PriorKey::read` → `registration_params` → call → `report_registration_outcome` (BUG-155, BUG-171, REQ-578 all pin that order; read the comments there before moving anything).
- `run_doctor` (~1223) connects with `Connection::connect` + `handshake()` itself; keep that in the wrapper for the shell path and pass `DoctorAttach::Fresh(hs)`; the "daemon: not running" and "rejected this CLI" arms stay in the wrapper (a session cannot be in those states).
- Do not touch slash.rs beyond what compiles; TASK-169 adds the rows.
