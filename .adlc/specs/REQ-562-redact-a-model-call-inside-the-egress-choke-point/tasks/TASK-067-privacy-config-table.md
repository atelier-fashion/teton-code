---
id: TASK-067
title: "The [privacy] opt-in table — off by default, and not a category binding"
status: draft
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

- [ ] A config with no `[privacy]` table loads with `privacy.redact == false` (AC-13's "off by default" leg).
- [ ] `[privacy]\nredact = true` parses; a round-trip test proves the value is read, not defaulted (LESSON-485: the fixture must discriminate — assert `true`, not merely "loads").
- [ ] AC-14 test: `ConfigurableCategory` has no `Redact` variant — assert `"redact".parse::<ConfigurableCategory>()` still fails with `RedactIsPinned`, and a `[[categories]]` TOML entry naming `redact` still fails to deserialize with a message naming the pin.
- [ ] AC-4 RPC/CLI legs re-asserted, not assumed: `config/set` deserializes the PROTOCOL type, which is a different type from the config `FromStr` path (LESSON-486 #2) — locate the protocol-side category type and assert `redact` is unbindable there too (a `config/set`-shaped payload naming `redact` is rejected), and that `policy set-category redact …` fails naming the pin. If REQ-558 tests already pin these exact paths, cite them by test name in the task completion note instead of duplicating.
- [ ] `[privacy]` with an unknown key behaves consistently with the rest of `Config`'s unknown-key posture (match existing serde attributes; do not invent a new posture).
- [ ] `cargo test -p teton-core` green; no clippy warnings.

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
