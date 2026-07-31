---
id: LESSON-456
title: "A `_`-discarded error is a silent downgrade — the daemon knew exactly why, and told the user something else"
component: "daemon/router"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "observability", "reliability"]
tags: ["error-classification", "discarded-reason", "first-run", "misattribution", "fallback-identifier", "dogfood"]
req: BUG-146
created: 2026-07-31
updated: 2026-07-31
---

## What Happened

The very first prompt after a `brew install` returned *"local engine could not
serve the turn"* — while the local engine was loading correctly and about to
open. The daemon had published the true, actionable state on the lifecycle
stream one line earlier ("loading and benchmarking them now — the local tier
opens when that completes"). Three independently reasonable decisions turned
that into a wrong answer:

1. `build_router` substitutes the literal id `"local"` when no providers are
   registered, so a fresh install routes every turn to a provider that exists
   nowhere — a fallback *identifier* standing in for "none".
2. The unresolvable provider was wrapped as `HarnessError::Engine`, typing a
   configuration-and-timing condition as an engine fault.
3. The handler matched `Err(HarnessError::Engine(_))` and substituted a fixed
   sentence, discarding the inner reason — which by then was
   `"no provider for this turn"`, i.e. not about the engine at all.

Each layer is defensible alone. Composed, they produced a message that blamed
the one component behaving correctly, at the exact moment a new user forms
their first impression.

## Lesson

**`_` in an error match arm is a decision to discard evidence.** Treat every
`Err(SomeError(_)) => fixed_string` as a bug until proven otherwise: it makes
all causes in that class indistinguishable, and the moment anything *else* gets
wrapped in that variant, the fixed string becomes actively false rather than
merely vague. If a variant's payload isn't worth surfacing, that is a sign the
variant is too broad — split it.

Two supporting rules this incident earned:

- **A fallback identifier is not "none".** Defaulting a missing id to a
  plausible literal (`"local"`, `"default"`, `"unknown"`) converts an absence
  the type system could have carried into a lookup that fails later, further
  from the cause, with the wrong name attached. Keep the `Option`.
- **When a component already classifies a state for one surface, reuse that
  classifier for every surface.** The lifecycle stream and the turn-failure
  path were describing the same machine at the same instant and disagreed. The
  fix routes both through one precedence, so they cannot drift.

## Why It Matters

Misattribution costs more than silence: it sends the reader to debug a healthy
subsystem, and it is indistinguishable from the genuine failure of that
subsystem — so the real event, when it comes, reads as noise. This one sat on
the first prompt of the first run, the least forgiving position in a product.
No test caught it because every test either configures a provider or waits for
the tier; the gap was the *unconfigured, mid-load* moment that only a real
install produces.

## Applies When

- Writing or reviewing any `match` on an error enum — check each `_` binding
  and ask what the discarded value would have said.
- A function returns a broad error variant for causes that are not the same
  kind of thing (see [[LESSON-442]] — exit codes that collide the same way).
- Defaulting a missing identifier to a literal rather than propagating absence.
- Two code paths describe the same runtime state to different audiences (an
  event stream and an RPC error; a log line and a UI toast) — give them one
  classifier, not two (see also [[LESSON-447]]: a degraded path must preserve
  the invariant *and* be visible).
