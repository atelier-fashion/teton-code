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
updated: 2026-08-07
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

Related: [[LESSON-441]], [[LESSON-485]].

Sibling trap in the same repo, different mechanism: `cargo test -p teton --test
cli_e2e` does not rebuild `tetond`, so a targeted e2e run silently exercises the
previously-built daemon — which makes a mutation look *survived* when it was
never applied. Build the workspace before any targeted e2e mutation check.
