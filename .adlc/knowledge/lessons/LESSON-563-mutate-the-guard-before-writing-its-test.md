---
id: LESSON-563
title: "Mutate the guard before you write its test; a mutation that reddens nothing is the finding"
component: "daemon/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security", "reliability"]
tags: ["mutation-testing", "vacuous-tests", "guard-verification"]
req: REQ-589
created: 2026-08-25
updated: 2026-08-25
---

## What Happened

A task charged with closing untested rules inverted the usual order: it applied the mutation
first, then wrote the test. Three mutations survived a green 2,343-test suite. The worst was an
over-budget approval recorded under a skill grant's *digest* key spelling, which would have
become a standing grant to run shell commands unprompted — the existing "accepting twice asks
twice" assertion could not see it.

The method kept paying out across two REQs. Later passes found: an ordering test that stayed
green in **both** orders, because its fixture client dispatched by request *type* rather than by
the order requests arrived; a privacy assertion on a fixture that could not produce the string it
refused; and a comment describing a newline guard whose assertion counted lines containing a
needle that only appears in the first half of a split — mutating the defusing function left all
68 targets green.

Moving a test does **not** preserve its bite. Across this work a mutation had to be re-run after
a move three separate times, and on one occasion the obvious spelling of the mutation failed to
compile — which was the only reason the reviewer noticed they had mutated the wrong site and
would otherwise have recorded a false "survived".

## Lesson

Write the mutation before the test, and record it in the test's doc comment so the next reader
can re-run it. After any move, merge or rebase that relocates a guard, re-run its mutation — a
relocated test does not keep its bite for free.

Prefer mutations that isolate. If reverting a whole line reddens a positive assertion, it proves
nothing about the negative one you actually care about; construct the mutation that fails only
the assertion under test.

## Why It Matters

A green suite is evidence that nothing *changed*, not that anything is *guarded*. In this
codebase a 2,343-test suite hid three surviving mutations and a 3,541-test suite hid three
Critical defects — twice, in the same functions. Every vacuous test is worse than no test,
because it stops the next person looking.

## Applies When

- Implementing or reviewing any security guard, permission gate, or refusal path.
- Any test whose name contains "cannot", "never", "only", or "without".
- After a rebase, cherry-pick, or merge that relocates tested code.
- Reviewing a refusal test: check for an accepting counterpart on the **same** fixture, or the
  payload may be refused for an unrelated reason (see LESSON-508 for the sibling case).
