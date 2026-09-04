---
id: LESSON-639
title: "A new fraction of a budget silently pre-empts every rule keyed on a larger fraction of it"
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["correctness", "review"]
tags: ["budget", "fractions", "room-fraction", "digest-threshold", "dead-rule", "REQ-618", "REQ-587"]
req: REQ-618
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-618 BR-4 added a rule: a skill body over 25 % of the route's byte budget is
refused for leaving the turn no room, even though it fits. The constant went in,
the unit tests passed, and five unrelated integration fixtures went red at once.

Reading them showed why. Two other shipped rules are keyed on *larger* fractions
of the same number:

- `digest_threshold_bytes` is about 37 % of the byte budget until it hits its own
  absolute cap. So a skill body large enough to be digested is, on every route
  below roughly a 350k-token window, already large enough to be refused for no
  room — and REQ-587 BR-7's "an expansion is folded whole, never condensed"
  bypass can no longer be *observed*, because the turn carrying it is refused
  first.
- REQ-587's Stage-B refusal needs `body + dynamic output > budget` with the body
  admitted at Stage A. The output is capped at 8,000 characters and the budget
  floors at 50,000, so a body inside a 25 % ceiling plus 8,000 bytes cannot
  exceed the budget on **any** route. That path became unreachable.

Neither consequence was in the spec, and neither would have been visible from
reading BR-4. What made them visible was fixtures whose bodies had been sized
against the *other* rules years earlier.

## Lesson

Before adding a rule of the form "X may take at most F of the budget", enumerate
every other rule keyed on a fraction of that same budget and write the fractions
down next to each other. A new fraction does not sit beside the others; it
**pre-empts every rule above it**, silently, because a subject that trips the
larger threshold has already tripped the smaller one.

The tell is cheap and mechanical: `grep` the budget struct for every field that
is a share of `budget_bytes`, compute each as a percentage on two or three real
routes, and sort them. If the new fraction is not the largest, every rule above
it is now conditional on the new one *not* firing — which is usually not what
anyone intended, and is invisible in a diff that only shows the new constant.

Pin the resulting order in a test that names which fraction is higher per route,
not one that asserts a single inequality. The relationship is what a future
change to *either* constant moves, and a test on one constant would go green
while the other slid past it. REQ-618's
`the_room_ceiling_and_the_digest_threshold_are_pinned_against_each_other` walks
four routes and records which side each falls on, including the one where the
digest threshold's absolute cap makes the order flip.

## Related

- REQ-618 BR-4, ASSUME-039 — the fraction, and the open question about its value.
- REQ-587 BR-7 — the digest bypass this pre-empts.
- LESSON-565 — the sibling failure in a conjunction: reasoning about one half of
  a two-currency guard. This is the same blind spot across *rules* rather than
  across currencies.
