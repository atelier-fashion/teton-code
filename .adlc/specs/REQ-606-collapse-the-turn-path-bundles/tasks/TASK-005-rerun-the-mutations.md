---
id: TASK-005
title: "Re-run every invariant mutation and measure AC-3 against the changed tree"
status: pending
parent: REQ-606
created: 2026-09-01
updated: 2026-09-01
dependencies: [TASK-002, TASK-003, TASK-004]
---

## Description

AC-3, AC-4, AC-5 — and the criterion the whole REQ turns on. AC-4 says the
invariants are **re-run, not re-asserted**: REQ-600 shipped a guard that
silently stopped covering its subject when code moved, and only re-running the
mutation found it. A green suite is not evidence that a mutation would still go
red.

Every mutation is applied to the **changed** tree, run, its observed output
recorded, and reverted — including the three expected to be unaffected, because
"expected unaffected" is precisely the claim LESSON-598 says cannot be read off
the source.

| # | invariant | instrument |
|---|---|---|
| 1 | typed-outcome arms before the generic remote arm | inversion |
| 2 | gate before the parse it guards | substitute: gate constructed exactly once |
| 3 | claim before the session-state re-read | inversion |
| 4 | no blocking wait on the turn path | substitute: no-blocking-wait assertion |
| 5 | `TurnContext` after the warming hold | inversion |

If a collapse has changed a signature such that invariant 2's ordering stops
being compiler-enforced, that is **a finding to record, not a substitution to
make quietly**.

AC-3 is measured, both figures re-derived rather than carried over: the
`run_prompt_turn` body span under REQ-600 AC-1's rule (signature line through
closing brace) and `run_session_turn_with_pressure_policy`'s maximum brace
depth excluding braces inside string, char and comment tokens.

## Files to Modify

- `.adlc/specs/REQ-606-collapse-the-turn-path-bundles/requirement.md` — verification

## Acceptance Criteria

- [ ] Invariants 1, 3, 5 each re-run on the changed tree and observed red
- [ ] Substitutes for 2 and 4 re-run under the same rule
- [ ] REQ-598 event fixture replays **unregenerated**
- [ ] `run_prompt_turn` body span re-derived and under 200
- [ ] `run_session_turn_with_pressure_policy` brace depth re-derived and <= 5
- [ ] `suppression_ratchet.rs` green at its recorded figure
- [ ] Full suite green, grepped for `FAILED`; clippy 0 under `deny`; fmt clean
