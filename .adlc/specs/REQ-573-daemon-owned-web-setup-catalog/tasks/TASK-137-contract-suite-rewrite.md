---
id: TASK-137
title: "Contract suite enumerates the typed catalog; guide sync goes bidirectional"
status: complete
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-136"]
---

## Description

Re-point the AC-8 gate (`web_setup_contracts.rs`) at the daemon catalog:
typed enumeration replaces source-text parsing of the CLI crate; the
self_config.md sync check becomes bidirectional (BR-5); every catalog entry
still drives the production request builder.

## Files to Create/Modify

- `crates/tetond/tests/web_setup_contracts.rs` — delete `FLOW_SUGGESTIONS`
  (`include_str!` of `../../teton/src/web_setup_ui.rs`, line ~79) and
  `suggested_endpoints()` (~208–221); enumerate
  `tetond::web_setup_catalog::suggestion_catalog()`; expectation table keyed
  by catalog `id` (LESSON-512 — independent of production parsing); rewrite
  `every_suggested_auth_template_has_a_contract` and
  `every_suggested_endpoint_has_a_contract` against the catalog; keep the
  three production-builder tests iterating catalog entries; header doc
  comment updated

## Acceptance Criteria

- [x] No `include_str!` of any path outside the tetond crate remains in the
      suite (AC-3); `BUNDLED_GUIDE` (self_config.md) parsing stays
- [x] Exhaustive zip both ways: a catalog entry with no expectation row FAILS
      with a message naming the entry ("suggestion with no contract test");
      an expectation row with no catalog entry FAILS (stale table)
- [x] Per keyed entry, the production builder assertions hold: GET via
      `Egress::lookup` with terms as `q` and endpoint path/query preserved;
      `search_auth_shape()`/`header_value()` produce the documented header
      name/value; `Config::validate()` accepts the suggested shape and the
      rendered TOML carries no raw secret (BR-4)
- [x] Guide sync bidirectional (AC-4): every backtick `{key}` template in
      self_config.md is a catalog `auth_template` or the generic default, AND
      every catalog `auth_template` appears in the guide; the SearxNG
      endpoint-shape string appears in both
- [x] The keyless SearxNG entry is asserted too: config with no key ref
      validates, and the built request carries no auth header
- [x] The prompt-size ceiling test and BUG-160 guide-content regression tests
      still pass unmodified
- [x] `cargo test -p tetond` green

## Technical Notes

The suite already links the tetond lib (uses `Egress`, `Config`,
`search_auth_shape`) so `use tetond::web_setup_catalog::suggestion_catalog;`
is the whole wiring. Keep fixture credential values in the house sentinel
style already present in the file (LESSON-497). Do not derive expected
headers by parsing `auth_template` with production code — that tests the code
against itself (architecture.md "Contract suite redesign").
