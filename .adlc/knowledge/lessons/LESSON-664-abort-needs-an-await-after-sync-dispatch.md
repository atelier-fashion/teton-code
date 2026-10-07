---
id: LESSON-664
title: "An abort needs an await boundary after every synchronous tool dispatch — or the aborted task takes one more step"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "tokio", "daemon"]
concerns: ["reliability", "security", "cost"]
tags: ["async-cancellation", "block_in_place", "yield_now", "child-turns", "agent-tool", "cancellation-point", "req-623", "bug-226"]
req: REQ-623
created: 2026-10-07
updated: 2026-10-07
---

## What Happened

REQ-623's child turn-loops run as tokio tasks and dispatch tools through
`block_in_place_if_multithread(|| tools.dispatch(..))` — the BUG-226 path.
When a child timed out or its parent was cancelled while the child sat inside
a synchronous `shell` call, tokio set the abort flag but could not act on it
until the task reached an `.await`. After the dispatch returned there was no
await before the loop folded the result, so the child kept going: it published
the call's `tool_call_update` after `agent_child_finished` had already said
the child ended, pinned the session local when the tool had read boundary
content, and in 1 of 14 runs sent the discarded tool output to its next model
call. Two strict e2e tests (`statuses::timed_out`,
`statuses::cancelled_with_tool_in_flight`) caught it; the fix is one line —
`tokio::task::yield_now().await` after the dispatch arm when
`current_child().is_some()` (`crates/tetond/src/harness/turn_loop.rs:2160`) —
and a unit test that fails 4 of 4 runs without it.

## Lesson

An abort is deferred until the next await. A task that runs synchronous code
via `block_in_place` (or joins a `spawn_blocking`) and then continues
synchronously — folding a result, publishing an event, deciding the next model
call — has no cancellation point in that stretch, so "cancelled" means "cancelled
after one more step". Put an explicit cancellation point (`yield_now().await`)
immediately after every such dispatch in any scope that can be aborted from
outside, and prove it with a test that aborts the task while the tool is parked
and asserts no further step happened.

## Why It Matters

One extra step is enough to bill a metered call after a spend share was
released, to send bytes that should have been discarded to a remote provider,
and to publish events out of the order the client renders by. The race is
timing-dependent, so ordinary unit tests pass; only a test that parks the tool
and aborts mid-call sees it.

## Applies When

- Aborting a tokio task whose body includes `block_in_place` or a joined
  `spawn_blocking`.
- The task continues synchronously after the blocking section before its next
  natural await.
- Nested or concurrent loops where a parent's cancellation or a deadline must
  guarantee the child stops before its next side effect — child turns, duties,
  any future background work the loop awaits.
