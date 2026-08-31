---
id: LESSON-598
title: "Decomposition invalidates \"source order is execution order\" — and every guard silently built on it"
component: "daemon/runtime"
domain: "refactoring"
stack: ["rust"]
concerns: ["maintainability", "reliability"]
tags: ["derived-checks", "source-scanning", "decomposition", "false-green", "ordering-invariants"]
req: REQ-600
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-600 split a 1,084-line `run_prompt_turn` into a 177-line orchestrator and
eight stage methods. The behaviour was preserved; the suite stayed green
throughout. What did not survive was an assumption nobody had written down.

Inside one function body, **textual order is execution order**. Several derived
checks in this codebase quietly relied on that, and a decomposition falsifies it
in one commit:

- `the_claim_is_taken_before_the_registry_is_re_read` compared the byte offsets
  of `try_begin_turn` and the registry re-read. Its span was unbounded — from
  the function's start to the end of the concatenated corpus. Review constructed
  the exploit and it worked: delete the re-read, put it in a helper *defined
  below*, call that helper from *above* the claim. Both patterns are still
  found, the offsets still compare correctly, and at runtime the registry is
  read before the claim — LESSON-539's exact regression, with a green test.
- `skill_turn.rs`'s BR-8 assertions compared the positions of `SkillStage::Body`,
  the consent seam, and `CarriedTurn::begin`. Those markers used to be
  statements in one body; afterwards they were statements in three *different
  function definitions*. The test then measured the order the stages are
  **defined in the file**, not the order they are **called** — so reordering the
  orchestrator's calls would break BR-8(c) while every assertion stayed green.
- Two more checks fired their vacuity floors, which is the good outcome: they
  said "renamed or moved — this check must follow it", and it did.

The dangerous direction is not the one the tests' own docs anticipated. Both
docs argued that *reordering the definitions* would redden them, which is true
and harmless. The false-negative — moving execution while leaving the definition
where it sits — is the one that matters, and neither doc addressed it.

## Lesson

**A source-scanning check that compares positions is asserting about a single
scope. Decomposition ends that scope, and the check does not notice.**

Three rules, each of which would have caught one of the above:

1. **Bound every span to the item you mean.** An unbounded `&source[start..]`
   is a claim about the rest of the file, and after a split the rest of the file
   is other functions. The bound is not tidiness; it is the difference between
   "inside this function" and "somewhere below here".
2. **Position checks belong on the call sequence, not the definitions.** After a
   decomposition the orchestrator's body is the only text where order still
   means order. Assert there. Definition order is a layout convention that a
   future edit is free to change — and *should* be free to change.
3. **Add a uniqueness floor to anything you locate by pattern.** "The registry
   re-read appears exactly once" turns relocation into a red test instead of a
   silent move. A check that finds its subject cannot tell you whether it found
   *the* subject or *a* subject.

And the meta-rule, which is why this is a lesson rather than three bug fixes:
**when a change alters program structure, re-run every derived check's mutation
— do not re-read the check.** A guard that has stopped covering its subject
looks exactly like a guard that passes. In this REQ,
`the_turn_path_takes_no_blocking_wait` silently stopped covering the stages the
moment one line moved into a helper; the inversion that had gone red went green,
and nothing else in 4,000 tests noticed.

## Why It Matters

These checks exist because the properties they hold are ones no compiler and no
integration test can see: an ordering whose violation only bites under a race, a
refusal that must happen above a commit point. They are the last line for
exactly the invariants a refactor is most likely to disturb — which makes "the
refactor quietly disarmed them" the worst possible failure, and a completely
plausible one.

It compounds with [[lesson-594]]: a decomposition changes what the corpus means.
That lesson is about scans seeing *less code*. This one is about scans seeing
the same code and drawing a conclusion that stopped being true.

## Applies When

Splitting any long function or module that source-scanning checks assert about;
writing a position-comparing check (`at(a) < at(b)`) — ask what scope makes that
comparison meaningful and bound the span to it; and reviewing a refactor that
leaves a derived check untouched, which is evidence the check was not consulted,
not evidence it still holds. See [[lesson-585]] for keying on the hazard,
[[lesson-596]] for watching the widest spelling, and [[lesson-597]] for why
re-derivation alone does not find these.
