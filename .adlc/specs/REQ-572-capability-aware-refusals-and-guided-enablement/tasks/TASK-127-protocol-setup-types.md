---
id: TASK-127
title: "Protocol: setup methods, capability state, events, error code"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: []
---

## Description

Add the additive wire vocabulary for REQ-572 to `teton-protocol`: the three
session-scoped setup methods, the typed capability-state enum, the two new
events, one new error code, and the `ConfigSnapshot` capability field.
Protocol stays v2 — everything is additive (`#[serde(default)]` where a field
joins an existing struct).

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `WebSetupPlanParams { session_id }` → `WebSetupPlanResult { state: WebCapabilityState, search_available: bool, search_gap: Option<String>, current_web: Option<WebTableSummary> }`; `WebSetupPreviewParams { session_id, tier, search_endpoint: Option<String>, search_key_ref: Option<String>, search_auth: Option<String> }` → `WebSetupPreviewResult { toml: String, search_host: Option<String>, warnings: Vec<String> }`; `WebSetupCommitParams` (same fields as preview) → `WebSetupCommitResult { applied: bool, tier: WebTier }`. Methods named `web/setup_plan`, `web/setup_preview`, `web/setup_commit` via the existing `RpcMethod` trait pattern (follow `web/override`).
- `crates/teton-protocol/src/events.rs` — `Event::WebSetupCompleted { tier: WebTier, config_path: String }`, `Event::WebSetupRejected { origin: String }`, and `Event::CapabilityDeadEnd { capability: String }` (consumed by TASK-129's unserved-turn and TASK-131's tier-gap emissions), all session-scoped (`EventEnvelope.session_id = Some`).
- `crates/teton-protocol/src/jsonrpc.rs` — `WEB_SETUP_INVALID = -32020` in the `application_error_codes!` block (next free after `SELF_APPROVAL_REFUSED = -32019`).
- `crates/teton-protocol/src/lib.rs` (or wherever `ConfigSnapshot` lives) — additive `web_capability: Option<WebCapabilityState>` with `#[serde(default)]`.
- `crates/teton-protocol/src/events.rs` or a shared module — `WebCapabilityState` wire twin: `Ready(WebTier) | OffAvailable | SearchUnavailable { reason: String }` (mirror of the teton-core type, same pattern as the existing `WebTier` wire twin noted in `teton-core/src/config.rs`).

## Acceptance Criteria

- [x] All three methods round-trip serialize/deserialize in unit tests, following the crate's existing method test pattern (`the_web_setup_methods_round_trip`, `no_setup_payload_has_anywhere_to_put_the_key`, plus the `METHOD` pins in `request_helper_fills_method_from_trait`)
- [x] `WEB_SETUP_INVALID` is -32020 and the macro table compiles with no renumbering of existing codes (`the_web_setup_code_is_the_next_free_one_and_renumbers_nothing`, beside the existing distinctness sweep over `error_code::ALL`)
- [x] `ConfigSnapshot` with the new field absent deserializes (forward-compat test with a pre-REQ-572 JSON fixture) (`a_snapshot_with_no_web_capability_key_reads_as_no_answer`, both directions plus non-vacuity)
- [x] Both events carry `session_id` scoping in envelope tests (`the_setup_events_are_session_scoped_under_their_wire_names` — all **three** events, including `capability_dead_end`)

## Technical Notes

Follow the `web/override` / `WebOverrideParams` shape exactly for method
binding. The wire `WebCapabilityState` uses a `reason: String` (already
human-readable) rather than a structured gap enum — the CLI renders it, never
branches on it. Do not touch existing variants or field order; additive only.

## Implementation Notes (TASK-127, 2026-08-13)

Three deviations from the letter of this file, all recorded rather than silent:

1. **`WebCapabilityState::Ready { tier }`, not `Ready(WebTier)`.** The enum is
   internally tagged (`#[serde(tag = "state")]`, the crate's `BlockCause`
   pattern), and serde cannot serialize a tagged newtype variant whose content
   is a string — it compiles and fails at runtime. A struct variant is the same
   flat wire object with the same tag.
2. **Events are newtype variants over named payload structs**
   (`Event::WebSetupCompleted(WebSetupCompleted { .. })`), matching every other
   variant in `Event`. Wire-identical to the inline struct variants this file
   sketches, since the enum is tagged-and-flattened.
3. **Three downstream one-liners, outside this crate, to keep the workspace
   compiling** — `ConfigSnapshot` gained a required field and `Event` gained
   variants, and both are matched exhaustively downstream:
   `tetond/src/runtime.rs` `snapshot_from_config` sends `web_capability: None`
   (TASK-129 wires the real derivation — deliberately not a second
   hand-rolled reading of `config.web.tier` here, which BR-3 forbids),
   `teton/src/main.rs`'s test fixture the same, and `teton/src/session_ui.rs`
   gained explicit no-render arms for the three events with a comment pointing
   at TASK-132, which owns their rendering.

`WebTableSummary`'s fields were not specified here; it carries `tier`,
`search_host` (host only, REQ-563 BR-7), `search_key_ref` and `search_auth`
(both non-secret by construction) and deliberately derives no `Default`, so
"there is no `[web]` table" stays distinguishable from "the table says off".
