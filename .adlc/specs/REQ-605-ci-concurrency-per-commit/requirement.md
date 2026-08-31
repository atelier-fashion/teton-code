---
id: REQ-605
title: "Let every commit's CI finish, so independently-green can be checked rather than assumed"
status: approved
deployable: false
created: 2026-09-01
updated: 2026-08-31
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
same criterion only by pushing one commit at a time and waiting for each run to
finish before pushing the next. The counting rule is *CI runs triggered on the
branch*, not commits, because one commit on that branch was pushed together with
another and never built alone: PR #249 shows **10 runs over 11 commits**, every
one `success` and every one strictly non-overlapping — 34m of run time spread
across a 2h01m span. Correct, and paid for in wall-clock on a change that was
otherwise ready.

macOS is not an incidental runner here. It is the one that caught LESSON-591's
detached-naming race, which passed 40/40 locally and on `ubuntu-latest`.

## Acceptance Criteria

- [ ] AC-1: Every commit **pushed as a tip** to one branch keeps a complete CI result:
      pushing commit *n+1* does not cancel commit *n*'s in-flight run, and both
      reach a terminal conclusion. Demonstrated on a real sequence of pushes,
      with the run ids and their conclusions recorded.
  - **Why "pushed as a tip" and not "every commit".** `ci.yml:3-7` triggers on
    `pull_request: branches: [main]` and `push: branches: [main]` only. On a
    feature branch the sole trigger is `pull_request`, and a **batched** push of
    several commits fires one `synchronize` event carrying the tip SHA — the
    intermediate commits never start a run at all, under any `concurrency`
    setting. Cancellation is a different defect from never-triggered, and this
    REQ closes the first. The second is named in Out of Scope.
- [ ] AC-2: The cost is measured and stated: runner-minutes for a representative
      multi-commit PR before and after, under a named counting rule.
- [ ] AC-3: The change does not reintroduce what `cancel-in-progress` was for —
      a force-push or a rapid re-push should not leave obsolete runs consuming
      the queue indefinitely. Say which behaviour is being traded for which.
- [ ] AC-4: `.github/workflows/*` still passes `actionlint`.

## Assumptions

- Sequential pushes are how the multi-commit REQs actually land. REQ-600's AC-7
  records seven commits pushed and waited on one at a time — its count as of
  that criterion's writing, before the branch grew to the ten runs the
  Description counts. AC-1's property is what makes that discipline unnecessary
  rather than merely tolerable.

## Open Questions

- [x] OQ-1 **(settled — ADR-1)**: the group becomes
      `ci-${{ github.ref }}-${{ github.sha }}` and `cancel-in-progress` stays
      `true`, still collapsing duplicate runs of the *same* commit.
  - **Dropping `cancel-in-progress`** was rejected as the worst of the three, not
    the simplest: `false` does not mean "run both", it means **queue**. Every
    commit would still serialize behind its predecessor — the exact wall-clock
    cost REQ-600 paid by hand. It is also the only candidate that creates the
    queue AC-3 worries about.
  - **Gating on event type** was rejected on evidence: `main`'s push runs have
    the same defect (run `33445087015` on `main` was cancelled after 219s), and
    AC-1's "one branch" includes `main`.
  - The trade AC-3 asks for is stated in ADR-2, and ADR-3 measures why it
    is cheap: a cancelled run costs almost exactly what a completed one costs,
    because the kill lands inside `Tests` after the toolchain install, cache
    restore and clippy have all been paid for.

## Out of Scope

- Changing which jobs run, or their matrix.
- **Giving intermediate commits of a batched push their own run.** That needs a
  new trigger (a `push:` on feature branches, or a merge queue), not a
  concurrency change, and it doubles the runs on every PR branch — a cost that
  deserves its own decision rather than riding along here. REQ-599 and REQ-600
  both pushed their steps one at a time; the property they needed is the one
  AC-1 states.

## External Dependencies

- None.
