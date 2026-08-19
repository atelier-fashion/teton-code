---
id: LESSON-541
title: "Parallel implementers in one worktree need disjoint files AND disjoint formatting and measuring — and a measuring task goes after the task it measures"
component: "adlc/process"
domain: "process"
stack: ["rust", "cargo"]
concerns: ["developer-experience", "reliability"]
tags: ["proceed", "parallel-tier", "worktree", "cargo-fmt", "file-ownership", "resident-ceiling", "req-583"]
req: REQ-583
created: 2026-08-19
updated: 2026-08-19
---

## What Happened

REQ-583's Phase 4 ran three task-implementers in parallel in ONE worktree with
disjoint file ownership, and two things crossed the lines anyway. (1) One agent
ran `cargo fmt --all` once; it reformatted the other two agents' uncommitted,
mid-edit files — formatting only, but enough to break an in-flight `Edit`
match. (2) The resident-prompt ceiling test measures the *rendered* prompt,
which includes every tool description; the task that reworded five
descriptions (+38 bytes) and the task that added the environment block were
originally in the same tier, so neither could know the final headroom. The
architecture caught the second before dispatch by sequencing the measuring
task after the rewording task (a deliberate "planned red" handoff of 18 bytes
of headroom); the first was caught only by the agent's own report.

## Lesson

Disjoint file ownership is necessary, not sufficient. Two more rules for a
parallel tier: every agent formats and checks only its own crate/files
(`cargo fmt -p <crate>` or `rustfmt <files>`, never `--all`); and any test
that measures a *composed* artifact (a rendered prompt, a byte budget, a
golden over several writers) belongs to the task that runs LAST over its
inputs, stated as a dependency, not an allowance. Tell each agent that a
compile error in a file it does not own means "wait and retry", never "fix".

## Why It Matters

A parallel tier that silently reformats a sibling's work or measures a tree
that is still moving produces a green commit from one agent and a red one from
the next — for reasons neither can see. Sequencing the measurer costs one tier
of wall-clock; the alternative is a guessed allowance that is wrong by exactly
the amount the next task adds.

## Applies When

`/proceed` or `/sprint` tiers with more than one implementer in a worktree;
any task whose acceptance test reads a composed artifact other tasks also
write to; any agent prompt that mentions `cargo fmt`.
