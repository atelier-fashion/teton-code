---
id: LESSON-600
title: "A cancelled run costs what a finished one costs — measure where the kill lands before trading a result for resources"
component: "ci"
domain: "developer-experience"
stack: ["github-actions"]
concerns: ["reliability", "performance", "developer-experience"]
tags: ["cancel-in-progress", "concurrency", "measurement", "false-economy", "macos", "cache"]
req: REQ-605
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

`ci.yml` carried the default-looking `concurrency: {group: ci-${{ github.ref }},
cancel-in-progress: true}`. Pushing commit *n+1* killed commit *n*'s run, and the
job still running was almost always `fmt · clippy · test (macos-latest)` — the
slowest of seven, and the runner that caught LESSON-591's race. It cost REQ-599
two commits with no macOS evidence, and cost REQ-600 two hours of wall-clock
avoiding it by hand.

The setting's justification is that cancelling saves resources. **Measured, it
saves almost nothing.** Over six real runs — three cancelled, three completed —
under a stated counting rule (per job `ceil(sec/60)`, summed):

| | job-seconds | job-minutes |
|---|---:|---:|
| cancelled (mean of 3) | 494 | 11.7 |
| completed (mean of 3) | 500 | 12.0 |

The reason is *where* the kill lands. In 4 of 4 cancelled runs the cancellation
hit the `Tests` step — **after** `Set up job`, `Checkout`, `Install pinned Rust
toolchain`, `Cache cargo build`, `Formatting` and `Clippy` had every one
succeeded. The run had already paid for the toolchain install, the cache restore
and a full clippy compile. Killing it returned about ten seconds of macOS time
and destroyed the entire macOS result.

It was worse than break-even: `Post Cache cargo build` is **skipped** on
cancellation, so the run never saved its cargo cache and the *next* run started
colder.

## Lesson

**A cancellation only saves the work that had not happened yet.** Where the kill
lands decides the saving, and on a pipeline whose expensive setup precedes its
verdict, the kill lands after almost all the cost and before all of the value.
That is the worst possible split: full price, no evidence.

Before trading a result for resources, measure three things rather than assuming
them: which step the cancellation actually interrupts, what fraction of the run's
cost precedes that step, and what teardown the cancellation *skips* (cache saves,
artifact uploads) that makes subsequent runs more expensive.

The general form: **"cancel the obsolete work" is a saving only when obsolescence
is detected early.** Detect it late and you have bought nothing and sold the
answer.

## Why It Matters

The setting reads as free hygiene, so nobody measures it, and the cost lands on
whichever runner is slowest — which is systematically the runner most likely to
catch a platform-specific defect. Here it was `macos-latest`, the only runner
that has ever caught an ordering bug in this repo. Two REQs paid for it: REQ-599
shipped with a criterion recorded NOT MET, REQ-600 paid two hours of serialized
wall-clock to work around it, and REQ-605 was needed to remove it.

Restated as the number that should have been checked at the start: keeping every
commit's result costs **+2%** of runner minutes against pushing freely, and
**exactly nothing** against the wait-for-each discipline that was actually being
used.

## Applies When

Configuring `cancel-in-progress`, a merge queue, or any "supersede the older
job" mechanism; reviewing a setting justified by resource saving where the saving
has never been measured; any pipeline whose expensive setup (toolchain install,
dependency compile, cache restore) precedes the step that produces the verdict.

Also whenever a cancellation path skips a teardown step — a skipped cache save or
artifact upload makes the *next* run more expensive, so the true cost is not
bounded by the cancelled run alone.
