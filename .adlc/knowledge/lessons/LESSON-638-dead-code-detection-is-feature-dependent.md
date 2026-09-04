---
id: LESSON-638
title: "Only the gated build can tell you the gated code is never called"
component: "daemon/runtime"
domain: "testing"
stack: ["rust"]
concerns: ["reliability", "correctness"]
tags: ["feature-flags", "dead-code", "all-features", "ci", "unwired-code"]
req: REQ-616
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-616 added two events, `local_window_decided` and `local_window_refused`. It
added the payload structs, the `Event` variants, the `window_event` composer, the
client render arms, and tests for the sentence each one produces.

Nothing ever published one. BR-3 and BR-4's entire reporting half did not exist,
and every check in the repository was green.

Nothing local could have caught it. Under default features `window_event` is dead
code — the loader that would call it is behind `--features llama` — and I had
annotated it `#[cfg_attr(not(feature = "llama"), allow(dead_code))]` **on
purpose**, with a comment explaining that the decision logic deliberately lives
outside the gate so CI can test it. That annotation is correct, and it is also
precisely what silenced the one signal that would have said "you never wired this
up". Only `--all-features` makes the `llama` arm live; only then does `dead_code`
fire; only then is it an error under `-D warnings`.

CI's `feature-gated targets compile (all features)` leg found it in 23 seconds.

## Lesson

**`dead_code` is not a property of a function; it is a property of a function
under a feature set.** When you move logic out of a feature gate so it can be
tested, you also move it out of the reachability analysis that would notice it
has no caller — and if you then silence the warning on the ungated path (which
you must, or the default build fails), the only remaining detector is a build
with the gate on.

So: a repo that gates any code needs an `--all-targets --all-features` clippy leg
with warnings denied, and that leg needs to be a **required** check. Compiling is
cheap where running is not — the gated tests here need real weights to run, but
only cmake to build.

And when you write `allow(dead_code)`, scope it to the configuration where the
code is genuinely unreachable (`cfg_attr(not(feature = …), …)`), never blanket.
The blanket form would have silenced the all-features leg too, and this would
have shipped.

## Why It Matters

An unwired feature is worse than an absent one. Everything around it — types,
tests, docs, render arms, a PR body describing the behaviour — asserts that it
works, so the next reader has no reason to check. That is LESSON-510 and BUG-167's
shape (existence checked, freshness not), and this repo's gated-compile job exists
because of exactly that history.

The cost of the job is about 20 seconds of macOS runner time per PR. The cost of
the class of defect it catches is a feature that appears to ship and does not.

## Applies When

Any repo with non-default features; moving logic out of a gate to make it
testable; writing `allow(dead_code)`; deciding whether an `--all-features` CI leg
is worth its runtime; reviewing a change that adds an event, a hook, or a
callback (ask what calls it).
