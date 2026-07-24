---
id: LESSON-443
title: "A guard keyed on a feature's absence disables itself when the feature lands"
component: "daemon/consent"
domain: "security"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["guard-condition", "latent-defect", "consent-gate", "coupling", "time-bomb"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

The consent gate that stops the daemon downloading model weights without the
user's answer was initialised as:

```rust
let local_gated = AtomicBool::new(engine.is_none() && consent.consent_required());
```

and the flow that performs the check was likewise spawned only when
`engine.is_none()`. Reusing "no engine is loaded" as a proxy for "we are in the
test/scripted configuration" reads harmlessly while no engine exists — and every
test passes. But the moment a real inference engine is wired (the explicitly
tracked next step), `engine.is_some()` makes `local_gated` unconditionally
`false` **and** the consent flow never spawns at all, so the deep verification
never runs, and the engine loads the weights before any gate. The gate would
have silently become a no-op in the same commit that first made it matter —
with no test failing, because no test can exercise a feature that does not
exist yet.

Found only because a security re-verify was asked the specific question "is
there a second gate-evaluation point, and can it be reached first?".

## Lesson

Never express a guard's condition in terms of the absence of a feature you
intend to build. Name the real condition: if the exemption is "this is a
scripted test engine", carry an explicit `scripted: bool`, not `engine.is_none()`.
A condition that is true only because a subsystem is unfinished is a time bomb
whose fuse is your own roadmap, and it is invisible to tests, review-by-diff,
and coverage alike — the failure is in code that does not exist yet.

Generalisation: whenever a security control's predicate mentions something
unrelated to the thing being controlled, ask what happens when that unrelated
thing changes.

## Applies When

Writing any gate/guard/feature-flag whose condition references a subsystem's
existence, emptiness, or absence (`is_none()`, `is_empty()`, `cfg!(not(...))`,
"no provider configured", "no engine loaded"); reviewing a control that passes
today for a reason incidental to its purpose; planning work that will make a
currently-false condition true.
