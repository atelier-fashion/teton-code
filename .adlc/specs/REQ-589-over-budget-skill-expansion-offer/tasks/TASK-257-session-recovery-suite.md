---
id: TASK-257
title: "Session-recovery suite: withdrawal, the observed-rejection memo, and the closed circle"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-249, TASK-250]
---

## Description

The second half of the integration coverage, split from TASK-253 so neither suite
carries more than three dependencies and each has one subject. This one owns D-8's
promise: **an approval must not leave the session hitting the same wall.**

## Files to Create/Modify

- `crates/tetond/tests/skill_over_budget_recovery.rs` (new)

## Acceptance Criteria

- [ ] **AC-22**: an accepted turn that fails at the window withdraws its expansion, and a
  real second turn in the same session assembles without it
- [ ] **AC-23**: after an observed rejection, the next offer for the same skill on the
  same route names the prior rejection and leads with the remedy. Two negative
  assertions guard BR-10's boundary: the record must not suppress the offer, and must not
  pre-answer it
- [ ] **AC-24**: after a `BindTierRemote` remedy is applied, an identical second
  invocation reaches **no offer at all**, because the route now fits — the end-to-end
  proof that the reported `/analyze` circle is closed
- [ ] The observed-rejection record is asserted to live in one store, daemon-side; a test
  proves the CLI does not memoize it (ASSUME-017)
- [ ] Every assertion is driven from real turns, never struct literals (LESSON-544/552)

## Technical Notes

AC-24 is the criterion that matters most to the user who filed this: it is the difference
between a feature that explains the dead end and one that removes it. Build it against the
same spawned-daemon fixture `context_pressure.rs:1095` uses, since that is the only
existing pattern that constructs a real local-engine route through a skill refusal.
