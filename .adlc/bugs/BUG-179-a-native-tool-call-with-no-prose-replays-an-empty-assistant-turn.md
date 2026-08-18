---
id: BUG-179
title: "A native tool call with no prose replays an empty assistant turn and fails the next remote call"
status: open
severity: high
created: 2026-08-18
updated: 2026-08-18
component: "daemon/harness"
domain: "providers"
stack: ["rust", "daemon", "openai-compat", "harness"]
concerns: ["correctness", "developer-experience", "observability"]
tags: ["kimi", "moonshot", "tool-calls", "native-tier", "empty-assistant", "transcript", "fallback", "400"]
---

## Description

With an OpenAI-compatible provider whose `tool_call_tier` is `native` (observed
with `kimi` / `kimi-k3` on Moonshot), **any turn the model answers with a
native `tool_calls` and no prose fails on the very next provider call**:

```
degraded: kimi (invalid response) — no fallback configured
error: prompt failed: provider failed and no fallback is configured
```

The tool the model asked for *does* run — the failure is the follow-up
request that folds the tool result back. Replaying Teton's exact follow-up
request shape against `https://api.moonshot.ai/v1/chat/completions` returns:

```
HTTP 400 {"error":{"message":"Invalid request: the message at position 2 with
role 'assistant' must not be empty","type":"invalid_request_error"}}
```

The same request with one word of assistant prose is accepted (200);
presence or absence of `tools` makes no difference. Any provider that rejects
an empty assistant message — Moonshot does; Anthropic's Messages API rejects
empty non-final text too — fails the same way. Every kimi tool-using turn is
therefore dead on arrival, and because the daemon discards the 4xx body the
CLI's `invalid response` wording points at the wrong end of the wire.

## Reproduction Steps

1. `teton provider add kimi --model kimi-k3` (openai-compatible endpoint), set
   its `tool_call_tier = "native"`, route the `build` tier to it with no
   fallback.
2. In a session, ask for anything that needs a tool (`read the README and
   summarize it`).
3. kimi-k3 answers with `reasoning_content` + `tool_calls` and
   `content: ""` — the tool runs, then the next call fails as above.

Deterministic reproduction without a key: script the OpenAI-compatible SSE
shape `{"delta":{"content":"","reasoning_content":"…","tool_calls":[…]}}` into
the `remote_loop.rs` transport and inspect the second request body — it
carries `{"role":"assistant","content":""}`.

## Expected Behavior

A native tool call with no prose is a complete, valid model turn. The
transcript records *what the model did* (the call), the next request carries a
non-empty assistant turn, and the loop continues to the tool-result fold and
the model's next step — on kimi exactly as it does on any provider that
happens to add a word of prose.

## Actual Behavior

The assistant block for that turn is recorded as the empty string. The next
request replays `{"role":"assistant","content":""}`, the provider answers 400,
the daemon classifies it as `ClientError{400}` → `InvalidResponse` → fallback,
and with no fallback configured the prompt fails. The daemon's stderr says
nothing about the status, so the reason is invisible without replaying the
request by hand.

## Environment

- Platform: macOS (Apple Silicon), launchd-started daemon
- Version: teton 0.1.21; present since the remote source landed (REQ-544
  TASK-013) — every remote native tool call has been recorded as prose-only

## Root Cause

**Confirmed by reading the path end to end (2026-08-18).** Two independent
gaps line up:

1. **The remote source drops the structured call from the transcript.**
   `RemoteProviderSource::produce_turn`
   (`crates/tetond/src/harness/completion.rs`) accumulates `text` from
   `TurnEvent::TextDelta` only; a native `TurnEvent::ToolCall` becomes the
   `TurnDecision` but is recorded nowhere in the text, and `call_in_text` is
   `false`. `openai_compat.rs::State::step` reads only `/delta/content`, and
   kimi-k3 streams `reasoning_content` + `tool_calls` with `content: ""`, so
   `text == ""`. The turn loop (`turn_loop.rs`, `TurnDecision::ToolCall` arm)
   then does `ctx.push_model(text.clone())` — an assistant block with empty
   text (`ContextManager::push_model` does not skip empty).

2. **Nothing between the context and the wire keeps an assistant message
   non-empty.** `ContextManager::prepare` (`context.rs`) emits the block as
   `{role: Assistant, text: ""}` — its M-8 shaping guarantees a non-empty,
   user-first, alternating sequence, but not non-empty *content* — and
   `openai_compat.rs::build_request` serializes it verbatim as
   `{"role":"assistant","content":""}`. Moonshot answers 400 →
   `classify_client_error` → `ProviderError::ClientError{400}` →
   `to_protocol_failure_class` (`router.rs`) = `InvalidResponse` → `classify`
   (`teton-providers/src/failure.rs`) = `Fallback` → `runtime.rs` "provider
   failed and no fallback is configured".

The same empty-assistant shape is reachable on two other paths and would 400
identically on kimi/Anthropic: an `EndTurn` whose `final_text` is empty (a
remote model that produced no content — e.g. `max_tokens` inside reasoning),
and the BR-6 verification nudge, which pushes the (possibly empty) text and a
system tool-result behind it. Gap 2 is therefore fixed at the choke point, not
only for the tool-call arm.

