---
id: ASSUME-027
title: "`run_prompt_turn`'s concerns factor into stages with nameable boundaries"
status: validated
req: REQ-600
created: 2026-08-31
resolved: 2026-09-01
---

## Assumption

That the seven concerns crammed into one 1,084-line `async fn` — session
claiming, skill expansion, routing, budget, consent, dispatch, commit — would
factor into stages each of which has a boundary you can name, rather than being
genuinely interleaved.

The spec required this be confirmed before committing to a shape, "and say so
plainly if they do not".

## Context

REQ-599 had already shown the opposite can happen: its `provider` slice measured
as scattered across 10,366 lines at planning time and was 375 contiguous lines
after four unrelated slices left. So cohesion was to be re-measured as the work
proceeded rather than assumed from the plan.

The evidence gathered up front was an escape analysis — for each candidate
boundary, how many values cross it. ADR-3 recorded eight boundaries leaking
between 0 and 8 values and concluded: *"A boundary that needed fifteen values
would not be a seam; these are."*

## Resolution

**Validated — the stages are real. The cost of crossing them was under-predicted.**

Eight stages shipped, each independently nameable, and `run_prompt_turn` went
from 1,084 lines to 188. The strongest evidence that the boundaries are genuine
is ADR-2's outcome: `TurnContext::new` and the warming hold it must follow were
57 lines apart inside the old body, held together by a comment, and are now
adjacent statements. A false seam could not have produced that.

**The qualification, re-derived after implementation.** The escape analysis
measured what crosses a boundary, not what a stage *needs*. Delivered:
`resolve_the_route` takes 13 values and returns 5; `run_attempts` takes 15
across three carriers — the figure ADR-3 said would disqualify a seam. A stage
needs everything its own sub-calls need, and the analysis only counted what
crossed the line.

That gap is why the REQ ended with thirteen parameter bundles where roughly five
carry an invariant. Filed as REQ-606 to classify them; the deliverable there is
the classification, not a smaller number.

**What to reuse.** Escape analysis is a good *cheap* signal that a boundary
exists. It is not a estimate of what the signature will cost, and it should not
be quoted as one — ADR-3's table now says so beside the original figures rather
than replacing them.
