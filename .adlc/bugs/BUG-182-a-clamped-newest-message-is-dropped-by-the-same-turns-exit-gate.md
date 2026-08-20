---
id: BUG-182
title: "A clamped newest message is dropped by the same turn's exit gate — the answer is retained without the question"
status: open
severity: medium
created: 2026-08-20
updated: 2026-08-20
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["correctness", "developer-experience"]
tags: ["context-budget", "truncate_to_budget", "clamp", "carry", "context_pressure", "req-586", "req-567", "req-561"]
---

## Description

When a user's message is large enough that `truncate_to_budget` middle-elides
it in place, the clamp fills the byte budget **exactly**. Appending the
model's reply then puts the context back over budget, so the turn's exit gate
runs and drops the now-oldest block — which is the user's own clamped message.
The conversation retains the answer without the question.

Found in REQ-586's TASK-193 while building the AC-10 fixture: the turn
publishes a *second* `context_pressure`, correctly announced, and the test
pins the elision as the first event and as the only one carrying
`newest_user_elided`.

This is pre-existing REQ-561/REQ-567 behaviour. REQ-586 did not cause it — it
made it **legible** (two events, and a notice in the turn's output), which is
BR-7's whole claim. Making it not *happen* is this bug.

## Reproduction Steps

1. On any route, send a single user message large enough to be middle-elided
   in place (larger than the byte budget on its own).
2. Let the turn complete normally.
3. Read the retained conversation, or `/verbose` the next turn.

## Expected Behavior

The message the user actually sent survives its own turn. Either the clamp
reserves room for the reply before filling the budget, or the turn is refused
up front the way an oversized skill turn is (REQ-585 BR-8) rather than being
clamped into something that cannot survive.

## Actual Behavior

Two `context_pressure` events — `block_elided` then `blocks_dropped` — and a
retained conversation holding the assistant's answer with no user turn before
it. Nothing is silent, but the next turn's context is missing the question the
answer belongs to.

## Environment

- Platform: any; found on macOS during REQ-586 (merged `c9e9265`)
- Applies to every tier; the smaller the budget, the easier to hit

## Root Cause

_(not investigated — filed from REQ-586's verify pass)_

`ContextManager::truncate_to_budget` clamps the last block to `room`
(`budget_bytes − non_last`, floored at 1,024) with no allowance for what the
turn is about to append. The generation reservation is subtracted from the
**window** when the budget is derived (REQ-586 BR-2), not from the budget when
a block is clamped, so the clamp is free to consume the whole of it.

## Resolution

_(open)_

Candidate shapes, cheapest first: reserve the turn's `gen_params.max_tokens`
(in bytes, at the route's floor) before computing `room`; or refuse a turn
whose newest block cannot fit with that reservation, reusing REQ-585 BR-8's
typed refusal; or retain the pre-clamp user text out-of-band so the carry
keeps the question even when the block is dropped.
