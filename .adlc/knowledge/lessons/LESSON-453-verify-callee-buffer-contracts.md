---
id: LESSON-453
title: "A spare-capacity API makes a zero-capacity call a silent no-op — read the callee's buffer contract, pin it with defect-shaped tests"
component: "inference/engine"
domain: "inference"
stack: ["rust", "encoding_rs", "llama.cpp"]
concerns: ["reliability", "testing"]
tags: ["buffer-contract", "spare-capacity", "utf8-streaming", "no-op-flush", "library-source-review", "post-merge-defect"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

PR #7 migrated token streaming to a stream-lifetime UTF-8 decoder and merged
green — clippy clean, real-model smoke passing. Reviewing the same diff for
PR #9, reading the *callee's* source showed two of its claims were not yet
true. `encoding_rs::Decoder::decode_to_string` writes only into the
destination `String`'s **spare capacity** (`vec[old_len..capacity]`) and
never grows it, so the end-of-stream flush into `String::new()` — capacity
zero — was a silent no-op: the U+FFFD the comment promised could never
appear. And llama-cpp-2's own `token_to_piece` wrapper reserves only
`bytes.len()`, while a token *completing* a held multi-byte character emits
up to 3 more bytes than it carried; on `OutputFull` the wrapper discards the
unconsumed input. Every test was green because the only end-to-end evidence
was ASCII.

## Lesson

When code hands bytes to an incremental decode/convert API, the review unit
is the **callee's buffer-ownership contract**, not the caller's diff: who
allocates, does the callee grow the buffer or write into spare capacity, and
what happens to unconsumed input on overflow? Read the callee's source (or
its documented worst-case sizing function — here `max_utf8_buffer_length`,
encoding_rs's "whole input will be consumed" contract) before trusting a
wrapper, including the dependency's own convenience wrapper. Then pin each
failure mode with a **defect-shaped, dependency-free unit test** (truncated
stream flushes U+FFFD; completing char bigger than its final piece), because
an end-to-end smoke over ASCII exercises none of the multi-byte boundaries
the code exists to handle. PR #9's fix owned the decode outright —
`PieceDecoder` over raw piece bytes with `max_utf8_buffer_length`
reservations — rather than layering guards on the under-reserving wrapper.

## Why It Matters

Silent-truncation defects produce no error, no panic, and no test failure —
just occasionally-mangled CJK/emoji in streamed output, unreproducible in
ASCII-only environments. "Merged green" was a hypothesis: both defects
shipped and were caught only because the next change re-read the library
source instead of trusting the merged comment's claims.

## Applies When

- Calling any streaming decoder/encoder/converter that takes a caller
  buffer (encoding_rs, C FFI out-buffers, `read`-style APIs) — especially a
  "flush" or `last = true` call, where an empty destination looks natural.
- Reviewing a merged-green change whose correctness claims live in comments
  rather than in tests shaped like the failure (see [[LESSON-448]] — fast
  test doubles masking a latency contract; same blindness, different
  contract).
- Wrapping a dependency's convenience API: check the wrapper's own buffer
  math before inheriting it; owning the primitive call can be smaller than
  guarding the wrapper.
