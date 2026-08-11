---
id: TASK-001
title: "PermissionLevel in teton-protocol: the level's shared identity"
status: complete
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Add the `PermissionLevel` enum to `teton-protocol` — the crate both the daemon
and the client depend on. It carries the level's *identity* (name, parse,
summary, denial sentence); the table it expands to is TASK-002's and stays in
`tetond` (ADR-B).

This lands first because everything else in the REQ is a caller of it.

## Files to Create/Modify

- `crates/teton-protocol/src/permissions.rs` — new module:
  - `pub enum PermissionLevel { Guarded, Edits, Plan, Full }`, deriving
    `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize`, with
    `#[serde(rename_all = "lowercase")]` so the wire spelling is the typed one
  - `pub const ALL: &'static [PermissionLevel]` — all four, in the documented
    order (guarded, edits, plan, full)
  - `fn name(self) -> &'static str` — exhaustive match
  - `fn parse(s: &str) -> Option<Self>` — exact, lowercase, closed set; no
    prefix matching and no case folding beyond an exact lowercase compare, for
    the reason `slash.rs::classify` gives about lenient matching
  - `fn summary(self) -> &'static str` — the one line `/permissions` prints per
    level
  - `fn denial_sentence(self, tool: &str) -> String` — the sentence a call
    denied *by the level* returns. Must keep the existing "Do not retry this
    tool; take a different approach or finish." clause (BR-2) and name the level
  - `impl Default` → `Guarded`
- `crates/teton-protocol/src/lib.rs` — declare and re-export the module

## Acceptance Criteria

- [ ] `name()` / `parse()` round-trip for every variant in `ALL`, driven by
      iterating `ALL` rather than by four hand-written cases (AC-17 shape: a
      fifth variant is covered without editing the test)
- [ ] `parse` rejects `""`, `"Guarded"`, `"guard"`, `"guarded "`, and `"all"` —
      an unknown level is `None`, never a nearest match
- [ ] `denial_sentence` names the level and retains the no-retry clause;
      asserted for every variant in `ALL`
- [ ] `summary()` is non-empty for every variant in `ALL`
- [ ] Serde round-trips each variant through JSON as its lowercase name
- [ ] `Default` is `Guarded` (the spec's `default_permission_level` default)
- [ ] `cargo test -p teton-protocol` green; no clippy warnings

## Technical Notes

Every function is an exhaustive `match` — that is what makes the compiler the
enforcer for BR-15/AC-17, so do not add a `_ =>` arm to any of them.

`PermissionConfig` deliberately does **not** move here (ADR-B): it is daemon
enforcement state, and putting it on the wire would invite a client to send one.
