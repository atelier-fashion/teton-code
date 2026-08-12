---
id: LESSON-509
title: "A bug report's proposed fix can rest on a premise the code does not support"
component: "adlc/architecture"
domain: "adlc"
stack: []
concerns: ["review", "documentation-integrity"]
tags: ["bug-triage", "broadcast-prompt", "request-id", "adr", "spec-hygiene", "BUG-162"]
req: REQ-570
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

BUG-162 reported that `model/confirm` takes no connection context, so any
handshaked same-UID connection can commit a multi-gigabyte download and a
daemon-wide model change. Its Resolution proposed the fix that had just worked
one scope down for `permission/respond` (REQ-569 TASK-107): **restrict answering
to the connection that raised the flow.** REQ-570's spec inherited that wording
into its Permissions table, and it survived spec validation.

It is not implementable for the bug's own headline method. Reading the code:
`model_selection_proposed` is raised by the daemon's first-run flow, which is
spawned beside `serve` and — by its own comment — may publish *before the daemon
accepts its first connection*. It is published `None`-scoped because local model
selection is a machine-wide fact. **There is no connection that raised it.**

The proposed fix had been generalized from a sibling case where a raiser
genuinely exists (a session's tool prompt) to one where it does not (a
daemon-raised broadcast).

## Why It Slipped Through

The analogy was strong and mostly right: same shape of defect, same scope
mismatch, same class of fix. What differed was a single structural fact — *who
raises the prompt* — that neither the bug report nor the spec review checked,
because both were reasoning about the **method** and the fact lives in an
unrelated module's startup path.

Inventing a raiser would have been worse than useless: first-claim-wins hands
the proposal to whichever connection races fastest, which an attacker wins as
easily as a user.

## The Lesson

**Before adopting a fix by analogy to a sibling case, verify the structural fact
the analogy turns on — in the code, not in the report.**

Bug reports propose remedies, and a well-argued remedy from a sibling defect
carries real authority. That authority is exactly what makes an unchecked
premise dangerous: the fix arrives pre-justified, so nobody re-derives it.

Note what the bug got *right*: its **Expected Behavior** section stated a weaker,
implementable bar — "answerable only by a connection entitled to answer it,
minimally not by the daemon's own spawned children". The implementable answer
was in the report all along, one section above the unimplementable one. When a
proposed fix fails, re-read what the report said the behaviour should *be*
before designing something new.

## How to Apply

- For any "restrict who may answer X" fix, first establish **who raises X**, and
  confirm it is a connection rather than the daemon, a timer, or startup.
- A `None`-scoped / broadcast event is a strong signal that no per-connection
  raiser exists.
- When the implementation departs from an input document, **correct the
  document**. BUG-162's Resolution now records what was actually built and why
  the original was unimplementable; leaving the wrong premise in place would
  have made the next reader "restore" it.
- Record the residual honestly: the standing rule that replaced it closes the
  *ambient* hole and not the determined-adversary case, which is a different and
  smaller claim than the original fix would have made.
