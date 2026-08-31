---
id: LESSON-593
title: "A decision-grade number gets re-derived — the instrument can be wrong, and so can the provenance"
component: "adlc/architecture"
domain: "refactoring"
stack: ["rust"]
concerns: ["developer-experience", "maintainability"]
tags: ["measurement", "robust-statistics", "provenance", "second-hand-numbers", "decomposition", "seams", "corrected"]
req: REQ-599
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

**This lesson was corrected on 2026-08-31 after an adversarial review. Its
original title was "A rationale id marks a change, not a subsystem — it cannot
locate a seam", and that conclusion was wrong.** The correction is the lesson.

REQ-599 set out to split a 14,183-line god module. The requirement proposed
finding seams by clustering the file's dense REQ/ADR/LESSON annotations, and
named this its central bet. Phase 2 measured each id's **span** —
`max(position) − min(position)` — and reported **1 of 19 ids clustered, 13
scattered**. The bet was declared refuted and the method discarded.

The measurement was reproducible and the parser was sound. The *statistic* was
not. `max − min` has zero breakdown resistance: for an id on 5–21 items in a
14k-line file, **one** outlying annotation forces the "scattered" verdict.

Re-measured on the same data with a robust statistic — the smallest window
holding 70% of an id's items:

| statistic | clustered | scattered |
|---|---:|---:|
| max-span (as published) | **1 / 19** | 13 |
| 70%-window | **5 / 19** | — |

And trimming a single extreme item per id moves max-span's own count from 1 to
5.

The decisive case is **REQ-581**: max-span 3,515 → filed as "loose, not a seam".
Its 70%-window is **219 lines**, holding 4 of its 5 items — and that window is
`ProbeAnswer`, `probe_outcome`, `to_protocol_health`, `stream_probe`, which is
*exactly* the set that became `runtime/provider.rs`. The id predicted the module
and the metric could not see it.

The sharpest part: REQ-599's own findings record that `provider` "measured as
scattered across 10,366 lines and was skipped for that reason." On that seam the
discarded proxy beat the census that replaced it.

## The same failure, from the other direction — REQ-602

Three commits after the correction above, `/validate` ran over **REQ-602**, a
spec written to clean up after REQ-599. Its AC-1 cited "**61** `pub(crate)`
items with no out-of-tree caller." Re-measured against the code: **48** counting
top-level items and `impl` methods, **52** including struct fields. Neither is 61.

Nothing was mis-measured this time, because nothing was measured. The figure came
from a review agent's report and was written into a spec on trust. The finding's
*direction* was solid and independently confirmed — a large majority of those
items genuinely have no caller outside the tree — but the number that would have
sized the work was decorative.

Two different faults, one class:

| | REQ-599 ADR-1 | REQ-602 AC-1 |
|---|---|---|
| number | 1 of 19 clustered | 61 items |
| computed by | me, correctly, with the wrong instrument | someone else; not re-derived |
| what failed | `max − min` has no breakdown resistance | provenance — it was never checked |
| what it decided | discard the method; write it into REQ-600 | size the cleanup |
| how it was caught | adversarial review | validation of a spec I wrote |

The second is the cheaper mistake and the easier one to repeat, because a
second-hand number arrives already sounding settled. The first at least had a
computation behind it.


## Lesson

**Two distinct claims got collapsed into one, and only the first was true.**

- **True, and still the useful half.** The requirement's *literal rule* — "where
  ids interleave across a proposed boundary, the boundary is wrong" — is fatal
  as written. In a file where changes are cross-cutting, every boundary has
  interleaving ids, so the rule condemns all of them and cannot be used to
  choose.
- **False, and asserted anyway.** That ids therefore "cannot locate a seam."
  Under a robust statistic several do: five of nineteen sit in tight windows, and
  those windows name real modules.

The right conclusion is narrower and more useful: rationale ids are a **weak
positive signal** — good for generating candidate boundaries, useless as a
rejection rule. Use them to propose; use structure to decide.

**And the method lesson, which generalises further than the subject: a number
that decides something is re-derived by whoever relies on it.** Two ways that
goes wrong, and both happened here within a week:

- **The instrument.** When a measurement is about to overturn a plan, check the
  statistic's breakdown point before trusting the headline. A max, a min, or a
  full range answers "is there any outlier" — which is rarely the question.
  "Where is the mass" needs a quantile or a densest-window.
- **The provenance.** A number arriving from a review, a report, or another
  agent is *evidence*, not a measurement. Re-derive it before it enters a spec,
  or carry it explicitly as second-hand — "a review sample reported N" — so the
  next reader knows what they are standing on.

The cheap version of both: **state the counting rule beside the count.** A figure
that says what it counted invites the check that a bare figure does not. REQ-602's
AC-1 now requires exactly that.

## Why It Matters

The original conclusion was written into REQ-599's ADR-1, its Assumptions, this
lesson's title, and — most expensively — into REQ-600 as settled guidance not to
re-propose the method. A follow-on REQ was being told to discard a signal that
partly works, on the strength of one fragile statistic.

The REQ-602 instance cost less only because validation caught it before
`/architect` sized work against it. It was written by someone who had just spent
a day correcting the first instance — which is the point. Knowing the rule did
not prevent the second occurrence, because the two feel completely different from
the inside: one is "I measured carefully", the other is "someone else measured".

It survived because everything downstream of the number was sound. The parse was
correct, the census reproducible, the reasoning about cross-cutting changes
genuinely true. Nothing in the chain was careless except the choice of
statistic, and no reviewer questions a number that reproduces.

## Applies When

Any measurement that decides whether to abandon an approach; computing "spread"
or "locality" over positions (prefer a densest-window or interquartile measure to
a range); writing a requirement whose method rests on a checkable assumption —
name the assumption *and the instrument*, because this one named the assumption
and got the instrument wrong; and reviewing a claim that a plan has been refuted,
where the reproducibility of the number is not evidence that the number answers
the question.

And for the provenance half: any time a figure moves from a review, an agent
report, a benchmark or a ticket **into an artifact that will be acted on** — a
spec, an AC, an architecture doc. Re-derive it, or label it second-hand. The
tell is a number you can state but could not reproduce right now.
