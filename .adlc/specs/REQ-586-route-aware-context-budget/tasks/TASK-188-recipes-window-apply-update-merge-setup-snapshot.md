---
id: TASK-188
title: "Recipes carry max_context; apply_update merges window fields field-wise; /provider setup writes the candidate's window; snapshot projects window"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-181"]
repo: teton-code
---

## Description

Make the window declarable and visible through the one `RegisterProvider`
path (ADR-7, BR-3, AC-5): vendor recipes carry a verified `max_context`;
`apply_update` merges `max_context`/`context_budget_cap` field-wise (`None`
preserves); the `/provider setup` commit writes the candidate's window; the
config snapshot always populates `max_context` (`Some(0)` = unknown).

## Files to Create/Modify

- `crates/tetond/src/provider_recipes.rs` — `ProviderRecipe.max_context: u32` (L101-150) with a per-vendor value verified against vendor docs and a "Verified <date>" comment (Anthropic `claude-opus-5` 200_000; OpenAI `gpt-5.6`, Moonshot `kimi-k3`, DeepSeek `deepseek-v4-pro`, xAI `grok-4.6` — verify each against the vendor's model page and cite it; Ollama `llama3.2` — the served default `num_ctx` is small and model-declared: use the conservative served default and say so in the note); `recipe_entries()` (L324-337) projects it; `the_catalog_ships_the_six_vendors_verbatim` (L371) golden updated; new contract test `no_recipe_ships_an_unknown_window` (every `max_context > 0`); `every_recipe_maps_onto_a_wire_entry_field_for_field` (L1019) destructures the field; the registration sweep (L677-684) registers with the recipe's window; `no_field_carries_anything_secret_shaped` (L896, `ALWAYS_PRESENT_PER_RECIPE` L926) updated.
- `crates/tetond/src/runtime.rs` — `apply_update` `RegisterProvider` arm (L9936-9964): `max_context: update.max_context.unwrap_or(existing.max_context)`, likewise `context_budget_cap` — preserve when `None`, write when `Some` (0 allowed = "unknown"); config→wire projection (L9785-9791): `max_context: Some(p.capabilities.max_context)`, `context_budget_cap: Some(..)`; `derive_provider_setup` (L5261-5366, esp. L5355-5366): the candidate's `max_context` (recipe default when the UI sent none); preview TOML shows the `max_context = N` line (tests at L21639/L22764 already assert such lines from fixtures).
- `crates/tetond/src/harness/docs/providers.md` — one sentence (≤ 300 B): "Declare the model's context window (`--max-context`); unknown = the default budget, stated by `/doctor`." (topic ceiling 4 KiB — `every_bundled_topic_is_under_the_ceiling`).
- Tests: `crates/tetond/tests/provider_setup_flow.rs` (L763-767 preserves hand-authored 200000 — also assert a fresh setup writes the recipe window), `provider_setup_contracts.rs` `every_recipe_reaches_the_plan_field_for_field` (L147), `config_preservation.rs:811` (register via wire without the fields → stored `max_context` untouched), `runtime.rs` `config_snapshot_round_trips_kinds_and_modes` (L11656).

## Acceptance Criteria

- [ ] AC-5 (daemon half): a `RegisterProvider` without the fields preserves a stored `max_context = 200000`; with `Some(128000)` writes it; the snapshot for a provider with no capabilities table reads `max_context: Some(0)`; every recipe has a non-zero window and the contract test pins it.
- [ ] `cargo test -p tetond provider_recipes runtime::tests --test provider_setup_flow --test provider_setup_contracts --test config_preservation` green; README/guide/topic ↔ catalog gates in `web_setup_contracts.rs` (L968, L1151, L1354) green (update README rows if they enumerate recipe fields).

## Technical Notes

- Tracer gotcha #6 (field-wise merge) and #11 (recipe pins). REQ-577 both-halves rule: cite the vendor doc per window.
- Commit as `feat(daemon): declare and carry the provider window — recipes, register, setup, snapshot [TASK-188]`.
