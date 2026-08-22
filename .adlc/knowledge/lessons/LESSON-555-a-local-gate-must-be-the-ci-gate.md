---
id: LESSON-555
title: "A local verification gate that is weaker than CI is worse than none — it converts a red build into a confident green"
component: "verification"
domain: "verification"
stack: ["rust", "ci"]
concerns: ["developer-experience", "verification-integrity"]
tags: ["clippy", "deny-warnings", "gate-script", "false-green", "unconditional-echo", "req-584", "bug-164"]
req: REQ-584
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

Twice in one session, work was committed on a verification signal that was not
one.

**First**, a shell one-liner ended `... | grep -E "^error" | head -5; echo
"clippy clean"`. The `echo` runs whatever the grep found, so "clippy clean"
printed while three clippy errors scrolled past directly above it. A commit went
out on that line.

**Second**, after that was replaced with a real gate script that checks exit
status, the script ran `cargo clippy --workspace --all-targets` — while CI runs
it with **`-D warnings`**. Three warnings (two unused `mut`, one unused binding)
passed the local gate and failed all three CI legs. The compiler had been
printing those warnings in every test run for the better part of an hour; they
were read as noise because the gate said green.

## Lesson

**A local gate must run the same command CI runs, and must fail on its exit
status.** Both halves matter and they fail differently:

- **Exit status, not output matching.** A grep for `^error` is a heuristic about
  formatting; an exit code is the tool's own verdict. Any pipeline that ends in
  an unconditional `echo "ok"` is a lie generator — the shell will happily print
  it after a failure.
- **The same flags.** `clippy` and `clippy -- -D warnings` are different
  predicates. A gate that runs the weaker one does not merely miss things: it
  actively teaches you to ignore the output that would have told you, because a
  green gate reframes real diagnostics as noise.

The second failure mode is the more insidious one, and it is the same shape as
BUG-164: a check that tests a *related* property (clippy ran; the daemon binary
exists) and is read as testing the property that matters (clippy passed as CI
runs it; the daemon is current).

## Why It Matters

Both failures cost a red CI run and a corrective commit, which is cheap. What is
not cheap is the habit: an hour of ignoring genuine compiler warnings because a
gate had certified the tree. The gate did not just fail to catch the problem —
it suppressed the signal that would have.

## Applies When

- Writing or reusing any pre-commit / pre-push verification script.
- Reading a summary line from a script you wrote in the same session — it is
  exactly as trustworthy as its exit-status handling.
- Noticing compiler output you are about to dismiss because "the gate is green".
  That is the moment to check what the gate actually runs.
- Porting a gate between repos or worktrees: the flags travel with the CI
  config, not with the script.
