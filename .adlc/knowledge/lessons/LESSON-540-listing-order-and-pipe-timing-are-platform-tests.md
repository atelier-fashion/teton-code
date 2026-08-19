---
id: LESSON-540
title: "A fixture that names \"the first listed entry\" or writes stdin after spawn is a platform test in disguise — CI's other OS is where it fails"
component: "daemon/harness"
domain: "testing"
stack: ["rust", "ci", "github-actions"]
concerns: ["reliability", "developer-experience"]
tags: ["readdir-order", "apfs", "ext4", "epipe", "broken-pipe", "flaky-test", "fixture", "ci-matrix", "req-583"]
req: REQ-583
created: 2026-08-19
updated: 2026-08-19
---

## What Happened

Two REQ-583 tests were green on every local run (macOS) and on the macOS CI
leg, and failed on ubuntu — twice, costing two CI round trips after a
fully-reviewed tip. (1) The zero-wall-budget walker tests planted `top.rs` and
`sub/` in one root and asserted the single entry handed over was `top.rs`; APFS
lists in hash order and happened to give `top.rs`, ext4 gave `sub/`. (2) The
new daemon-less `--cwd /nope` test wrote `hello\n` to the CLI's stdin after
spawn; the CLI now exits before reading anything — the very behaviour under
test — so on the faster Linux runner the write hit `EPIPE`. The shared
`cli_e2e` harness had the same `.expect("write teton stdin")` shape and had only
been passing by timing.

## Lesson

Any assertion about *which* directory entry comes first, or any write to a
child's stdin that the child might never read, is an assertion about the
filesystem or the scheduler, not about the code. Make fixtures
order-independent (a single entry when "the one entry" matters; sort, or accept
either, when it does not) and make stdin writes tolerate `BrokenPipe` when an
early exit is a legitimate outcome — assert the kind, keep every other error
fatal. And when a fix moves a check boundary (here: the wall clock from "before
each descent" to "after every entry"), grep every test that pinned the old
boundary before pushing.

## Why It Matters

A PR that is locally green, reviewer-approved and fixed is still two CI
iterations from merge when the matrix's other OS disagrees about listing order
or pipe timing; each iteration is a full build. The fixes are one-line; the
cost is in finding them after the fact.

## Applies When

Walker/enumeration tests with small fixtures; any test that spawns the CLI with
piped stdin; any change that moves where a budget or clock is checked; any
repo whose CI runs macOS and Linux (APFS hash order vs ext4/tmpfs order).
