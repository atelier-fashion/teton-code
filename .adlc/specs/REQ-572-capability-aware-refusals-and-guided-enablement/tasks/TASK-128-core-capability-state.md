---
id: TASK-128
title: "Core: pure capability-state derivation and [web] table rendering"
status: draft
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: []
---

## Description

The shared classifier (spec BR-3): a feature-free, pure function deriving
`WebCapabilityState` from `WebConfig` + local-model presence, plus the single
`[web]`-table renderer that both preview and commit consume (spec BR-7's
"what was confirmed is what is written" by construction). Lives in
`teton-core` so daemon and tests table-test it without a socket ("policy is
pure, mechanism is gated").

## Files to Create/Modify

- `crates/teton-core/src/capability.rs` — new module: `pub enum WebCapabilityState { Ready(WebTier), OffAvailable, SearchUnavailable { reason: SearchGap } }`; `pub enum SearchGap { NoLocalModel }`; `pub fn web_capability_state(web: &WebConfig, local_model_present: bool) -> WebCapabilityState`. Ready(tier) for any tier > Off with the search leg serviceable; SearchUnavailable when tier == Search and the local model is absent (REQ-563 BR-14 / product decision 1b — fetch tiers stay Ready inside it, carry that in the variant's docs); OffAvailable for Off/no table.
- `crates/teton-core/src/config.rs` — `pub fn web_table_toml(web: &WebConfig) -> Result<String, toml::ser::Error>`: render exactly the `[web]` table as it will appear in the full document (delegate to the same serde path `Config::to_toml` uses — serialize a shim struct `{ web: WebConfig }` so the bytes match the full-document rendering, and unit-test that the section extracted from `Config::to_toml()` equals this function's output for the same config).
- `crates/teton-core/src/lib.rs` — export the new module.

## Acceptance Criteria

- [ ] Table-driven tests cover every (tier × local-model-present) cell of `web_capability_state`, including: no `[web]` table → OffAvailable; tier=fetch_any_url → Ready regardless of model presence; tier=search + model absent → SearchUnavailable{NoLocalModel}
- [ ] `web_table_toml` output is byte-identical to the `[web]` section of `Config::to_toml()` for at least three configs (fetch-only, search keyless, search with key_ref + search_auth), asserted by a test
- [ ] `register_web_tool`'s registration condition and `web_capability_state`'s Ready condition are the same predicate — pinned by a test in this crate asserting Ready ⟺ tier > Off (the daemon-side consumption is TASK-131)

## Technical Notes

Do NOT add config fields. `WebCapabilityState` here is the semantic type; the
wire twin lives in teton-protocol (TASK-127) with `From` conversions where the
daemon crosses the boundary. Keep `SearchGap` closed (one variant today) —
the enum exists so a future gap (e.g. endpoint unreachable) is a compile-time
reminder at every match site.
