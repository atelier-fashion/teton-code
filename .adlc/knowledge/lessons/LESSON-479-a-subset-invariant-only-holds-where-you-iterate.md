---
id: LESSON-479
title: "A subset invariant is only tested in the direction your loop iterates — write the equation down, then check which half you wrote"
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security", "test-coverage"]
tags: ["invariant", "drift-guard", "test-design", "prompt-injection", "false-confidence"]
bug: BUG-151
created: 2026-08-04
updated: 2026-08-04
---

## What Happened

BUG-148 shipped a drift guard for the harness's prompt-injection posture, on the principle
that "frame is frame in both directions": what the model must not *emit* is what untrusted
content must not *introduce*. The test looked like the principle:

```rust
for marker in FLAT_ANCHORED_MARKERS.iter().chain(CHATML_ANCHORED_MARKERS) {
    assert!(starts_with_frame_label(marker) || starts_with_envelope_tag(marker), ...);
}
```

It iterates the **output** markers. So it proves `output ⊆ input` and nothing else — while
the principle it was written to defend is a **set equality**.

The missing half was not hypothetical. At that exact commit, `<mcp-tool-result` was already
in the input alphabet (BUG-148 defused it) but absent from both output marker sets, so a
model could emit a fabricated MCP envelope uncut. That is BUG-149 — and it sat green under
two drift guards. A sibling test *did* iterate the input tags, but only asserted
layer-exclusivity, which stays true no matter what the output side contains. BUG-149 was
found by reading the code.

## Lesson

**State the invariant as an equation before writing the loop, then check which half the
loop actually tests.** `A ⊆ B` and `A = B` look identical in prose ("these must match",
"both directions"); they differ by a second loop. A guard that tests one containment and is
*described* as symmetric is worse than no guard, because it converts an unchecked property
into a believed one.

The practical procedure:

1. Write the set relation explicitly in the test's comment — `output ⊆ input`, not "the
   sets agree."
2. If the relation is equality, there are two loops. If there is one loop, say in the
   comment which direction is *not* covered.
3. Enumerate the deliberate asymmetries and encode each as an explicit skip with its
   reason, so the exemption is reviewable. Here: closing envelope tags are input-only by
   construction (a model emitting `</tool-result>` has closed nothing it opened, so it has
   forged nothing), and transcript labels need no reverse check because the input predicate
   *derives* its alphabet from the output sets — containment is structural, not asserted.

And the standing rule that caught this one honestly: **a passing test proves nothing until
you have seen it fail.** Reverting `<mcp-tool-result` from both marker sets — reproducing
the pre-BUG-149 state — is what demonstrated the new assertion is a guard rather than a
restatement.

## Why It Matters

Drift guards are load-bearing precisely where nobody is looking; their whole purpose is to
fail later, in a change nobody connected to the original reasoning. A half-invariant fails
in the worst possible way — it stays green through the exact defect it was written to
prevent, and its existence discourages the manual check that would have caught it. Two
bugs' worth of green was not evidence of coverage; it was evidence that nothing had
exercised the untested direction.

## Applies When

Writing any test that asserts two collections "agree" — marker sets, allow/deny lists,
serializer/deserializer field coverage, enum-to-string round trips, config schema vs
defaults, API request/response models; reviewing a test whose comment says "both
directions"; adding a member to one side of a paired pair of constants.

## Related

- [[LESSON-477]] — split the sanitizer by authoring layer. The invariant this lesson is
  about is the guard for that split.
- [[LESSON-474]] — derive the input alphabet from the output alphabet, so the two cannot
  drift. That derivation is what makes the transcript half of the equality structural and
  the envelope half the part needing an assertion.
