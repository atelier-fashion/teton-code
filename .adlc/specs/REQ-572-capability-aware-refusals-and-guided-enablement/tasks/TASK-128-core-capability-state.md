---
id: TASK-128
title: "Core: pure capability-state derivation and [web] table rendering"
status: complete
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

- [x] Table-driven tests cover every (tier × local-model-present) cell of `web_capability_state`, including: no `[web]` table → OffAvailable; tier=fetch_any_url → Ready regardless of model presence; tier=search + model absent → SearchUnavailable{NoLocalModel} — `every_tier_crossed_with_local_model_presence_has_one_stated_answer` (all 8 cells, guarded against a future tier by a sweep over `WebTier::ALL`), `a_config_with_no_web_table_is_off_but_available`, `a_fetch_tier_is_ready_whether_or_not_a_local_model_is_present`, `search_without_a_local_model_is_blocked_per_query_and_still_exposed`
- [x] `web_table_toml` output is byte-identical to the `[web]` section of `Config::to_toml()` for at least three configs (fetch-only, search keyless, search with key_ref + search_auth), asserted by a test — `the_web_table_renderer_reproduces_the_documents_section_byte_for_byte` (substring-of-the-document *and* section-equality, so neither a dropped nor an extra key can hide); the one asymmetry, an unset table the document omits entirely, is pinned by `an_unset_web_table_is_the_one_section_the_document_omits`
- [x] `register_web_tool`'s registration condition and `web_capability_state`'s Ready condition are the same predicate — pinned by a test in this crate asserting Ready ⟺ tier > Off (the daemon-side consumption is TASK-131) — **satisfied as exposure ⟺ tier > Off**, not `Ready` ⟺ tier > Off: `SearchUnavailable` is a configured, *registered* capability whose search leg blocks per query (REQ-563 BR-14 leaves the fetch tiers inside the ceiling serviceable), so pinning registration to `Ready` would un-register the tool on every machine without a local model — the opposite of the intent, and it would contradict TASK-131's own AC ("registers iff not OffAvailable"). The predicate is `WebCapabilityState::exposes_web_tool()` and the pin is `tool_exposure_is_exactly_a_tier_above_off_in_every_cell`

## Technical Notes

Do NOT add config fields. `WebCapabilityState` here is the semantic type; the
wire twin lives in teton-protocol (TASK-127) with `From` conversions where the
daemon crosses the boundary. Keep `SearchGap` closed (one variant today) —
the enum exists so a future gap (e.g. endpoint unreachable) is a compile-time
reminder at every match site.

## Implementation notes (as built)

- No config fields added; `WebConfig` is untouched. `SearchGap` is closed (no
  `#[non_exhaustive]`), as specified.
- Two small members beyond the listed surface, both there to keep the
  "one classifier, one sentence" property from decaying into a convention:
  `WebCapabilityState::exposes_web_tool()` (the registration predicate TASK-131
  consumes — see AC-3 above) and `SearchGap::as_str()` → `"search needs the
  local model"` (the requirement's own wording for the named missing piece, so
  the daemon, the status line and the setup flow cannot each invent a phrasing
  of the same gap).
- No `From` conversions to the wire twin: TASK-127 was still landing the
  protocol types in parallel, and this task declares no dependency on it. The
  conversion belongs at the daemon's boundary (TASK-129/130/131), which is
  where the wire type is actually named.
