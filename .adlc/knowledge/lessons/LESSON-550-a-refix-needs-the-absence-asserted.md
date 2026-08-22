---
id: LESSON-550
title: "A defect fixed once comes back unless a test asserts the absence, not the remedy"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "security", "reliability"]
tags: ["provenance", "boundary", "regression", "verify", "symlink", "skills", "negative-assertion"]
req: REQ-587
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

REQ-587's six-reviewer verify panel found 3 Critical defects against a fully
green 3,541-test suite. **Two of the three were defects REQ-585 had already
found and fixed once**, recurring in the same functions:

- `runtime.rs`'s `spawn_title_session` still called
  `name_session(.., &Provenance::empty())`, and for a skill turn the prompt it
  is handed *is* the expansion body. REQ-585's fix moved the naming later — and
  left a comment explaining why — but never touched the provenance. `title_route`
  resolves remotely until a session is tainted, and the title fires on the first
  substantive prompt, before any taint exists.
- `skills/discovery.rs` followed a symlinked **project root**. REQ-585 closed
  this at the *leaf* (`<dir>/SKILL.md`); two of the four roots are built from
  the session root, so a cloned repo shipping `.claude/commands -> ../../..`
  escaped the jail one level up.

Both fixes were real. Both were tested. Neither test could see the recurrence.

## Lesson

**Test the absence of the defect, not the presence of the remedy.**

REQ-585's title test asserted that the naming attempt was *spent on the
expansion* — which is the design intent, and stayed true while the leak
returned. The test that catches it asserts what the title duty's transport
**received**: with a boundary configured, the captured request body must not
contain the skill body. That assertion fails the moment `Provenance::empty()`
comes back, and it was verified by applying exactly that mutation.

The symlink half has the same shape: the fixture exercised the *user* root, so
the rule "roots are followed" was pinned in one direction only. A guard scoped
to one of several call sites needs a fixture at each site, or the untested one
is where it regresses.

## Why It Matters

A re-fix is more dangerous than a first fix, because the code carries a comment
saying the problem was handled and a test whose name claims it. Both invite the
next reader — and the next reviewer — to skip it. The privacy claim here is a
product promise: ~2 KB of a `local-only` body left the machine while the UI
reported the turn pinned local.

## Applies When

Fixing any defect a previous REQ already fixed; writing a test for a guard that
has more than one call site; asserting a boundary, jail, or egress rule; or
reviewing a change whose comments say a hazard was already addressed.
