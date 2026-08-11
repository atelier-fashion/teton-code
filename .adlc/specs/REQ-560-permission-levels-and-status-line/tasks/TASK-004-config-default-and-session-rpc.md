---
id: TASK-004
title: "default_permission_level config plus the session/permissions RPC"
status: pending
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-003]
---

## Description

Give the level a configured starting value and a way for a client to read and
change it mid-session (ADR-D, OQ-3). The level is session-scoped and is never
written back (BR-6) — the deliberate asymmetry with REQ-559's persisted effort.

## Files to Create/Modify

- `crates/teton-core/src/config.rs`:
  - `PermissionsConfig { default_level: PermissionLevel }` with
    `is_unset`/`Default`, declared among the table fields (before the
    array-of-table fields, per the TOML-ordering comment on `Config`)
  - `Config.permissions: PermissionsConfig`, `#[serde(default, skip_serializing_if = "PermissionsConfig::is_unset")]`
  - An unparseable level is a **structural** config error (`Config::validate`),
    not a silent fallback — it names something that does not exist, which is the
    class `validate` already carries
- `crates/teton-protocol/src/methods.rs`:
  - `SessionPermissionsParams { session_id: SessionId, level: Option<PermissionLevel> }`
  - `SessionPermissionsResult { level: PermissionLevel, changed: bool }`
  - `impl RpcMethod`, `METHOD = "session/permissions"`
- `crates/tetond/src/runtime.rs`:
  - `permission_gate_for` seeds the gate with `config.permissions.default_level`
    and the `[web] permission_allow` list, replacing the
    `self.permission_config.clone()` + `apply_web_permission` pair
  - the `permission_config` field is removed or reduced to the default level;
    update the two construction sites (~1283, ~1427) and the use at ~14112
  - a `session_permissions(session_id, Option<level>)` runtime method that
    resolves the session's gate, reads or sets, and returns the current level
- `crates/tetond/src/server.rs` — dispatch the new method, following the
  `web/override` handler for shape and for session-attachment authorization

## Acceptance Criteria

- [ ] **AC-6**: `/permissions full`, then a full daemon restart and a fresh
      session, starts at `guarded` — nothing was persisted. Assert the config
      file on disk is unchanged as well as the new session's level
- [ ] A configured `default_permission_level = "edits"` is the level a new
      session starts at; absent config starts at `guarded`
- [ ] An invalid level in config is a `Config::validate` error naming the field
      and listing the four valid spellings — the daemon refuses to start rather
      than silently choosing one
- [ ] Setting returns `changed: true`; setting the level it already holds
      returns `changed: false` (the `WebTaintOverride::lift` idempotence
      precedent, so an announcement stays honest)
- [ ] Reading (`level: None`) never mutates
- [ ] A second client attached to the same session observes a level set by the
      first (surface-parity, REQ-544 BR-4)
- [ ] `session/permissions` requires attachment to the named session, matching
      `web/override`'s authorization
- [ ] `SessionPermissionsParams::METHOD == "session/permissions"` asserted, as
      the other methods do
- [ ] `cargo test --workspace` green; no clippy warnings

## Technical Notes

**BR-6 is a "do not" task**: there must be no write path from `set_level` to the
config file. The reviewer will look for one.

The gate is created once per session (`permission_gate_for`'s doc comment
explains why) and the level lives on it, so a level change is naturally
session-scoped and naturally visible to every client attached to that session.

`session/permissions` is a client RPC, never a harness tool. That placement is
the enforcement for the spec's Permissions row — a model emitting a tool call by
that name, or tool output containing `/permissions full`, reaches nothing.
