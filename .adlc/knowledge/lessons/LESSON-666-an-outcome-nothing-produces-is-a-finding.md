---
id: LESSON-666
title: "An outcome the protocol defines but nothing produces is a finding"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience"]
tags: ["max-tokens", "stop-reason", "reasoning", "generation-reservation", "dead-variant", "seam"]
req: BUG-229
created: 2026-10-07
updated: 2026-10-07
---

## What Happened

A remote reasoning turn (`kimi-k3`, effort `high`) spent its whole output cap
thinking (`output_tokens: 1024, reasoning_tokens: 1021`), answered nothing, and
the session printed `turn ended (EndTurn)`. Two things had to go wrong at once:

- **One number, two meanings.** `LOCAL_GENERATION_RESERVATION` (1,024) was
  sized for the local tier, whose reply scanner ends a turn long before the cap
  (BUG-147). Remote routes inherited it through `HarnessConfig::default()`
  because `Router::harness_config_for` stamped the profile and the budget but
  not the output cap. A reasoning model counts its thinking against
  `max_tokens`, so on a remote route the same number meant "room to think and
  answer", and 1,024 was not enough room.
- **The truncation fact was dropped at a seam.** Both adapters mapped
  `length`/`max_tokens` to `StopReason::MaxTokens`, and the protocol had a
  `MaxTokens` variant for a turn's end. But `RemoteProviderSource` kept only
  `usage` from `TurnEvent::Completed`, `SourceTurn` had no field to carry the
  stop, and the loop returned `EndTurn` unconditionally. The variant was
  defined at both ends and produced by nothing in between.

## Lesson

1. **Grep for the producer of every outcome variant.** A variant that is
   declared, serialized and rendered but never constructed is either dead or a
   dropped fact. Here `teton_protocol::methods::StopReason::MaxTokens` had zero
   construction sites in `tetond`, which is the whole bug in one search.
   When a terminal event carries more than one field (`usage` *and*
   `stop_reason`), check that every field crosses the seam. Destructuring with
   `..` or taking `completion.usage` alone is where it disappears.
2. **When a constant serves tiers with different semantics, give each tier its
   own and make the sent value and the reserved value one read.** The fix
   stamps `inputs.reservation` (the same `BudgetInputs` that `budget_for`
   derives from) onto `gen_params.max_tokens`, so the room the budget
   subtracts and the room the request asks for cannot diverge. A remote cap is
   bounded above by the smallest model output ceiling you support (a value
   above it is a 400 on *every* request), so pick it conservatively and make
   exhausting it visible rather than reaching for a bigger number.
3. **Moving a reservation moves every budget-sized fixture.** Twelve tests
   failed. Re-size the fixture to keep the property it names (above a floor,
   under a quarter) rather than re-pinning the new number. Three would have
   silently changed subject: a 32k window started deriving a *floored* pair,
   and a "resolvable" leg became unresolvable (LESSON-640, LESSON-645).

## Why It Matters

A truncated answer reported as a finished one is the worst failure shape for
an agent: nothing in the UI, the transcript or the logs says anything went
wrong, and the only witness was the cost ledger read by hand. The same shape
reappeared one layer up within a week: REQ-623's subagent dispatch folds a
child's `MaxTokens` into `completed` (followed up separately).

## Applies When

- Adding or reviewing a `StopReason`/status/outcome enum, or a seam that
  consumes a provider's terminal event.
- Any constant shared between the local tier and remote routes (caps,
  reservations, thresholds), especially one whose meaning depends on the model
  (reasoning tokens, tool-call grammar).
- Changing a budget reservation or window derivation: expect arithmetic-sized
  fixtures to move and re-check what each still proves.
