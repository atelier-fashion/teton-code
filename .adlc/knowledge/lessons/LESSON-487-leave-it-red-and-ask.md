---
id: LESSON-487
title: "When the spec and the tests disagree, leave it red and ask"
component: "daemon/harness"
domain: "process"
stack: ["rust", "daemon"]
concerns: ["verification", "architecture", "agent-delegation"]
tags: ["red-tests", "architecture-correction", "delegation", "event-emission"]
req: REQ-561
created: 2026-08-07
updated: 2026-08-07
---

## What happened

REQ-561's architecture specified that each duty emit `route_decided` at
**resolution**. An implementer built exactly that. Three tests went red,
including a REQ-544 privacy test whose premise is that a category-less
`route_decided` naming `local` *is* the session taint pin.

The implementer did not edit those three tests. It reported the failure, named
the contradiction, and offered the alternative — leaving the branch red.

The architecture was wrong. Duties resolve eagerly once per turn but usually
never perform, so resolution-time emission announces model calls that never
happen — and with five duties wired that is five spurious events per turn. The
fix was to emit on **perform**, recorded as ADR-8.

This was not a one-off. Across REQ-561 **four** specified constraints turned out
to be the thing that was wrong: this one, a `SessionTitled` payload shape serde
cannot express, a call site that could not exist because `Tool::run` is
synchronous, and an AC that named the wrong test row as its discriminator. Every
one surfaced the same way — implemented as written, left red, reported.

## Why it matters

Editing a test to accommodate a behaviour change converts an architectural
mistake into a silent, permanent one. Here it would have retired a privacy
assertion.

The rate is the point: a spec written by someone who has not yet read the code is
wrong often enough that "the spec is right, fix the test" is a bad default. The
implementer is the first person to hold both, so it is the first point where the
disagreement is visible at all.

## How to apply

Instruct implementers explicitly: **if a spec constraint appears wrong, say so
and leave it red rather than editing a test to fit.** Naming the prior rate
("this has happened three times in this REQ and every time the constraint was
what was wrong") measurably raises the chance they will.

Then pair the correction with a **negative** test. `a_performed_digest_announces_
its_route` stayed green under the very mutation it existed to catch; only
`a_digest_that_never_runs_announces_nothing` went red. A positive test that
passes under both the correct and incorrect design pins neither (LESSON-485).

Related: [[LESSON-441]], [[LESSON-484]], [[LESSON-485]].
