---
id: LESSON-642
title: "A remote id re-check cannot see an allocation that has not happened yet"
component: "adlc/proceed"
domain: "adlc"
stack: ["claude-code"]
concerns: ["reliability", "sprint"]
tags: ["id-collision", "assume", "sprint", "concurrency", "BUG-210", "REQ-618"]
req: REQ-618
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-618 ran in a three-REQ sprint and allocated `ASSUME-039` during its wrapup.
So did REQ-615, which merged first. Two `ASSUME-039-*.md` files landed on `main`
— REQ-615's write-gate assumption and REQ-618's room-fraction one — and REQ-616
had meanwhile taken 040 and 041.

The `/proceed` id re-check (REQ-545) did run, at Step 0, and it was correct: at
branch-creation time nothing on the remote carried 039. The colliding allocation
happened *afterwards*, in a sibling worktree, and neither branch could see the
other until one of them pushed.

## Lesson

`adlc_recheck_id` closes the gap between an id allocated in a *previous* session
and a remote that has moved since. It cannot close the gap between two sessions
allocating concurrently, because at the moment either one checks, the other's
allocation does not exist anywhere yet. Re-checking at Step 0 is checking at the
one moment the answer is guaranteed stale by the end of the run.

Two things follow.

**Allocate knowledge ids at wrapup, not at Step 0, and re-check them there.**
The window between allocation and push is what makes the collision possible, and
at wrapup that window is minutes rather than hours. REQ-618's assumption was
written at Phase 5 and pushed at Phase 8; the sibling merged in between.

**Expect the collision and make renumbering cheap rather than preventing it.**
A knowledge id appears in the file name, the frontmatter, and every prose
reference in code and context docs — REQ-618's appeared in four files. Renumber
with a single sweep, and leave a line in the renumbered artifact saying what it
was called before, because the merged PR body and commit messages still carry the
old number and cannot be edited.

## Related

- BUG-210 — the cross-repo id collisions this is the single-repo, single-sprint
  form of.
- REQ-545 — the `/proceed` id re-check, which is doing its job and is not the
  fix for this.
