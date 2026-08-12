---
id: LESSON-489
title: "A test that reads src/ races the mutation pass that verifies it"
component: "daemon/tests"
domain: "verification"
stack: ["rust", "daemon"]
concerns: ["verification", "test-determinism"]
tags: ["mutation-testing", "source-scanning", "flaky-tests", "false-positive"]
req: REQ-561
created: 2026-08-07
updated: 2026-08-12
---

## What happened

Two Phase-5 reviewers independently reported unreproducible multi-test failure
clusters under load. One named `call_sites` and the duty-seam tests; the other
was a `cli_e2e` failure. Neither reproduced in quiet reruns, and both were
initially suspected to be a real race in newly-detached async work.

A stress loop (270 runs under concurrent load) did not reproduce it — but
explained it. This repo has tests that scan **production source as text**:
`call_sites.rs` derives its "has a call site" marker that way, and `duty.rs`'s
AC-8 seam assertions do the same. They walk the tree, then read each file:

```rust
let text = std::fs::read_to_string(path).expect("readable source file");
```

Any writer touching `src/` between the walk and the read panics the test.
Reproduced deliberately: **11 failures in 24 runs**.

The writer, in every observed case, was **the reviewers' own mutation passes** —
apply a mutation, run the suite, observe red, revert. That is the repo's primary
verification technique (LESSON-441), and it rewrites a source file between
`cargo test` invocations by construction.

## Why it matters

The failure mode is a **false positive in the direction that wastes the most
time**: a mutation pass produces a cluster of red tests unrelated to the
mutation, in the two modules most likely to be involved in a seam change. It
looks like a real finding. Two experienced reviewers were misled by it in the
same REQ.

It is worse than an ordinary flake because it makes the verification technique
itself occasionally lie, and it fires precisely when you are relying on that
technique most.

## How to apply

If a test reads the source tree at runtime, treat concurrent modification as an
expected condition, not an impossible one — skip a file that vanished or became
unreadable mid-scan and say so, rather than `expect`-ing it. Keep the loud
failure for a file that genuinely should be there.

More generally: **before believing a mutation-pass failure cluster, check whether
any failing test reads the filesystem.** If it does, re-run it quiet before
diagnosing anything.

Filed as BUG-159 — not fixed in REQ-561, because hardening a seam-scanner's
deliberate loud-failure posture is its own decision.

## Amendment (2026-08-12, when BUG-159 was actually fixed)

Three things this lesson got slightly wrong, learned by applying it:

**A partly-applied lesson leaves the sharpest edge intact, and looks done.**
Between the filing and the fix, `production_sources` (plural) *was* hardened —
during REQ-562/REQ-570, citing this lesson — which silently fixed four of the
five tests named above. The fifth, `call_sites`' own sweep, still used a
different singular read and still raced. Anyone reading the two hardened call
sites would reasonably conclude the lesson had landed. **When a lesson names a
pattern, fix every instance in one pass or record which were left**, because a
half-applied fix is indistinguishable from a whole one at a glance.

**The race has more windows than the read.** This lesson (and BUG-159) framed it
as walk-then-*read*. The walk itself has the same shape: `path.is_dir()` is a
separate syscall from the `read_dir` that follows it, so a *directory* removed
mid-walk panics too. Whenever a "listing is stale by the time we use it" bug is
found, ask what else was derived from that listing.

**"Skip a file that vanished" is not right everywhere, and the split is
*who asked*.** A sweep may skip — nobody named that file, so its absence is a
fact about timing. A caller reading a *specific* module by name must still fail
loudly, or a renamed module passes against an empty string. BUG-159's own
suggested fix said to make the shared singular read skip; doing that literally
would have broken the by-name caller.

**Tolerating a skip needs a floor.** Every skip shrinks what a set-based sweep
sees, and those callers assert "every source has property P" — so each missed
file passes a little more easily, and a scan seeing *nothing* passes completely.
The fix pairs each tolerance with a minimum-file-count assertion, verified by
mutation: neutering the walk turns the sweeps red on the floor rather than green.
A tolerance without a floor converts a loud failure into a silent pass, which is
a worse bug than the one being fixed.

Related: [[LESSON-441]], [[LESSON-485]].

Sibling trap in the same repo, different mechanism: `cargo test -p teton --test
cli_e2e` does not rebuild `tetond`, so a targeted e2e run silently exercises the
previously-built daemon — which makes a mutation look *survived* when it was
never applied. Build the workspace before any targeted e2e mutation check.
