---
id: BUG-229
title: "A remote reasoning turn spends its 1,024-token output cap thinking and ends silently as EndTurn"
status: open
severity: high
created: 2026-09-26
updated: 2026-09-26
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience"]
tags: ["max-tokens", "reasoning", "kimi", "end-turn", "generation-reservation", "remote-route", "analyze"]
introduced_by: []
attribution: none
---

## Description

A turn routed to a remote reasoning model (`kimi-k3` on the `think` tier,
effort `high`) ended in the middle of the work with no text and no tool call,
and the daemon reported it as an ordinary `EndTurn`. The user saw
`turn ended (EndTurn)` after a tool result and nothing else.

The final model call's cost record shows why:
`output_tokens: 1024, reasoning_tokens: 1021`. The call hit its output cap
while still reasoning, so the answer was empty.

## Reproduction Steps

1. Configure a remote reasoning provider (observed: kimi, `kimi-k3`) and route
   `design` to its `think` tier with effort `high`.
2. In a teton-code session, run `/analyze` (any multi-step skill works; it
   just needs a model call that reasons for more than ~1k tokens before acting).
3. After a few tool calls, one model call spends its whole budget reasoning.

Observed 2026-09-26, session `sess-cy6w6jagfw5b0tynjda7x150t8`, transcript
`20260926T134736Z-sess-cy6w6jagfw5b0tynjda7x150t8.jsonl`, record 87 (the last
`cost_recorded` before shutdown).

## Expected Behavior

A remote turn gets an output cap sized for the remote model, with room for
reasoning plus prose plus a complete tool call. If the cap is still hit, the
turn says so: the provider's `length` / `max_tokens` stop surfaces as
`MaxTokens` (or the call is retried or continued), never as a silent
`EndTurn` with an empty reply.

## Actual Behavior

- `HarnessConfig::default().gen_params.max_tokens` is
  `LOCAL_GENERATION_RESERVATION` = 1,024 (`harness/budget.rs:343`, used at
  `harness/turn_loop.rs:580`). The comment there sizes it for "prose plus a
  complete tool call" on the local tier (BUG-147). The same value goes to the
  remote call via `completion.rs:653`, where a reasoning model counts its
  thinking against it.
- The turn loop's no-tool-call branch returns `StopReason::EndTurn`
  unconditionally (`turn_loop.rs:2668`), even though the provider layer maps `length`/`max_tokens` to
  `StopReason::MaxTokens` (`teton-providers/src/lib.rs:148`). The truncation is
  invisible to the user and to the transcript.

## Environment

- Platform: macOS, Teton Code v0.1.36 (`main` at `4aede48`)
- Provider: kimi / `kimi-k3`, `think` tier, effort `high`, window bound
  (1,000,000 tokens)

## Root Cause

(to confirm during investigation) A local-tier generation reservation is
reused as the remote output cap. On top of that, a max-tokens stop with an
empty reply is folded into `EndTurn`.

## Resolution

(filled after fix)

## Files Changed

(filled after fix)
