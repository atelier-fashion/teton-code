---
id: LESSON-593
title: "A rationale id marks a change, not a subsystem — it cannot locate a seam"
component: "adlc/architecture"
domain: "refactoring"
stack: ["rust"]
concerns: ["developer-experience", "maintainability"]
tags: ["decomposition", "god-module", "traceability", "seams", "refuted-assumption"]
req: REQ-599
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

REQ-599 set out to split a 14,183-line god module and proposed a method: the
file is densely annotated with REQ/ADR/LESSON/BUG ids explaining why each branch
exists, so let those ids reveal the seams. Where a stage's ids cluster, that is
a boundary; where they interleave across a proposed boundary, the boundary is
wrong. The requirement named this its **central bet** and asked that it be
validated early.

Measured, by parsing every production item with its attached doc block:

- Of 19 REQ ids appearing on three or more items, **1 was clustered and 13 were
  scattered across essentially the whole file.** REQ-561 spanned 13,547 lines of
  14,183.
- Inside the 1,084-line function to be decomposed, of 13 ids appearing more than
  once, only **4 were local to one stage** and **5 spanned the entire function**.

Read literally, the rule condemned every possible boundary and made the REQ
unimplementable.

## Lesson

**A rationale id records which decision a line serves, not which subsystem it
belongs to.** Those coincide only when a change introduced a self-contained
subsystem — one of nineteen cases here. Changes to a well-trodden path are
overwhelmingly cross-cutting: a single REQ adds a budget check, a dispatch arm,
a commit-path branch and a failure arm, and stamps its id on all four.

Seams come from **structure**: which types exist, which `impl` blocks hold what,
what is contiguous once unrelated neighbours leave. In this file the census that
did work was blunt — 43 types, 28 impl blocks, and one `impl` block of ~7,000
lines that was the actual problem.

Traceability ids remain worth preserving through a move — that is a separate
rule with its own enforcement. Preserving them and navigating by them are
different jobs, and the second one does not work.

## Why It Matters

The bet was reasonable, which is what makes it worth recording: dense
documentation *looks* like a map. Following it would have produced boundaries
that split cohesive code and joined unrelated code, defended by the fact that a
rule in the requirement endorsed them.

The general form: when a plan proposes a **proxy** for the thing it actually
needs — ids for seams, line counts for complexity, test counts for coverage —
measure the proxy against the real thing before building on it. The requirement
was right to demand this be validated early; the cost of the whole exercise was
one afternoon's measurement, and it changed the entire approach.

## Applies When

Planning a decomposition of any large module; proposing a heuristic for where
boundaries lie; writing a requirement whose method rests on an assumption about
the code's shape — name the assumption and say how it will be checked, as this
one did.
