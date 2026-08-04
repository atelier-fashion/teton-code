---
id: LESSON-480
title: "A session spawned from a feature worktree ships the feature branch as its baseline — a 'scoped fix' PR can carry a snapshot of in-flight work"
component: "adlc/process"
domain: "adlc"
stack: ["git", "adlc"]
concerns: ["process", "reliability"]
tags: ["worktree", "spawned-session", "background-task", "snapshot-pr", "rebase-onto", "parallel-sessions", "squash-merge"]
req: REQ-555
created: 2026-08-04
updated: 2026-08-04
---

## What Happened

During REQ-555's verify phase, a review agent found a pre-existing daemon
defect (event/response interleaving) and filed a background-task chip. The
user started that chip as a separate session — which began from the REQ-555
feature worktree's state, mid-pipeline, at commit `e3f7fe8`. The fix session
did its job (PR #42: order a client's events ahead of the response), but its
squash commit carried **the entire REQ-555 branch snapshot** — 2,300+ lines
of slash-command work through TASK-037 — alongside the ~370-line daemon fix,
because the feature branch state was its baseline.

When PR #42 merged, `main` contained a *prefix* of the still-open REQ-555
branch. GitHub marked the feature PR `CONFLICTING`, and — the LESSON-461
shape — its CI went silent (zero checks on a conflicted PR). The feature
branch meanwhile had two verify-fix commits `main`'s snapshot lacked.

Recovery was clean because the snapshot was an exact ancestor state:
`git rebase --onto origin/main e3f7fe8` replayed only the post-snapshot
delta (zero conflict hunks), and the composed suite — including the fix
PR's own new ordering tests — ran green before force-pushing (LESSON-449).

## Lesson

A session spawned from a worktree inherits that worktree's branch as its
baseline, and everything on it rides along into the spawned session's PR.
Two rules follow:

1. **Spawning**: a fix session spun off from an in-flight feature worktree
   should branch from `origin/<integration-branch>`, not from the worktree's
   HEAD — or explicitly scope its commit to the fix's files. Check
   `git log origin/main..HEAD` in the new session before pushing: if it
   lists commits you didn't write, your baseline is someone's in-flight work.
2. **Recovering**: when a snapshot of your branch lands on main via someone
   else's squash merge, do not merge or plain-rebase — find the snapshot
   point and `git rebase --onto origin/main <snapshot-commit>` so only your
   post-snapshot delta replays. Then run BOTH change sets' tests: the
   snapshot may have been taken before fixes that the other PR's reviewers
   never saw.

## Why It Matters

The snapshot PR passes review looking like a scoped fix while actually
shipping unreviewed in-flight feature work — REQ-555's post-snapshot verify
fixes (a security gate among them) were NOT in what PR #42 merged, so `main`
briefly carried the feature in its pre-hardening state. And the feature PR's
conflict is silent: CI stops running with no failure signal, so the first
symptom is "no checks reported", which reads as infrastructure noise unless
you know zero-checks means conflicted (LESSON-461).

## Applies When

- Starting any spawned/background session (a chip, a bugfix, a hotfix) while
  a feature worktree is checked out — check your baseline before pushing.
- A feature PR flips to CONFLICTING right after an apparently-unrelated PR
  merges — diff the merged squash against your branch before assuming a real
  collision; an ancestor-snapshot conflict resolves with `rebase --onto`.
- Reviewing a "fix" PR whose diffstat is far larger than the fix it names.

## Related

- [[LESSON-449]] — compose intents when rebasing parallel fixes (the
  post-rebase both-suites rule applied here).
- [[LESSON-461]] — a conflicted PR is CI-silent; zero checks is a symptom.
