---
id: TASK-127
title: "Protocol: setup methods, capability state, events, error code"
status: draft
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

- [ ] All three methods round-trip serialize/deserialize in unit tests, following the crate's existing method test pattern
- [ ] `WEB_SETUP_INVALID` is -32020 and the macro table compiles with no renumbering of existing codes
- [ ] `ConfigSnapshot` with the new field absent deserializes (forward-compat test with a pre-REQ-572 JSON fixture)
- [ ] Both events carry `session_id` scoping in envelope tests

## Technical Notes

Follow the `web/override` / `WebOverrideParams` shape exactly for method
binding. The wire `WebCapabilityState` uses a `reason: String` (already
human-readable) rather than a structured gap enum — the CLI renders it, never
branches on it. Do not touch existing variants or field order; additive only.
