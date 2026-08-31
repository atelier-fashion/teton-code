---
id: ASSUME-028
title: "Sequential pushes are how the multi-commit REQs actually land"
status: validated
req: REQ-605
created: 2026-08-31
resolved: 2026-08-31
---

## Assumption

That multi-commit REQs in this repo land as a series of pushes, one commit at a
time — not as a single batched push of the whole branch. REQ-605's value depends
on it: a per-commit concurrency group only helps commits that actually start a
run, and in a **batched** push only the tip does.

## Context

`ci.yml` triggers on `pull_request` and on `push: branches: [main]` only. On a
feature branch the sole trigger is `pull_request`, and a batched push of several
commits fires one `synchronize` event carrying the tip SHA — the intermediate
commits never start a run under any `concurrency` setting.

So if REQs typically landed as one batched push, this change would buy nothing:
there would be no earlier run to protect. AC-1 was deliberately scoped to
"every commit **pushed as a tip**" for this reason, and giving intermediate
commits of a batched push their own run was placed explicitly out of scope
(it needs a new trigger, not a concurrency change).

## Resolution

**Validated**, against the two REQs that motivated REQ-605.

- **REQ-599** pushed seven commits as tips; REQ-602 later found two of them had
  their `macos-latest` job cancelled — cancellation that only happens if each
  commit started its own run.
- **REQ-600** (PR #249) shows 10 CI runs over 11 commits, every one `success`,
  and every one **strictly non-overlapping** — the first started 20:10 UTC and the
  last finished 22:11. Exactly one commit was pushed together with another and
  never built alone.

Both patterns confirm the assumption, and REQ-600's is the sharper evidence: the
non-overlapping intervals are the visible signature of a human waiting for each
run before pushing the next — the discipline AC-1's property makes unnecessary.

REQ-605's own branch then reproduced it a third time: eight commits, eight runs,
six overlapping consecutive pairs.

**Residual risk, unchanged and accepted:** a contributor who batches a whole
branch in one push still gets one run for the tip. That gap is real, was named in
Out of Scope, and is not addressed here.
