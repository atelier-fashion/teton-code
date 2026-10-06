---
id: TASK-432
title: "Router tier request resolution and the gate's observable ask-await"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: []
repo: teton-code
---

## Description

Two small seams the child runner (TASK-427) consumes, split out so that task stays the
runner alone. (1) `Router` accepts an optional `Tier` request: the tier's binding when one
is configured, else the category's default route, then the boundary pin — a request the
router cannot honour is not a refusal (BR-6). (2) `PermissionGate` exposes when an ask is
awaiting a human (a guard or an `Instant` pair around the pending await) so
`PausableDeadline` can stop the child's clock during consent (BR-5).

## Files to Create/Modify

- `crates/tetond/src/router.rs` — `resolve_with_tier_request` (or an `Option<Tier>` on the existing resolver); table-driven tests: binding present, binding absent, pinned under a boundary
- `crates/tetond/src/harness/permissions.rs` — ask-await observation hook; test that the hook fires only while an ask is pending
- `crates/tetond/tests/routing.rs` — the three-row tier-request matrix against a spawned daemon config
- `crates/teton-core/src/category.rs` — the pure policy, `resolve_with_tier_request` (conventions: router policy decisions live in `teton-core`); `resolve_row` factored out of `resolve` so a requested row is screened by the same code and sentences
- `crates/tetond/src/call_sites.rs` — registers the new resolving entry point with the call-site scan (`the_scan_covers_every_router_entry_point` fails on any unregistered `resolve*` method)

## Acceptance Criteria

- [x] `tier: build` with a Build binding → that route; without one → the category default; under a boundary pin → local, in every case with no refusal
- [x] The parent's own route is unaffected by a child's request (pure function: no shared state)
- [x] The ask-await hook reports the pending interval and nothing outside it
- [x] Mutation recorded: drop the binding lookup and name the row that reddens

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/tetond/src/router.rs::tests::tier_request_binding_default_then_pin` | yes |
| AC-9 | test-case | `crates/tetond/tests/routing.rs::child_tier_request_matrix` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/permissions.rs::tests::ask_await_hook_brackets_the_pending_interval` | yes |

## Technical Notes

- Conventions: router policy decisions are pure functions in `teton-core` with table-driven tests — put the tier→binding→default→pin resolution there if `Router` already delegates policy that way; keep `router.rs` the I/O edge.
- REQ-558's four-tier binding table is the lookup; no new routing policy.
