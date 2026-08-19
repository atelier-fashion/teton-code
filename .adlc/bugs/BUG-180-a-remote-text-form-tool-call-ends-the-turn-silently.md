---
id: BUG-180
title: "A remote provider's text-form tool call ends the turn silently: nothing runs, nothing renders"
status: open
severity: high
created: 2026-08-19
updated: 2026-08-19
component: "daemon/harness"
domain: "providers"
stack: ["rust", "daemon", "openai-compatible", "anthropic"]
concerns: ["correctness", "tool-calling", "developer-experience"]
tags: ["remote-source", "text-form", "tool-call", "parse_reply", "stream-gate", "empty-turn", "kimi", "edit", "routing", "one-grammar"]
---

## Description

On a remote provider, the model answers a prompt by writing its tool call
**as text** — the `{"tool": "<name>", "arguments": {…}}` object the system
prompt tells every model to reply with — instead of as a structured
`tool_calls` / `tool_use` block. The harness treats that reply as the
model's *final answer*, the display gate hides it (it is tool-call-shaped),
and the turn ends. The user sees an empty turn: no tool status line, no
text, no error — just the blank line that closes a successful turn. The
JSON is committed to the conversation as the assistant's answer.

Observed 2026-08-19 on teton 0.1.23: local `qwen3-coder-30b-a3b` plus
`kimi` (openai-compatible, `kimi-k3`, `tool_call_tier = "native"`) bound to
the `build` tier. In a freeform session the user asked *"show me the
skills"*. The `route` classifier assigned `edit` → `build` → Kimi
(`cost.db` row 55, 07:37:38, `category = edit`, 161 output tokens of which
74 reasoning). The turn produced no output at all. The user asked again —
*"why didn't you show me the ADLC skills?"* — which classified off `edit`,
ran on the local tier (which *does* parse text-form calls), and worked: it
said "let me check what's in the skills directory", ran three `ls`
commands, and listed the skills. The second turn's prompt was ~372 tokens
longer than the one before the Kimi turn — the previous answer plus two
user lines plus an ~87-token assistant reply nobody was shown.

## Reproduction Steps

1. Configure a native-tier remote provider (Kimi K3 via Moonshot
   reproduces it) and bind it to `build`, with a local model serving the
   `route` classifier and the unbound tiers.
2. `teton`, then ask something the classifier files under `edit` that
   needs a tool — or anything at all, on a turn where the model chooses the
   system prompt's text grammar over the API's native tool field.
3. The turn ends with nothing on screen and no tool run. `cost.db` shows
   the remote call was made and answered (non-zero `output_tokens`).
   `--verbose` shows `route_decided` naming the remote provider; nothing
   else names the reason.

Deterministically, without a provider: a `RemoteProviderSource` over a
provider whose stream is `TextDelta`s spelling
`{"tool":"shell","arguments":{"command":"ls"}}` and **no**
`TurnEvent::ToolCall` returns `TurnDecision::EndTurn` with that JSON as
`final_text`; `StreamGate::for_format(Flat)` over the same chunks returns
`live = ""` and `finish(true) = None`.

## Expected Behavior

A tool call the harness's own system prompt taught — and that the local tier
would have dispatched from the identical text — is dispatched from a remote
provider too: the tool status line appears, the tool runs, the result is
folded, and the model continues. A call to a tool that does not exist is
folded back as a correction (the `Malformed` path), never accepted as an
answer. A turn whose entire reply is a tool-shaped object must not end as a
successful empty turn.

## Actual Behavior

`RemoteProviderSource::produce_turn` recognizes only native
`TurnEvent::ToolCall`s; anything else is `EndTurn { final_text: text }`.
The loop emits the `StreamGate`'s held tail only on an end-of-turn whose
scanner did *not* stop, and a closed tool-object is a stop — so the JSON is
neither displayed nor dispatched. The loop records
`StopReason::EndTurn`, the CLI prints its closing blank line, and
`ctx.push_model(final_text)` commits the bare JSON as the assistant's turn.

## Environment

- Platform: macOS (Apple Silicon), Darwin 25.6.0
- Version: teton 0.1.23 (the remote source has never parsed text-form
  calls; the exposure widened with BUG-178's `append_tool_call`, which now
  renders every remote call into the carried history in the text grammar,
  teaching by example). Provider `kimi` = openai-compatible `kimi-k3`,
  native tier, bound to `build`; local `qwen3-coder-30b-a3b`.

## Root Cause

**Confirmed by reading and by a failing-then-passing unit test
(2026-08-19).** The harness teaches one tool-call grammar and the remote
source honours a different one:

1. **Every provider is taught the text grammar.** `build_system_prompt`
   (`crates/tetond/src/harness/turn_loop.rs`) says *"To call a tool, reply
   with ONLY a JSON object on its own: {"tool": "<name>", "arguments":
   { ... }}"*, and that string is the `system` field of every
   `TurnRequest`, native tier or not. The carried conversation reinforces
   it: local replies are text-form calls, and since BUG-178 a remote call
   is rendered onto its prose in the same grammar (`append_tool_call`), so a
   native-tool model sees a history of assistant turns that called tools by
   writing JSON. A request that also carries native `tools` gives the model
   two legitimate ways to call; which one it picks is the model's choice on
   the turn (BUG-178 saw Kimi use the native field; this turn it used the
   text).