**Observability:** the 4xx body is read only to sniff for an effort refusal
(`teton-providers/src/lib.rs::classify_client_error`) and then discarded;
the daemon never logs the status, so `degraded: kimi (invalid response)` is
all the user sees. Logging the provider's free-text `message` was considered
and rejected: it is provider-authored and can echo request content, which
REQ-547 BR-11 / conventions.md forbid in a logged structure. The status and
provider id are content-free by construction and are what the daemon now
prints.

## Resolution

Fixed at both gaps, independently tested, plus the observability line:

1. **The transcript records the call (semantic fix).**
   `RemoteProviderSource::produce_turn` now records a native tool call that
   arrived with no prose as the call itself, rendered as the compact
   `{"tool":"<name>","arguments":{…}}` object — the one tool-call shape the
   system prompt teaches every model — and answers `call_in_text: true` for
   it. That block is non-empty, gives the model back its memory of what it
   asked for when the result folds in, and is exactly the local tier's shape,
   so OQ-1's cancellation trim (`carry.rs::trim_dangling_tool_call`) drops it
   whole if the turn is cancelled at the permission gate instead of committing
   an empty assistant turn. A turn that carried prose *and* a call is
   unchanged: it still records the prose alone with `call_in_text: false`,
   because appending the call there would hand the trim a block whose first
   tool-call-shaped JSON might be something the prose merely quoted (the
   truncation `call_in_text` exists to prevent). Kimi's `reasoning_content`
   replay rules for native `tool_calls` do not apply: the replayed turn is
   plain text with no `tool_calls` field, which Moonshot accepts (verified by
   the reporter's manual replay with one word of prose).

2. **The wire never carries an empty assistant message (choke point).**
   `ContextManager::prepare` skips an assistant block whose neutralized text
   is empty or whitespace-only, so the user-role blocks either side merge as
   any same-role neighbours do. This is the seam that already owns the
   provider-acceptable shape (REQ-544 M-8's user-first, alternating,
   non-empty sequence), and it covers the other empty-turn shapes the loop can
   push — an `EndTurn` that produced no text, and the BR-6 verification nudge
   behind it. The block itself stays in the context and in the flat
   rendering (the local tier's rendering is byte-identical, REQ-554); only the
   structured sequence omits it. Both provider adapters serialize what they
   are handed, unchanged.

3. **The daemon says what the provider answered.** At the failure site in
   `runtime.rs` the daemon prints one stderr line naming the provider, the
   typed `ProviderError` (`provider returned client error status 400` — a
   status and a class name, content-free by construction), and what happens
   next (fallback / retry / no fallback). The provider's free-text `message`
   is deliberately not logged (REQ-547 BR-11: provider-authored text can echo
   request content).

Regression tests, each proven to fail against the pre-fix code:
- `remote_loop::a_native_tool_call_with_no_prose_never_replays_an_empty_assistant_turn`
  — end to end through the real OpenAI-compatible adapter and egress, with a
  kimi-k3-shaped SSE turn (`reasoning_content` + `tool_calls`, `content: ""`),
  capturing the follow-up request body: no assistant message is empty
  (fails pre-fix with exactly the reported shape), and the assistant turn IS
  the call (fails with only the `prepare` backstop in place — the two layers
  are pinned independently); roles are `system, user, assistant, user`; the
  stand-in never streams to the user; both turns billed with the reasoning
  split.
- `completion::tests::a_native_call_with_no_prose_is_recorded_as_the_call_it_made`,
  `…::a_native_call_with_prose_still_records_the_prose_alone`,
  `…::the_native_call_stand_in_is_one_call_with_no_prose_before_it`.
- `context::tests::prepare_never_emits_an_empty_assistant_message` (mid-
  conversation, trailing, and whitespace-only shapes; flat rendering
  unchanged).
- `carry::tests::a_cancelled_remote_turn_with_no_prose_commits_no_assistant_block_at_all`.
- `conformance::openai_a_reasoning_only_tool_call_yields_the_call_and_no_text`
  — the adapter's output for the kimi shape (the precondition the harness has
  to handle).

Not changed (out of scope, noted for a follow-up): a remote model in the
reduced tier that obeys the system prompt and emits `{"tool":…}` as *text*
is not parsed for a call — the remote source ends the turn with the JSON as
its answer. Pre-existing, unrelated to the empty-assistant failure.

## Files Changed

- `crates/tetond/src/harness/completion.rs` — `RemoteProviderSource::produce_turn`
  records a no-prose native call as `render_native_call(name, arguments)` with
  `call_in_text: true`; new `render_native_call`; `SourceTurn` docs; three
  unit tests + `SilentCallProvider`.
- `crates/tetond/src/harness/context.rs` — `ContextManager::prepare` emits no
  message for an empty/whitespace assistant block; doc; unit test.
- `crates/tetond/src/carry.rs` — gate-level test for the no-prose remote
  cancellation.
- `crates/tetond/src/runtime.rs` — content-free stderr line at the remote
  failure site (provider, typed error, next step).
- `crates/tetond/tests/remote_loop.rs` — `ScriptedSseTransport` records
  request bodies; kimi-shaped turn builder; end-to-end regression.
- `crates/teton-providers/tests/conformance.rs` — kimi-shaped fixture and
  adapter-level test.
