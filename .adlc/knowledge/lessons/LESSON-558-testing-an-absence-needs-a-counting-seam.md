---
id: LESSON-558
title: "\"It costs nothing when it's off\" is a claim about work not done — untestable until the work is made countable"
component: "verification"
domain: "verification"
stack: ["rust"]
concerns: ["opt-in-features", "test-design"]
tags: ["seam", "absence", "opt-in", "test-double", "vacuous-test", "req-588"]
req: REQ-588
created: 2026-08-23
updated: 2026-08-23
---

## What happened

REQ-588's ADR-6 promises that an un-opted-in machine performs no ceiling check,
builds no accumulator, and does **no pricing lookup**. The obvious test — send a
request with no ceiling and assert it forwards — proves almost nothing: it passes
whether or not a pricing lookup happened, and it would keep passing if someone
later moved the lookup outside the `Option` guard.

The test that means something gave the meter double a counter on `can_price` and
asserted **zero calls**.

## The lesson

**An absence cannot be asserted against behaviour that is identical either way.**
To test that work did not happen, make the work observable — a counter on the
seam, a double that records being consulted — and assert the count.

Two reinforcing tricks, both cheap:

- **Count the seam.** `assert_eq!(meter.queries(), 0)` is a direct statement of
  the claim, where "it forwarded" is not.
- **Rig the double to fail loudly if consulted.** The same double was set to
  report *everything* unpriceable. If a stray lookup ever happens, the call is
  refused and the test fails on the forward assertion too — so the test cannot
  pass by accident even if someone deletes the counter assertion.

## How to apply

- Whenever an ADR claims a feature is free when disabled, ask what would be
  observably different if it were not. If the answer is "nothing", the claim is
  currently untested however many tests surround it.
- Add the seam in the test double, not in production code — this needs no
  instrumentation on the hot path, just a double that remembers.
- Pair the counter with a double whose consultation would be *fatal*. Two
  independent ways to catch the same regression, for one extra line.
