---
id: TASK-049
title: "Config schema: tiers and categories replace the phase routing table"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-048]
---

## Description

Replace `Config.routing: Vec<RoutingPolicy>` with the tier/category table, and make
a `categories.redact` key fail at load naming the pin.

## Files to Create/Modify

- `crates/teton-core/src/entities.rs` — `TierBinding`, `CategoryOverride`; retire
  `RoutingPolicy` (its `phase` field is the dispatch key being removed)
- `crates/teton-core/src/config.rs` — `Config.tiers`, `Config.categories`,
  `Config.judgment_default`; validation; error variants

## Acceptance Criteria

- [ ] TOML round-trips `[[tiers]]` and `[[categories]]` through
      `to_toml`/`from_toml` (the REQ-557 round-trip test is the precedent).
- [ ] A `[[categories]] name = "redact"` entry is **rejected at load**, and the
      message names `redact` and says it is pinned local by construction — not a
      bare serde "unknown variant" that reads like a typo (AC-4).
- [ ] A tier or category binding naming an unregistered provider id is rejected at
      load, naming the id and listing registered ids — the REQ-557 BR-6 shape.
- [ ] `judgment_default` is a real config key with a documented default of `edit`,
      and it appears in the `config/get` projection (AC-12, BR-9).
- [ ] A config with **no** tier bindings still loads — an empty table is
      incomplete, not corrupt (REQ-557 ADR-E's posture).
- [ ] Existing `[[routing]]` entries still deserialize (TASK-055 migrates them);
      a config carrying them does not fail to load.

## Technical Notes

**Do not delete `RoutingPolicy` until TASK-055 has migrated it.** The migration
reads the old shape, so the type must survive long enough to be read. Mark it
deprecated and remove it in TASK-055's change, not this one.

**`freeform` routing entries are already rejected** (`ConfigError::FreeformRoutingPolicy`,
`config.rs:337`) and that rejection **stays**. See the architecture's "Corrections
to the Requirement": BR-10's "drop the freeform entry" describes a config that has
never loaded.

**AC-4's message quality is the acceptance criterion, not the rejection.** Serde's
default unknown-variant error already names `redact`; the criterion is that a user
reading it understands the key is *forbidden*, not *misspelled*.
