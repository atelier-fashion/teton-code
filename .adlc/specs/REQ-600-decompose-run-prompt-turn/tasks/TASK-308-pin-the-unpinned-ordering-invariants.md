---
id: TASK-308
title: "Pin the three BR-3 invariants that only a comment holds"
status: complete
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

AC-4. REQ-599 named five ordering invariants as *the* behavioural risk of this
change. Exploration found only two are pinned by a test that would fail on
inversion:

| # | invariant | today |
|---|---|---|
| 1 | typed-outcome arms before the generic remote arm | PINNED (3 tests) |
| 2 | gates before the parses they guard (LESSON-520) | **comment only** |
| 3 | the claim before the session-state read (LESSON-539) | **comment only** |
| 4 | presence gates keep reader-loop freedom (LESSON-518) | **no test located** |
| 5 | `TurnContext` after the warming hold (BR-2.1) | PINNED (1 test) |

This task writes 2, 3 and 4 **against the code as it stands**, before anything
moves. Written after the restructure they would pin the new shape rather than
the invariant, and the restructure would proceed with no net.

## Files to Create/Modify

- `crates/tetond/src/runtime/mod.rs` — tests for invariants 2 and 3
- `crates/tetond/tests/multi_client.rs` — or a new sibling, for invariant 4

## Acceptance Criteria

- [ ] Invariant 2 (gate before parse) has a test that fails when the gate fetch
      at `mod.rs:4522` is moved after `accept_invocation` at `4542`. LESSON-520
      is the reason it matters: a gate placed after a parse makes an
      invalid-payload test vacuous, so the test must assert on a payload the
      parse would reject.
- [ ] Invariant 3 (claim before session-state read) has a test that fails when
      the claim at `4460` is moved after the `session_cwd` re-read at `4474`.
      Per LESSON-539 the scenario is a `/cd` landing in that window; the test
      must mutate session state between the two points, not merely run a turn.
- [ ] Invariant 4 (reader-loop freedom) has a test in LESSON-518's shape: a
      parked verifier on a `#[tokio::test(flavor = "multi_thread")]` runtime, a
      concurrent RPC served on the same connection while the gate is parked, and
      a channel signalling gate entry so the test cannot pass by never reaching
      the gate. **If no presence gate on this path can be parked, record that
      finding** rather than writing a test that cannot fail — an invariant that
      turns out not to be testable by inversion is a result, not a criterion to
      drop quietly.
- [ ] Each inversion is **run and its observed output recorded** in the test's
      doc comment — not predicted. REQ-602 shipped a mutation table containing
      an outcome that could not occur, which meant a bound nobody had seen fire
      (LESSON-597). A mutation that fails to compile is recorded as such.
- [ ] Suite green, grepped for `FAILED`.

## Technical Notes

The two pinned invariants show the shape to copy.
`the_turn_context_carries_the_router_rebound_by_the_hold` (`mod.rs:21519`)
guards BR-2.1 by observing whether the `title` duty lands — an *observable
consequence* of the ordering, not the ordering itself. That is the right
instrument: asserting "line A precedes line B" is a source-scan, and it passes
against code that has the order right and the behaviour wrong.

Do not regenerate or weaken the REQ-598 event fixture to make a test pass
(LESSON-569, AC-5).
