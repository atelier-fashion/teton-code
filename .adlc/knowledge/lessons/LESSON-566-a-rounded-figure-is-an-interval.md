---
id: LESSON-566
title: "A rounded figure is an interval — never re-derive a decision from a rendered number"
component: "adlc/spec"
domain: "harness"
stack: ["markdown"]
concerns: ["reliability", "correctness"]
tags: ["evidence", "provenance", "rounding", "field-reports"]
req: REQ-590
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

REQ-590 reversed one of its own decisions mid-implementation, and the argument for the reversal
was: *the reported `/analyze` body is 31,014 bytes, so the new 30,720-byte budget refuses it by
294.* That figure was quoted in a commit message, an ADR, a requirement, a runbook and two test
assertion messages.

It was never measured. The only record of the field event is REQ-589's **rendered** sentence,
"about 4,097 words / **31 KB**" — and the renderer is `(bytes + 500) / 1_000`. So "31 KB" means
the true count lies anywhere in **[30,500, 31,499]**, an interval that **straddles 30,720**. At
the low end the body would have served; at the high end it was refused by 779. The record could
not decide the question it was being used to decide.

`31_014` appeared nowhere in the repository's history before the reversal commit that introduced
it. A different figure for the same event (`31_744`) sat in another file. Derived claims — "6,143
words spare", "1,754 bytes spare", "7.57 B/word" — all inherited the invented precision, and 7.57
sat just above the 7.5 crossover the whole argument turned on.

Correcting it produced a **second** error in the same argument: the honest density range was
stated as "6.5–7.7 B/word" when `30,500 / 4,097 = 7.44`. That slip inflated the one figure that
made the reversed decision look defensible (+15% where the truth was +0.76%).

## Lesson

**A rendered number is evidence about an interval, not a point.** Before re-deriving anything
from a figure that appears in prose, find the renderer and compute what the figure admits. If the
decision boundary falls inside that interval, the record cannot settle it — say so and find
another argument.

Two habits fall out. **Check the provenance of any number that becomes load-bearing** — `git log
-S` on the literal will show whether it predates the argument that needs it. And **when a
conclusion survives without the disputed number, lead with the argument that does not need it**:
here the crossover (pure arithmetic) and strict non-regression (`min(10240, 32768/d) ≥ min(4096,
32768/d)` at every density) were both sound and neither required a measurement.

## Why It Matters

Precision is persuasive. "Refused by 294 bytes" ends an argument in a way "somewhere in a
1 KB interval that straddles the budget" does not — which is exactly why the invented version
propagated to nine files unchallenged while the honest one would have invited the question that
found the real evidence. The conclusion happened to be right; the reasoning offered for it was
not the reasoning that supported it.

## Applies When

- Any number taken from a log line, an error message, a rendered report, or a spec that quotes one.
- Reversing or ratifying a decision on the strength of a single figure.
- Writing a test that pins a derived quantity — spare capacity, a margin, a ratio — to a precision
  the source cannot carry.
