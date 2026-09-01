---
id: LESSON-610
title: "A session scratchpad is shared by every subagent of that session — generic filenames collide"
component: "adlc/tooling"
domain: "process"
stack: ["adlc"]
concerns: ["reliability", "developer-experience"]
tags: ["sprint", "parallel-runners", "scratchpad", "evidence-integrity", "collision"]
req: REQ-604
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

The `/sprint` runners for REQ-603, REQ-604 and REQ-606 ran concurrently in
separate git worktrees — correct isolation for *source*. Each was told its
scratchpad was "session-specific, isolated from the user's project".

It is session-specific. It is not **agent**-specific. All three runners were
launched from the same parent session, so all three resolved to one directory:

```
/private/tmp/claude-501/<project-key>/<one-session-uuid>/scratchpad
```

REQ-604 wrote its workspace test output to `suite.txt`, `suite2.txt`,
`suite3.txt` and its PR body to `pr-body.md` — generic names chosen as if the
directory were private. REQ-606 used the same names for the same purposes,
because they are the obvious names.

REQ-606's first suite run then reported "56 passed" from a results file that had
been clobbered. It was caught only because the surviving output mentioned
`.worktrees/REQ-604` — a path REQ-606 has no reason to contain. REQ-606
subsequently isolated itself into a `REQ-606-only/` subdirectory.

REQ-604's own files were checked afterwards and were intact: each contained four
`req604_event_order` hits, zero REQ-606 references, and only `.worktrees/REQ-604`
paths. So REQ-604 was the *clobberer*, not the clobbered — which is worse to
discover than the reverse, and is only discoverable at all by looking.

## The Lesson

**Concurrent agents sharing a session share its scratchpad. Namespace every
scratch path by the work item, not by the agent's belief that it is alone.**

The failure mode is the dangerous kind: a results file is still a valid results
file after being overwritten. Nothing errors. A green summary is read back, the
numbers are simply another run's — and "56 passed" is not obviously wrong unless
someone knows what the total should be.

Worktree isolation does not help here, because the scratchpad is deliberately
*outside* the project. The two isolations are orthogonal and only one of them was
being reasoned about.

## How to Apply

- Prefix every scratch file with the work item: `REQ-604-suite.txt`, or better, a
  `REQ-604/` subdirectory created once at Phase 0.
- The same applies to `CARGO_TARGET_DIR` and any other shared build or output
  location handed to a concurrent runner.
- When a measurement looks surprising — a test count far from expectation — check
  the file's provenance before trusting or debugging it. Grep it for a path that
  belongs to somebody else.
- The durable fix belongs in the toolkit rather than in any one repo: a sprint
  runner should be handed a scratch path already namespaced by its work item, so
  no runner has to remember. Recorded here because this is where it was observed
  and where the next sprint will meet it.

**See also LESSON-611** (REQ-606), the reader's side of this same event: how a
clobbered results file presents to the runner that receives it, and why "56
passed, 0 failed" is not self-evidently wrong. This lesson is how not to cause
the collision; 611 is how to notice it has happened to you.
