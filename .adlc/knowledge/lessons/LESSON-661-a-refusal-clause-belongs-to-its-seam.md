---
id: LESSON-661
title: "A clause saying what did not happen belongs to the seam, not the caller — and a test can assert it while its own non-vacuity check proves it false"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability"]
tags: ["refusal-wording", "composer", "reroute", "skills", "non-vacuity", "test-contradiction"]
req: REQ-587
created: 2026-09-25
updated: 2026-09-25
---

## What Happened

BUG-227. BR-8's skill refusal chose its closing clause by *who asked*
(`SkillCaller`). The typed-user clause, "Nothing was sent and no provider saw
this turn", was true where it was written: the pre-dispatch stages, which run
before `CarriedTurn::begin`. REQ-587 then reused the same composer at the
mid-turn reroute guard, and fixed only the **model** arm there (BUG-188). A typed
`/analyze` that had already run two billed kimi calls was then pinned to local by
a shell classifier verdict, refused by the guard, and told the user no provider
had seen the turn.

The integration test for that exact seam asserted the false clause. A few lines
below, its own non-vacuity check asserted that the expansion *had* reached the
provider. The test held both claims and passed.

## Lesson

1. **A clause about what did not happen is a property of the point in the
   pipeline where the sentence is composed, not of who asked.** When a composer
   is reused at a new seam, re-derive every negative clause ("nothing was sent",
   "no provider saw", "nothing was folded") at that seam. Give the seam its own
   entry point (`skill_refit`) so the old stages cannot pass the wrong tail, and
   the new one cannot forget to.
2. **Read a test's non-vacuity assertions against its main assertions.** A
   non-vacuity check states a fact about the run. If the main assertion claims
   the opposite fact about the same run, the test is pinning a bug. Here "the
   expansion reached the provider" sat beside "no provider saw this turn".
3. Staying inside the clippy argument limit by adding
   `#[allow(too_many_arguments)]` is refused by `suppression_ratchet`. Bundle
   the pair that is really one measurement (`Candidate { fit, body_bytes }`)
   instead.

## Why It Matters

The refusal sentence is the user's one account of what happened. A false
"nothing was sent" hides spend that was already made, and hides the actual
cause, a mid-turn pin. The user then goes looking for a skill-size problem
instead of the shell pin that moved the turn.

## Applies When

- Reusing a message composer, error constructor or typed outcome at a new stage
  or seam (reroute, retry, fallback, cancel).
- Writing or reviewing a test whose non-vacuity setup proves something happened
  (a request was sent, a block was committed).
- Any sentence that claims an absence: nothing sent, nothing kept, nothing ran.
