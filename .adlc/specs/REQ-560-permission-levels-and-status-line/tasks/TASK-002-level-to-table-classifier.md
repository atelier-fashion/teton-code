---
id: TASK-002
title: "table_for: the one function that turns a level into a policy table"
status: pending
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-001]
---

## Description

Add `table_for(level) -> PermissionConfig` to the daemon's permission module —
the single place a level becomes policy (ADR-A, BR-1, BR-15) — and re-express
the two existing presets as delegations to it, so no second table survives.

Also narrow `apply_web_permission` so a config-supplied standing consent can
only relax an `Ask`, never overrule a `Deny` (ADR-C).

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs`:
  - `pub fn table_for(level: PermissionLevel) -> PermissionConfig` — exhaustive
    match producing the four tables of ADR-A:
    - `Guarded`: default `Ask`; `read`/`glob`/`grep` → `Allow`; `edit` → `Ask`;
      `shell` → `Ask`
    - `Edits`: as `Guarded` but `edit` → `Allow`
    - `Plan`: default **`Deny`**; `read`/`glob`/`grep` → `Allow`
    - `Full`: default `Allow`; each `WEB_PERMISSION_KEYS` entry → `Ask`
  - `coding_defaults()` → `table_for(PermissionLevel::Guarded)`
  - `permissive()` → `table_for(PermissionLevel::Full)`, keeping both doc
    comments (the `permissive()` comment explaining why web stays `Ask` is
    load-bearing and must survive)
  - `apply_web_permission`: only `set` a key whose current `policy_for` is
    `PermissionPolicy::Ask`
  - derive `PartialEq` on `PermissionConfig` so tests can compare tables
- `crates/tetond/src/harness/mod.rs` — export `table_for` if the module's
  re-export list is explicit

## Acceptance Criteria

- [ ] **AC-1**: a characterization test spells out, literally in the test body,
      the expected `(tool, policy)` rows and default for all four levels and
      asserts `table_for` produces them. It must NOT be written as
      `table_for(Guarded) == coding_defaults()` — post-delegation that is a
      tautology and catches nothing
- [ ] A separate assertion pins the delegation itself: `coding_defaults()` and
      `permissive()` equal `table_for(Guarded)` / `table_for(Full)`, so a future
      edit that reintroduces a second table fails here
- [ ] **AC-17 (table half)**: iterating `PermissionLevel::ALL`, `table_for`
      answers for every variant and every resulting table is internally
      consistent (`policy_for` on an unlisted name returns the table's default)
- [ ] **OQ-2**: an unknown, server-supplied tool name (e.g.
      `"mcp__srv__anything"`) resolves to `Ask` at `guarded`/`edits`, **`Deny`
      at `plan`**, and `Allow` at `full` — asserted by name-free lookup, proving
      no level enumerates MCP tools
- [ ] `apply_web_permission` upgrades `Ask` → `Allow` (today's behaviour,
      unchanged) and leaves a `Deny` alone — asserted against a `plan` table
- [ ] The whole existing `tetond` suite is green, in particular
      `web_consent_matrix` and `web_lookup_egress`, which depend on
      `permissive()` leaving the web keys asking
- [ ] `cargo test -p tetond` green; no clippy warnings

## Technical Notes

`table_for` is the classifier BR-15 names. Keep it an exhaustive `match` with no
`_` arm.

The read-only allowlist (`read`, `glob`, `grep`) is safe to enumerate because it
is first-party and closed; the mutating set is open and must never be
enumerated — that asymmetry is the whole of ADR-A and the answer to OQ-2.

`Guarded` keeps the redundant explicit `edit`/`shell` `Ask` rows that
`coding_defaults()` has today, so the table is byte-equal to the current one
(BR-1) rather than merely equivalent.
