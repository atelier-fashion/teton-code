---
id: BUG-178
title: "A native tool call leaves an empty assistant turn in the transcript, and the next request is refused"
status: resolved
severity: high
created: 2026-08-18
updated: 2026-08-18
component: "daemon/harness"
domain: "providers"
stack: ["rust", "daemon", "openai-compatible", "anthropic"]
concerns: ["correctness", "tool-calling", "developer-experience"]
tags: ["native-tools", "tool_calls", "empty-content", "kimi", "moonshot", "400", "invalid-response", "transcript", "turn-loop", "oq-1", "cancellation"]
---

## Description

On a remote provider at `tool_call_tier = "native"`, the model answers a
prompt with a structured tool call (`tool_calls` on OpenAI-compatible,
`tool_use` on Anthropic) and — as such models usually do — **no prose**. The
tool runs. The very next request to the same provider is refused with HTTP
400, the CLI prints

```
>> degraded: kimi (invalid response) — no fallback configured
error: prompt failed: provider failed and no fallback is configured
```

and the turn is abandoned. It happens on the second model call of **every**
tool-using turn, so a native-tier remote provider with no fallback cannot
complete any task that needs a tool.

Observed 2026-08-18 on teton 0.1.21 with `kimi` (openai-compatible,
`kimi-k3`, native tier) bound to the `build` tier: `/provider test kimi`
passed (a single-message probe never exercises this path), the model called
`shell`, the command ran, and the follow-up request died. The command's own
non-zero exit was incidental — the same happens after a successful tool.

Reproduced at the wire the same day: replaying Teton's exact follow-up
request shape against `https://api.moonshot.ai/v1/chat/completions` returns

```
HTTP 400 {"error":{"message":"Invalid request: the message at position 2
with role 'assistant' must not be empty","type":"invalid_request_error"}}
```

The identical request with one word of assistant prose is accepted (200);
`tools` present or absent makes no difference. Anthropic's Messages API has
the same rule ("all messages must have non-empty content except for the
optional final assistant message"), so the exposure is provider-agnostic.

## Reproduction Steps

1. Configure an OpenAI-compatible provider whose model uses native function
   calling (Kimi K3 via Moonshot reproduces it), with
   `[providers.capabilities] tool_call_tier = "native"`, and bind it to the
   `build` tier with no fallback.
2. `teton`, then ask anything that needs a tool: "list the files in this
   directory".
3. The model calls `shell` (or `glob`); the tool runs; the next model call
   fails with `degraded: <provider> (invalid response) — no fallback
   configured`.

## Expected Behavior

The tool result is folded and the model continues. The transcript records
what the model called, so the assistant turn that made the call is neither
empty nor amnesiac.

## Actual Behavior

The assistant turn is recorded as an empty block; the follow-up request
carries `{"role":"assistant","content":""}`; the provider refuses it with a
400 that Teton labels "invalid response" and classifies as *fallback*; with
no fallback the turn fails. Nothing on the CLI or in the daemon log names the
reason — the 4xx body is discarded after a bounded sniff for the effort
field, so the operator is left with "invalid response".

## Environment

- Platform: macOS (Apple Silicon)
- Version: teton 0.1.21 (present since REQ-544 M-8 introduced the remote
  role-typed request); provider `kimi` = openai-compatible `kimi-k3`, native
  tier

## Root Cause

**Confirmed at the wire (2026-08-18).** Three links, none of them a
provider quirk:

1. **The remote source records only prose.** `RemoteProviderSource::produce_turn`
   (`crates/tetond/src/harness/completion.rs`) accumulates `text` from
   `TurnEvent::TextDelta` only; a structured `TurnEvent::ToolCall` becomes
   the decision and is written **nowhere** in the transcript, and the turn is
   reported with `call_in_text: false`. `openai_compat.rs::State::step` reads
   `/delta/content` alone — Kimi K3 streams `reasoning_content` and
   `tool_calls` with `content: ""` — so `text` is empty for a pure call.
2. **The loop pushes it as-is.** The `TurnDecision::ToolCall` arm of
   `run_session_turn_with_source` (`turn_loop.rs`) does `ctx.push_model(text)`
   when `!call_in_text`; `ContextManager::push_model` does not skip empty
   text. The block is committed empty, with no record of the call.
3. **`prepare()` renders it faithfully.** `ContextManager::prepare`
   (`context.rs`) maps the block to a `MessageRole::Assistant` message with
   empty text; `openai_compat.rs::build_request` (and
   `anthropic.rs::build_request`) send `{"role":"assistant","content":""}`
   verbatim. The provider refuses; `to_protocol_failure_class` (`router.rs`)
   maps a 400 to `InvalidResponse`, `failure::classify` says *Fallback*, and
   `runtime.rs` reports "provider failed and no fallback is configured".

A second consequence of link 1: a remote tool-call turn **cancelled at the
permission gate** is committed by `CarriedTurn::commit_now` (`carry.rs`)
without OQ-1's trim (no `pending_tool_call`, because the loop used
`push_model`) — i.e. as that same empty assistant block — which would put
the empty turn in **every later prompt** of the session and wedge it on the
same 400.

