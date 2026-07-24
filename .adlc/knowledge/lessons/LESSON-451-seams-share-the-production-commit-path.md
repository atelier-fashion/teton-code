---
id: LESSON-451
title: "A test seam fakes the boundary, never the commit path — factor the shared half out and reuse it"
component: "daemon/runtime"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "reliability"]
tags: ["test-seam", "fake", "staging", "commit-path", "coverage-gap", "engine-slot"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

The in-process consent-gate tests used fake `LocalEngineLoader`s that staged
engines in their own private maps and never touched the runtime's engine
slot. Every gate flow was covered, yet the production property "`Ready` opens
the tier on the slot's *fact*, not the loader's claim" was pinned by exactly
one unit test — the adversary flagged it — and the real commit path
(`stage → supersede re-check → commit → EngineSlot::install`) was exercised
end-to-end only by the manual AC-13 dogfood. When PR #6 added the
`TETON_FAKE_ENGINE_LOADER` seam for the cross-process suite, the same
mistake was available again: a fake with its own commit shortcut would have
made the e2e pass while the production commit stayed uncovered.

## Lesson

A test double should replace only the part that is genuinely impossible in
the test environment — here the GGUF parse and the hardware measurement —
and must route everything downstream through the production code. The
mechanical way to guarantee that is to **factor the shared half into one
type** (`StagedEngines`: the staging map + the only staged→serving
transition) and have both the real and the fake implementor hold it. Then
the seam cannot drift: the e2e's commit *is* `EngineSlot::install` on the
runtime's real slot, and the gate's re-check/commit/abandon discipline is
tested every CI run instead of once per dogfood. Fixed benchmark figures
should be recognizable-and-passing constants asserted exactly on the wire,
so the event provably carries the loader's report rather than a default.

## Why It Matters

A fake with a private commit path converts "the chain works" into "the fake
works" — the assertions all pass while the one transition that matters in
production runs only on a developer's machine with an 18 GB model. Coverage
gaps of this shape are invisible in coverage numbers (the lines are green via
the unit test) and get found by adversarial review or by production.

## Applies When

- Designing any gated test seam or fake for a flow with a staged/committed
  or prepare/apply split — fake the acquisition, share the application.
- A trait has one production implementor and test doubles: check whether the
  doubles bypass a side effect the production one performs, and whether any
  test pins that side effect end-to-end.
- Reviewing a seam PR: ask "which production lines does the fake path *not*
  execute?" (see also [[LESSON-445]] — stage-then-commit after authority
  re-check, the discipline this seam must inherit rather than reimplement).
