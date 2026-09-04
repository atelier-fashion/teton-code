---
id: LESSON-643
title: "A conflicted PR is not 'green pending merge' — it is untested, because the forge computes no checks at all"
component: "adlc/pipeline"
domain: "ci"
stack: ["github-actions", "git"]
concerns: ["reliability", "process"]
tags: ["merge-conflict", "ci-signal", "false-green", "rebase", "sprint", "phase-7"]
req: REQ-617
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-617 finished Phase 6 with an open PR and a local suite at 4,336 passed / 0
failed. A sibling REQ merged, the PR went `CONFLICTING`/`DIRTY`, and Phase 7
halted on the trial-merge gate. The state file recorded, accurately, that "the
workspace suite was green on the branch tip" and that "CI has NOT run on the
tip" — and the two sentences read like the same claim in different words.

They were not. GitHub does not compute checks for a PR whose merge commit is
unresolvable, so no CI had touched the branch for its last **ten** commits; the
last green run was on `5f3d4e5`, the pipeline-state initializer. When the
rebase finally landed and CI ran, the tip failed on the **first** attempt at a
defect that had been sitting in the branch since Phase 4:
`a_failed_command_comes_back_raw_with_no_interpretation` pinned an exit status
that only one `/bin/sh` produces. Nothing about the conflict caused it. The
conflict only hid it.

## Lesson

Treat "PR conflicting" as "this branch has no CI signal," not as "CI will run
after the rebase." The local suite is a different instrument: it runs one
platform, one shell, one scheduler. Everything a matrix leg would have caught
is still ahead of you at the moment you *think* the work is done, and it lands
during the merge, which is the worst place to discover it.

Two concrete moves. When a REQ halts on a conflict, say in the state file that
the branch is **unverified on CI**, not merely that CI has not run — the reader
resuming it is deciding how much slack to leave. And when the rebase is done,
expect the first CI run to be a genuine first run: budget for a fix cycle
rather than treating a red leg as "the rebase broke something."

## Why It Matters

The two defects this run found — the shell-dependent assertion and a stale
"measured" band in a fixture comment — were both older than the conflict, and
both would have been caught days earlier by any CI run on the tip. Instead they
surfaced inside Phase 8, after the resolution work, where a red leg is easy to
misread as damage from the merge. It cost two extra CI cycles and a diagnosis
that started from the wrong hypothesis.

## Applies When

- A PR is reported `CONFLICTING`, `DIRTY`, or `BEHIND` and work continues on it.
- Recording verification evidence in `pipeline-state.json` while a REQ is held.
- Resuming a merge-blocked REQ, in a sprint or solo.
- Reading "the suite was green" in any handoff note — ask green *where*.
