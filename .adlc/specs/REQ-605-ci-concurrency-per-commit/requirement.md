---
id: REQ-605
title: "Let every commit's CI finish, so independently-green can be checked rather than assumed"
status: complete
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

`.github/workflows/ci.yml` sets — lines 10-12 on `main` as this was written; the
block moves down the file once this REQ lands, so the citation is to the state
being described, not to the fixed one:

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

- [x] AC-1: Every commit **pushed as a tip** to one branch keeps a complete CI result:
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
- [x] AC-2: The cost is measured and stated: runner-minutes for a representative
      multi-commit PR before and after, under a named counting rule.
- [x] AC-3: The change does not reintroduce what `cancel-in-progress` was for —
      a force-push or a rapid re-push should not leave obsolete runs consuming
      the queue indefinitely. Say which behaviour is being traded for which.
- [x] AC-4: `.github/workflows/*` still passes `actionlint`.

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

## Verification (TASK-316)

### AC-2 — the cost, measured, with the counting rule beside the count

Two rules, both stated so the figures can be checked:

- **Rule R — raw job-minutes.** Per job, `ceil(wall-clock seconds / 60)`, summed
  over the run's jobs, from each job's `started_at`/`completed_at` in the Actions
  API. Vendor-neutral.
- **Rule W — weighted job-minutes.** Rule R with GitHub's per-OS multipliers
  (`ubuntu-latest` ×1, `macos-latest` ×10). **This repo is public, so GitHub
  bills nothing for any of it** — W is a resource-intensity proxy and an estimate
  for the same workload on a private repo, not an invoice.

Both count all **seven** job runs. The `timing` endpoint's `billable` field is
*not* the source: it returns `total_ms: 0` for every job on this public repo.

Measured over six real runs on this workflow — three cancelled under the old
config (REQ-599's branch), three completed (REQ-600's branch, which avoided
cancellation only by waiting):

| run | outcome | job-sec | Rule R | Rule W |
|---|---|---:|---:|---:|
| 33338739669 | cancelled | 482 | 11 | 47 |
| 33338614984 | cancelled | 466 | 11 | 47 |
| 33328885941 | cancelled | 535 | 13 | 67 |
| 33444782077 | success | 486 | 12 | 57 |
| 33442340561 | success | 515 | 12 | 57 |
| 33441618721 | success | 499 | 12 | 57 |

Mean cancelled **11.7 R / 53.7 W**; mean completed **12.0 R / 57.0 W**.

For a representative multi-commit PR — **7 commits pushed as tips**, REQ-599's
actual shape — there are two honest "before" baselines, because the two prior
REQs used different disciplines:

| | Rule R | Rule W | commits with complete macOS evidence |
|---|---:|---:|---:|
| **Before-A** — push freely (REQ-599): 6 cancelled + 1 complete | 82.2 | 379 | 1 of 7 |
| **Before-B** — wait for each (REQ-600): 7 complete | 84.0 | 399 | 7 of 7 |
| **After** — this change: 7 complete | 84.0 | 399 | 7 of 7 |

**Against Before-A the change costs +1.8 R (+2%) / +20 W (+5%)** and buys six
macOS results that did not previously exist. **Against Before-B it costs exactly
nothing** and hands back the wall-clock REQ-600 paid: 10 runs across a 2h01m span
for 34m of actual run time.

The reason the premium is so small is the finding underneath it: **a cancelled
run costs almost what a completed one costs.** In all three cancelled runs the
kill lands inside the `Tests` step, *after* `Set up job`, `Checkout`, `Install
pinned Rust toolchain`, `Cache cargo build`, `Formatting` and `Clippy` have every
one succeeded. Cancelling returns about ten seconds of macOS time (cancelled
macOS `check`: 175s, 162s, 221s; completed: 184s, 226s, 179s) and destroys the
entire macOS result. It also **skips `Post Cache cargo build`**, so the run does
not save its cargo cache and the next run starts colder.

**Corroborated on this branch — with a caveat that keeps it out of the figures.**
R1 below (`33450647558`) is a fourth cancelled run, and it shows the identical
step pattern: kill inside `Tests`, everything before it green, `Post Cache cargo
build` skipped — this time on *both* `check` legs, ubuntu as well as macOS. So
the pattern is now 4 for 4. But R1 cost **less** than the three historical runs
(10 R / 46 W against 11–13 R / 47–67 W), because the next push came 63 seconds
after it started — far faster than anyone actually authoring a commit. It is a
demonstration of the mechanism, not a representative cost, so the before figures
above stay on the three historical runs, which were cancelled at real authoring
pace. Counting a deliberately-hurried cancellation as typical would understate
the cost of the old behaviour and flatter this change.

### AC-3 — which behaviour is traded for which

**Given up:** automatic cancellation of a *superseded* commit's run. A force-push
replacing X with Y no longer kills X's run; it finishes on a tree nobody merges.

**Gained:** every commit pushed as a tip keeps a complete result on every runner,
`macos-latest` included.

