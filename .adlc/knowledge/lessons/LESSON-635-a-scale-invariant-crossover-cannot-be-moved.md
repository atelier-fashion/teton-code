---
id: LESSON-635
title: "A scale-invariant crossover cannot be moved by raising the limits"
component: "daemon/harness"
domain: "routing"
stack: ["rust"]
concerns: ["correctness", "reliability"]
tags: ["context-budget", "conjunctive-guards", "crossover", "acceptance-criteria", "scale-invariance"]
req: REQ-616
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-616's AC-4 asked for a test asserting "the byte half is never the binding
half for prose or code" — at the new 262,144-token window, as though the raise
would make it true.

It could not. The budget pair is `(usable × 2/3 words, usable × 2 bytes)`, so the
two halves meet at exactly **3 bytes per whitespace-word** — a ratio with no
window in it. Raising 32,768 → 262,144 multiplies both halves by the same 8.226
and moves the crossover by nothing at all. The committed corpus then settles the
question the other way: prose measures 5.56 B/word and code 6.80, both above 3,
so the byte half binds — before the change and after it.

The criterion was not merely unproven. It was unprovable by any change to the
window, and implementing it faithfully would have meant writing a test that had
to fail.

## Lesson

LESSON-565 says to compute the crossover before changing either limit in an
AND-of-limits. This is the sharper corollary: **compute whether the crossover
depends on the limits at all.**

Write the crossover as a ratio and look for the limit in it. If both conjuncts
derive from one quantity by fixed factors, the ratio is a constant and the
binding conjunct is a property of the **content**, not of your change. An
acceptance criterion asserting which conjunct binds is then a claim about the
corpus, and the only honest way to settle it is to measure the corpus — which
takes one command and no implementation at all.

The reviewer's version: when a criterion says "after this change, X will bind
instead of Y", ask what units X and Y are in. If the change scales both by the
same factor, the criterion is describing something the change cannot affect.

## Why It Matters

A criterion of this shape fails in the most expensive way available: it survives
spec review (it sounds like a measurable claim), survives architecture (nothing
about the design contradicts it), and is only discovered by whoever sits down to
write the test — after the design that was supposed to deliver it has been
built. On REQ-616 the crossover was one division and the corpus one `python3`
loop; both were available at spec time.

It also hides the real result. The genuine gain here is 8.226× of the byte half
— the one that actually binds — which is a better claim than the false one, and
nobody would have stated it while the false claim was in the way.

## Applies When

Changing any limit in a conjunctive guard; reviewing an acceptance criterion that
predicts which of several limits will bind; raising a budget, quota, window or
threshold that is derived from the same base as its sibling.
