---
id: LESSON-513
title: "A feature-gated target is invisible to every refactor"
component: "daemon"
domain: "harness"
stack: ["rust", "daemon", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["llama", "feature-gate", "compile-drift", "ci-blind-spot", "bug-166", "bug-164"]
req: REQ-571
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-571 added a `SessionId` parameter to `LocalEngineSource::new`. The change
updated every call site the compiler could see, and the workspace was green.
`crates/tetond/tests/template_smoke.rs` is `#![cfg(feature = "llama")]` — CI
never compiles it, because compiling it means building llama.cpp — so its call
site kept the old two-argument shape and `origin/main` shipped a target that
does not build. The break sat invisible from the REQ-571 merge until a manual
`--features llama` build tripped over it a day later (BUG-166).

## Lesson

"It compiles, so every call site is updated" is a verdict scoped to the
features you compiled with. `cfg`'d-out code is not type-checked, so a
shared-API change can be complete for the always-on surface and silently
incomplete for every gated one. A feature-gated target therefore rots by
default: nothing re-checks it between the manual occasions someone turns the
feature on, and those occasions are exactly when it is needed.

BUG-164 is the runtime flavor of the same blind spot (a gated e2e can pass
against a stale daemon); this is the compile flavor (a gated target stops
building at all). Both reduce to: a manual gate's health is itself unmonitored.

## Why It Matters

The gated targets here are not optional extras — `template_smoke` is REQ-554
AC-6's acceptance vehicle, the one real-weights check that the template path
emits a well-formed tool call (mock-only green is not acceptance, LESSON-433).
A gate that fails to *build* fails closed at the worst moment: when someone
finally needs real-weights evidence, they first have to archaeology a compile
error introduced by an unrelated change, on a machine that may also need the
17 GiB weights and a llama.cpp build before the error even shows.

## How to Apply

- After changing any API shared with gated code, sweep the gated surface:
  `cargo check -p tetond -p teton-inference
  --features tetond/llama,teton-inference/llama --tests`. With llama.cpp
  already built it is seconds; the native build cost is paid once per
  machine, not per sweep.
- Enumerate what the sweep must cover with
  `grep -rl 'feature = "llama"' crates --include='*.rs'` — the gated surface
  moves, and the sweep is only as good as its coverage.
- When adding a new feature gate, ask what re-checks it and when. If the
  answer is "whoever next runs it by hand", say so in the gated file's header
  comment, the way `template_smoke.rs` documents its manual invocation.
- Standing candidate: a CI leg that caches the llama.cpp build and runs the
  check-only sweep — it would close this class without running any weights.

## Related

- BUG-166 — the fix this lesson is drawn from.
- BUG-164 / LESSON-510 — the runtime flavor: existence is not freshness, and a
  manual gate's health is unmonitored by construction.
