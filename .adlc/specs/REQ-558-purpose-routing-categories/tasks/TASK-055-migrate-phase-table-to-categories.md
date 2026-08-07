---
id: TASK-055
title: "One-shot migration from the phase routing table to tiers and categories"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-06
dependencies: [TASK-049, TASK-051]
---

## Description

Migrate an existing `[[routing]]` table to the category table, reporting each
one-to-many expansion by name so a user who wanted the expanded entries to differ
knows to split them (BR-10, AC-7).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `migrate_routing_to_categories`, using the
  same `category_for_phase` map as dispatch (ADR-F)
- `crates/tetond/src/runtime.rs` — run it in the load path beside the REQ-557
  model migration; delete `RoutingPolicy` once nothing reads it

## Acceptance Criteria

- [x] The documented mapping is applied: `spec` + `architect` → `design`,
      `implement` → {`edit`, `shell`}, `review` → `review`, `io` → {`digest`,
      `triage`, `title`, `compact`}.
- [x] Each one-to-many expansion is **reported by name** at migration time
      (AC-7) — a user with one `implement` rule is told it became `edit` and
      `shell`.
- [x] Migration runs once: a second start finds nothing to migrate, rewrites
      nothing, and reports nothing new. The guard is the absence of the old table.
- [x] The config write is **atomic** — reuse `write_config_atomically`
      (BUG-155 C3). A migration that cannot be saved leaves the config
      byte-for-byte intact and says so.
- [x] A freeform routing entry is still **rejected at load**, unchanged (see
      notes).
- [x] The mapping function is the *same* one structured dispatch uses (ADR-F) —
      a test asserts they agree for all five phases.

## Technical Notes

**AC-7's fixture is five phases, not six.** `Config::validate` already rejects a
`[[routing]]` rule targeting `freeform` (`ConfigError::FreeformRoutingPolicy`,
`config.rs:337`), so a config carrying that entry has never loaded and the
migration has nothing to drop. BR-10's "the `freeform` entry is dropped" describes
an unreachable state — see "Corrections to the Requirement" in architecture.md.
Implement the five valid phases and assert the freeform entry stays rejected.

**Follow REQ-557's migration shape exactly** (`migrate_and_report_provider_models`,
`runtime.rs:1747`): key on the absence of the new state, report what changed, write
only if something changed, write atomically, and report the still-broken remainder
at startup. That function also carries BUG-155's fixes — atomic write, and the
default-provider migration — so it is the current, corrected pattern rather than
the original one.
