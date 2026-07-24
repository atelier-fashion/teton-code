---
id: LESSON-444
title: "A C library's assert is a process abort — validate inputs before the FFI boundary"
component: "inference/engine"
domain: "inference"
stack: ["rust", "llama.cpp", "ffi"]
concerns: ["reliability", "security"]
tags: ["ggml-assert", "abort", "n-batch", "input-validation", "ffi-boundary", "dogfood"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

The first dogfooded local session turn killed the entire daemon. `LlamaEngine::complete`
decoded the whole prompt in one `ctx.decode()` call; llama.cpp enforces its logical
batch ceiling with `GGML_ASSERT(n_tokens_all <= cparams.n_batch)` — a C `abort()`,
not a returnable error — and a ~5k-token prompt (a folded `read` of a real source
file) crossed the default 2,048 ceiling. No Rust panic, no `Result`, no catch: the
process died mid-turn with a native backtrace. An earlier configuration only *seemed*
safer because a different limit (batch capacity) happened to fail first through a
Rust-visible error path.

## Lesson

Treat every limit a C/C++ dependency enforces with an assert as a **precondition you
must establish on the Rust side**, not an error you can handle. Enumerate the
callee's asserts (grep the vendored source for `GGML_ASSERT`/`assert(` on the call
path), then either make the input structurally incapable of violating them (chunk
prompt decoding at `n_batch`; pass the same constant to the context so the two can
never drift) or refuse with a typed error before the FFI call. First-compile and
happy-path smoke tests prove nothing here — the aborts live on the input-size tails.

## Why It Matters

An assert in a library is a whole-process crash in a daemon that owns sessions,
sockets, and an event bus: every client of every session dies with the one
over-budget turn. These failures are invisible until real inputs arrive (mocks and
fixtures are small by construction), which is exactly when the cost is highest.

## Applies When

Calling any native library from a long-lived process — llama.cpp/ggml especially,
but equally SQLite, image codecs, or anything vendored via `-sys` crates; sizing
buffers, batch counts, or context windows that a C layer also enforces; reviewing
"it compiled and the smoke test passed" claims about never-exercised FFI paths.
