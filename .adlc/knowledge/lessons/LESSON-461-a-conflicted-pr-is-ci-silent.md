---
id: LESSON-461
title: "A conflicted PR is CI-silent — zero checks is a symptom, not a pending state"
component: "distribution/release"
domain: "adlc"
stack: ["github-actions", "ci", "git"]
concerns: ["reliability", "process"]
tags: ["merge-conflict", "pull-request-runs", "merge-ref", "parallel-sessions", "ci-silence"]
req: REQ-550
created: 2026-08-01
updated: 2026-08-01
---

## What Happened

Mid-pipeline, REQ-550's PR stopped producing CI runs entirely: `gh pr checks`
reported "no checks reported", retriggering with an empty commit changed
nothing, and the checks watch exited cleanly having watched nothing. The
cause was invisible from the checks view: a parallel PR (#14) had merged to
main touching the same workflow file, putting the REQ-550 PR into
`CONFLICTING`. GitHub runs `pull_request` workflows against the synthetic
merge ref (`refs/pull/N/merge`), and a conflicted PR has no merge ref — so
runs are not created at all. No failure, no pending, no signal; just
absence. The diagnosis came from `gh run list` (other branches' runs were
flowing normally) plus `gh pr view --json mergeable` (`CONFLICTING DIRTY`).

## Lesson

When a PR shows zero checks where checks are expected, check `mergeable`
BEFORE retriggering or blaming CI: `gh pr view <n> --json
mergeable,mergeStateStatus`. `CONFLICTING` means no merge ref, which means
no `pull_request` runs will ever be created for that head — the fix is the
rebase/merge, not a retrigger. Corollary for pipelines that wait on checks:
a wait loop that treats "no checks" as "not started yet" will wait forever
on a conflicted PR; poll mergeability alongside checks. And in
multi-session repos this is the *expected* signal that main moved under you
(see [[LESSON-449]] for how to compose the resulting merge).

## Why It Matters

CI silence reads as either "green enough" or "still queueing" depending on
the reader's optimism; both readings burn time (an empty-commit retrigger
round-trip here) and the second can hang an autonomous pipeline
indefinitely. The mergeable field turns an invisible state into a one-call
diagnosis.

## Applies When

- Any wait-for-checks step in an autonomous pipeline (poll
  `mergeable`/`mergeStateStatus` with the checks).
- Diagnosing "CI didn't run" on a PR — especially in a repo with parallel
  sessions or human-and-agent concurrent work.
- Writing status tooling like `/manifest`: a CONFLICTING PR should surface
  as blocked-on-rebase, not as checks-pending.
