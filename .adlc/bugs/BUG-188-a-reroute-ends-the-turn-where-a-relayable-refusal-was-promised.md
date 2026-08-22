---
id: BUG-188
title: "A model-invoked expansion caught at a mid-turn reroute ends the turn instead of relaying"
status: resolved
severity: medium
created: 2026-08-22
updated: 2026-08-22
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience"]
tags: ["skills", "skill-tool", "reroute", "refit", "budget", "req-587-residual"]
---

## Description

REQ-587 BR-6 and BR-9 say a refusal is a typed outcome the model can relay —
never a crash, never silent. That holds everywhere except one seam, and REQ-587
ships with the exception stated rather than closed.

A model-invoked expansion caught by `skill_would_not_survive_refit` at a
mid-turn reroute **ends the prompt turn** with
`error_code::SKILL_EXPANSION_TOO_LARGE` instead of reaching the model as a tool
result.

## Reproduction Steps

1. Configure a session with a declared remote window and a `fallback_id`.
2. Have the model invoke a skill whose expansion fits the first route.
3. Trigger a mid-turn reroute after the expansion is committed — the privacy pin,
   or a provider fallback — onto a narrower budget.
4. The turn ends with the typed error; the model gets no relayable result.

## Expected Behavior

The refusal reaches the model as a tool result naming the skill, its size, the
budget and REQ-586's bound, and the turn continues.

## Actual Behavior

The turn ends. The client receives the typed `-32023` with BR-8's full sentence,
so it is neither silent nor a crash — it is a turn that stops where the spec
would prefer one that continues.

## Root Cause

Both `skill_would_not_survive_refit` call sites sit in `run_prompt_turn`'s
`'turn` retry loop, **after** `run_session_turn_with_source` has returned. There
is no `ToolCall` id in scope there and the expansion is already a committed
block, so a tool result is not expressible without restructuring the retry —
which REQ-587 deliberately did not propose.

## Resolution

Closing it means giving the retry a way to fold a result back into the loop it
just left.

## Files Changed

- `crates/tetond/src/runtime.rs` — both guard sites
- Pinned, not assumed:
  `crates/tetond/tests/skill_turn.rs::a_reroute_after_a_committed_model_expansion_refuses_rather_than_eliding_it`
- Recorded in `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` Deferred

## Closed — 2026-08-22

Closed by folding the result back into the loop it just left, which is what the report said closing it would take. `ContextManager::withdraw_block` edits the committed block in place — the expansion is replaced by the refusal the model reads, and the withdrawn block's provenance is **absorbed** into `DroppedProvenance` rather than shed, exactly as the budget gate's own drop path does (a skill block that shed its sources would let a `local-only` body egress next turn). The caller decides what is possible: a **model** call is withdrawn and relayed and the turn continues; a **typed** `/name` still ends the turn, because there is no call to answer; an unfindable block still ends the turn rather than continuing over a conversation it does not recognise. The caller fork is pinned by mutation.
