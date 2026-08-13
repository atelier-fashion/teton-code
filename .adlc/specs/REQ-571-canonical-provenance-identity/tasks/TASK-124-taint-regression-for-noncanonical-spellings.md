---
id: TASK-124
title: "Regression coverage for taint reached via a non-canonical spelling"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-9. This adds no pinning behavior — BUG-156 already delivered the
failover/retry pin and is resolved. It proves the existing pin is now actually
*reached* when a session is tainted through a spelling that previously matched
nothing, and that the second-hop web-lookup channel closes with it.

## Files to Create/Modify

- `crates/tetond/tests/provenance_egress.rs` — add the AC-8 cases.

## Acceptance Criteria

- [ ] AC-8: a session tainted via a non-canonical spelling (absolute-inside-root, and separately `..`-traversing) reaches the existing local-tier pin.
- [ ] AC-8: a subsequent model-composed `web_fetch` in that session is refused.
- [ ] A control asserts a session tainted via the canonical spelling behaves identically — the two paths must not diverge.
- [ ] The test names BUG-156 and states that it verifies existing behavior is reached, not new behavior added, so a future reader does not mistake it for duplicate coverage.
- [ ] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

`context_is_sensitive` (`crates/tetond/src/runtime.rs:6112-6124`) calls the same
`inspect` + `BoundaryMatcher` path, so it inherits the TASK-119 fix with no
production change here. The web gate is at
`crates/tetond/src/egress/lookup.rs:882-893`, keyed on
`Authorship::ModelComposed` + `taint.is_tainted`.

If this task requires a production change, that is a signal TASK-119 is
incomplete — investigate there rather than patching the taint path.
