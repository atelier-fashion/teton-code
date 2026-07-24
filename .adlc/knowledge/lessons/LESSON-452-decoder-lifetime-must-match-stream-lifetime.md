---
id: LESSON-452
title: "A stateful decoder's lifetime must match the stream it decodes — per-chunk wrappers silently drop bytes"
component: "inference/engine"
domain: "inference"
stack: ["rust", "llama.cpp", "encoding_rs"]
concerns: ["reliability", "i18n"]
tags: ["utf-8", "streaming", "token-decoding", "partial-bytes", "deprecated-api", "token-to-piece"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

`LlamaEngine::complete` decoded each generated token with llama-cpp-2's
`token_to_str(token, Special::Tokenize)`. Reading the shim's source showed it
is not just deprecated — it constructs a **fresh** `encoding_rs` UTF-8 decoder
on every call, decodes with `last = false`, and drops the decoder on return.
A single BPE token can end mid-way through a multi-byte UTF-8 character; the
decoder's internal state is what carries those partial bytes to the next
token. Dropping it per call silently lost them, so streamed CJK/emoji text
garbled at token boundaries (bytes vanished — no error, no replacement char).
Three deprecation warnings on every `--features llama` build were the only
visible symptom.

## Lesson

When a chunked stream passes through a stateful transform (UTF-8 decoding,
compression, any multi-byte codec), the transform's state must live exactly as
long as the stream: one decoder per stream, created before the loop, flushed
(`last = true`) after it. A per-chunk convenience wrapper around a stateful
decode is a data-loss bug wearing an ergonomic API. The fix (PR #7): one
`encoding_rs` decoder across the generation loop via `token_to_piece`; empty
pieces (token starts a character, bytes held) are counted but not streamed;
stream end flushes so a dangling partial surfaces as U+FFFD, never silence.

## Why It Matters

The failure is invisible in ASCII-only testing — every English smoke test
passes while any non-Latin user gets corrupted output. Deprecation warnings
deserve a source read, not a suppress: here the deprecation note ("use
token_to_piece") was the library author flagging exactly this correctness
hazard. Grep-check: any loop calling a `*_to_str`-shaped helper per chunk on
byte-oriented data is suspect.

## Related

- LESSON-447 — same family: a lossy path (there a fallback, here a per-call
  decoder) silently defeating the duty it wraps.
- LESSON-444 — the engine-wiring close that deferred this item.
