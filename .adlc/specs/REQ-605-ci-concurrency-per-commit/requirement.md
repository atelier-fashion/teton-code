---
id: REQ-605
title: "Let every commit's CI finish, so independently-green can be checked rather than assumed"
status: draft
deployable: false
created: 2026-09-01
updated: 2026-09-01
component: "ci"
domain: "developer-experience"
stack: ["github-actions"]
concerns: ["reliability", "developer-experience"]
tags: ["ci", "concurrency", "cancel-in-progress", "req-599-followup", "macos"]
---

## Description

`.github/workflows/ci.yml:10-12` sets:

```yaml
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
```

One in-flight run per ref. Pushing commit *n+1* cancels commit *n*'s run — and
the job that is still running when that happens is almost always
`fmt · clippy · test (macos-latest)`, the slowest of the seven.

**This has already cost two REQs.** REQ-599's AC-11 required each commit in its
sequence to be independently green; it was ticked without checking, and REQ-602
found two of its seven commits had their macOS job cancelled. REQ-600 met the
same criterion only by pushing each of six commits and waiting for its CI to
finish before pushing the next — correct, and it cost hours of wall-clock on a
change that was otherwise ready.

macOS is not an incidental runner here. It is the one that caught LESSON-591's
detached-naming race, which passed 40/40 locally and on `ubuntu-latest`.

## Acceptance Criteria

- [ ] A push of several commits to one branch leaves **every** commit with a
      complete CI result, not only the tip. Demonstrated on a real multi-commit
      push, with the run ids recorded.
- [ ] The cost is measured and stated: runner-minutes for a representative
      multi-commit PR before and after, under a named counting rule.
- [ ] The change does not reintroduce what `cancel-in-progress` was for —
      a force-push or a rapid re-push should not leave obsolete runs consuming
      the queue indefinitely. Say which behaviour is being traded for which.
- [ ] `.github/workflows/*` still passes `actionlint`.

## Assumptions

- Adding `github.sha` to the concurrency group is the obvious mechanism, but it
  is not the only one (per-commit runs can also be had by gating on the event
  type). The AC is about the property, not the mechanism.

## Out of Scope

- Changing which jobs run, or their matrix.

## External Dependencies

- None.
