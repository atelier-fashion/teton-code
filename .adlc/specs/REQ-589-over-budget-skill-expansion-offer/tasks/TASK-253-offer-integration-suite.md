---
id: TASK-253
title: "Integration suite for the offer"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-247, TASK-248, TASK-250]
---

## Description

A dedicated suite — the ACs cut across too many existing files to bolt onto `skill_turn.rs`. Drive everything end-to-end from a real turn (LESSON-544/552).

## Files to Create/Modify

- `crates/tetond/tests/skill_over_budget_offer.rs` (new)
- `crates/tetond/tests/egress_capture.rs` — AC-18's not-sent legs
- `crates/tetond/tests/skill_tool_loop.rs` — AC-5, model path never offered

## Acceptance Criteria

- [ ] AC-1 reproduces the reported failure (4,097 words vs 4,096, `bound: local engine`) and accepting dispatches the expansion whole
- [ ] AC-3, AC-4, AC-5, AC-6, AC-7, AC-7a, AC-7b, AC-9, AC-10, AC-11, AC-18, AC-22, AC-23, AC-24 each have a named test
- [ ] Every new wire fact is driven from a real turn, never a struct literal; mutating a producer line reddens the suite (AC-12, LESSON-544/552)
- [ ] Only reachable (bound, verdict) cells are exercised — no vacuous tests (LESSON-520)
- [ ] Run with `--no-fail-fast`; build the workspace before any targeted `-p tetond --test` run
- [ ] **AC-15**: the dogfood runbook is authored as part of this task — reproduce the reported `/analyze` failure on a local-engine route, accept the offer, and record the measured pair, the verdict, and the outcome (not just pass/fail). Execution happens at wrapup on a real machine; the runbook itself is a deliverable here, and it is the first data point REQ-590 needs

## Technical Notes

Fixture gap: `skill_turn.rs`'s Harness cannot build LocalEngine/UserCap/RedactScan routes. Use `context_pressure.rs`'s spawned-daemon pattern (:1095 is the only existing local-route skill refusal) and `budget.rs`'s `remote(window, cap, redact_scan)` for the remote bounds.
