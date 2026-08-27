---
id: LESSON-569
title: "Seven assertions that passed but could not fail — and the three ways they got that way"
component: "cli"
domain: "clients"
stack: ["rust", "cli"]
concerns: ["test-determinism", "developer-experience", "reliability"]
tags: ["mutation-testing", "vacuous-test", "test-oracle", "fixture", "recording-surface", "structural-sweep", "verify-dont-trust"]
req: REQ-592
created: 2026-08-26
updated: 2026-08-26
---

## What Happened

REQ-592 produced **seven** assertions that were green and could not have gone red. They fell into
three distinct causes, which is what makes them worth writing down together — the fix for one does
not catch the others.

**The test consulted the code under test for its own oracle.** The CJK wrapping test's escape
hatch for an over-wide row was `wide_break(row, width).is_none()`. A module that had stopped
finding *any* break opportunities satisfied it perfectly. Caught only because the mutation run
printed "STILL PASSES".

**The fixture was built around the wrong failure mechanism.** A test meant to prove that clearing
the code-fence bit mid-turn damages code was built on `**ptr` and `*y * z`, on the theory that
those would pick up emphasis. They do not — unpaired asterisks survive classification, and
`*y * z` fails the space-flank rule. The real damage is *word-wrapping*: a long code line re-flowed
mid-token. The fixture had to be rebuilt around a line three times the terminal width. (The wrong
theory was mine, and it reached a code comment before the mutation caught it.)

**The chosen test surface abstracted away the property under test.** Two ACs — AC-9 and AC-10 —
specified `RecordingSurface` for assertions about the pending *byte buffer*. That surface records
semantic `(kind, text)` calls and has no buffer, so it cannot distinguish "emitted" from "held".
Both were unreachable as written. This was a specification error, made twice, by me.

Three more of the same family: a `set_width` call site that failed no test when deleted; a whole
75-test suite that stayed green under an inverted feature gate, because every fixture reply was a
plain paragraph that wraps identically at 80 columns; and a `sed` rename of mine that silently did
nothing (BSD sed has no `\b`) and left the suite green *because nothing had changed*.

## Lesson

**"The tests pass" is evidence only if you have shown the tests can fail.** The cheapest way to
show it is to break the thing and watch — and REQ-592's implementers ran that check routinely
enough that all seven surfaced before merge.

Three specific traps, in the order they are easy to fall into:

1. **Never let the oracle call the subject.** If the expected value is computed by the code under
   test, the test asserts self-consistency, not correctness. Name the expected rows outright.
2. **Verify the failure mechanism before building a fixture around it.** A fixture is a theory
   about how the code breaks. Test the theory first, or the fixture pins the wrong thing.
3. **Pick the test surface by what the property lives in, not by what is easy to assert.** A
   semantic recorder is the right tool for call order and the wrong tool for buffer state.

And a fourth, for reviewers: **a suite passing under an inverted gate tells you the suite cannot
see the gate.** That measurement — "invert it, count what fails" — belongs in the test's own
comment, so the next reader does not delete the one guard as redundant.

## Why It Matters

Every one of these was green in CI. A REQ can ship with a full suite, clean clippy, and seven
assertions that would not notice the feature being deleted. The only thing that separated the real
coverage from the decorative coverage was someone running the mutation.

## Applies When

Writing any test whose expected value is derived rather than stated. Choosing a test double —
ask what property you are asserting and whether that double has it. Adding a structural or gate
test — measure what fails without it and record the number. Reviewing a fix that arrives green:
ask which mutation was run, and treat "it passes" and "it would fail if broken" as two different
claims.

## Related

- [[LESSON-481]] — a gate that hides a feature also hides its tests; the inverted-gate finding is
  that lesson measured rather than predicted.
- [[LESSON-529]] — a display helper is a second parser; its re-enactment corollary is why REQ-592's
  table fixture is read from disk instead of transcribed.
- [[LESSON-568]] — the documentation-side sibling: an unverified causal claim in an ADR.
