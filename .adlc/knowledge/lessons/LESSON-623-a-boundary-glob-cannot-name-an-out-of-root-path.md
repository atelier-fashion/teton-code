---
id: LESSON-623
title: "A boundary glob cannot protect a path the provenance seam never names — put the rule in the jail"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "security"]
tags: ["boundary", "provenance", "tool-jail", "path-resolution", "validation-gate"]
req: REQ-611
created: 2026-09-03
updated: 2026-09-03
---

## What Happened

REQ-611's first architecture protected the transcript directory by composing a
`local-only` boundary row for it into `effective_boundaries()`. The task-phase
`/validate` run refuted it: `ProvenanceId::from_resolved(root, path)` strips the
session root and errors otherwise, so a file *outside* the root never receives
an id for any glob to match, and `ToolContext::resolve` had already refused the
path before minting one. Inside the root a boundary would only have *tainted* a
read that must not happen at all. The same draft's AC-11 refused any directory
inside the root, which on macOS would have disabled transcripts for every
home-rooted session because the default directory sits under `$HOME`.

## Lesson

A protection that keys on a file identifier only reaches files that actually
receive that identifier. Before committing a design to the boundary layer,
trace the seam that mints the identifier (`from_resolved`, root-relative) and
the one that consumes it (`BoundaryMatcher::match_path` on a block's `source`
at egress). If the target can fall outside that seam's domain, the rule belongs
where the path is *resolved* — here a denied prefix in the tool jail and the
walker policy — not where egress is *inspected*. Refusal is also the stronger
outcome: a boundary taints a read, a jail denial prevents it.

## Why It Matters

The boundary row would have shipped green: it compiles, `effective_boundaries`
tests pass, and nothing in the suite asks whether a glob can match an absolute
path. The failure is silent — a row that exists in config and never matches a
real file — and it would have surfaced only as a missing `privacy_block` in
production. Catching it cost one validation pass; catching it after
implementation would have cost the composer's region test, seven read sites,
and a spec rewrite.

## Applies When

Designing any rule of the form "content under directory X must not leave the
machine" or "tools must not touch X"; reviewing an architecture that reuses
`DEFAULT_BOUNDARIES`' shape for a non-repo location; deciding between a
boundary (taint at egress) and a jail denial (refusal at resolve); and writing
an AC that says a read "pins the session local" — ask first whether the read
can happen at all.
