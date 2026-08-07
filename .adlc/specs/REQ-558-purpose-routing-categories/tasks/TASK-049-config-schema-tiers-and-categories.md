---
id: TASK-049
title: "Config schema: tiers and categories replace the phase routing table"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-048]
---

## Description

Replace `Config.routing: Vec<RoutingPolicy>` with the tier/category table, and make
a `categories.redact` key fail at load naming the pin.

## Files to Create/Modify

- `crates/teton-core/src/category.rs` — `TierBinding` and `CategoryOverride`
  already live here (TASK-048 put them beside `resolve`, which cannot exist
  without its input types). **Import them; do not redefine them in
  `entities.rs`.** They are re-exported at the crate root.
- `crates/teton-core/src/entities.rs` — leave `RoutingPolicy` in place; TASK-055
  deletes it once its migration has read it
- `crates/teton-core/src/config.rs` — `Config.tiers`, `Config.categories`,
  `Config.judgment_default`; validation; error variants

## Acceptance Criteria

- [x] TOML round-trips `[[tiers]]` and `[[categories]]` through
      `to_toml`/`from_toml` (the REQ-557 round-trip test is the precedent).
      — `the_tier_and_category_tables_round_trip_through_toml`, and the
      re-serialized document is re-`load`ed so a round-trip that produces an
      invalid config fails too.
- [x] A `[[categories]] name = "redact"` entry is **rejected at load**, and the
      message names `redact` and says it is pinned local by construction — not a
      bare serde "unknown variant" that reads like a typo (AC-4).
- [x] The same holds for `name = "route"`. Both are pinned local and neither has a
      `ConfigurableCategory` variant, so both are unrepresentable in config —
      `route` because BR-5 forbids classification ever going remote, so no
      binding for it could name a valid state.
      — `a_categories_entry_naming_a_pinned_category_says_pinned_not_misspelled`
      asserts both, and `every_pinned_category_names_its_own_pin` derives the
      pinned set from `configurable()` so a third one fails until it has its own
      sentence.
- [x] A tier or category binding naming an unregistered provider id is rejected at
      load, naming the id and listing registered ids — the REQ-557 BR-6 shape.
      — all four dangling slots (tier provider/fallback, category
      provider/fallback).
- [x] `judgment_default` is a real config key with a documented default of `edit`,
      and it appears in the `config/get` projection (AC-12, BR-9).
      — **the config half only.** The key exists, defaults to `edit`, round-trips,
      and serializes unconditionally so it is readable from the file. The
      `config/get` projection lives in `tetond`'s `ConfigSnapshot` and
      `teton-protocol`, outside this task's crate scope; AC-12's projection leg
      is carried by whichever task adds the field to `ConfigSnapshot`.
- [x] A config with **no** tier bindings still loads — an empty table is
      incomplete, not corrupt (REQ-557 ADR-E's posture).
- [x] Existing `[[routing]]` entries still deserialize (TASK-055 migrates them);
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

## Implementation Notes (written at completion)

Three calls made here that the task file did not settle:

1. **The AC-4 message is bought in `ConfigurableCategory`'s `Deserialize`, not in
   `Config::load`.** The derived impl was replaced with a hand-written one that
   routes through the existing `FromStr`, so every format gets the pinned
   sentence at once — config TOML and any future JSON-RPC payload that binds a
   category — rather than at whichever call site remembered to check
   (LESSON-484). The *rejection* is still structural: no `Redact`/`Route`
   variant exists to deserialize into, so ADR-B's "unrepresentable, not merely
   forbidden" property is untouched. Only the wording changed.
2. **A duplicate `[[tiers]]` or `[[categories]]` row is a validation error.**
   `CategoryTable::tier_binding` resolves first-row-wins, so a second row would
   be silently ignored — a knob that does nothing, which is the defect BR-1
   exists to remove. Same posture as `DuplicateProvider`/`DuplicateMcpServer`.
   Not in any AC; rejected rather than documented.
3. **`judgment_default` serializes unconditionally** (no `skip_serializing_if`),
   following `local_model.auto_accept`'s precedent. A key that vanishes from a
   written-out config whenever it holds its default is the hidden constant AC-12
   forbids.

**Left for the crate that owns it:** three fields were added to `Config`, and
`tetond`'s test module has two exhaustive `Config { … }` literals
(`crates/tetond/src/runtime.rs`, in `an_unconfigured_default_provider_is_none_not_a_synthesized_id`
and `two_provider_spec_config`) that will not compile until they gain the new
fields or a `..Config::default()` tail. `cargo build -p tetond` is unaffected —
only `cargo test -p tetond`.
