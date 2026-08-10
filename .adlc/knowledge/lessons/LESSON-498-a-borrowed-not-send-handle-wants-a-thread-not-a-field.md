---
id: LESSON-498
title: "A !Send FFI handle bound to a borrow wants a thread, not a struct field"
component: "inference/local"
domain: "inference"
stack: ["rust", "llama.cpp", "ffi"]
concerns: ["reliability", "latency"]
tags: ["self-referential", "send", "thread-affinity", "lifetimes", "unsafe"]
req: REQ-564
created: 2026-08-10
updated: 2026-08-10
---

## What Happened

REQ-564 needed one `LlamaContext` to survive across agent turns so a turn could
prefill only its new suffix. The obvious shape — add a
`cache: Option<LlamaContext>` field beside the `LlamaModel` the engine already
owns — is blocked twice over, and each blocker on its own looks like something
you could push through:

1. **Self-reference.** `LlamaModel::new_context<'a>(&'a self, …) ->
   LlamaContext<'a>` ties the context's lifetime to a borrow of the model.
   Storing both in one struct is a self-referential type: `unsafe` lifetime
   erasure, or a crate like `ouroboros`.
2. **`!Send`.** `LlamaContext` holds a raw `NonNull<llama_context>` and the
   binding declares no `unsafe impl Send` — while `LlamaModel` has both `Send`
   and `Sync`. But the trait required `Send`, the daemon shares the engine as
   `Arc<Mutex<dyn Engine>>`, and successive turns run on *different*
   `spawn_blocking` threads.

The tempting route was `Box` the model for a stable address, erase the
lifetime, and `unsafe impl Send` the wrapper. It is a small diff. It also
asserts that llama.cpp contexts — including their Metal command queues — have
no thread affinity, which is a claim about a callee we could not discharge from
its source. The failure mode if wrong is not a returned error: it is sporadic
memory corruption in the subsystem holding 17 GB of a user's RAM.

Giving the model and its context **one owned thread** dissolved both problems
at once. The borrow became an ordinary stack borrow. The context never crossed
a thread boundary, so `Send` stopped being a question rather than becoming an
assertion. The engine became a handle over a request/reply channel, and the
module contains no `unsafe` and no new dependency.

## Lesson

When a foreign handle is **both** lifetime-bound to a parent **and** `!Send`,
those are not two obstacles to route around — together they are the callee
telling you its ownership model. Give the pair a thread that owns both and talk
to it over a channel. Drop order does the rest, and the compiler enforces it:
declare the child after the parent and reverse-declaration drop frees the child
first, so the safety argument is checked rather than commented.

The discipline generalizes past this binding: an `unsafe impl Send` on a
foreign type is not a lifetime workaround, it is a **claim about the foreign
library's threading model**. If you cannot point at the callee's source or docs
establishing it, you are not removing a compiler restriction, you are removing
the compiler's ability to tell you that you were wrong (LESSON-453's rule,
applied to thread-safety instead of buffer sizing).

Note also what thread-confinement bought for free: eviction and completion
arrive on the same channel, so an evict can never race an in-flight generation.
The design that avoided `unsafe` also removed a whole category of concurrency
bug that a mutex-guarded field would have needed care to avoid.
