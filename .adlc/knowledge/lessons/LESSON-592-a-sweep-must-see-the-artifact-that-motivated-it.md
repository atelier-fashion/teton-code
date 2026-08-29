---
id: LESSON-592
title: "A sweep's vocabulary must cover the artifact whose loss motivated it"
component: "adlc/verification"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "developer-experience"]
tags: ["traceability", "sweep", "vacuity", "vocabulary", "mutation-testing", "spec-defect"]
req: REQ-598
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-598 shipped a traceability sweep so a refactor could not silently detach
rationale comments from the code they explain. The motivating incident was
concrete: a method inserted between `config_snapshot`'s doc comment and its
attribute, so the comment documented the wrong item.

The task specified two things that could not both hold:

- the id vocabulary is `(REQ|ADR|LESSON|BUG|TASK|ASSUME)-\d+`
- prove the sweep by reproducing that exact insertion against `config_snapshot`

At the base commit, `config_snapshot`'s doc block carries exactly `BR-6` and
`AC-11` — no id from that vocabulary. The item was invisible to the sweep, and
the first run of the mandated mutation **passed silently**. The sweep failed at
the one job it existed to do, and only running the mutation revealed it.

Widening the vocabulary then exposed a second problem: `BR-6` and `AC-11` are
numbered *within their REQ*, not globally — 357 items carry a `BR-6`. That makes
them right for a per-item claim ("this item kept its ids") and useless for a
workspace-scoped one ("this id annotates nothing any more"), because some `BR-6`
always survives somewhere. One vocabulary could not serve both arms.

## Lesson

**Check the corpus contains the motivating example before trusting the sweep.**
A detector is defined by what it can see, and a vocabulary chosen from the
general shape of the problem will not necessarily include the specific artifact
that prompted it.

And when one corpus serves two different claims, **split it by the property each
claim needs**. A globally unique id supports "this disappeared from the
workspace". A locally-scoped id supports only "this item changed". Using one set
for both either blinds the strict arm or makes the broad arm vacuous.

## Why It Matters

A sweep that cannot see the thing it was built for is decorative, and it is
worse than nothing: it occupies the slot a real guard would have, and its green
result is read as coverage. Nobody re-derives a passing check's assumptions.

The only reason this was caught is that the mutation was actually executed
rather than described. The first run passing was the finding — a mutation that
does not go red is evidence about the *test*, not reassurance about the code.

## Applies When

Building any source-scanning or corpus-based check (traceability sweeps,
convention linters, region checks, ratchets); choosing a pattern or vocabulary
that defines what a check can perceive; or writing an acceptance criterion that
names both a matching rule and a specific example the rule must catch — verify
the example satisfies the rule before accepting the criterion.
