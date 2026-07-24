---
id: LESSON-447
title: "A best-effort fallback must preserve the invariant it backs up — and fail loudly"
component: "daemon/harness"
domain: "inference"
stack: ["rust", "llama.cpp"]
concerns: ["reliability", "observability"]
tags: ["fallback", "summarizer", "context-window", "silent-failure", "degradation", "truncation"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

The harness's tool-result summarizer (`summarize_if_large` in
`crates/tetond/src/harness/context.rs`) existed to keep oversized tool results
from evicting the conversation on a small context window. Its error path was
`Err(_) => text.to_owned()` — on any engine failure it folded the **raw,
unbounded** result into context and told nobody. Worse, the failure and the
duty were correlated: the same pathological input that made the result
oversized (a minified single-line file) also blew the summarizer's own prompt
past the engine window, so the fallback fired precisely on the inputs the
guard existed for. The guard was a no-op exactly when it mattered, and the
first dogfooded local turn died on a folded read of `egress/mod.rs` with
nothing in any log naming the summarizer as the culprit.

## Lesson

When a best-effort step guards an invariant (here: "nothing oversized enters
context"), its failure fallback must **still enforce the invariant by a
degraded means**, not skip it. The fix (PR #5): on engine failure, fold a
mechanically truncated head+tail of the result — dumber than a summary, but
still bounded — and report the engine error on the outcome (`SummarizeOutcome`)
so the caller logs it. Also bound the guard's own inputs: the summarizer now
never receives more than `SUMMARIZER_INPUT_MAX_BYTES`, so the guard cannot be
broken by the input it is guarding against. "Best-effort, never fatal" is only
acceptable when the fallback is safe AND the failure is observable.

## Why It Matters

A silent identity fallback converts a hard failure into a delayed, harder-to-
diagnose one: the turn dies downstream (over-window prompt) with no trace of
the real cause. Any `Err(_) => input.clone()` on a transformation whose
*purpose* is to shrink/sanitize/bound its input is this bug. Audit for the
pattern wherever a guard degrades: the degraded path must hold the same
invariant, and the degradation must be visible (log, event, or typed outcome).

## Related

- LESSON-446 — the currency mismatch (approx-words vs BPE) that made the
  trigger miss these inputs in the first place; this lesson is about the
  fallback and observability half of the same incident.
- LESSON-444 — FFI asserts abort the process; the typed over-window error this
  fallback now backstops was introduced there.
