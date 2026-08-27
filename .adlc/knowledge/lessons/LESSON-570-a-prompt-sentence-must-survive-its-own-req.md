---
id: LESSON-570
title: "A prompt sentence must be true after the REQ ships, not before it"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "correctness"]
tags: ["system-prompt", "capability-claim", "bug-181", "clause", "surface-truth", "prompt-budget"]
req: REQ-592
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

REQ-592 added an output-format clause to the system prompt so the model would stop writing
180-column tables into an 80-column terminal. The spec's BR-1, which I wrote, said the clause
should state that the terminal **"renders no Markdown."**

That was true when I wrote it and **false by the end of the same REQ**. The other half of REQ-592
teaches the CLI to render bold, emphasis, code spans, headings and tables. Had it shipped, the
system prompt would have carried a false claim about Teton's own surface — which is precisely
BUG-181's defect class, and BUG-181 is the bug REQ-592 cites as its motivation. We would have
fixed a false-claim-about-our-own-product bug by introducing another one in the same commit.

The implementer flagged it rather than deviating unilaterally, and the fix was to lead on the fact
that *stays* true: the terminal is **narrow**, and a wide table is unreadable however it is laid
out. Narrowness survives the renderer; absence-of-rendering did not.

A guard was added that is stronger than the wording fix: an assertion that **no line of the whole
prompt** claims the surface renders no markdown — swept across the guide and tool docs, not just
the clause. The cheapest way for the false sentence to return is someone restoring the "clearer"
earlier wording without knowing why it went, and a doc comment does not stop that.

## Lesson

**Write a prompt sentence against the product as it will exist when the REQ lands, not as it exists
when you write the spec.** A REQ that changes a capability and describes that capability in the
same breath has a window where the description is true of neither the old product nor the new one.

Two supporting habits:

- **Prefer the invariant fact over the current fact.** "Narrow" is a property of terminals;
  "renders no Markdown" was a property of one build. When both would serve, the durable one costs
  nothing and does not expire.
- **Guard the negation across the whole prompt, not the clause.** The clause is where you put the
  sentence; the guide, the tool docs, and a future capability clause are where it can come back.

## Why It Matters

The system prompt is the one place the model learns facts about Teton it cannot observe. A false
sentence there is not a typo — it is the model confidently telling a user something wrong about the
product, which is exactly what BUG-181 was filed for. And prompt-adjacent behaviour is chaotic
under byte-level changes (BUG-168), so a sentence that has to be re-tuned later is expensive.

## Applies When

Adding or amending any system-prompt clause, especially in a REQ that also changes the capability
the clause describes. Reviewing a spec's prompt wording — ask "is this still true after the rest of
this REQ merges?" Any assertion of the form "Teton cannot X" — the guard belongs on the whole
prompt, and the reason belongs in the const's doc comment.

## Related

- [[LESSON-532]] — presence in context is not instruction following; the clause is the
  nice-to-have, the renderer is the guarantee. REQ-592's architecture is that conclusion, reached
  independently before the lesson was found.
- [[LESSON-548]] — a remedy is a claim about your own surface, and must be verified against it.
