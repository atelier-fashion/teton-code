---
id: LESSON-665
title: "Two sub-dollar units that both look like 'micros' met at one seam with no conversion — name the constant, and never let a test pin an identity"
component: "daemon/cost-ledger"
domain: "cost"
stack: ["rust", "daemon"]
concerns: ["cost", "reliability"]
tags: ["units", "micro-cents", "usd-micros", "spend-ceiling", "conversion-constant", "identity-test", "bug-231", "req-588", "req-623"]
req: REQ-588
created: 2026-10-08
updated: 2026-10-08
---

## What Happened

REQ-588 converted `[cost] prompt_ceiling_usd` to "micro-cents" (100,000 per
USD) at the config edge and rendered the accumulator on the same assumption.
The ledger prices every call in `usd_micros` (1,000,000 per USD) and the
metered body added that figure to the accumulator as-is. Both names end in
"micros"; both are integers; nothing in the type system told them apart. The
ceiling bound at a tenth of the configured dollars and the refusal reported
ten times the real spend. It shipped in v0.1.3x and survived REQ-623, which
moved the unconverted line into a shared `spend_units` helper and wrote a unit
test asserting `spend_units(35_000) == 35_000` — a test that pinned the
identity and so certified the bug. A reviewer's probe during REQ-623's e2e
work ("a $0.44 call reported as $4.40 against a $0.05 ceiling") was the first
time anyone compared a rendered figure to a known price.

## Lesson

When two quantities with different scales cross a seam, put the conversion in
exactly one place and give it a name that states both units
(`USD_MICROS_PER_MICRO_CENT = 10`); every reader of one unit that feeds the
other goes through it. Then write the test that could only pass with the
conversion present: derive the ceiling from a real price through the real
config edge and assert the relationship (a 1.5× ceiling is not reached, the
sentence renders the price). A test that asserts `f(x) == x` at a unit boundary
is not a test of the conversion — it is a vote for its absence.

## Why It Matters

A cost control that binds 10× early makes the product's cost promise wrong in
the user's favour and its reporting wrong against it, silently, for every
user with a ceiling set. Every fixture written while the bug was live was
sized against the inflated figure (two e2e ceilings needed re-tuning ×0.1), so
the longer such a seam stays unnamed the more tests certify it.

## Applies When

- Two integer quantities with sub-unit scales (micros, milli, cents, basis
  points) flow into one accumulator, comparison or renderer.
- A helper "passes a value through as-is today" at a seam between modules
  that each own a unit.
- A unit test's expected value equals its input.