**Why not both:** `concurrency` keys on a *string* and has no notion of ancestry,
so it cannot distinguish "n+1 builds on n" from "Y replaces X". AC-1's property
and `cancel-in-progress`'s purpose are one mechanism pointed in opposite
directions.

**Why the waste is bounded, not indefinite** — AC-3's explicit worry: an
abandoned run is not orphaned. It runs its jobs to their own conclusions and
stops; nothing re-queues it, and GitHub's job timeout caps the worst case.
Nothing accumulates, because distinct-SHA groups never queue behind one another.
The failure mode AC-3 names is structurally unreachable here — and *reachable*
under `cancel-in-progress: false`, which is why that alternative was rejected:
`false` does not mean "run both", it means **queue**.

Residual risk, named and deliberately not quantified: concurrent runs still
consume **account-level** runner concurrency, a separate limit from workflow
`concurrency`, with a tighter cap on macOS. Nothing in this REQ measured it, so
no figure is quoted. It delays a run; it does not destroy its result.

### AC-4 — actionlint

`actionlint 1.7.12` — the version pinned in `ci.yml`'s `tooling` job
(`ACTIONLINT_VERSION`) — shellcheck-backed, run exactly as CI runs it:

```
actionlint -color -shellcheck shellcheck .github/workflows/*.yml   # rc=0
```

Clean on the baseline tree before any edit, and clean after both workflow
commits. The `tooling` job re-runs it on every commit in this PR.

### AC-1 — the property, observed on a real push sequence

Five commits were pushed to `feat/REQ-605-ci-concurrency-per-commit` one at a
time, each while the previous run was still in flight. In-flight status was
**verified** with `gh run view <id> --json status` immediately before each push,
not inferred from timing — a push made after the previous run had already
finished proves nothing about cancellation, and would be a vacuous observation.

The branch carries **both** configurations, so every run is labelled with the one
it actually ran under. `pull_request` workflows run from the PR's merge ref, so
the config in force for a run is the one on the branch at that commit: commits
before `5b5af36` ran under the old ref-only group, `5b5af36` and later under the
new per-commit group.

| # | run id | commit | config | start–end (UTC) | conclusion |
|---|---|---|---|---|---|
| R1 | 33450647558 | `3c664a8` | old | 23:25:54–23:27:21 | **cancelled** |
| R2 | 33450729742 | `ebe6841` | old | 23:27:02–23:30:37 | success |
| R3 | 33450814843 | `5b5af36` | **new** | 23:28:15–23:31:53 | success |
| R4 | 33450882606 | `8cbdb7c` | **new** | 23:29:20–23:32:32 | success |
| R5 | 33451033687 | `a165c72` | **new** | 23:31:27–23:34:11 | success |

Every consecutive pair overlapped, so every observation is live:

| pair | configs | overlap | earlier run's conclusion | counts as |
|---|---|---:|---|---|
| R1 → R2 | old → old | 19s | **cancelled** | **before** |
| R2 → R3 | old → **new** | 142s | success | mixed — **excluded** |
| R3 → R4 | new → new | 153s | success | **after #1** |
| R4 → R5 | new → new | 65s | success | **after #2** |

**Before.** R1 was in flight — with `fmt · clippy · test (macos-latest)` running,
checked before the push — when `ebe6841` was pushed, and R1 ended `cancelled`.
The defect reproduces on demand. It took **both** `check` legs this time, ubuntu
as well as macOS.

**Mixed, and excluded rather than claimed.** R2 → R3 is *not* evidence for either
half. R2's group was the old ref-only string and R3's the new per-commit one, so
those two could not have collided whatever the setting. Recorded because the
sequence contains it, counted as nothing.

**After.** R3 and R4 each stayed alive through a subsequent push and reached
`success`. R3 is the stronger case: it was still running when **both** R4
(23:29:20) and R5 (23:31:27) started, and finished at 23:31:53 regardless. At
23:31:27–23:31:53 three new-config runs were in flight at once — a state the old
shared group made unreachable.

**AC-1 is MET.** Not from a single-commit push, and not from the absence of a
cancellation: from four live pairs, one of which reproduces the old behaviour and
two of which show the new.

Only R5's own successor is unrecorded — the run for the commit that writes this
table, which cannot record its own conclusion. Its id is noted in the PR and its
result is confirmed green before merge.

**Postscript, added at wrapup once the branch was complete.** The table above is
conservative. The finished branch ran **8 commits / 8 runs**, with **6**
overlapping consecutive pairs, of which **4** are after-observations under the
new config (`5b5af36`→`8cbdb7c` 153s, `8cbdb7c`→`a165c72` 65s,
`593dbab`→`e6e28ff` 18s, `e6e28ff`→`d9bd165` 108s). Every commit from `5b5af36`
onward is independently green on all seven job runs. The one `cancelled`
conclusion on the branch is R1, produced deliberately as the before-observation.
The extra pairs are recorded here rather than in the table above because they
accrued after AC-1 was ticked, and they strengthen the verdict without changing
it.