2. **Only the local source reads that grammar.** `parse_reply`
   (`harness/reply.rs`) has exactly one production caller,
   `LocalEngineSource::produce_turn` (`harness/completion.rs`).
   `RemoteProviderSource::produce_turn` assembles a `ToolCall` decision
   solely from `TurnEvent::ToolCall` events; with none, the text becomes
   `TurnDecision::EndTurn { final_text }` — the turn's *answer* — and
   `call_in_text: false`.
3. **The display gate hides exactly that answer.** The loop streams every
   source through `StreamGate::for_format(Flat)`, which holds back any
   top-level object with a `"tool"`/`"name"` key and, on
   `finish(final_answer = true)`, flushes a held tail only when the scanner
   had not stopped — a closed tool-object *is* a stop (`Stop::ToolObject`),
   so `flushable_len()` is the object's start and nothing after it is ever
   shown. That is correct for a call the loop is about to present on the
   tool status line; for a call the loop has just decided is an answer, it
   is a silent drop. Prose *before* the object would have streamed; the
   model obeyed "ONLY a JSON object", so there was none.

So the turn ends `EndTurn`, no `tool_call` event is emitted, the CLI's
non-verbose path prints one blank line, and the bare JSON is pushed as the
assistant block. The next turn's model (local, here) sees a dangling call
with no result after it and re-derives the work.

Why it went to the remote provider at all: freeform turns are classified
into `edit | design | debug | review` by the local `route` classifier
(REQ-558); only `build` (`edit`, `shell`) is bound on this machine, so an
`edit` answer — or an unparseable answer, whose declared default is also
`edit` — is the one outcome that leaves the local tier. That is routing
working as configured; it is why the defect surfaces intermittently rather
than on every turn.

**Validation.** Re-read `produce_turn` (both sources), `StreamGate::finish`,
`ReplyScanner::process`, and the loop's `EndTurn` arm; confirmed no other
caller of `parse_reply`; confirmed the loop's `ToolCall` arm always emits
`tool_started` (so the empty turn cannot have been a dispatched call), and
that the CLI's success path renders nothing but the blank line. A scratch
gate test over the JSON chunks printed `live="" tail=None`.

## Resolution

**One grammar, both sources (LESSON-494).** `RemoteProviderSource::produce_turn`
now reads its prose with the same `parse_reply` the local tier uses when —
and only when — the provider sent **no** native tool call:

- A text-form call to an exposed tool becomes `TurnDecision::ToolCall`,
  the text is cut at the call's end (`clean_len`, dropping any
  continuation past it), `dropped_calls` counts any further text-form
  calls, and `call_in_text: true` says the call is already in the text —
  so the loop's BUG-178 rendering is not applied twice and the block
  still ends with the call for the OQ-1 trim.
- A text-form call to an unknown tool, or with non-object arguments, is
  `TurnDecision::Malformed` — folded back as the same correction the local
  tier gets, under the same turn ceiling — rather than accepted as an
  answer.
- Prose with no tool-keyed object is `EndTurn`, exactly as before; a quoted
  object without a `tool`/`name` key (`{"port": 8080}`) is not a call.
- A native `TurnEvent::ToolCall` still wins outright and the text is left
  alone: a native-tool model's prose is prose, however call-shaped some JSON
  in it may look (REQ-567 OQ-1's reasoning stands for that path).

Nothing changes for the display: the `StreamGate` already hid the JSON,
and now the tool status line presents the call it was hiding it *for*.

Deliberately **not** changed here, noted for follow-up:

- The system prompt still teaches the text grammar to native-tier
  providers alongside the API's `tools` field. Dropping the instruction for
  native providers would not remove the exposure — the carried history
  renders every call in that grammar by BUG-178's design — so accepting
  both grammars at the source is the fix; trimming the mixed signal is a
  prompt-quality follow-up.
- A remote reply whose *first byte* is a fabrication marker (`Assistant:`
  at line start, a ChatML control token) is still wholly suppressed by the
  gate and committed whole. Different cause (the frame-forgery axis, BUG-148
  family), not seen in the field, out of scope here.
- The display gate's tool-shape test is wider than the parser's grammar, on
  **both** tiers and since BUG-147: `ReplyScanner` stops at the first closed
  top-level object whose bytes *contain* `"tool"` or `"name"`, while
  `parse_reply` requires the object to parse and to carry that key at the
  top level. A tool-shaped object that is not valid JSON (a trailing comma),
  or a quoted object with a nested `"name"`, is hidden from the screen but
  is no call — the turn ends with the rest of the reply unseen. Same family
  of symptom, different cause (a heuristic and a grammar that should be one
  rule), pre-existing and shared by the local tier; worth its own report.

## Files Changed

- `crates/tetond/src/harness/completion.rs` — `RemoteProviderSource::produce_turn`
  parses a text-form call with `parse_reply` when no native call arrived
  (`clean_len` cut, `dropped_calls`, `call_in_text: true`); `call_in_text`
  and `chat_format` docs updated. Tests: the text-form call is a call (and
  its continuation is cut), an unknown-tool call is `Malformed`, a quoted
  non-call object still ends the turn, a native call wins and leaves
  call-shaped prose alone, and — through `run_session_turn_with_source`
  over a real `RemoteProviderSource` — the tool status line is presented,
  the tool runs, the model is called again, the raw JSON never streams, and
  the assistant block carries the call exactly once. The three BUG-180 tests
  go red with the old `EndTurn` arm restored; the loop test and the
  text-form test also go red with `call_in_text` forced `false`
  (mutation-checked locally).
- `crates/tetond/src/harness/reply.rs` — `parse_reply` doc names both
  readers.
