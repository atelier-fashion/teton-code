---
id: TASK-135
title: "Protocol: WebSetupCatalog types, suggestion_catalog field, shared generic template"
status: complete
parent: REQ-573
created: 2026-08-14
updated: 2026-08-14
dependencies: []
---

## Description

Add the catalog vocabulary to `teton-protocol`: `WebBackendSuggestion`,
`WebSetupCatalog`, the additive `suggestion_catalog` field on
`WebSetupPlanResult`, and the shared `GENERIC_SEARCH_AUTH_TEMPLATE` constant
(ADR-B). Pure protocol change — no daemon or CLI behavior yet.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `WebBackendSuggestion` and
  `WebSetupCatalog` structs near `WebSetupPlanResult` (~line 1449); new field
  `#[serde(default, skip_serializing_if = "Option::is_none")] pub
  suggestion_catalog: Option<WebSetupCatalog>` on `WebSetupPlanResult`;
  extend `the_web_setup_methods_round_trip` (~line 2643) and add the
  absent-field deserialization test
- `crates/teton-protocol/src/lib.rs` — `pub const
  GENERIC_SEARCH_AUTH_TEMPLATE: &str = "Authorization: Bearer {key}";` with a
  doc comment naming both consumers (daemon catalog default, CLI BR-3
  degraded offer)

## Acceptance Criteria

- [x] Struct shapes match architecture.md "Protocol changes" exactly: `id`,
      `label`, `endpoint` required strings; `host`, `auth_template`, `notes`
      optional with `#[serde(default, skip_serializing_if)]`; `needs_key`
      bool; catalog carries `default_auth_template` + `backends`
- [x] Derives match house style for sibling types (`Debug, Clone, PartialEq,
      Eq, Serialize, Deserialize`); no `Default` that could manufacture data
- [x] Round-trip test covers a populated catalog (all optional fields both
      present and absent across entries)
- [x] A `WebSetupPlanResult` JSON **without** `suggestion_catalog` deserializes
      with `None` (AC-1 absent-field direction, BUG-158 additive-skew rule)
- [x] A populated result serialized then deserialized is equal; the field is
      omitted from the wire when `None`
- [x] `PROTOCOL_VERSION` untouched (min == max == 2);
      `this_build_advertises_only_the_version_its_types_can_read` still passes
- [x] `cargo test -p teton-protocol` green

## Technical Notes

Precedent for field placement and serde attributes: `search_gap` /
`current_web` on the same struct. Precedent for a `Vec`-carrying result:
`ModelListResult` (methods.rs ~479). Keep doc comments in the existing style
("the field is then absent from the wire"). The constant goes in `lib.rs`
beside the protocol version block so both binaries import it from the crate
root.
