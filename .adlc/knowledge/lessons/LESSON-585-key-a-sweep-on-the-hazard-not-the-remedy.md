---
id: LESSON-585
title: "A sweep keyed on the remedy's shape cannot see an omission of the remedy"
component: "daemon/harness"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["security", "testing"]
tags: ["source-scan", "region-check", "sweep", "vacuity-floor", "subprocess"]
req: REQ-596
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-596's AC-8 asked for a source-level check that a spawned child's environment
is built only by the one shared composer. The check scanned for `.envs(` and
asserted each call's argument came from `compose_child_env`.

It could not see the worst case. A `Command` that calls **no** env method at all
inherits the daemon's entire environment — every configured credential included —
and has no `.envs(` for the scan to find. The guard whose whole job was to notice
a second way in was structurally blind to the most dangerous one, and would have
passed in silence.

The fix was a second sweep keyed on the **hazard**: every process spawn must call
`env_clear()`. Identifying "a process spawn" then had its own trap — the first
draft keyed on the imported type name and silently matched only 1 of the 2 real
spawns, because one file writes `Command::new("sh")` after a brace-import.
**The vacuity floor caught it** — the `spawns >= 2` assertion failed loudly
instead of the sweep passing over a single site.

## Lesson

Key a sweep on the **hazard** you are guarding against, not on the shape of the
remedy you expect to find. A scan for "the remedy, applied correctly" can only
ever grade the sites that already tried; the site that never tried is invisible
to it.

And put a floor on every sweep — an assertion that it saw the number of sites you
know exist. A sweep's failure mode is seeing *less*, and every site it misses
makes it pass more easily.

## Why It Matters

An omission is the commonest way a security invariant breaks, and it is the case
a remedy-shaped scan is guaranteed to miss. Worse, the scan's existence creates
confidence: a reviewer sees "AC-8: enforced by a source check" and stops looking.
Here the floor turned a silent 50%-blind sweep into a loud failure within one
test run.

## Applies When

Writing any tree-wide source scan, region check, or lint that enforces "every X
must do Y"; reviewing a sweep-based acceptance criterion; deciding what a
guard's own test should assert about its coverage.
