---
id: TASK-050
title: "Router dispatches by category; delete AUXILIARY_SIGNALS and route_freeform"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-048]
---

## Description

The headline change. The router resolves every turn through the category chain in
**both** modes, and the ten-word substring list that decided routing in the default
experience is deleted.

## Files to Create/Modify

- `crates/tetond/src/router.rs` — `resolve_structured` takes a category;
  `resolve_freeform` resolves through the category chain; both call
  `teton_core::category::resolve`
- `crates/tetond/src/heuristics.rs` — **delete** `AUXILIARY_SIGNALS`,
  `route_freeform`, `FreeformDuty`, `classify_duty`, and their tests
- `crates/tetond/src/runtime.rs` — `build_router` builds the tier/category table

## Acceptance Criteria

- [ ] A freeform turn resolves through category → (override or tier) → provider,
      reading the configured table (BR-1). The table is consulted on **every**
      turn, freeform included.
- [ ] `AUXILIARY_SIGNALS`, `route_freeform`, `FreeformDuty`, and `classify_duty`
      no longer exist anywhere in the workspace (BR-2 — "deleted, not relocated").
- [ ] Session taint remains the **outermost** check, evaluated before any category
      resolution (BR-7). A test asserts a tainted session routes local with
      `think` bound to a remote provider.
- [ ] The category resolver's provider is screened through `Router::is_routable`,
      so an unusable provider is never selected (ADR-E, BUG-155).
- [ ] Existing router tests that assert phase dispatch are migrated to category
      dispatch; the `AUXILIARY_SIGNALS` tests are deleted rather than adapted.
- [ ] `resolve_local_pin`'s behavior is unchanged.

## Technical Notes

**Taint ordering is a privacy guarantee, not a preference.** `runtime.rs:1281`
checks `session_taint.is_tainted` *before* any routing decision, and REQ-558 must
not move it. Category routing is a cost decision; the boundary is a guarantee
(BR-7, LESSON-432).

**Deleting the heuristic removes a hazard the test suite depended on not
noticing.** BUG-155 found that `AUXILIARY_SIGNALS` silently sent any prompt
containing "summarize"/"explain"/"describe"/"grep" to the local tier — which made
boundary tests vacuous whenever the fixture prompt happened to contain one. After
this task that trap is gone; TASK-057 adds the assertion that replaces vigilance
with a check.

**Do not reintroduce a two-way split.** The temptation is a "cheap vs expensive"
fast path. That is `AUXILIARY_SIGNALS` with new names — the category chain is the
only dispatch.
