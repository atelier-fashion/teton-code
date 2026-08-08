---
id: TASK-067
title: "The [privacy] opt-in table — off by default, and not a category binding"
status: complete
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: []
repo: teton-code
---

## Description

Add `PrivacyConfig { redact: bool }` (default `false`) to `teton-core`'s
`Config`, deserialized from a new top-level `[privacy]` table (BR-10, OQ-3).
Re-assert — with tests that would go red if a later change reopened it — that
the switch is NOT a category binding: `ConfigurableCategory` still has no
`Redact` variant and `[[categories]]` naming `redact` is still rejected at load
naming the pin (AC-14).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `PrivacyConfig` struct, `privacy` field on `Config` with `#[serde(default)]`, parse/default tests
- `crates/teton-core/src/category.rs` — tests only if the AC-14 assertions naturally live beside `ConfigurableCategory`; no production change

## Acceptance Criteria

- [x] A config with no `[privacy]` table loads with `privacy.redact == false` (AC-13's "off by default" leg).
- [x] `[privacy]\nredact = true` parses; a round-trip test proves the value is read, not defaulted (LESSON-485: the fixture must discriminate — assert `true`, not merely "loads").
- [x] AC-14 test: `ConfigurableCategory` has no `Redact` variant — assert `"redact".parse::<ConfigurableCategory>()` still fails with `RedactIsPinned`, and a `[[categories]]` TOML entry naming `redact` still fails to deserialize with a message naming the pin.
- [x] AC-4 RPC/CLI legs re-asserted, not assumed: `config/set` deserializes the PROTOCOL type, which is a different type from the config `FromStr` path (LESSON-486 #2) — locate the protocol-side category type and assert `redact` is unbindable there too (a `config/set`-shaped payload naming `redact` is rejected), and that `policy set-category redact …` fails naming the pin. If REQ-558 tests already pin these exact paths, cite them by test name in the task completion note instead of duplicating.
- [x] `[privacy]` with an unknown key behaves consistently with the rest of `Config`'s unknown-key posture (match existing serde attributes; do not invent a new posture).
- [x] `cargo test -p teton-core` green; no clippy warnings.

## Technical Notes

- BR-10's distinction is load-bearing: the switch answers "does the scan run at
  all", the (absent) binding answers "which provider serves it". Do NOT add any
  provider/model/tier key to `[privacy]` — the pin is the point.
- Existing single-value config sections: check how `[[boundaries]]` and other
  sections declare defaults and mirror that style.
- If `policy show` or config snapshots serialize `Config`, verify the new field
  serializes without breaking existing snapshot tests; fix fixtures in this task
  if so (the ConfigSnapshot v1/v2 handshake gate from e523d3d governs protocol
  snapshots — confirm whether `privacy` rides in ConfigSnapshot and, if it does,
  keep it additive-with-default there too).

## Completion Notes

**Shape shipped.** `PrivacyConfig { redact: bool }` in
`crates/teton-core/src/config.rs`, `Config.privacy` with
`#[serde(default, skip_serializing_if = "PrivacyConfig::is_unset")]` — the
`[local_model]` treatment, mirrored: the inner bool serializes unconditionally
so a config that names `[privacy]` states its posture, and the table stays out
of a config that never opted in. No provider/model/tier key, per BR-10.

**Unknown-key posture.** `Config` carries no `deny_unknown_fields` anywhere, so
a stray key is ignored; `[privacy]` inherits that and invents nothing.
`an_unknown_key_in_the_privacy_table_is_ignored_like_any_other_unknown_key`
asserts the two behave alike, and that a `provider_id` written into `[privacy]`
anyway binds nothing and does not survive serialization.

**Non-vacuity (LESSON-485).** Mutating the field to `#[serde(skip_deserializing)]`
— "the value is never read from the document" — turns four of the five new
teton-core tests red; the fifth is the default-leg test, which correctly stays
green. Mutation run and reverted.

**AC-4, RPC leg — NEW test, because the existing ones stop short.**
`teton_protocol::ConfigurableCategory` derives `Deserialize`, so the JSON path's
rejection is serde's, not `RedactIsPinned` (LESSON-486 #2 exactly).
`teton-protocol/src/lib.rs::a_binding_can_name_nine_categories_and_only_nine`
already pins the bare *type* against `"redact"`, but nothing pinned a
`config/set`-**shaped payload**, which is what `server.rs` actually deserializes.
Added `methods.rs::a_config_set_payload_naming_a_pinned_category_cannot_be_deserialized`,
which derives the payload from a valid one by swapping only the category name.

**AC-4, CLI leg — cited, not duplicated.** `policy set-category redact …` is
already pinned by `crates/teton/src/main.rs::policy_set_category_rejects_a_pinned_category_by_naming_the_pin`
(asserts the pin sentence and its own reason for both pinned categories).
Message quality on the config-file path: `config.rs::a_categories_entry_naming_a_pinned_category_says_pinned_not_misspelled`.

**ConfigSnapshot: `Config` does not ride in it.** `snapshot_from_config`
(`tetond/src/runtime.rs`) hand-projects field by field, and
`ConfigSnapshot.privacy` is already taken — it is `Vec<PrivacyBoundaryConfig>`,
the boundary list, unrelated to this table. So `privacy.redact` reaches no
protocol surface, nothing was added to `ConfigSnapshot`, and the e523d3d v1/v2
handshake gate is untouched. Note the name collision for whoever later wants to
expose the opt-in over the wire: `ConfigSnapshot.privacy` already means
boundaries.
