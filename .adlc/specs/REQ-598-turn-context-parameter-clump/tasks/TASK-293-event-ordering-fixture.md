---
id: TASK-293
title: "Capture the pre-refactor event-ordering fixture on the base commit"
status: complete
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: []
---

## Description

Record the BR-1 / AC-10 golden event sequence for one full turn **before any
refactor commit lands**, and commit it as a checked-in fixture.

This task is first in the DAG for a reason that is not scheduling convenience:
a fixture generated after the refactor is an oracle computed by the subject —
conventions.md's first named trap, and the mechanism behind three of the seven
false-green assertions REQ-592 shipped (LESSON-569). The fixture's value is
entirely in its provenance.

## Files to Create/Modify

- `crates/tetond/tests/fixtures/req598_turn_event_order.txt` — the golden
  sequence, captured on the base commit
- `crates/tetond/tests/turn_event_order.rs` — the comparison test

## Acceptance Criteria

- [ ] The fixture is captured while `HEAD` is still the branch's base commit
      (no `TurnContext` type exists yet) and is committed in **this** task's
      commit, before TASK-294 begins.
- [ ] The recorded sequence covers a full turn: claim through commit, including
      `route_decided` and the turn's terminal event.
- [ ] The test compares the live sequence against the fixture as an **ordered**
      list, not a set or a count.
- [ ] The test is shown to fail when two adjacent events are transposed; the
      mutation and its observed failure are recorded in the test's doc comment.
- [ ] The fixture file carries a header comment naming the commit sha it was
      captured at.

## Technical Notes

Use the existing event-recording seam the runtime tests already use (see the
`carry_runtime` / recorded-context helpers in `runtime.rs`'s test module and the
`EventBus` subscriber pattern in `crates/tetond/tests/`). Do not invent a new
capture mechanism — a bespoke recorder is a second thing to be wrong.

Normalize only what is genuinely non-deterministic (turn ids, timestamps,
durations). Normalizing anything else weakens the guard; if a payload field
looks unstable, find out *why* before scrubbing it.
