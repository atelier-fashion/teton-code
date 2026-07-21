---
id: TASK-010
title: "tetond router: phase policy routing, remote wiring, degradation"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-008, TASK-009]
---

## Description

Wire the harness to remote providers through the router: phase → provider via
the policy table (BR-5, pure logic already in teton-core), `route_decided`
events with reasons, freeform heuristic routing, capability-based degradation
profiles applied to the harness, and provider fallback on failure (AC-7).

## Files to Create/Modify

- `crates/tetond/src/router.rs` — resolve (session phase, policy, provider health) → provider; apply degradation profile to harness settings; emit `route_decided` with reason
- `crates/tetond/src/harness/loop.rs` — accept router-chosen provider + profile per turn (modify)
- `crates/tetond/src/heuristics.rs` — freeform-mode routing heuristics (local for classify/summarize duties, configured default for coding turns); every decision still emits `route_decided` (BR-5)
- `crates/tetond/tests/routing.rs` — policy routing per phase; fallback on simulated provider failure completes the session with `provider_degraded` (AC-7); local-tier-unavailable bypass (BR-8)

## Acceptance Criteria

- [x] Structured-mode calls route per policy table; `route_decided` reason names the rule that fired (BR-5, AC-3 backend)
- [x] Freeform heuristic decisions also emit `route_decided` with reasons (BR-5)
- [x] Simulated provider failure mid-session falls back per failure class and completes (AC-7)
- [x] Local tier unavailable (pressure/benchmark-disabled) → router bypasses without blocking the loop (BR-8)
- [x] Weak-capability provider gets the degraded harness profile (reduced tools, shorter max-turns, mandatory verify) (BR-6)

## Technical Notes

Router consumes policy evaluation from teton-core (pure, tested) — this task
is wiring + events + degradation application, not policy logic. Remote calls
go through egress (TASK-007) so BR-1/BR-2 hold by construction; add one
integration test asserting a routed remote call produced both a CostRecord and
passed boundary inspection.