Why the local tier never hit this: its call *is* the reply text (parsed out
of it, kept through `clean_len`), so a local tool-call block is never empty
and always names its call.

## Resolution

Three layers, each pinned by a test that goes red without it (mutation-checked
locally):

1. **The block a tool-call turn pushes always ends with the call.** In the
   loop's `TurnDecision::ToolCall` arm, a remote call (`!call_in_text`) is
   rendered onto the prose with `reply::append_tool_call` — the
   `{"tool": …, "arguments": …}` object the system prompt teaches and
   `parse_reply` reads, `tool` first, compact, name JSON-escaped, one newline
   after non-empty prose — and every tool-call turn is pushed through
   `push_model_call`. So the transcript records what the model called (with
   or without prose), the assistant turn is never empty, and a remote turn
   parked at the permission gate is pending like a local one. `call_in_text`
   keeps its meaning (who put the call there); the guarantee lives at the one
   seam every source passes through rather than in each source's report.
2. **The cancellation trim cuts the trailing call.** `prose_before_tool_call`
   now identifies the dangling call as the *trailing* call-shaped object of
   the block — which is where the loop puts it for both sources (a local
   reply is cut at `clean_len`, a remote call is appended last) — instead of
   the first one. That is what lets OQ-1's "retain prose, drop incomplete
   tool work" hold for a remote turn whose prose *quotes* something
   call-shaped (`{"name": "serde", …}`): the quote is ahead of the call and
   survives; a bare call drops its block whole, never a blank turn. Text whose
   last object is followed by anything but whitespace is left alone. The
   never-read `ParsedReply::call_start` went with it; the key rule is shared
   as `tool_call_name`.
