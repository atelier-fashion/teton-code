---
id: LESSON-597
title: "Re-derivation checks the figures you thought to check; only an adversary finds the ones you didn't"
component: "adlc/architecture"
domain: "process"
stack: []
concerns: ["maintainability", "developer-experience"]
tags: ["verification", "adversarial-review", "measurement", "provenance", "self-review"]
req: REQ-602
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

REQ-602 existed because REQ-599 shipped with figures nobody had re-derived. Its
final task was named "final verification, with every figure carrying its rule."
That task did real work: it caught that AC-1 paired a *token-occurrence* count
(130) with a *declaration* count (5) — two rules, one arrow — and corrected it to
88 → 5 with the rule stated. It caught that the doc-path figures ("31 of 42")
survived under no corpus. It recorded one criterion as **deliberately not done**,
with the reason, rather than performing it because it was written down.

Then a three-agent adversarial panel ran and found nine more things, including
one Critical and five Majors. Among them:

- The crate-wide surface was **4, not 5**. One item was kept `pub(crate)` to
  "match its `pub(crate)` accessor" — and nobody had asked why the *accessor*
  was `pub(crate)`. It had no out-of-tree caller either.
- A ratchet blind to bare `pub`, the wider spelling ([[lesson-596]]).
- A flat directory read that the acceptance criterion **named explicitly** and
  the task had not touched. The task's own accounting — "three sharing a helper,
  two carrying a local copy" — covered six of seven sites and nobody noticed the
  arithmetic.
- A recorded mutation outcome that **cannot occur**: two `assert!`s in one test
  cannot both fire, so the lower bound had never actually been observed firing,
  in a doc comment that said twice each outcome was recorded "as observed".
- REQ-602's own `architecture.md` still asserting the discarded "143 → 8" while
  its requirement called that figure baseless. The document written to reconcile
  *another* REQ's stale plan had never been reconciled with itself.

Every one of these was inside the scope the verification task had just walked.

## Lesson

**Re-derivation and adversarial review find different things, and neither
substitutes for the other.**

Re-derivation takes a list of figures and recomputes them. It is excellent at
what it does and it is bounded by the list. The nine findings above were not
recomputations of stated figures — they were questions nobody had asked:

| verification asks | an adversary asks |
|---|---|
| is this number right? | is this the right number to state? |
| does the guard pass? | what does the guard *not look at*? |
| did the task do what it said? | does the arithmetic in what it said add up? |
| is the figure re-derived? | is it re-derived **everywhere it appears**? |

The last row is the cheapest and the most repeated. A corrected figure gets
corrected in the artifact you are editing. Its copies — in the architecture doc,
in a test's module header, in a sibling spec — keep asserting the old value, and
each copy reads as independent confirmation to the next person.

Three practical rules, all of which would have caught something here:

1. **Correct a figure everywhere it appears, in the same commit.** Grep the
   discarded value, not just the artifact you noticed it in. "143" appeared in
   three files; the fix touched one.
2. **Check the arithmetic in your own accounting.** "Three sharing, two local"
   over "seven sites" is visibly six. Stating a decomposition invites this check
   and is worth doing for that reason alone.
3. **When a justification points at another item, follow it one hop.** "It must
   match its `pub(crate)` accessor" is not a reason; it is a deferral. The
   accessor had no reason either.

## Why It Matters

The expensive belief is that a careful verification pass makes review optional.
It reads that way from the inside — every number was recomputed, every guard was
run, the deliberately-skipped step was recorded with its reason. That REQ still
shipped a security-relevant guard watching the wrong door.

It is also the second time in this line that knowing a rule did not prevent
breaking it ([[lesson-593]] records the first: a spec written by someone who had
spent the previous day correcting exactly that mistake). The pattern is not
carelessness. It is that self-review searches the space you are already looking
at, and that space is defined by the same assumptions that produced the defect.

## Applies When

Closing out any REQ whose value rests on stated figures or on derived checks;
deciding whether a verification pass is sufficient before merge; correcting a
number that has been copied into more than one artifact; and reading a
justification of the form "X must be wide because Y needs it" — follow it to Y.

The cheap version: **before merging, grep the repository for every figure you
discarded during the work.** If a discarded number still appears anywhere, it is
still being trusted by someone.
