---
id: LESSON-499
title: "When a cargo feature hides the code from CI, extract the decision out of it"
component: "inference/local"
domain: "inference"
stack: ["rust", "cargo", "ci"]
concerns: ["testing", "reliability"]
tags: ["feature-flags", "test-coverage", "pure-functions", "test-doubles"]
req: REQ-564
created: 2026-08-10
updated: 2026-08-10
---

## What Happened

The real llama.cpp binding lives behind the non-default `llama` cargo feature,
so default builds and CI never compile it — a deliberate choice (no cmake in
CI, ADR-006). REQ-564's most delicate logic is the prefix-reuse rule: when may
a turn reuse resident KV, how much, and what is the honest reason when it may
not. Written where it naturally wanted to go — next to the `clear_kv_cache_seq`
call it drives — every one of those decisions would have shipped with **zero**
automated coverage, in the subsystem where a wrong answer silently corrupts a
user's answers.

Splitting it in two fixed that. The *decision* became a pure function over
plain `i32` token ids in a module with no feature gate and no binding
dependency; the gated module was left holding FFI. The acceptance suite's
scripted engine then calls the **same** decision function and the **same**
window guard the real engine calls.

The payoff showed up immediately in a place the split was not designed for:
writing the tests surfaced two fixture bugs that were really modeling bugs
(the resident prefix is prompt *plus generated* tokens, and a transcript that
appends anything else diverges), and the shared guard meant the suite could not
pass against a laxer boundary than production enforces.

## Lesson

A cargo feature that CI does not build is a **coverage boundary**, not just a
build option. Anything on the far side of it is untested by construction, so
the design question is not "how do I test the gated module" but "how much of
the gated module has no business being there". Push every decision that can be
expressed over plain data across the boundary; leave the gated side holding
only the calls that genuinely need the foreign library.

The test double must then consume the *same* extracted policy, not a
reimplementation of it. A double with its own copy of the rule tests that two
implementations agree with each other's bugs — and it is exactly the shape that
lets someone later "fix the test" instead of the code. Where the double and
production share the function, a divergence between them can only be in the
mechanism, which is precisely the part you already knew you could not test and
must verify by hand.

Say so in the test module's header, too. A green suite over a scripted engine
proves the policy and the plumbing; it does not prove the FFI does what the
policy asked. Letting a pass imply more than it shows is LESSON-448's mistake
in a new costume.
