---
id: LESSON-564
title: "Parallel agents in one worktree share one git index — commit by path, never `git add`"
component: "adlc/spec"
domain: "harness"
stack: ["git"]
concerns: ["reliability", "developer-experience"]
tags: ["parallelism", "worktrees", "git-index", "race-conditions"]
req: REQ-589
created: 2026-08-25
updated: 2026-08-25
---

## What Happened

During implementation, two task agents ran concurrently in a single worktree. One staged its
files with `git add` while the other was still writing. The first agent's commit swallowed the
second's staged work, producing a commit that claimed one task's changes and contained both.

It was caught only because the second agent verified its own commit diff and found files missing.
Nothing about the repository state looked wrong: the tree was clean, the tests were green, and
the history read plausibly. A commit that contains more than it says is invisible to every check
except reading the diff against the task that authored it.

## Lesson

When more than one agent shares a worktree, the git **index** is shared mutable state and
`git add` is a write to it. Use `git commit --only <path>…`, which stages and commits the named
paths without disturbing anything else another process has staged.

Instruct every dispatched agent explicitly. This is not discoverable — an agent working alone has
no reason to suspect the index is contended, and `git add` is the idiom it will reach for.

The alternative is one worktree per agent. That is correct but costs setup time and disk per
agent, so it is worth paying only when agents genuinely mutate the same files; a shared worktree
with `--only` covers the common case where they mutate different ones.

## Why It Matters

The failure is silent and it corrupts history rather than breaking a build. Attributing work to
the wrong commit undermines every later operation that reasons about which commit carried what —
notably a carve-out, where `git log -S` over a mis-attributed commit gives a confidently wrong
answer about where code lives.

## Applies When

- Dispatching more than one implementation agent against a single checkout.
- Any pipeline phase that runs tasks concurrently.
- Writing agent briefs — state the constraint in the brief, not in the skill's preamble, because
  the brief is what the agent reads at the moment it commits.
