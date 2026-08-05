---
id: TASK-052
title: "route_decided and CostRecord carry the category and tier"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-048]
---

## Description

Make the routing decision legible on the wire: every `route_decided` names the
category, the tier, the provider, and the reason (AC-8, REQ-544 BR-5).

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `RouteDecided` gains `category` and
  `tier`; `phase` becomes genuinely optional. `CostRecord` gains `category`
- `crates/teton-protocol/src/lib.rs` — wire enums for `Category` and `Tier`
- `crates/tetond/src/router.rs` — `Route::route_decided()` populates them
- `crates/tetond/src/cost/{mod,ledger,report}.rs` — thread `category` into the
  ledger alongside the retained `phase`

## Acceptance Criteria

- [ ] Every `route_decided` carries a category, a tier, a provider, and a
      **non-empty** reason (AC-8).
- [ ] The payload is constructed from `CategoryResolution` — not recomputed
      (ADR-D, BR-6).
- [ ] `CostRecord` carries the category while retaining `phase`; the ledger schema
      migration adds the column without rewriting existing rows.
- [ ] A round-trip test covers the new fields, and an existing-row read test proves
      a pre-REQ ledger still loads.

## Technical Notes

**The ledger is append-only with a no-update trigger** (`ledger.rs:58`). Adding a
column must be an `ALTER TABLE`-style additive migration; historical rows get a
NULL category, which is correct — they predate the concept.

**Do not let `phase` and `category` disagree.** For a structured turn both are set
and `category_for_phase` relates them (ADR-F). A test should assert that
relationship holds on a structured turn rather than trusting the two writers.
