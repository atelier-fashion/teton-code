---
id: LESSON-591
title: "A golden event sequence must not pin the position of a detached task's event"
component: "daemon/session"
domain: "testing"
stack: ["rust", "tokio"]
concerns: ["reliability", "testing"]
tags: ["golden-fixture", "flaky", "detached-task", "event-ordering", "ci-only-failure"]
req: REQ-598
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-598's BR-1 guard was a golden fixture recording one turn's event sequence,
captured before the refactor so the oracle could not be computed by the subject.
It recorded:

    route_decided, session_titled, route_decided, session_update, prefix_cache

Two of those come from the turn path and are deterministic: the title duty's
route decision, published synchronously inside `spawn_title_session`, and the
turn's own, published later by `emit_route_decided`. The third,
`session_titled`, is published by the **detached** naming task — `tokio::spawn`,
handle dropped, and `spawn_title_session`'s own docs say nothing on the turn path
reads its result.

CI failed on `macos-latest` while `ubuntu-latest` passed **on the same commit**,
with 40/40 green locally. On the slower runner the naming task finished *after*
the turn's route decision, which made the two `route_decided` entries adjacent —
and the fixture's collapse-consecutive-duplicates rule merged them, shortening
the sequence.

The fixture was pinning the scheduler.

## Lesson

A golden sequence may only pin orderings the system actually guarantees. An
event published by a detached task has no guaranteed position relative to
anything, so **exclude it from the sequence and assert its arrival separately**.

Two corollaries that made the fix safe:

- **Normalize both sides identically.** The filter is applied to the recorded
  fixture and to the live run, so anything it hides on one side it hides on the
  other and it cannot excuse a real reordering.
- **Do not regenerate the fixture.** Its value was its pre-refactor provenance.
  Changing the *comparison* preserves that; regenerating the *data* would have
  destroyed it and produced a green test proving nothing.

Removing the racy element also let the collapse rule narrow to the one event it
was written for (the per-chunk `session_update`), which made the comparison
*stricter* — both route decisions now stay two entries instead of merging.

## Why It Matters

Platform-split CI results on one commit are a race, and treating them as a real
behavior change sends you looking for a defect that is not there. The reverse
error is worse: "re-run it, it's flaky" leaves a fixture that is green by luck
and blind to the ordering it was built to protect.

Here the collapse rule turned a harmless timing difference into a *changed
sequence length*, which is why the failure looked like a lost event rather than
a reordering. A rule that merges adjacent duplicates will do that whenever a
racy element sits between two identical ones.

## Applies When

Writing or reviewing a golden/snapshot test over an event stream; a test asserts
ordering across anything spawned, detached, or otherwise not awaited; a suite is
green on one platform and red on another for the same commit; or a fixture
applies a normalization (collapse, dedupe, sort) that can change a sequence's
length.
