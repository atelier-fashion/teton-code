---
id: LESSON-538
title: "A structured event the loop acts on must also be recorded in the transcript — and every fixture that had prose hid that it wasn't"
component: "daemon/harness"
domain: "providers"
stack: ["rust", "openai-compatible", "anthropic"]
concerns: ["correctness", "tool-calling", "testing", "developer-experience"]
tags: ["native-tools", "tool_calls", "transcript", "empty-content", "400", "invalid-response", "kimi", "moonshot", "turn-loop", "oq-1", "cancellation", "wire-shape", "fixture-bias", "diagnosability"]
req: BUG-178
created: 2026-08-18
updated: 2026-08-18
---

## What Happened

REQ-544 M-8 gave the turn loop a second source of model turns: a remote
provider whose tool calls arrive as **structured events** (`tool_calls` on
OpenAI-compatible, `tool_use` on Anthropic) beside the streamed prose. The
remote source turned the event into the loop's decision — the tool ran — and
recorded only the prose in the transcript, `call_in_text: false`. Nothing was
wrong with any single piece: the call was acted on, the prose was kept, the
result was folded. What no piece owned was that the assistant *turn* in the
conversation should say what the assistant *did*.

Two things followed, and the first hid the second for a long time:

- Every remote fixture in the suite had prose beside its call. So the
  assistant block was never empty in a test, the follow-up request was never
  `{"role":"assistant","content":""}`, and no adapter test ever sent that shape
  to a mock that would refuse it. A native-tool model that says nothing before
  it calls — the common case; Kimi K3 streams `reasoning_content` and
  `tool_calls` with `content: ""` — produced exactly that block, and Moonshot
  answered the next request with HTTP 400 ("the message … with role
  'assistant' must not be empty"; Anthropic's rule is the same). Teton labelled
  the 400 "invalid response", classified it *fallback*, had none, and abandoned
  the turn. Every tool-using turn on the user's configured `build` tier died on
  its second model call. `/provider test` had passed minutes earlier: a
  single-message probe has no assistant turn to be empty.
- Even with prose, the transcript was amnesiac: the model saw `Tool result
  (shell): …` after its own turn but not the command it had asked to run.

Diagnosis needed a hand-built replay of Teton's request against the vendor,
because the one fact that named the defect — a 400 to a request Teton built —
reached the user as "invalid response" and the daemon log not at all (the 4xx
body is discarded after a bounded sniff for the effort field, by design).

The fix put the guarantee at the one seam every source passes through: the
loop's `ToolCall` arm renders a structured call onto the prose in the reply
grammar the system prompt teaches (`{"tool": …, "arguments": …}`) and pushes
every tool-call turn as a pending call; the OQ-1 cancellation trim cuts the
*trailing* call so prose that merely quotes call-shaped JSON survives; and
`prepare()` skips an empty block outright, because an `EndTurn` with no text (a
thinking model spending its whole budget on reasoning) reaches the same 400 by
a different road. The remote source now writes one content-free stderr line
naming the provider and status when a turn is refused. Two sessions fixed this
in parallel (BUG-178 / BUG-179); the wire-level regression — the real adapter,
every request body recorded, the follow-up's assistant message asserted
non-empty *and* equal to the recorded call — came from the other branch and
was carried over, because it is the only test that would have caught the
original omission.

## Lesson

1. **When a source delivers something structured that the loop acts on, ask
   what the transcript says happened.** A decision consumed by the loop and a
   turn recorded in the conversation are two different obligations; the second
   is easy to forget when the first is what the feature is about. The check is
   mechanical: after the tool ran, read the assistant block back and ask
   whether a model seeing only that block could tell what it did.
2. **A wire-shape rule belongs at the seam that shapes the wire.**
   `prepare()` already guaranteed "starts with a user turn, never empty
   messages array" because those are hard 400s; "no message is empty" is the
   same class of rule and has more than one upstream writer. Fix the writer
   that was wrong *and* enforce the invariant where the sequence is built —
   otherwise the next writer re-opens the hole (the `EndTurn`-with-no-text path
   was that next writer, already there).
3. **Give the suite the fixture that has nothing in it.** Every remote fixture
   had prose; the empty case was the real one. For any accumulator that
   "may be empty for a pure X", write the test where it *is* empty, and drive
   it through the real adapter with the request body recorded — a wire-shape
   claim discharged by inspecting the harness's context is not discharged at
   all (conventions.md: code inspection is not acceptance).
4. **When a provider refuses the request you built, say so where you can see
   it.** A typed status on the daemon's stderr is content-free by construction
   and turns "invalid response" into "HTTP 400 to the turn request" — which is
   the difference between suspecting the key, the tool, or the network, and
   suspecting the request. Diagnosability is part of the fix.

## Why It Matters

The failure was total for the affected configuration and invisible to the
suite: a native-tier remote provider without a fallback could not complete any
task that needed a tool, and the symptom pointed everywhere but at the cause.
The same omission — act on the structured thing, forget to record it — is
available to any future source (a different provider protocol, a local engine
with native tool tokens, an MCP-driven turn), and the same fixture bias
(every test case has the convenient extra content) is available to any
accumulator. The transcript is what the model reasons over; a conversation
that hides the model's own actions from it degrades every later turn even
when the wire accepts it.

## Applies When

- Adding or changing a `CompletionSource`, a provider adapter, or anything
  that turns a structured model event into a loop decision — check the block
  the loop pushes, not just the decision.
- Building any request sequence for a remote provider: enforce shape rules
  (non-empty content, role alternation, leading role) at the seam that
  builds it, and test them at the wire with the real adapter.
- Writing fixtures for a streamed reply: include the shape with **no** prose,
  no content, empty deltas — the degenerate case is the one that ships.
- Surfacing a provider failure to the user: make sure the daemon log carries
  the typed status even when the CLI has to summarize.
