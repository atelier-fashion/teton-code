---
id: LESSON-565
title: "Raising one limit in a conjunction buys nothing you have not computed the crossover for"
component: "daemon/router"
domain: "routing"
stack: ["rust"]
concerns: ["reliability", "correctness"]
tags: ["context-budget", "conjunctive-guards", "crossover", "acceptance-criteria"]
req: REQ-590
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

A `/analyze` turn was refused at 4,097 words against a 4,096-word local budget. REQ-590 raised the
word half to 10,240 and, deriving both halves from the engine's window for consistency, moved the
byte half from 32,768 down to 30,720.

The guard is `words ≤ W AND bytes ≤ B`. The reported body was ~4,097 words at roughly 7.5 bytes
per word — so it cleared the new word guard with 6,000 words to spare and **hit the new byte
guard instead**. The refusal did not go away; it changed currency.

Nothing caught it. Not the eight ADRs, not an adversarial pass on the spec, not three exploration
agents, not sixteen acceptance criteria, not a 3,896-test suite. Every one of them examined the
word half — the half that was demonstrably improving — in isolation. It surfaced only when a task
sized a test fixture against the *real* reported body instead of a round number.

The number that explains all of it is the **crossover**, `B / W`: the content density at which
the binding conjunct switches. It moved from 8 bytes/word to 3.2. Real content is denser than
3.2, so the guard that binds flipped from words to bytes — and the same one number predicts why
raising the word half 2.5× bought +50% on prose, 0% at 7.5 B/word, and **−6.25% on code**.

## Lesson

**For any AND-of-limits, compute and publish the crossover before changing either limit**, and
state which conjunct binds for the target content before and after. One row of a table, at spec
time.

The reviewer's version, which needs no arithmetic: **diff the fields of the field report against
the fields of the acceptance criterion.** Every measurement the report carries that the criterion
does not restate is a place the failure can move to. The report carried two numbers; AC-12 quoted
one. A criterion naming one conjunct cannot distinguish "the refusal is gone" from "the refusal
changed currencies" — and it will go green while the second is true.

## Why It Matters

Raising a limit feels monotone. In a conjunction it is not: it can leave the target case refused,
and it can make the effective budget *smaller* for a whole class of content while every headline
number improves. Here the fix shipped through a full pipeline and would have merged having not
fixed the case it was built for, with every check green.

## Applies When

- Any guard of the form `a ≤ A && b ≤ B` where a change moves one limit — context budgets, rate
  limits paired with size limits, quota-plus-concurrency.
- Two limits denominated in different units over the same artifact (see LESSON-446).
- Writing an acceptance criterion from a field report: check the report for measurements the
  criterion does not name.