3. **`prepare()` never emits an empty message.** An assistant block with no
   text — reachable independently from an `EndTurn` that produced none (a
   thinking model spending its whole `max_tokens` on `reasoning_content`,
   which the adapter drops) — is skipped at the seam that already shapes the
   wire sequence (M-8's user-first rule); neighbours merge as same-role blocks
   do. The block and the flat rendering are untouched.

Plus, for the next diagnosis: `RemoteProviderSource` writes one content-free
line to the daemon's stderr when a provider fails the turn — before it
answered (the request refused: `provider returned client error status 400`)
or mid-stream — naming the provider. Provider-side failures only (the ones
with a `FailureClass`); privacy blocks, effort refusals and build errors
already announce themselves.

Verified locally: `cargo test --workspace --no-fail-fast` 2916 passed / 0
failed across 57 targets; `cargo clippy --workspace --all-targets -- -D
warnings` clean; `cargo fmt --all --check` clean.

**Parallel fix.** A second session on this machine fixed the same defect as
BUG-179 in [PR #177](https://github.com/atelier-fashion/teton-code/pull/177)
(`fix/bug-179-empty-assistant-turn`): it records the call only for the
no-prose case (a prose-bearing remote turn still replays as prose alone),
adds the same `prepare()` guard, logs at the runtime's failure site, and adds
an end-to-end regression through the real openai-compatible adapter with
recorded request bodies (`crates/tetond/tests/remote_loop.rs`) plus a
conformance fixture. Only one of the two lands. **Decision (user,
2026-08-18): this one lands; #177's wire-level test and conformance fixture
were carried over onto it (relabelled BUG-178) before merge, and #177 is
closed unmerged.** BUG-179 never reached `main`, so `.adlc/bugs/` carries one
record for the defect.

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — `ToolCall` arm renders a remote
  call onto the prose and always pushes via `push_model_call`; the OQ-1
  wiring test rewritten to the new contract (`ParkingSource` in both shapes);
  BUG-178 regression `a_remote_tool_call_with_no_prose_is_recorded_as_the_call_not_a_blank_turn`.
- `crates/tetond/src/harness/reply.rs` — `append_tool_call` (render in the
  reply grammar); `prose_before_tool_call` finds the trailing call;
  `tool_call_name` shared key rule; `ParsedReply::call_start` removed; tests
  for the look-alike prose, trailing chatter, and render/parse round trip.
- `crates/tetond/src/harness/context.rs` — `prepare()` skips an empty block;
  `push_model_call` doc; test `prepare_skips_an_empty_assistant_turn_rather_than_sending_it`.
- `crates/tetond/src/harness/completion.rs` — `note_failure` on the remote
  source at both failure points; `SourceTurn::call_in_text` doc.
- `crates/tetond/src/carry.rs` — trim doc; the cancelled-remote-turn test
  now uses the shape the loop pushes; new
  `a_cancelled_remote_call_with_no_prose_leaves_no_blank_turn_behind`.
- `crates/tetond/tests/remote_loop.rs` — carried over from PR #177
  (BUG-179, the parallel fix): `ScriptedSseTransport` records every request
  body; a kimi-shaped turn builder (`reasoning_content` + `tool_calls`,
  every `content` delta empty); end-to-end regression
  `a_native_tool_call_with_no_prose_never_replays_an_empty_assistant_turn`
  through the real openai-compatible adapter — the second request's
  assistant message is the recorded call, never empty, roles strictly
  alternate, the call never streams to the user, both turns billed. Red on
  the unfixed loop arm (0 assistant turns: the `prepare()` guard hides the
  empty block but cannot record the call), green with the fix.
- `crates/teton-providers/tests/conformance.rs` — carried over from PR #177:
  the kimi-shaped fixture; the adapter yields exactly one `ToolCall` and no
  `TextDelta` — the legitimate adapter output the harness has to record.
- `CHANGELOG.md` — `[Unreleased]` → Fixed entry.
- `docs/manual-verification.md` — BUG-178 runbook (OUTSTANDING until
  dogfooded on the shipped binary against a real native-tool provider).

## Deployment

- Merged to `main` as `ca585cd` via
  [PR #178](https://github.com/atelier-fashion/teton-code/pull/178)
  (2026-08-18; CI 7/7 green on the final head, MERGEABLE/CLEAN). The parallel
  [PR #177](https://github.com/atelier-fashion/teton-code/pull/177) (BUG-179)
  was closed unmerged; its wire-level test travelled with #178.
- No CI/CD deploy target — this repo ships through Homebrew releases (plain
  OSS flow, no staging/production). The fix is in `[Unreleased]` and reaches
  installs with the next tag; the `docs/manual-verification.md` BUG-178
  runbook is the dogfood confirmation on the shipped binary against a real
  native-tool provider (OUTSTANDING).
- Knowledge: LESSON-538 (`.adlc/knowledge/lessons/`), Key Pattern "The block
  a tool-call turn pushes ends with the call, whichever source produced it"
  (`.adlc/context/architecture.md`).
