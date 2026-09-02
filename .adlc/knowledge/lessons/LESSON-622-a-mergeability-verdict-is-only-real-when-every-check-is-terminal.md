---
id: LESSON-622
title: "A forge mergeability verdict is only real when every check in the rollup is terminal"
component: "ci/branch-protection"
domain: "ci"
stack: ["github-actions", "gh"]
concerns: ["reliability", "developer-experience"]
tags: ["mergeStateStatus", "BLOCKED", "UNSTABLE", "branch-protection", "required-checks", "evidence", "polling", "req-608"]
req: REQ-608
created: 2026-09-02
updated: 2026-09-02
---

## What Happened

REQ-608's AC-2 required the defect to be demonstrated on the forge, not
predicted locally: a PR whose only red job is the unrequired `gated` job must
be shown mergeable under the current protection. The throwaway PR (#272) was
polled every minute on `gh pr view --json mergeable,mergeStateStatus`. The
first three polls read `BLOCKED`. Had the loop stopped at "a terminal-looking
answer", the recorded evidence would have been the *opposite* of the truth —
"protection already blocks a red gated job" — and the REQ's central claim would
have been refuted by its own measurement.

`BLOCKED` on those polls meant "a *required* check has not reported yet". It
flipped to `UNSTABLE` — GitHub's term for "mergeable with a failing
non-required status" — only when the last required check landed, three minutes
in. After the protection edit the same head flipped to `BLOCKED` within
fifteen seconds, this time meaning what it looked like.

## Lesson

`mergeStateStatus` is a function of the *current* rollup, and `BLOCKED` is
overloaded: it covers "required check failed" and "required check pending"
with one word. Read the verdict only after every entry in
`statusCheckRollup` has a terminal conclusion, and record that condition
next to the value. The REST twin (`mergeable_state`) has the same overload.
A poll loop that exits on the first non-`UNKNOWN` answer is a loop that
records the pending state as a finding.

## Why It Matters

This was an evidence step: the number written into the spec is the claim the
whole REQ rests on, and a mid-flight read would have produced a confident,
verbatim, wrong one. The same overload bites automation that gates on
`mergeStateStatus` — a "wait until not BLOCKED" loop is correct, a "BLOCKED
means protection is working" assertion is not. LESSON-461 is the neighbour:
a conflicted PR has *no* rollup at all and reads as "pending" forever.

## Applies When

Reading `mergeable`/`mergeStateStatus` (or REST `mergeable_state`) as
evidence or as a gate; writing a wait loop over PR checks; recording a forge
verdict into a spec, a task file, or a PR body.
