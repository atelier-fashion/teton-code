---
id: LESSON-645
title: "A 'measured' band recorded in a comment ages silently, because the test only ever stands at one point in it"
component: "daemon/tests/fixtures"
domain: "testing"
stack: ["rust"]
concerns: ["maintainability", "reliability"]
tags: ["fixture-tuning", "stale-comment", "drift", "re-measure", "context-budget", "bug-193"]
req: REQ-617
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

`skill_over_budget_offer.rs` has a `(Window, FitsWindow)` cell whose declared
window has to sit inside a band: the body must be over budget, and the measured
prompt must still fit `window × 2`. Three REQs in a row moved it. Each left a
paragraph, and the last one before this REQ ended: *"The band is one thousand
tokens wide, measured. 31,000 passes; 32,000 fails."*

Resolving the rebase, the obvious move was to nudge the window by the amount the
prompt had changed. Sweeping it instead — the thing the comment said had already
been done — found 29,600 failing, **29,800 through 100,000 passing**, and 150,000
failing the other way. The band was not one thousand tokens wide. It had been,
once; REQ-616's engine-window work and REQ-586's derived budget widened it, and
nothing re-measured because nothing had to: the test stands at one point, and a
point inside a wider band is still inside it.

Every REQ that read "one thousand tokens wide" and nudged by the minimum was
therefore parking the fixture a few hundred bytes from an edge it believed was
unavoidable, when there were forty thousand tokens of room in the other
direction.

## Lesson

A number in a comment that describes a *range* has no test. The assertion pins
one value; the range around it is prose, and prose about a derived quantity
decays every time anything upstream of the derivation moves. So: when a comment
tells you the shape of the space you are tuning in, re-derive it before you
trust it — a sweep is usually a shell loop and a few minutes — and write down
what you actually observed, including the edges you found, so the next person
re-measures a smaller distance rather than inheriting your reasoning.

This is BUG-193's shape one level up. BUG-193 was a *value* drifting while its
test stayed green, and the fix was to pin the value with `assert_eq!`. A band
cannot be pinned that way without asserting the edges, which usually is not
worth it — so the honest alternative is to say in the comment that the figure
is a reading with a date on it, and to re-take the reading.

## Why It Matters

Tuning a fixture to the minimum that passes guarantees the next prompt edit
moves it again, which is exactly the churn the comment was complaining about
while causing it. Re-measuring turned a fourth consecutive nudge into a choice
with four kilobytes of clearance.

## Applies When

- A comment states a measured range, band, threshold distance or "N bytes of room".
- Re-tuning a fixture whose constants are derived from a budget or a window.
- Any figure whose inputs live in another module and move on their own schedule.
- Reading a comment that says "measured" — check the date and what has landed since.
