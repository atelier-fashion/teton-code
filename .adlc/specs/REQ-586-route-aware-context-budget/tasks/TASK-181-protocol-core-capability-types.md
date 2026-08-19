---
id: TASK-181
title: "Protocol + core + capability types: wire ProviderConfig window fields, RouteDecided budget/bound, ContextPressure event, BudgetBound, recipe/candidate max_context, config context_budget_cap"
status: complete
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: []
repo: teton-code
---

## Description

Add the additive type surface every other task builds on — no behaviour. Wire
`ProviderConfig` gains `max_context`/`context_budget_cap`; `RouteDecided` gains
`budget_tokens`/`bound`; a new `context_pressure` event; `BudgetBound` and
`ContextPressureKind` enums; `ProviderRecipeEntry.max_context` and
`ProviderSetupCandidate.max_context`; `teton-core` `ProviderCapabilities.context_budget_cap`;
`teton-providers` `CapabilityProfile.context_budget_cap` round-trip. No
`PROTOCOL_VERSION` bump (architecture.md "Data model / protocol changes", ADR-7).

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `ProviderConfig` (L675-695): `max_context: Option<u32>`, `context_budget_cap: Option<u32>`, both `#[serde(default, skip_serializing_if = "Option::is_none")]`, doc: "daemon always populates `max_context` on the snapshot (`Some(0)` = unknown); `None` = the daemon predates the field" (the `RouteDecided.effort` rule, events.rs:396-407). `ProviderRecipeEntry` (L1847-1883): `max_context: u32` (`#[serde(default)]`). `ProviderSetupCandidate` (L2021-2060): `max_context: Option<u32>` (default/skip). Round-trips: extend `config_set_round_trips_each_update_variant` (L3366) with both fields set; add `a_client_predating_the_window_fields_still_reads_a_provider_that_carries_them` beside `a_client_predating_redact_enabled_still_reads_a_snapshot_that_carries_it` (L3183); recipe/candidate round-trips beside L3938-4102.
- `crates/teton-protocol/src/events.rs` — `BudgetBound { Window, DefaultUnknown, RedactScan, UserCap, LocalEngine }` and `ContextPressureKind { BlocksDropped, BlockElided, RefitOnReroute }` (`#[serde(rename_all = "snake_case")]`, `Copy`, `Eq`); `RouteDecided` (L364-410): `budget_tokens: Option<u64>`, `budget_bytes: Option<u64>`, `bound: Option<BudgetBound>` (default/skip; doc copies the `effort` additivity note); `Event::ContextPressure(ContextPressure { kind, dropped_blocks: u64, elided_bytes: u64, newest_user_elided: bool, budget_tokens: u64, budget_bytes: u64, bound: BudgetBound })` — **no `session_id`** (L2048-2059 flatten rule); `Event::name()` arm `"context_pressure"` (L189-222); rows in `event_names_match_the_spec_events_table` (~L2393) and the every-variant list (~L2527-2571); `context_pressure_round_trips_under_its_wire_name` copied from `context_cleared_round_trips_under_its_wire_name`; `route_decided_round_trips` (L2754) extended; skew test copied from `a_client_predating_the_cause_field_still_reads_a_frame_that_carries_one` (L2858).
- `crates/teton-core/src/entities.rs` — `ProviderCapabilities` (L85-117): `context_budget_cap: u32` with `#[serde(default, skip_serializing_if = "is_zero")]` (0 = none) so `config_preservation.rs:838-842`'s canonical `[providers.capabilities]` rendering is unchanged; keep `Copy`; doc `max_context` "0 = unknown → default budget, stated in /doctor".
- `crates/teton-core/src/config.rs` — round-trip test beside L2519-2529 for `context_budget_cap`; **no** `validate()` rule (a cap above the window is inert, ADR-7).
- `crates/teton-core/src/config_doc.rs` — a delta round-trip case beside L1479-1488 for `context_budget_cap`.
- `crates/teton-providers/src/capability.rs` — `CapabilityProfile.context_budget_cap: u32`; `from_core`/`to_core` (L49-69) carry it; `core_roundtrip_is_lossless` (L116) extended.
- Placeholder arms so the workspace compiles: `crates/teton/src/session_ui.rs` `render_event` (L450-872, exhaustive) gets `Event::ContextPressure(_) => {}` with `// TASK-190 renders this`; every `RouteDecided { .. }` literal (`DutyRoute::announcing` in `crates/tetond/src/harness/duty.rs` ~L648 builds one through `Route::route_decided()`/a struct — check which; `digest.rs:347`, `tools/grep.rs:928`, `harness/redact.rs:2877`, `router.rs:264-279`, `crates/teton/src/session_ui.rs:2553,2593,2632,2833`) gains `budget_tokens: None, budget_bytes: None, bound: None`; every wire `ProviderConfig { .. }` literal (`tetond/src/runtime.rs:5359,11660,11747,11885,11919,12183`; `teton/src/main.rs:3417,4010,6179,6210`; `teton/src/cli_rows.rs:1560`; `teton/src/provider_test_ui.rs:861,881`; `tetond/tests/config_preservation.rs`) gains `max_context: None, context_budget_cap: None`; `ProviderRecipeEntry`/`ProviderSetupCandidate` literals in `crates/teton/src/provider_setup_ui.rs:1684-1727` and `tetond/src/provider_recipes.rs:324-337` get the field (recipe values are TASK-188's; use `0` here only in the projection so TASK-188's contract test turns red until it lands — say so in a comment).

## Acceptance Criteria

- [x] `cargo test -p teton-protocol -p teton-core -p teton-providers` green; `PROTOCOL_VERSION` unchanged; `event_names_match_the_spec_events_table` and the every-variant list carry `context_pressure`.
- [x] A `ProviderConfig` JSON without the two fields deserializes; one with them round-trips; a pre-field struct reads a frame/snapshot that carries them (skew tests both directions).
- [x] `RouteDecided` JSON without `budget_tokens`/`bound` deserializes; with them round-trips; `ContextPressure` round-trips under `"event":"context_pressure"` with the envelope's `session_id` and no payload `session_id`.
- [x] `config_preservation.rs` canonical-rendering witness (L831-843) still passes byte-for-byte (the core field is skipped when 0).
- [x] `cargo build --workspace --all-targets` compiles (placeholder arms and literals).

## Technical Notes

- Copy shapes from `SessionRoot`/`ContextCleared` (TASK-174 did this for REQ-583). `is_zero` helper: `fn is_zero(v: &u32) -> bool { *v == 0 }` beside the struct.
- Do not render anything in the CLI here; do not change `Event::name()` ordering beyond appending.
- Commit as `feat(protocol): window fields, budget/bound on route_decided, context_pressure event [TASK-181]`.
