---
id: TASK-042
title: "Mutation-check the feature and document the loading window"
status: draft
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: [TASK-040, TASK-041]
---

## Description

Prove the suite actually fails when the feature is disabled (AC-8), and record
the user-visible behaviour where the project records such things. A suite that
stays green with the animation frozen has not tested the animation — that is the
whole of LESSON-441 and LESSON-464.

## Files to Create/Modify

- `crates/teton/src/loading.rs` — mutation-check documentation comment naming what breaks which test
- `crates/teton/tests/pty_e2e.rs` — the idle-render mutation assertion, if it belongs at e2e level rather than unit
- `docs/manual-verification.md` — the restart step gains the indicator observation
- `README.md` — one line in the session section, if the behaviour is user-facing enough to warrant it

## Acceptance Criteria

- [ ] Freezing the tick (making `frame` ignore its `tick` argument) fails at
      least one test. Demonstrate by actually applying the mutation and
      recording the failing test name — not by asserting it would.
- [ ] Removing the idle-render path (reverting the loop to block on
      `read_line`) fails at least one test — the AC-2 pty leg is the expected
      one; confirm it is.
- [ ] `docs/manual-verification.md` §6's restart step tells a human what they
      should now see during the load window, replacing the BUG-152 notice line
      added earlier with the live behaviour.
- [ ] Every AC in the requirement is traceable to a test or a manual-verification
      line — produce the mapping and record any AC that ends up with neither
      (there should be none; if there is, that is a finding, not a footnote).

## Technical Notes

- The two mutations are the ones that matter because they correspond to the two
  halves of the REQ: the state machine's motion (TASK-039) and the loop's
  timeliness (TASK-038). A mutation that only breaks a compile is not evidence
  (LESSON-464: a new control needs its own known-bad in the same pass).
- `docs/manual-verification.md` already gained a BUG-152 line in §6 about the
  `>> model still loading —` notice. That notice is the *fallback* now (BR-9),
  not the primary experience — update it rather than adding a second bullet that
  contradicts it.
- Keep the README edit small or skip it with a note; the in-session command
  table there is about commands, and this is ambient behaviour.
