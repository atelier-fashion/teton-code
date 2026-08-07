---
id: BUG-157
title: "A turn's final answer is appended after the budget gate, so a turn can end over budget"
status: open
severity: low
created: 2026-08-07
component: "daemon/harness"
domain: "context-management"
found_by: REQ-561 TASK-063
---

## What happens

The turn loop's `EndTurn` arm calls `ctx.push_model(final_text)` **after** the
last `truncate_to_budget()` gate has run. Nothing re-checks the budget
afterwards, so a turn can finish with the context marginally over its budget.

Measured during REQ-561 TASK-063: **4,005 bytes against a 4,000-byte budget.**

## Why it is low severity, not none

The overshoot is bounded by one model answer, and the *next* turn's gate corrects
it before anything is sent — so this does not compound across turns and does not
produce an unbounded context.

It is worth fixing anyway because it makes the budget invariant "the context is
under budget after a turn" false as stated, which is exactly the kind of
almost-true invariant that a later change builds on. REQ-561's own AC-14 had to
be written against *the state the gate guarantees* rather than against turn-end
state, and its loop-level test pins `max_turns: 1` for this reason. A test
working around an invariant is a signal the invariant is wrong.

## Not caused by REQ-561

This predates the `compact` duty and is unrelated to it — `compact` and
`truncate_to_budget` both run before the arm in question. REQ-561 found it while
writing the AC-14 fallback tests and is reporting rather than fixing it, since
changing when the final answer is appended is a behaviour change outside that
REQ's scope.

## Suggested fix

Either re-run the budget gate after `push_model` on the `EndTurn` path, or append
the final answer before the gate rather than after it. The second is likely
simpler but changes what the gate can drop — it could elide the answer it just
produced, so the first is probably correct.

Whichever is chosen, the test to add is the one that does **not** pin
`max_turns: 1`: assert the context is under budget at turn end, not merely at the
gate.

## Related

- REQ-561 AC-14 (the budget is enforced by `truncate_to_budget`, not by the duty)
- ADR-4 in `.adlc/specs/REQ-561-*/architecture.md`
