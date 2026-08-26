---
id: LESSON-567
title: "When one derivation splits into two, the documentation drifts before the code does"
component: "daemon/harness"
domain: "harness"
stack: ["rust"]
concerns: ["maintainability", "reliability"]
tags: ["doc-comments", "provenance", "asymmetry", "false-invariants"]
req: REQ-590
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

REQ-590 left the local context budget's two halves with **different provenance**: the word half
derived from the engine window, the byte half a constant. Before, both came from one number.

A four-agent verify panel attacked the result. The derivation held — `derive`, `window_pair`, the
floor split, a closed recursion, digest scaling and the compaction chain each survived a
deliberate attempt to break them. **Every Critical and Major the panel raised was a sentence.**

Four doc comments shipped saying the opposite of what runs. The worst sat on the field that
carries the value and read: *"the local tier's bytes come from the engine's own window … rather
than from `LOCAL_BUDGET_TOKENS × APPROX_BYTES_PER_TOKEN`. That is what keeps a full assembled
prompt inside the window instead of a whole generation past it."* Every clause was false: the byte
half **is** that product, and it out-claims the window by exactly one generation. It was the
sentence a future reader would have cited to undo the decision.

The same shape recurred at every level. A module doc named one uncovered corpus class when the
same diff added a second. A test-index attributed an acceptance criterion to the one leg that
could not discriminate. The spec's rule for a latent hazard reasoned from a budget that *falls*
with the window, when after the change it does not fall at all — the hazard had **inverted
direction** and grown to a 5.3× overclaim.

Nothing was caught by the compiler, the 3,896-test suite, or clippy. Prose has no type system.

## Lesson

**Splitting one derivation into two makes every sentence about "the pair" a candidate falsehood.**
When provenance diverges, grep for the old joint description and re-read each hit against the new
values — the field's own doc, the module doc, test-index tables, the spec's rules, and any open
question whose reasoning assumed the halves moved together.

Pay particular attention to **latent rules**: a hazard nobody can trigger today is documented
once and never re-read, so an inverted one survives indefinitely and points the next engineer the
wrong way at exactly the moment they need it.

## Why It Matters

A false invariant is worse than a missing one: the next reader trusts it and stops checking. Here
they would have been told the byte half is window-derived and safe, when it is neither — and the
one artifact that would have corrected them was the ADR, which is not what you read when you open
the file.

## Applies When

- Any change that gives two related values different sources.
- Reversing a decision mid-implementation — the reversal reaches the code and the decision record,
  and reliably misses the comments in between.
- Reviewing a diff where the logic is provably correct: that is when to start reading the prose,
  not when to stop (see LESSON-561).
