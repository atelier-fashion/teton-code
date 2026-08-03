---
id: LESSON-472
title: "A text-completion agent loop must contain the model's turn at three layers: stop, cut, and surface"
component: "tetond/harness"
domain: "agent-loop"
stack: ["rust", "llama.cpp"]
concerns: ["correctness", "ux"]
tags: ["weak-models", "stop-sequences", "hallucination", "transcript-format", "turn-loop"]
req: BUG-147
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

The local tier drives a plain text engine with a flat transcript rendering
(`User:` / `Assistant:` / `Tool (name):` blocks). With no stop mechanism, a
weak model completed past its own turn every time: it fabricated `Tool (read):`
results (plausible-looking file contents for files that did not exist), echoed
the harness's own untrusted-content framing back as fake tool results, queued
batches of tool calls, and ran until the token cap cut it mid-JSON. The raw
mess was streamed to the user verbatim AND folded into context — so the next
prompt *taught the model that fabricating tool results is the house style*,
compounding every turn. Separately, extra tool calls beyond the first were
silently dropped, so the model re-emitted the same batch turn after turn
(BUG-147).

## Lesson

A harness that parses tool calls out of free text must contain the turn at
three layers, not trust the model to end it:

1. **Stop**: end generation the moment the turn is decidable — the first
   complete top-level tool-call object, or any transcript-frame marker the
   prompt format itself uses (`User:`, `Tool (`, and harness-authored framing
   like `<tool-result`) appearing at a line start. The engine callback needs a
   continue/stop return for this; a fire-and-forget `on_token` cannot express
   it.
2. **Cut**: whatever still got generated, truncate the reply at that boundary
   *before* it reaches context. Context folding is the contagion vector — one
   fabricated tool result in context begets more.
3. **Surface**: anything the harness ignored (extra tool calls, dropped
   capabilities) must be told to the model explicitly. A model cannot
   distinguish "ignored" from "lost" and will retry forever.

Corollary: any string the harness injects into context (framing, markers) is
also a string the model will learn to emit — treat your own scaffolding text
as fabrication markers in the scanner.

## Why It Matters

Without all three layers the loop burns its entire turn budget producing zero
real work while *looking* busy — the worst failure mode, because transcripts
appear active (tool lines, results, prose) while every artifact in them is
invented. It also wastes seconds of local inference per turn generating text
that is thrown away.

## Applies When

Building or modifying any loop that drives a text-completion model with an
inline tool-call protocol (no native structured tool-calling); designing
prompt/transcript formats for such loops; debugging an agent that repeats
identical tool calls or reports results for operations that never ran.
