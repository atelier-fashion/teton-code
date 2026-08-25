---
id: LESSON-560
title: "A spec rule with no owner decision attached is the one that silently gets dropped"
component: "adlc/spec"
domain: "harness"
stack: ["markdown"]
concerns: ["reliability", "developer-experience"]
tags: ["acceptance-criteria", "scope-loss", "verification", "open-questions"]
req: REQ-591
created: 2026-08-25
updated: 2026-08-25
---

## What Happened

REQ-591 shipped 12 business rules and 15 acceptance criteria through a full pipeline with a
green 3,879-test suite. A verify panel found **BR-10 / AC-8 had never been implemented at all**.

AC-8 required that no surface claim a refusal that did not happen, and said explicitly: *"The
existing test that pins the contradictory line is corrected, not preserved."* The test was
preserved. Worse, it now asserted the contradiction as an invariant on **both** legs, with a
comment justifying it — so an unattended session at a trusted root printed
`… was refused without asking …` and then ran the skill, and a test guaranteed it would keep
doing so.

The reason it slipped is structural, not careless. Every other rule in that spec had an open
question and an owner decision attached (OQ-1…OQ-5 → D-1…D-5), and each decision generated a
task. BR-10 was a bare rule with no OQ and no decision. The sweep task that closed "the rules
that were written and never checked" listed the ACs it closed — AC-8 was not among them, and
nothing noticed, because the artifact that would have noticed was the decision record that did
not exist.

## Lesson

At the end of architecture, cross-check the BR/AC list against the decision list and name every
rule that has **no** decision, task, or open question pointing at it. Those are the candidates
for silent scope loss — not the contested ones. A rule nobody argued about is a rule nobody
scheduled.

Do not treat a green suite as coverage of the spec. A test can pin the exact defect its
acceptance criterion asks you to remove, and it will be green while doing so.

## Why It Matters

The dropped rule here was a log-integrity guarantee in security code: an operator scraping logs
could not distinguish a genuine unattended refusal from a successful trusted run. It merged, and
was only caught because a review pass read the acceptance criteria against the tests rather than
against the code.

Attention follows contention. Anything the owner had to decide gets built; anything nobody
disputed can evaporate between spec and merge with no artifact recording that it did.

## Applies When

- Closing out any spec phase with more acceptance criteria than tasks.
- A REQ accumulated owner decisions mid-flight (the decided items crowd out the undecided ones).
- Writing an AC of the form "the existing test is corrected" — a test that must *change* is
  invisible to any check that only asks whether tests pass.
