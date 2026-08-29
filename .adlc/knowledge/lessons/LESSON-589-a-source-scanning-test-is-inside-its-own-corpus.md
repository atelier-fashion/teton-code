---
id: LESSON-589
title: "A source-scanning test is inside its own corpus, and substring matching has no word boundary"
component: "tetond/tests"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "maintainability"]
tags: ["structural-tests", "source-scan", "region-check", "false-positive", "req-597"]
req: REQ-597
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-597 added two structural tests that scan source text: AC-8's region check
(the builtin boundary set is composed in exactly one place) and AC-9's
shared-body check (both `boundary list` surfaces still call one function).

Both were wrong on first run, in two different ways, and each failure looked
like a real violation:

1. **The scan counted itself.** `include_str!("main.rs")` inside `main.rs` puts
   the test's own filter line — `.filter(|line| line.contains("boundary_list_on("))` —
   into the corpus. The count came back one too high, reporting a surface that
   did not exist. Fixed by truncating at the `#[cfg(test)]` marker and scanning
   the production half only, which is what the codebase's older scans already do
   for a different reason (BUG-159).

2. **A substring is not an identifier.** `origin_label` matched the unrelated,
   pre-existing `tier_origin_label`, so the "exactly one renderer" assertion
   reported a second renderer that was a different function about a different
   kind of origin. Fixed by checking the character before the match is not
   alphanumeric or `_`.

A third, milder case in the same REQ: a per-row loop over a composed collection
went **vacuous** under the mutation that emptied the collection, so the test
survived the mutation it existed to catch. Fixed by asserting the length before
iterating.

## Lesson

Three rules for a test that reads source text:

- **Exclude yourself.** The test's own body is in the file it scans. Truncate at
  the test-module boundary before counting anything, or the test's own vocabulary
  becomes findings.
- **Match identifiers, not substrings.** `contains("foo(")` finds `bar_foo(`.
  Check the preceding character, or the check will report violations that are
  someone else's unrelated function — and a structural test that cries wolf gets
  deleted rather than fixed.
- **Count before you loop.** `for x in collection { assert!(...) }` passes
  trivially on an empty collection, which is exactly the state the mutation you
  are guarding against produces.

All three produce the same class of damage: a structural check that is *wrong
about its own subject*. A false positive costs as much trust as a miss, and a
structural test is the kind most likely to be silenced rather than debugged.

## Applies To

- Any `include_str!`-based region, sweep, or count check.
- `boundary_coverage.rs`-style tests in this repo, which are growing in number.
- Reviewing a new structural test: run it against a deliberately-introduced
  violation *and* confirm it is green on the untouched tree — both directions,
  because these fail in both.
