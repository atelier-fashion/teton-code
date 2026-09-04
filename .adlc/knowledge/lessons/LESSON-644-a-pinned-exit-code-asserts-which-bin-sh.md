---
id: LESSON-644
title: "Pinning a shell builtin's exit status and its error string asserts which /bin/sh the runner ships"
component: "daemon/harness/shell"
domain: "testing"
stack: ["rust", "shell", "github-actions"]
concerns: ["portability", "reliability"]
tags: ["dash", "bash", "exit-code", "platform-matrix", "over-specified-assertion", "acceptance-criteria"]
req: REQ-617
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-617's AC-7 was written from a live transcript: *"`shell: cd /nonexistent &&
pwd` returns exit 1, the raw stderr, the `ERROR:` line, and no `[shell: …]`
interpretation."* The test asserted exactly that — `content.contains("(exit 1)")`
and `content.contains("No such file or directory")`.

Both halves are properties of **bash**, which is macOS's `/bin/sh`. Ubuntu's
`/bin/sh` is dash: a failed `cd` exits **2**, and the diagnostic reads `sh: 1:
cd: can't cd to /nonexistent`. The macOS CI leg passed and the Linux leg failed,
on the same commit, with a message about "the raw exit status must survive"
printing a result in which the raw exit status had, in fact, survived — as 2.

The AC's *claim* was that a failed command reaches the model unedited. Its
*wording* accidentally claimed something narrower and untrue.

## Lesson

When a test reads output that a shell produced, ask which part of it the product
owns. Here the harness owns the `(exit N)` frame, the `[stderr]` marker, the
`ERROR:` line and the absence of an interpretation; the shell owns N and the
sentence after `cd:`. Assert the frame and the *shape* of what it carries —
parse `(exit N)` and assert it non-zero, match the stderr line on the builtin
and the path every `/bin/sh` names — and the test keeps its meaning on both.

The corollary is about acceptance criteria, not just tests. An AC transcribed
from one observed run carries that run's incidental values, and they read as
requirements to whoever implements it. `exit 1` was never the requirement.
Correct the AC when you find this, with the reason, rather than quietly
loosening the test underneath a criterion that still says 1.

## Why It Matters

A green macOS leg and a red Linux leg on identical code is the most expensive
shape a CI failure takes: the first hypothesis is always "what did my change
do differently on Linux," and the answer is nothing. It cost a full CI cycle
here, on a branch that also had a genuine flake in flight, which made both look
like one problem.

## Applies When

- Asserting on the output of `std::process::Command`, `sh -c`, or any shell.
- Writing an AC from a captured session transcript or a bug report.
- A test passes on one CI leg and fails on another with the same commit.
- Reviewing a test whose needle is an OS error message.
