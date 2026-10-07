---
id: BUG-229
title: "A remote reasoning turn spends its 1,024-token output cap thinking and ends silently as EndTurn"
status: resolved
severity: high
created: 2026-09-26
updated: 2026-10-07
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

Confirmed. Two defects, and either alone would have been survivable:

1. **Remote routes sent the local tier's output cap.** `HarnessConfig::default()`
   sets `gen_params.max_tokens` to `LOCAL_GENERATION_RESERVATION` (1,024),
   sized for the local reply scanner, which ends turns long before the cap
   (BUG-147). `Router::harness_config_for` built every remote route's config
   from that default and stamped only the profile and the budget, so the
   1,024 went through unchanged to `TurnRequest::max_tokens`
   (`completion.rs`). `Router::budget_inputs_for` subtracted the same 1,024
   from a remote window. A reasoning model counts thinking against
   `max_tokens`, so at effort `high` it used 1,021 of the 1,024 tokens
   thinking and had nothing left to answer with.
2. **The provider's stop reason was thrown away.** The adapters map
   `length`/`max_tokens` to `StopReason::MaxTokens` on
   `TurnEvent::Completed`, but `RemoteProviderSource::produce_turn` kept only
   `completion.usage` from that event. `SourceTurn` had no field for the stop,
   and the turn loop's end-of-turn arm returned `StopReason::EndTurn`
   unconditionally. The protocol's `MaxTokens` variant existed and nothing
   ever produced it.

Attribution: none. The blamed commits (`8ecffbac`, `84f40271`, `83826257`,
`71238fec`) carry no REQ trailer; the lines are older than the trailer
convention.

## Resolution

- **A remote cap, reserved and sent as one number.** New
  `budget::REMOTE_GENERATION_RESERVATION` = 8,192, read through
  `remote_generation_reservation()`. `Router::budget_inputs_for` subtracts it
  from a remote window, and `Router::harness_config_for` stamps the same
  `inputs.reservation` onto a remote route's `gen_params.max_tokens`, so the
  room the budget reserves is the room the request asks for. The local tier is
  unchanged at 1,024. Why 8,192: a `max_tokens` above a model's output ceiling
  is a 400 on every request, and 8,192 is the largest value every documented
  provider accepts (DeepSeek's `deepseek-chat` stops there). `big_window_notice`
  derives under the same reservation.
- **A cap hit is reported as `MaxTokens`.** `SourceTurn` gains
  `stopped_at_cap`. The remote source sets it from the provider's `Completed`
  stop reason; the local source sets it when `completion_tokens` reaches the
  cap it asked for. The turn loop's end-of-turn arm returns
  `StopReason::MaxTokens` when it is set, so the CLI prints
  `turn ended (MaxTokens)` instead of `turn ended (EndTurn)`.
- **Consequence, deliberate:** every remote budget is 7,168 tokens smaller.
  A 128k window goes from 84,650 to 79,872 words and a 1M window from 665,984
  to 661,205. Fixtures sized against the old figures were re-sized (not
  re-pinned) so each still exercises what it names; see Files Changed.

Tests (each mutation-checked; mutations recorded in the doc comments):
`a_remote_turn_that_ran_out_of_output_says_so`,
`a_local_turn_cut_at_its_cap_says_so`,
`a_turn_whose_answer_ran_out_of_output_ends_as_max_tokens`,
`a_remote_route_sends_the_remote_output_cap_its_budget_reserved`. Reverting
the remote reservation alone reddens 6 router tests.

Not in scope: a per-provider `capabilities.max_output` override for models
that need more than 8,192 (a long reasoning run at effort `high`). With this
fix that case is visible as `MaxTokens` rather than silent.

## Files Changed

- `crates/tetond/src/harness/budget.rs`: `REMOTE_GENERATION_RESERVATION` and
  its accessor; `big_window_notice` derives under it; figures in prose and in
  the notice test updated.
- `crates/tetond/src/router.rs`: remote `budget_inputs_for` reservation;
  `harness_config_for` stamps the remote cap; new router test; golden and
  literal budgets re-captured (the doc comment on `BUDGET_FOR_GOLDEN` records
  the move).
- `crates/tetond/src/harness/completion.rs`: `SourceTurn::stopped_at_cap`,
  set by both sources; two tests.
- `crates/tetond/src/harness/turn_loop.rs`: the end-of-turn arm returns
  `MaxTokens` when the cap stopped generation; test; test sources updated.
- `crates/teton-protocol/src/events.rs`: doc figure.
- `crates/tetond/tests/e2e/ac_matrix.rs`: AC-15b's fallback window
  32,000 → 40,000 (32,000 now floors).
- `crates/tetond/tests/skill_over_budget_offer.rs`: the ADR-6 rule-2 fixture's
  window 30,000 → 40,000 (30,000 now floors, which leaves the resolvable leg
  nothing to prove).
- `crates/tetond/tests/skill_turn.rs`: three 60 KB skill bodies → 55 KB (the
  128k quarter is now 59,904 B); the digest table row and reservation updated.
- `crates/tetond/tests/redact_egress.rs`, `crates/tetond/tests/routing.rs`:
  derive under the remote reservation the router really uses.

## Deployment

Merged 2026-10-07 as atelier-fashion/teton-code#333 (squash `7118be42`). There's
no deploy target: this repo ships by release (plain OSS flow), so the fix
reaches users with the next tagged release.

The merge with `main` picked up REQ-623 (subagent dispatch): the redact-scan
golden row combines both changes (79,872 words / 182,403 bytes), and
`tools/agent.rs`'s test source gained `stopped_at_cap`. Follow-up: a child
agent cut off at its cap is still reported `completed` by
`runtime/child_turn.rs` (separate bug in flight).

Lesson: LESSON-666.
