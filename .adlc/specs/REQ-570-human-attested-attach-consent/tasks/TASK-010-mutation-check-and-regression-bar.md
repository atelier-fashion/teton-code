---
id: TASK-010
title: "AC-11 mutation check and the AC-8 regression bar"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-002, TASK-006, TASK-007, TASK-008, TASK-009]
---

## Description

This REQ ships mostly **refusals**, and a refusal is verified only by proving it
is load-bearing (LESSON-441). AC-11 names four mutations; each must make at least
one test red.

## Files to Create/Modify

- No production change expected. Test additions where a mutation survives.

## Acceptance Criteria

- [ ] Mutation 1: making the attestation verifier return success unconditionally
      makes at least one test red.
- [ ] Mutation 2: dropping the single-use/expiry binding (BR-6) makes at least
      one test red.
- [ ] Mutation 3: restoring the unattested self-approval routing arm (BR-3) makes
      at least one test red.
- [ ] Mutation 4: removing a method's connection check (BR-10a) makes at least
      one test red — for **each** of the seven, not one representative.
- [ ] AC-8 regression bar re-asserted end to end: single-client create → prompt →
      stream and the creator's own attach run with zero new prompts.
- [ ] Each mutation is actually **run** and seen red, then restored. A mutation
      check asserted from reasoning is not a mutation check.

## Technical Notes

- **BUG-159 hazard, called out by name.** `call_sites.rs` and `harness/duty.rs`
  read production source with `.expect("readable source file")` after walking it,
  so any writer touching `src/` mid-run panics those five tests. A mutation check
  is exactly that pattern. Do **not** run mutation checks concurrently with
  edits; if you see that panic, it is BUG-159, not the mutation.
