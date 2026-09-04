---
id: LESSON-631
title: "Per-process temp names collide in a multi-session daemon"
component: "tetond/repo_context"
domain: "correctness"
stack: ["rust"]
concerns: ["concurrency", "data-integrity"]
tags: ["temp-file", "rename", "o_excl", "pid", "unlink"]
req: REQ-613
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

The `--force` writer staged `TETON.md` at `TETON.md.<pid>.tmp` and, on finding that name taken, reasoned "only this process could have written it, so it is a dead run's leftover" and unlinked it. The daemon is one process serving many sessions, and `session/context` runs on a spawned task, so two concurrent `--force` runs at one root drew the same name: the second unlinked the first's half-written file, and the first then renamed the second's partial bytes over the user's notes.

## Lesson

In a long-lived multi-session process the pid is not a lock. Scratch names need a per-call serial beside the pid, a collision is retried with a fresh name, and a writer must never unlink a path it did not just create with `O_EXCL`. The cost is that a killed run can leak one inert scratch file; that is the correct trade against publishing a truncated file.

## Why It Matters

The whole point of write-then-rename is that the target holds the old bytes or the new ones and never a mixture. A collision handler that unlinks reopens exactly that window.

## Applies When

Any temp-then-rename write in `tetond`, and any "this name can only be ours" reasoning in a process that serves more than one client.
