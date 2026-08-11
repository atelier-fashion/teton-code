---
id: LESSON-507
title: "A superseded ADR that keeps its original Decision line is still the one that gets implemented"
component: "adlc/architecture"
domain: "adlc"
stack: []
concerns: ["documentation-integrity", "review"]
tags: ["adr", "supersede", "task-drift", "mutation-check", "spec-hygiene"]
req: REQ-557
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-557's architecture reached the right answer twice and recorded the wrong one
three times.

ADR-E ("a missing model is a **usability** condition, not a **validity**
condition") explicitly overturned ADR-B, and said so in the strongest terms an
ADR can: *"putting it in `validate()` is the obvious reading and it is wrong."*
But ADR-B kept its original title — "required in validation" — and its original
Decision line: *"required-ness is enforced in `Config::validate()`."* Only its
Consequences paragraph pointed forward to ADR-E.

The stale claim then propagated into the places an implementer actually reads
first:

- The Approach section's seam list: *"`Config::validate()` enforces both."*
- TASK-043's Description: *"Both are enforced in `Config::validate()` rather than
  by the deserializer."*
- TASK-043's own migration criterion: unresolvable providers are *"left for
  `validate()` to reject by id"* — which contradicts **that same file's** two
  criteria above it, both of which correctly pin that `validate()` accepts.

The implementation survived only because TASK-047 carried Mutation check C —
"moving the model requirement into `Config::validate()` makes at least one
startup test red." The test caught what the prose was still asking for. Found at
`/validate` six days after merge; no code defect, four documents wrong.

## Lesson

When an ADR supersedes an earlier one, **edit the earlier one's Decision line**,
not just its Consequences. A reader resolving "what was decided?" reads the title
and the Decision and stops — that is what those fields are for. A forward
reference buried below them is a footnote to a sentence that already answered the
question incorrectly.

Then grep the superseded claim across the whole spec directory. An ADR reversal
that lands only in the ADR is half-applied: the Approach summary and every task
file that restated the original decision are still carrying it.

## Why It Matters

A task file is an instruction to an implementer who may not read the ADRs at all.
A contradiction *inside one task file* — as TASK-043 had — means at least one of
its criteria is guaranteed to be implemented wrong, and there is no signal saying
which. Here the contradiction pointed at a change that bricks daemon startup on
every pre-existing config (see [[LESSON-506]]).

The rescue was a mutation check, not a review. That is worth noting in both
directions: the mutation check earned its keep, and prose review did not catch a
plain self-contradiction in a 100-line file.

## Applies When

- Any ADR written to overturn an earlier ADR in the same document, especially one
  added during a verify or re-verify pass.
- Writing or validating task files that restate an architectural decision in
  their own words rather than citing it.
- `/validate` on a REQ whose architecture has more ADRs than the task graph has
  tiers — a sign decisions were revised after tasks were cut.
