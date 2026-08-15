---
id: TASK-155
title: "CLI: provider_setup_ui walkthrough + `provider setup` COMMANDS row"
status: draft
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-152"]
---

## Description

The client half. A new `crates/teton/src/provider_setup_ui.rs` mirroring `web_setup_ui.rs`: TTY gate (walk vs printed instructions), `plan` RPC, lenient vendor resolution against the plan's catalog (ADR-2), model prompt defaulting to `example_model` (labeled example), endpoint handling (skip for `anthropic`; otherwise offer the recipe endpoint, accept a base URL, compose+echo before the key prompt via the shared `settle_endpoint` core — ADR-8), `ask_secret` for the key, routing checklist over `plan.tiers` with the tier argument (or `think`) pre-selected, `preview` RPC, render (indented TOML, dial host, warnings, `replaces` line), confirm `[y/N]`, then `PriorKey::read` → keychain store → `commit` RPC with `expect_digest`; on refused commit run the shared undo and report; on transport error leave the keychain alone and say how to verify. Cancel at any prompt writes nothing. Register the `provider setup` row in `COMMANDS` so dispatch and `/help` come from one table (REQ-555 BR-7). Args: optional `<vendor>` and optional `<tier>`.

## Files to Create/Modify

- `crates/teton/src/provider_setup_ui.rs` — new; `pub fn run(conn, ctx, keychain, vendor: Option<&str>, tier: Option<&str>) -> anyhow::Result<()>`; `enum Gate { Walk, Instructions }` via the same typed-input predicate `web_setup_ui` uses; `fn resolve_vendor(catalog, arg) -> Resolution { One(entry) | Many(Vec) | None }` (pure, tested); `fn instruction_lines(catalog, vendor, tier) -> Vec<String>` (pure: the CLI recipe for BR-11); `fn render_preview(&ProviderSetupPreviewResult) -> Vec<String>` (pure, plain text, no ANSI — LESSON-517); the drive fn over the `SetupIo` seam
- `crates/teton/src/slash.rs` — `CommandSpec { name: "provider setup", args: Args::Optional("[vendor] [tier]"), handler: handle_provider_setup, .. }` beside `web setup`; `handle_provider_setup` parses up to two whitespace-separated args, gets `default_keychain()`, calls `provider_setup_ui::run`; typed-input-only like `/model set` and `/web setup`
- `crates/teton/src/main.rs` — make `settle_endpoint`'s pure compose+echo core `pub(crate)` (or lift it to a small `pub(crate) fn settle_endpoint_text(kind, input) -> Result<Settled, String>`) so the UI calls it rather than mirroring it (LESSON-528)
- `crates/teton/src/lib.rs` or `main.rs` module list — `mod provider_setup_ui;`
- `crates/teton/src/provider_setup_ui.rs` (tests) — `resolve_vendor`: `kimi`, `Kimi`, `moonshot`, `Moonshot (Kimi)`, `Moonshot/Kimi` → the same entry; `deep` → None; an arg matching two labels → Many; `instruction_lines` for `kimi think` names `teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3` and `teton policy set-tier think kimi`; `render_preview` has no ANSI bytes; a scripted `SetupIo` walk (fake keychain, fake connection returning canned plan/preview/commit): happy path stores key AFTER confirm and sends `expect_digest`; cancel at each of the five prompts → no store, no commit call, config untouched; refused commit on a fresh key → keychain entry deleted; refused commit on rotation → prior value restored; both outcomes appear in surface text; anthropic vendor → no endpoint prompt asked (count prompts); tier arg `build` → `build` pre-selected, no arg → `think`
- `crates/teton/src/slash.rs` (tests) — `/help` lists `provider setup`; piped/non-typed input for `/provider setup kimi` yields the instruction lines and consumes no stdin (mirror the `/model set` typed-input-only test)

## Acceptance Criteria

- [ ] `/provider setup kimi think` in a TTY session walks vendor → model → key → routing → preview → confirm; on `y` the key is in the fake keychain under account `kimi` and the commit params carry `key_ref = keychain://teton/kimi` and the preview's digest
- [ ] The key never appears in any `SetupIo` surface line, in the plan/preview/commit params other than as a `keychain://` reference, or in any log the flow writes (assert by scanning captured output for the fake key bytes — LESSON-519)
- [ ] All cancel/refuse/rotate paths in the tests above hold; instruction lines match `teton provider add` syntax exactly (a test builds the line and runs the CLI's own arg parser over it)
- [ ] `/help` derives the row from `COMMANDS`; no hand-maintained help text
- [ ] `cargo test -p teton` green; clippy clean

## Technical Notes

`web_setup_ui.rs` (~2840 lines) is the template — read `run`/`drive` (L332–501), the `Gate` (L212–241), the `SetupIo` seam, and how it orders "store key → commit → undo on refusal". Reuse `PriorKey` and the undo fn from `keychain.rs` (BUG-171 moved them there — do NOT reimplement). Keychain account = provider id, ref = `keychain::auth_ref_for(id)` (ADR-5). Vendor resolution normalises both sides with `to_ascii_lowercase` and drops non-alphanumerics; do not import a fuzzy-match crate. Anthropic: skip the endpoint prompt; the preview's TOML shows the composed default. If `settle_endpoint` cannot be cleanly split, add a thin `pub(crate)` wrapper in main.rs the UI calls — the rule is one compose seam, not zero refactors.
