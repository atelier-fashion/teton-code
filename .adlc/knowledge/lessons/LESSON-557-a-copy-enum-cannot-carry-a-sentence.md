---
id: LESSON-557
title: "A typed outcome's sentence dies at the first `Copy` boundary it crosses — compose it where the facts are, not where the condition is detected"
component: "egress"
domain: "error-handling"
stack: ["rust"]
concerns: ["user-facing-messages", "type-design"]
tags: ["copy", "transport-error", "typed-outcome", "refusal", "req-588", "req-586"]
req: REQ-588
created: 2026-08-23
updated: 2026-08-23
---

## What happened

The spend ceiling is detected at the egress choke point, which composes a full
refusal naming what the prompt spent and the ceiling it reached. That message
was then thrown away one layer up: `EgressError::SpendCeilingReached { message }`
converts into `TransportError`, and `TransportError` is `Copy`, so the variant
had to be a unit. The `String` had nowhere to ride.

Nothing failed. The code compiled, the tests passed, and the ceiling worked. The
user just got "provider failed unrecoverably" — because the error also, by
design, has no `failure_class`, so it fell through to the generic remote arm.

## The lesson

**Detection and explanation happen at different layers, and the boundary between
them is often `Copy`.** A `Copy` enum can carry the *fact* that something was
refused; it cannot carry the sentence about it. Deciding to compose at the point
of detection is a decision to lose the message at the first such boundary.

Compose at the surface that has the facts. In this case the daemon's turn loop
already held every one of them — the prompt's accumulator, the ceiling from the
same config the choke point read, the route's provider and model — so the
sentence cost nothing to rebuild there and travelled no further than the place
that renders it.

`PrivacyBlocked` already worked this way: it carries a `BlockDetail` enum, not
prose, and each surface words it. The pattern was there to copy and wasn't.

## How to apply

- Before giving a typed outcome a `String` payload, look at every enum it must
  cross. If any is `Copy`, the string will not survive.
- Prefer carrying **facts** (an enum, a couple of integers) and composing at the
  surface. It also makes the one-composer rule enforceable, since there is then
  exactly one place the words live.
- A typed outcome with no `failure_class` needs its **own arm** on the turn path.
  Removing it from the failure machinery is only half the job — the other half is
  saying something useful instead, and the generic arm will happily say something
  useless.
