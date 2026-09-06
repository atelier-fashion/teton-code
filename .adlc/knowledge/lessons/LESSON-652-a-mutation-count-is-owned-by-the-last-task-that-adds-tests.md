---
id: LESSON-652
title: "A mutation count is stale the moment a later task adds tests — the last task to add tests owns the count"
component: "adlc/spec"
domain: "testing"
stack: ["rust"]
concerns: ["reliability"]
tags: ["mutation-testing", "coverage", "task-graph", "verification-record", "req-619"]
req: REQ-619
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-619's TASK-401 restored a retired rule as a mutation and recorded the
result as "6 red across the workspace", naming the six. TASK-403 then added
thirteen end-to-end tests to the same rules. Re-run after both landed, the
same mutation reddened 24; the identity mutation recorded as 3 reddened 15;
every one of ten records was wrong. None was wrong when written — each was
measured before the suite it now describes existed.

## Lesson

A record that names a workspace-wide count belongs to the **last** task that
adds tests to that rule, not the task that wrote the code. When a later task
widens coverage, re-run the recorded mutations and rewrite the counts — after
touching the mutated file, so cargo really rebuilds (a stale test binary
served a mutated build once in this run). LESSON-598 says re-run after a
change to program structure; a change to the test population invalidates a
count exactly the same way.

## Why It Matters

The count is the finding (conventions.md). A reader trusting the stale "6"
concludes the e2e suite does not cover the identity rule and either
over-invests or, worse, believes a gap that no longer exists. And the large
re-measured numbers turned out to be one leak cascading through a
process-global capture assertion — which the record has to say, or a future
reader counts one finding twenty times.

## Applies When

- A REQ splits into tasks and a later task adds integration or end-to-end
  tests to rules an earlier task already recorded mutations for.
- Any verify pass that re-runs a recorded mutation: rewrite the record with
  the observed figure and name what reddened, never just the number.
