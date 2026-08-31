---
id: ASSUME-026
title: "The BR-3 ordering invariants are the whole behavioural risk of decomposing the turn path"
status: invalidated
req: REQ-600
created: 2026-08-31
resolved: 2026-09-01
---

## Assumption

REQ-600's spec stated it plainly: *"The ordering invariants in BR-3 are the whole
behavioural risk. Everything else here is relocation and renaming."*

The plan followed from it. Three unpinned invariants were written first, before
any code moved, precisely so the restructure would have a net. That was the right
call and it is not what this entry disputes.

## Context

REQ-599 named five ordering invariants as the risk of splitting the turn path,
and REQ-600 inherited the framing. It shaped the whole REQ: TASK-308 came first
and cost the most care, the architecture doc's strongest claim (ADR-2) is about
an ordering, and AC-4 is the criterion with the most conditions attached.

Treating relocation and renaming as *not* a behavioural risk is what made a
mechanical regex rename over an extracted region feel safe.

## Resolution

**Invalidated — by the one Critical this REQ shipped, which was not an ordering
defect at all.**

Moving locals into parameter bundles required re-spelling every use
(`edited` → `latches.edited`). Applied as a word-boundary regex over the
extracted region, it rewrote a **string literal that is pushed into the model's
context** — BR-6's mandatory-verification nudge became *"You latches.edited a
file but have not latches.verified the change."* That fires under the `Degraded`
harness profile, which exists for weak models: the reader least able to parse it
is the only one who ever sees it. Thirteen comment sentences took the same
damage.

Every ordering invariant held. The behaviour change came from the half of the
work the assumption had classified as risk-free — and "renaming" was named in
the assumption's own sentence as the safe part.

Two second-order findings point the same way:

- Three of the four ordering guards the REQ shipped were weaker than claimed
  (one keyed on the remedy rather than the hazard, one asserted against an
  outcome the code makes impossible, one sliced to the end of the corpus). So
  even the ordering half was less protected than the plan assumed.
- No test asserts on the nudge sentence. The compiler, clippy and 4,074 tests
  were all silent, because none of them is capable of noticing.

**What replaces it.** In a relocation, the risk is not only "does the order still
hold" but "did the edit stay inside the code". A mechanical edit whose scope is a
block of *text* rather than a syntax tree reaches strings and comments, and those
are exactly where the automated checks do not go. See
[[lesson-599-a-bulk-rename-does-not-stop-at-code]].
