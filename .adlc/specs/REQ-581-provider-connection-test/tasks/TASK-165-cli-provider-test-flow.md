---
id: TASK-165
title: "CLI: provider_test_ui — preview, confirm, call, typed report; /provider test and teton provider test; teton cost probes"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: [TASK-160]
---

## Description

One flow module with one seam, two call sites (BR-7), the confirm gate (BR-2),
the typed report (BR-3/BR-5), and the `teton cost` probe line.

## Files to Create/Modify

- `crates/teton/src/provider_test_ui.rs` (new) — `pub(crate) trait TestIo { surface, prompter, config_get(...) , provider_test(ProviderTestParams) }` (REQ-579 `SetupIo` shape); `pub(crate) fn run(io, session_id, provider_id, auto_yes: bool, typed_input: bool) -> anyhow::Result<()>`: fetch `config/get`; unknown id → error line naming registered ids; local kind → the daemon's own refusal (call the method; render its `INVALID_PARAMS` message); preview lines (`provider: kimi (openai-compatible, kimi-k3) — <stored endpoint>` and `this sends one minimal request (≈ 20 tokens) to that endpoint. proceed? [y/N]`); non-TTY without `--yes` → one line "needs a terminal or --yes; nothing was sent" and no RPC; `--yes` skips the question; `n`/EOF → "nothing was sent" and no RPC; on `y` call `provider/test` and render: `Reached` → `<id> <model>: reachable — answered in <latency> (<in> in / <out> out, <$x recorded | unpriced>); provider health: <health>.` plus one line naming the tiers/categories bound to it from the snapshot ("`build` routes here (edit, shell)" — reuse the snapshot's resolved routing; skip the line if nothing routes there, saying so); `Refused`/`UnknownModel`/`ServerError`/`Unreachable` → `<id> <model>: <verb> — <reason>. Nothing else was sent.` + the remedy line for `Refused` (`/provider setup <id>` to store a new key, or `teton provider add <id> --model <model>`); `RateLimited` → "…rate limited; try again shortly"; RPC error → `prompt failed`-class error line. Unit tests with a `RecordingSurface` + scripted prompter + canned answers: preview text; `n` → zero `provider_test` calls; `--yes` → one call, no question; non-TTY without yes → zero calls; each outcome's line (assert wording by variant, never by prose parsing); the credential value never appears (feed a reason that names `keychain://teton/kimi` and assert that is what prints).
- `crates/teton/src/slash.rs` — `COMMANDS` row `provider test` (`Args::Required`, summary "Test a registered provider with one consented call: /provider test <id>"); `handle_provider_test` → `provider_test_ui::run(...)` with `ctx.auto_accept_model` as `auto_yes` and `ctx.typed_input`; the `/help` table test picks the row up automatically (REQ-555 BR-7 test) — assert `/provider test` is listed.
- `crates/teton/src/main.rs` — `ProviderAction::Test { id }`; dispatch arm → `run_provider_test(&paths, &id, cli.yes)`: `ensure_connected` (passive ctx is not enough — this sends), `session/create` freeform with cwd, then `provider_test_ui::run` with a `StdinPrompter` and `typed_input = stdin is a TTY`; the session ends with the process.
- `crates/teton/src/session_ui.rs` — `render_event` arm for `Event::ProviderTested` (a `Notice`, elsewhere-aware like `format_provider_setup_completed`: "provider `kimi` tested in another session (…)"); `format_provider_tested`; unit test.
- `crates/teton/src/cost_ui.rs` — when `report.probe_calls > 0` render `  probes: N connection test(s) — billed like any call, counted apart` after the totals; unit test on `render_report_view` with `probe_calls: 1` and `0`.
- `crates/teton/src/main.rs` (module list) — `mod provider_test_ui;`.

## Acceptance Criteria

- [ ] `cargo test -p teton --bin teton` green with the new tests; `/help` lists `/provider test`.
- [ ] Decline, EOF, and non-TTY-without-`--yes` each make **zero** `provider/test` calls (asserted on the `TestIo` double).
- [ ] Every `ProviderTestOutcome` variant renders a distinct line; no line contains a credential value; the reason is printed verbatim.
- [ ] `teton cost` output gains the probes line only when `probe_calls > 0`.

## Technical Notes

Copy `provider_setup_ui.rs`'s `SetupIo` + `is_yes` + `CONFIRM_QUESTION` shape and its test doubles. The preview shows the *stored endpoint* verbatim (that is what Teton POSTs, REQ-578) and the report shows the daemon's `dial_host` (LESSON-529: no client-side host parsing). `auto_yes` for the session is `UiContext::auto_accept_model` (the one `--yes` flag, REQ-555 BR-4b precedent). Read `read_config_view` (main.rs) for how the snapshot is fetched and cached; the tiers/categories bound to a provider are in `ConfigSnapshot`'s resolved routing (`TierRouteView`/`CategoryRouteView`).
