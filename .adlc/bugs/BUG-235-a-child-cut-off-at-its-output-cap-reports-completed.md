---
id: BUG-235
title: "A child cut off at its output cap reports completed"
status: open
severity: medium
created: 2026-10-07
updated: 2026-10-07
component: "daemon/runtime/child_turn"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "developer-experience"]
tags: ["max-tokens", "subagent", "child-status", "silent-success", "agent-tool"]
introduced_by: [REQ-623]
attribution: derived
---

## Description

BUG-229 (#333) made a prompt turn end with `StopReason::MaxTokens` when its last
model call stopped at the route's output cap (`SourceTurn::stopped_at_cap`), so
a reasoning model that spent the whole cap thinking reads as truncated rather
than done. REQ-623's subagent dispatch did not get the same treatment. In
`crates/tetond/src/runtime/child_turn.rs` the child's stop reason is matched for
`MaxTurnRequests` and `Cancelled`, and **everything else** goes to
`finished_on_its_own`. A child cut off at `max_tokens` therefore reports
`ChildStatus::Completed`, often with an empty or half-sentence `report`.

That is BUG-229's silent success, moved into the child report the parent model
reads. The parent is told the child finished, so it reasons over "the child
found nothing" or over a fragment presented as a final answer.

## Reproduction Steps

1. Start a prompt turn that calls `agent` with a task that is likely to exhaust
   the child's output cap (a remote reasoning tier with an 8,192-token
   `REMOTE_GENERATION_RESERVATION`, or a local tier whose completion uses all of
   `gen_params.max_tokens`).
2. The child's last call stops at the cap (`stopped_at_cap: true`). The turn loop
   returns `StopReason::MaxTokens`.
3. Read the `agent` tool result: the child's `status` is `completed`.

## Expected Behavior

The child's status says it did not finish, and the parent is told why and what
to do about it.

## Actual Behavior

`status: completed`, and `report` is empty or holds a reply cut off mid-sentence.

## Environment

- Platform: any; `tetond` on main after #333 (7118be42)
- Version: workspace at 5822b0e7

## Root Cause

`Work::run` in `crates/tetond/src/runtime/child_turn.rs` folds the turn
outcome with a catch-all:

```rust
StopReason::MaxTurnRequests => /* turns_exhausted */,
StopReason::Cancelled => Ended::of(ChildStatus::Cancelled),
_ => finished_on_its_own(outcome.final_text, &tool_calls),
```

`finished_on_its_own` returns `Completed` unless the gate refused every call.
REQ-623 was written while a cap hit still ended `EndTurn` (BUG-229's bug). When
BUG-229 added a real `MaxTokens` outcome, the `_ =>` arm swallowed it with no
compile error, because a catch-all match never asks about a new variant.

## Resolution

**Decision: reuse `failed`; no new `ChildStatus` variant.** `ChildStatus` is a
deliberately closed set ("a status this build does not know is an error to
decode"), and the CLI decodes it on `agent_child_finished` and
`agent_call_finished`. Under `teton-protocol/src/lib.rs`'s rule ("advertise only
what the types can actually read"), a ninth variant needs `PROTOCOL_VERSION`
bumped 2→3 on both ends, and REQ-623 BR-10 would need to say "nine". The
alternative, `completed` with a marker in the report text the way
`turns_exhausted_report` marks its report, leaves the status field claiming
success, which is the shape this bug is about.

A cap-truncated child now ends `ChildStatus::Failed` with
`error = "max_tokens: this child's reply reached its N-token output cap before it
finished; nothing it wrote is returned — give it a narrower task, or a tier whose
cap is larger"`:

- **Code `max_tokens`**: the wire spelling of `StopReason::MaxTokens`, the same
  token a prompt turn that ran out this way ends with. It isn't a JSON-RPC
  error code, because a cap stop is not an RPC error, so no `error_code`
  constant was minted.
- **The number**: the cap of the route the last call ran on
  (`st.route.harness.gen_params.max_tokens`, after any reroute). This follows
  BR-10, which has every bound-ended status carry the number that ended it.
- **`report` empty**: BR-10 keeps a report to `completed` and `turns_exhausted`.
  The partial reply stays out of `error` as well (model output does not go in
  error strings).
- The stop-reason match is now **exhaustive** (`EndTurn | Refusal` go to
  `finished_on_its_own`; there is no `_`). A future `StopReason` variant is a
  compile error at this site rather than another silent `completed`.

REQ-623 BR-10 is amended (3) to state this. The `failed` docs on `ChildStatus` and
`ChildResult::error`, and the bundled guide's `agent` section (the text the model
reads), now describe the `max_tokens` error.

**Test**: `runtime::child_turn::tests::a_child_cut_off_at_its_output_cap_ends_failed`
drives the real `LocalEngineSource`. A scripted `Reply::AtCap` returns a
completion that uses the whole `max_tokens` it was sent, which is the local
tier's only cap witness, and that yields `stopped_at_cap: true`. Two capped
children are driven: one cut off mid-sentence and one empty, the thinking-only
shape. The cap in the error is checked against the `max_tokens` the engine
recorded, not against a value computed by the child's code. A control child
sends the same text under the cap and still ends `completed`.

**Mutation** (run 2026-10-07, reverted): deleting the `MaxTokens` arm and folding
it into the `finished_on_its_own` arm (the pre-fix `_ =>`), under
`cargo test --workspace --no-fail-fast`, gives **1 red of 4,890**: this test, at
the `cut-off` child's status. A first attempt at the mutation, adding
`MaxTokens` to the later arm while the earlier arm stayed, was unreachable and
stayed green. It is noted here because it would have "proved" nothing.

**Suite**: `cargo test --workspace --no-fail-fast`: exit 0, 4,890 passed, 0
failed across 83 targets, and no `FAILED` in the output. `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo fmt` are clean.

## Files Changed

- `crates/tetond/src/runtime/child_turn.rs`: `MaxTokens` arm → `Ended::failed(output_cap_error(cap))`; exhaustive stop-reason match; `Reply::AtCap` fixture, the engine's recorded caps, and the new test
- `crates/tetond/src/harness/child.rs`: `OUTPUT_CAP_REACHED` (`"max_tokens"`) and the `output_cap_error` composer
- `crates/teton-protocol/src/agent.rs`: `ChildStatus::Failed` and `ChildResult::error` docs name the `max_tokens` case (doc-only; no wire change)
- `crates/tetond/src/harness/docs/commands.md`: the `agent` guide's `failed` bullet tells the model what `max_tokens` means and what to do
- `.adlc/specs/REQ-623-subagent-dispatch/requirement.md`: BR-10 amendment (3)
