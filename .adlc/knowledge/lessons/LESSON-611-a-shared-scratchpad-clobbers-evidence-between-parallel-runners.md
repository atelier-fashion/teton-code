---
id: LESSON-611
title: "A shared scratchpad makes one runner's results indistinguishable from another's"
component: "adlc/sprint"
domain: "verification"
stack: ["adlc"]
concerns: ["verification", "reliability"]
tags: ["parallel-sprint", "false-green", "evidence-integrity", "scratchpad", "subagent"]
req: REQ-606
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-606 ran as one of four concurrent `/sprint` pipeline-runners. Each runner
had its own git worktree, its own `target/`, and its own branch — the isolation
the sprint design is built on. What they did **not** have was their own
scratchpad: the session scratchpad path is keyed on the *parent* session, and
sibling subagents of one parent share it.

REQ-606 captured its acceptance evidence the obvious way:

```
cargo test --workspace --no-fail-fast 2>&1 | tee "$SP/suite.txt" | tail -5
```

REQ-604's runner, working in a different worktree at the same moment, chose the
same filename. The summed result read **56 passed, 0 failed** against an
expected ~4,000 — and the only reason that was caught is that the file's
compiler output named `.worktrees/REQ-604` in its paths.

**The writer's side is LESSON-610**, filed by REQ-604's own wrapup, which
confirmed the mechanism from the other end rather than leaving it inferred: the
scratchpad is *session*-specific but not *agent*-specific, all three runners of
the cluster resolved to one directory, and REQ-604 wrote `suite.txt`,
`suite2.txt`, `suite3.txt` and `pr-body.md` into it. REQ-604 established it was
the clobberer, not the clobbered — its own files were intact. This lesson is the
**reader's** side of the same event: what it is like to receive the clobbered
file and have to notice.

Had the two runs been closer in size, the number would have looked plausible and
**REQ-606 would have published another REQ's test results as its own.**

## Why It Is Worse Than An Ordinary Race

Three properties compound:

- **A clobbered results file is not corrupt.** It is a perfectly well-formed
  `cargo test` transcript. Every downstream check — `grep -c FAILED`, the
  `test result:` sum, `EXIT=` — reads it happily and reports green.
- **Worktree isolation actively hides it.** The whole point of per-REQ worktrees
  is that runners cannot see each other, which is exactly why a runner does not
  think to ask who else is writing to its output path.
- **The tell is incidental.** The absolute paths in compiler output are the only
  thing that distinguished the two runs, and they appear in the *build* section
  most summaries throw away. `tail -5` discards them.

This is the LESSON-569 shape at the level of *evidence collection* rather than
test authorship: an artifact that passes every check while measuring something
other than its subject.

## The Rule

**A parallel runner's evidence file must be namespaced by the thing that makes
it unique — the REQ id — not by what the file contains.** `suite.txt` is a
description of content and every sibling produces the same content-description.
`suite-REQ-606.txt`, or better a per-REQ subdirectory, cannot collide.

Two supporting habits, both cheap:

- **Sanity-check the magnitude before trusting a green.** A suite that reports
  56 tests where thousands were expected is telling you something even when
  `failed: 0`. The project already grep`s for `FAILED` (LESSON-533); the count
  deserves the same suspicion as the verdict.
- **Read the provenance, not just the verdict.** The absolute paths in a
  captured build log say which tree produced it. On a parallel sprint that is a
  fact worth one `grep`.

## Scope

Applies to every artifact a sprint runner writes outside its worktree: test
transcripts, measurement scripts, baseline copies extracted with `git show`,
redacted diffs staged for delegation. REQ-606 also kept a `measure.py` and a
`base/` tree of baseline sources in the shared scratchpad; those happened not to
collide, and "happened not to" is the whole problem.

The durable fix belongs in the harness — give each subagent its own scratchpad,
or key the path on the subagent rather than the parent session. Until then it is
the runner's job, and the runner cannot see the collision coming.

**See also LESSON-610** for the writing discipline (namespace every scratch path
by the work item, at Phase 0, before anything is written). The two are
deliberately not merged: 610 tells a runner how not to cause this, 611 tells a
runner how to notice it has happened to them. A runner that follows 610
perfectly can still be the victim of one that does not — which is exactly the
case here, since REQ-606 was following no convention at all and neither was
REQ-604.
