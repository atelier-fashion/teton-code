---
id: TASK-350
title: "Add the [shell] config table with its single allow_ssh_agent key"
status: draft
parent: REQ-607
created: 2026-08-31
updated: 2026-08-31
dependencies: []
repo: teton-code
---

## Description

Add `ShellConfig` and `Config::shell`, carrying exactly one key —
`allow_ssh_agent: bool`, default `false`. This is the whole of the opt-in's
user-facing surface (BR-5, BR-6).

Follow `PrivacyConfig` verbatim as the pattern: a `#[derive(... Default ...)]`
struct with an `is_unset` helper, `#[serde(default,
skip_serializing_if = "ShellConfig::is_unset")]` on the `Config` field, and the
inner field serialized **unconditionally** so a config that names `[shell]` at
all states its posture rather than leaving it to be inferred.

Declare the field among the other tables, **before** the array-of-table fields
(`providers`), for the TOML-ordering reason the neighbouring doc comments give.

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — new `ShellConfig` struct + `is_unset`; new `Config::shell` field
- `crates/teton-core/src/config_doc.rs` — document the `[shell]` table and its key if this file enumerates the config surface

## Acceptance Criteria

- [ ] `ShellConfig` has **exactly one** field, `allow_ssh_agent: bool`, and it is
      not a list, a string, or an enum — BR-5's "does not accept a list"
- [ ] `Config::default().shell.allow_ssh_agent` is `false` (BR-8's default half)
- [ ] A config with no `[shell]` table round-trips without emitting one
      (`is_unset`), and a config that sets the key round-trips with it visible
- [ ] `cargo test -p teton-core` passes

## Verification

| rule | kind | artifact | benign_path |
|---|---|---|---|
| BR-5 | test-case | `crates/teton-core/src/config.rs` — `shell_config_carries_one_boolean_key` | no |
| BR-6 | test-case | `crates/teton-core/src/config.rs` — `shell_config_carries_one_boolean_key` | no |
| BR-8 | test-case | `crates/teton-core/src/config.rs` — `shell_config_defaults_to_withholding_the_agent` | yes |

## Technical Notes

Check whether `config_doc.rs` derives a documented surface from the `Config`
struct or restates it by hand — if the latter, the `[shell]` row must be added
there too or the documented surface silently drifts from the real one.

Do **not** add anything to `Config::validate()`. A bool has no structural error
mode, and `validate()` is fail-closed and gates daemon startup (conventions.md).
