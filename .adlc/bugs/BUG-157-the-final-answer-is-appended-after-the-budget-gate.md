---
id: BUG-157
title: "A turn's final answer is appended after the budget gate, so a turn can end over budget"
status: open
severity: low
created: 2026-08-07
updated: 2026-08-12
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

## Root Cause

The loop's budget gate runs **at the top of each iteration**, and both of the
loop's exits are reachable without passing it again.

1. **`EndTurn`** — `ctx.push_model(final_text)` appends the model's answer and
   returns immediately. This is the path the report measured: **5 bytes** over a
   4,000-byte budget.
2. **`MaxTurnRequests`** — found while fixing the first, and **not named in this
   report**. The turn-cap check sits *above* the gate, so a previous iteration's
   pushes (a model turn plus its tool result) leave by that door ungated. It is
   the larger of the two: a padded fixture ends **2,936 bytes** over.

Fixing only the reported exit would have left the postcondition false at the
other, with a comment claiming it held.

## Resolution

`ctx.truncate_to_budget()` before both returns — the report's first option
("re-run the budget gate after `push_model`"), which it judged correct, and it
is. The second option (append before the gate) was rejected for the reason the
report gives: the gate could elide the answer it just produced.

Safe for the answer: `truncate_to_budget` drops oldest-first, never removes the
last block, and at worst middle-truncates it. `final_text` is returned whole and
has already been streamed, so what the user receives is untouched — only what the
next turn carries is bounded.

### Verification

`harness::turn_loop::tests::a_turn_ends_under_budget_however_it_ends` asserts the
postcondition at **both** exits, and deliberately does not pin `max_turns: 1` —
that pin is the workaround this bug is about.

Mutation-checked per gate, and the fixture had to be parameterised to make both
legs real. Each exit needs the *opposite* shape to be able to fail: `EndTurn`
needs the gate to leave the context at the budget edge so a short answer tips it
(`pad: 0`), while `MaxTurnRequests` needs the post-gate pushes to breach the
budget alone (`pad: 3_000`). With a single fixture one leg always passed
regardless of its gate — a green test with a dead half, which is the same
complaint this bug makes about `max_turns: 1`.

| Mutation | Result |
|---|---|
| Remove the `EndTurn` gate | red — "ends by answering: 5 bytes over" |
| Remove the `MaxTurnRequests` gate | red — "ends by exhausting its turns: 2936 bytes over" |

Workspace: 2218 passing across 45 targets, fmt and clippy clean.

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — gate both loop exits; add the
  turn-end postcondition test and the parameterised fixture

## Suggested fix

Either re-run the budget gate after `push_model` on the `EndTurn` path, or append
the final answer before the gate rather than after it. The second is likely
simpler but changes what the gate can drop — it could elide the answer it just
produced, so the first is probably correct.

Whichever is chosen, the test to add is the one that does **not** pin
`max_turns: 1`: assert the context is under budget at turn end, not merely at the
gate.

## Do this as part of the fix

REQ-561's ADR-4 wants the budget gate to be **structurally** unconditional. At
TASK-065 the mutation `if compaction.degraded { ctx.truncate_to_budget(); }` was
found to leave the whole suite green — an *equivalent mutant at the loop*,
because every reachable outcome there is already under budget. That equivalence
depends on how many blocks the loop has pushed before compaction runs, which is
exactly what this bug's fix changes.

**So: re-run that mutation after fixing this.** If it becomes catchable, add the
test — the guarantee would then rest on a discriminating assertion rather than on
a reachability argument, which is where ADR-4 wants it.

## Related

- REQ-561 AC-14 (the budget is enforced by `truncate_to_budget`, not by the duty)
- ADR-4 in `.adlc/specs/REQ-561-*/architecture.md`, including its "honest
  limitation" note
