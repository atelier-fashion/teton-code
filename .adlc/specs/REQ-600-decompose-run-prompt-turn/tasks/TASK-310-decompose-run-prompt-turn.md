---
id: TASK-310
title: "Decompose run_prompt_turn into a named stage sequence"
status: draft
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-309]
---

## Description

AC-1. `run_prompt_turn` is 1,084 lines carrying session claiming, context
assembly, routing, the warming hold, harness and tool assembly, skill expansion,
budget settlement, an attempt loop and commit. Its body must become a sequence of
named stages under 200 lines, measured by body span (signature line through
closing brace) — the same rule that reports it at 1,084 today.

## Files to Create/Modify

- `crates/tetond/src/runtime/turn.rs` — the stages and the orchestrator

## Acceptance Criteria

- [ ] `run_prompt_turn`'s body is under 200 lines by the stated rule.
- [ ] The stages follow ADR-1: `&self` methods, `TurnContext` carrying the
      bundle, and **`route` an explicit parameter** — `turn_context.rs` ADR-3
      excludes `route` deliberately, because it is rebound on every fallback
      reroute and keeping it in the signature is what keeps the reroute visible.
- [ ] No `TurnStages` type is introduced. `turn_context.rs` forbids the shape in
      terms: "a context that starts answering questions becomes a second place
      for turn logic to live, which is exactly what REQ-599 has to untangle."
- [ ] `TurnContext::new` appears in the orchestrator body as a named statement
      **immediately after the warming hold**, so BR-2.1's ordering is held by the
      shape rather than by a comment 57 lines away (ADR-2). The existing test
      `the_turn_context_carries_the_router_rebound_by_the_hold` still passes.
- [ ] All five BR-3 invariants still pinned — TASK-308's three plus the two that
      already were. Re-run each inversion against the **restructured** code and
      confirm it still goes red; a test that pinned the old shape and silently
      stopped biting is the failure this ordering exists to prevent.
- [ ] AC-5: the REQ-598 event-ordering fixture replays identically. It is **not
      regenerated** — a golden file computed by its own subject is not an oracle
      (LESSON-569).
- [ ] Suite green, grepped for `FAILED`; clippy clean under `deny`; fmt clean.

## Technical Notes

Measured stage boundaries and the values crossing each, from the current body —
the evidence these are seams and not convenient line numbers:

| stage | lines | escaping values |
|---|---:|---:|
| claim the session | 71 | 2 |
| assemble config, gate, skills | 27 | 3 |
| resolve the route (incl. hold) | 106 | 5 |
| **`TurnContext::new` — the pivot** | — | 1 |
| assemble harness, tools, system | 157 | 7 |
| settle expansion and budget | 227 | 8 |
| run the attempt loop | 423 | 4 |
| commit the outcome | 24 | 0 |

Stages before the pivot cannot take a `TurnContext` — it does not exist yet and
BR-2.1 says it must not. They take explicit parameters and return small named
values. That asymmetry is the invariant expressed in types, not an inconsistency
to smooth over (ADR-3).

`mod.rs:25940` asserts `.offer_or_refuse_over_budget(` has **exactly two** call
sites, "both `run_prompt_turn`'s own budget stages". Both move here; the
assertion must be repaired in TASK-312, not deleted.
