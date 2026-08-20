---
id: LESSON-544
title: "A test that builds the wire value by hand leaves the line that produces it unguarded — the feature can ship dead with the suite green"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["reliability", "developer-experience"]
tags: ["testing", "mutation-testing", "wire-protocol", "producer-consumer", "vacuous-test", "seam", "req-586"]
req: REQ-586
created: 2026-08-20
updated: 2026-08-20
---

## What Happened

REQ-586 put four new facts on the wire: a route's window label, whether the
redact scan bounds a route, and — added during the verify pass — whether a
declared cap was floored (twice, once on `route_decided` and once on the
`/doctor` snapshot). Each had a good test. Each test built the wire value by
hand:

```rust
let decided = RouteDecided { bound_floored: Some(true), .. };
assert_eq!(format_route(&decided), "… (bound: user cap — floored: …)");
```

That test proves the **renderer**. It says nothing about the one production
line that puts `Some(true)` there. Mutating each producer — `.with_window_label(...)`
deleted from `CarriedTurn::begin`, `.with_redact_scan(config.privacy.redact)`
→ `false`, `bound_floored: Some(self.budget.floored)` → `Some(false)`, the
snapshot's `floored_budget` in either direction — left the **entire workspace
suite green**. Four features whose daemon-side half could have shipped dead
while their client-side half was thoroughly tested.

Three of the four were found only because a reviewer ran the mutations by
hand. The fourth was found in the *confirmation* pass, in code written to fix
the first three.

## Lesson

**A fact that crosses a seam needs a test on each side and one that crosses
it.** Rendering tests may build their input by hand — that is what makes them
fast and exhaustive. But every such test implies a second one nobody writes:
*does the producer actually emit this?* Ask it explicitly, and prefer a test
that drives the real producer and reads the real consumer over two tests that
meet at a literal.

The tell is mechanical and worth grepping for: a test that constructs a wire
type with a struct literal is testing a consumer. Find the line that builds
that type in production; if no test reaches it, the feature is one typo from
silently not existing.

Mutation is the only cheap way to see this. A green suite cannot distinguish
"guarded" from "unobserved", and reading coverage does not help — the producer
line is *executed* by dozens of tests, just never *asserted* on.

## Why It Matters

This is the failure that ships. Nothing crashes, nothing turns red, the PR
reads well, and the feature is simply absent in production — discovered by a
user, months later, as "I thought this told me when that happened." It cost
this REQ four instances in one branch, and the fourth was introduced by the
fix for the first three, which is the strongest evidence that the shape is
easy to reproduce while actively thinking about it.

## Applies When

Adding any field to a protocol type, event, or config snapshot; wiring a
config flag into a subsystem (`with_*` builders are the classic site); any
change where one crate produces a value and another renders it. Also when
reviewing: if the only test near a new wire field constructs that field
literally, the producer is unguarded until proven otherwise.
