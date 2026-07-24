---
id: LESSON-448
title: "Test-double speed masks executor blocking — pin async offload with a gated engine on one worker"
component: "daemon/harness"
domain: "inference"
stack: ["rust", "tokio", "llama.cpp"]
concerns: ["performance", "reliability", "testing"]
tags: ["spawn-blocking", "async", "tokio-worker", "blocking-pool", "single-worker-test", "rendezvous", "mutation-testing"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

`LocalEngineSource::produce_turn` and `summarize_if_large` called the
synchronous `Engine::complete` inline inside async fns. Every test — unit,
integration, e2e — passed, because every engine they ever ran against
(`MockEngine`, `ScriptedFileEngine`) completed in microseconds. The moment a
real `LlamaEngine` landed (PR #3), the same code meant seconds-long inference
inside a tokio worker's poll: N concurrent local sessions would park N workers
(the rest queueing on the engine's `std::sync::Mutex`) and the whole daemon —
every client's RPCs — would stall. Nothing in the test suite could ever have
caught it, because the defect was a latency contract, not a logic error.

## Lesson

A synchronous trait call inside an async fn must be judged by the trait's
**latency contract**, not by how fast the current implementations happen to
be. If any legitimate implementor can take more than microseconds, the call
rides `spawn_blocking` (with the engine as an owned `Arc<Mutex<dyn _>>`, and
streaming callbacks bridged back over a channel — PR #4's shape).

Pin the property with a deterministic regression test, not timing sleeps: a
**gated engine** whose `complete` signals "started", then blocks until
released — with a fallback timeout so a regression fails instead of
deadlocking — on a `worker_threads = 1` runtime. While the engine is provably
mid-completion, an unrelated spawned task must run to completion, **and the
gated work must still be in flight when it does** (`!handle.is_finished()`).
That ordering assertion is load-bearing: without it, the old inline code
passes too, because the worker frees up once the fallback elapses and the
unrelated task then runs. Mutation-verify by reinstating the inline call and
watching both tests fail (`crates/tetond/tests/nonblocking_inference.rs`).

## Why It Matters

Executor starvation is a whole-process outage that presents as "the daemon is
hung", far from the offending call site, and only under production load with
production implementations — the most expensive possible place to discover it.
The fix also forces an API decision (owned handles for `'static` closures)
that is much cheaper to make when the code is written than after callers
proliferate.

## Applies When

- An async fn calls any synchronous trait method (inference, disk, FFI, IPC)
  where a real implementor can block for more than microseconds.
- Reviewing async Rust where tests only ever exercise fast doubles
  (see also [[LESSON-433]] — single-platform/single-double false confidence).
- Writing regression tests for "does not block the runtime" properties:
  rendezvous + single worker + ordering assertion + fallback timeout, never
  wall-clock sleeps.
