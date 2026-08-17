---
id: TASK-153
title: "Daemon: provider_setup_plan + provider_setup_preview runtime fns and handlers, gated like web/setup_*"
status: complete
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-152"]
---

## Description

Implement the two read-only halves of the trio in the daemon. `plan` returns the recipe catalog (mapped 1:1 from `provider_recipes::recipe_catalog()`), the ids of providers already registered, and every routable tier with its current binding. `preview` takes a candidate, builds candidate `Config` = current + provider row (by identity, replacing an existing id if present) + one tier binding per requested tier, composes the endpoint via `teton_core::compose_endpoint`, validates, renders the exact TOML delta through the REQ-574 writer in memory, digests it, computes `dial_host` with the dial-time authority parser, and collects warnings (`replaces existing provider <id> (model a → b)`, unpriced model, cleartext endpoint). Both handlers take `refuse_unmintable_session_id` + `may_drive` and refuse foreign callers in-response only (no event — LESSON-513).

**Covers:** AC-5, AC-6 (compose + URL-shape refusal on the daemon side), AC-12 (replaces + warning)

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `pub fn provider_setup_plan(&self) -> ProviderSetupPlanResult`; `pub fn provider_setup_preview(&self, c: &ProviderSetupCandidate) -> Result<RenderedProviderSetup, RpcError>` (a private struct carrying `toml, digest, dial_host, warnings, replaces, candidate_config`) — factor the candidate-build + validate + render into `fn derive_provider_setup(&self, c) -> Result<RenderedProviderSetup, RpcError>` so TASK-154's commit calls the same fn (BR-3, BR-9); reject a `key_ref` that does not parse as a keychain reference with `PROVIDER_SETUP_INVALID`
- `crates/tetond/src/provider_recipes.rs` — `pub fn recipe_entries() -> Vec<ProviderRecipeEntry>` mapping every `ProviderRecipe` field-for-field; contract test asserting the mapping is total (every recipe, every field equal)
- `crates/tetond/src/server.rs` — `handle_provider_setup_plan`, `handle_provider_setup_preview` beside the `handle_web_setup_*` fns; add both to the sync `dispatch()` match; both gated by `refuse_unmintable_session_id` + `may_drive`; foreign caller → `NOT_ATTACHED` (the existing code `web/setup_*` uses for a foreign caller — there is no `SETUP_REJECTED_NONUSER`) in the response, no event
- `crates/tetond/src/runtime.rs` (tests) — unit tests: plan catalog == `recipe_entries()`; existing ids listed; tiers list every routable tier with current binding; preview of a fresh `kimi` candidate renders a `[[providers]]` row + `[policy.tiers.think]` and a stable digest; same candidate twice → same digest; a candidate whose `id` exists → `replaces` populated + warning; missing `model` on a remote kind → `PROVIDER_SETUP_INVALID` before anything else; a base URL `https://api.moonshot.ai/v1` composes to `…/v1/chat/completions` in the rendered TOML; a backslash-in-authority URL is refused; `key_ref = "sk-…"` (raw) is refused; `dial_host` equals the host the request builder would dial

## Acceptance Criteria

- [ ] `provider/setup_plan` from the session's own connection returns catalog (== recipes), existing ids, tiers; from a foreign connection returns `NOT_ATTACHED` (the existing code `web/setup_*` uses for a foreign caller — there is no `SETUP_REJECTED_NONUSER`) and emits no event
- [ ] `provider/setup_preview` returns TOML whose bytes, if written, would be exactly what commit writes (same `derive_provider_setup`); digest is deterministic for a given (current config, candidate)
- [ ] The candidate config is built by identity (`id`), never by array position (LESSON-522)
- [ ] All unit tests listed above pass; `cargo clippy -p tetond --all-targets` clean

## Technical Notes

Read `web_setup_preview` / `web_setup_commit` in runtime.rs (~L4000–4175) first — the "candidate rebuilt and re-validated, digest taken over the rendered text, writer handed the digested bytes as-is" shape is the contract. Reuse whatever helper renders the delta for web (search `render` near `web_setup_preview`); if it is web-specific, factor a small generic "render candidate vs current" helper rather than a second renderer. `dial_host`: use the same authority split the transport uses (LESSON-529 — grep `authority` in teton-providers/teton-core; do NOT write a new URL parser). Warnings are `Vec<String>` of plain sentences — no ANSI (LESSON-517). Unpriced-model warning: reuse the price-table lookup the cost meter uses.
