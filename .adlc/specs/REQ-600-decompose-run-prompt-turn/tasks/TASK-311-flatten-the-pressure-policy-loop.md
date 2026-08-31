---
id: TASK-311
title: "Flatten run_session_turn_with_pressure_policy from brace depth 9 to 5"
status: draft
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-308]
---

## Description

AC-3. `run_session_turn_with_pressure_policy` is 762 lines at **brace depth 9**
(11 by indentation). AC-3 gates on the brace rule and requires the indentation
figure reported alongside, because the two disagree by 2 against a target of 5.

Depth distribution, measured:

| depth | lines |
|---:|---:|
| 6 | 236 |
| 7 | 93 |
| 8 | 56 |
| 9 | 46 |
| 10 | 5 |

200 lines sit at depth ≥ 7 in two clusters; the peak is `turn_loop.rs:1859`.
Roughly 430 lines must move into helpers to reach 5.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs`

## Acceptance Criteria

- [ ] Max brace depth inside the fn is **5 or below**; the indentation-rule
      figure is measured and recorded alongside it.
- [ ] Behaviour unchanged: this is a flattening, not a redesign. The BR-3
      ordering invariants that touch this file still hold and their tests still
      fail on inversion.
- [ ] `crates/tetond/tests/suppression_ratchet.rs` stays green **or its
      `REACHED` is updated deliberately with what collapsed.** `turn_loop.rs`
      carries 4 `too_many_arguments` suppressions and the ratchet is bounded on
      both sides at 13, so removing one trips the *lower* bound. That is the
      ratchet working: a drop is only good news if it was intended, and saying
      which suppression went and why is the point.
- [ ] Suite green, grepped for `FAILED`; clippy clean under `deny`; fmt clean.

## Technical Notes

Independent of TASK-309/310 — different file, different parameter cluster
(REQ-598 ADR-2) — so it can run alongside the relocation. It depends on TASK-308
only because the invariant tests are the net.

Extracting the depth-6 band (236 lines) is what actually moves the number; the
deeper clusters ride along inside it. Prefer early-return guards over nested
`if let`, which is what produced the 46 lines at depth 9.
