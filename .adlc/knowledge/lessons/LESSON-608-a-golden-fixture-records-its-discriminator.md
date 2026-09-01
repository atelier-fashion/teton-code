---
id: LESSON-608
title: "A golden fixture must record its discriminator, not rely on a positional rule that happens to work"
component: "daemon/runtime"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["reliability", "maintainability"]
tags: ["golden-fixture", "event-ordering", "discriminator", "lesson-591-followup", "req-598-followup"]
req: REQ-604
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

LESSON-591 established that a golden event sequence must not pin the position of
an event published from a detached `tokio::spawn`. REQ-598 applied it on the
*live* side correctly — the title duty's `route_decided` is dropped by
`rd.category == Some(Category::Title)`, a field the event carries.

On the *fixture* side it could not do the same, because the recorded file holds
only event names. So it drops the detached decision positionally:

```rust
// Drop the title duty's decision — the fixture's header names it
// as the first of the two.
let mut seen_route = false;
let kept = recorded.into_iter().filter(|n| {
    if n == "route_decided" && !seen_route { seen_route = true; return false; }
    true
});
```

The test's own doc comment describes this as "one rule, applied through the
discriminator each side actually has" — which is honest about the asymmetry but
still leaves a positional rule in the comparison. It is correct today only
because that file happens to contain exactly two `route_decided` entries and the
detached one happens to be recorded first.

REQ-604 captured two more sequences. In the skill scenario the detached decision
is **not** first — across 288 capture runs it landed after `skill_invoked` 143
times and before it once. "The first of the two" would have been wrong in the
common case and right in the rare one.

## The Lesson

**Record the discriminator in the fixture file, so neither side reasons about
position.** REQ-604's fixtures write `route_decided[category=title]` rather than
a bare name, and both sides filter on the same field:

```rust
fn is_detached(&self) -> bool {
    self.name == DETACHED_TITLED
        || (self.name == ROUTE && self.category.as_deref() == Some(TITLE))
}
```

A positional rule in a golden file is a latent bug with a scope equal to the
file's current length. It does not announce itself when the file grows, when a
second instance of the event appears, or when the thing it indexes moves — and
the last of those is precisely what a detached event does for a living.

Two properties fall out that the positional form did not have:

- **A malformed line fails loud.** An entry whose discriminator does not parse
  yields `category: None`, so it is *not* dropped, survives into the comparison,
  and trips either the route-count floor or the sequence assertion. A typo can
  only make the suite louder.
- **The rule is stated once.** The live side and the fixture side run the same
  predicate over the same field, so there is no asymmetry for a later reader to
  re-derive — or to get wrong.

## How to Apply

When writing any golden file whose entries are filtered before comparison, ask
what the filter keys on. If the answer is "position", the file is under-specified
— add the field the filter actually means to the recorded form. The cost is a
parser of about ten lines.
