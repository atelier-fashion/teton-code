---
id: ASSUME-045
title: "REQ-614's project-root provenance rule and REQ-615's write gate are independent axes that cannot contradict"
status: validated
req: REQ-614
created: 2026-09-04
resolved: 2026-09-04
---

## Assumption

That ADR-614-2 — a `rooted` provenance verdict requires `RootKind::Project` —
and REQ-615's `root_gate.rs` — a write is refused at `RootKind::Home` or
`RootKind::FilesystemRoot` — can both be true of the same session without one
weakening the other.

## Context

The two REQs were written concurrently in one sprint and each cites the other
by name: ADR-614-2's rationale rests on "REQ-615 makes a home root loud by
other means", and `root_gate.rs`'s module docs defer the `sh -c` /  `xargs`
indirection spellings to "REQ-614's opaque-verb territory". Neither author saw
the other's merged code, and REQ-614 sat blocked behind REQ-615 for a rebase,
so the composition was asserted before it was ever observed. The user's
rebase instruction made checking it an explicit precondition of merging.

## Resolution

**Validated by reading both rules against the same `RootKind` enum**, which
the rebase left unchanged at four variants.

They gate different questions. REQ-614 decides what a command's result may
*claim about its reach* (an egress-facing read question); REQ-615 decides
whether a command may *change the filesystem* (a user-facing write question).
No root kind receives contradictory instructions, because no single decision is
made twice.

The one kind where the two rules visibly differ is `Plain`: REQ-615
deliberately permits writes there (BR-4's carve-out — a plain directory is
where a user scaffolds a new project, and REQ-613's `TETON.md` write must keep
working), while REQ-614 still yields `unknown` for it. That is coherent in both
directions and not a gap: "you may create files here" and "I cannot prove what
a command here could read" are compatible claims about a folder that is not a
project. Both rules err toward the conservative side of their own axis, so
their composition is at least as strict as either alone — which is the property
that makes composing them safe without a joint test.

Residual, recorded rather than papered over: `root_gate.rs` states plainly that
it is a guard rail and not a sandbox, and REQ-614 does not change that.
