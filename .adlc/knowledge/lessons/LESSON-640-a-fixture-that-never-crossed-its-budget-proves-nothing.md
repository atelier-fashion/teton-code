---
id: LESSON-640
title: "Run the inversion on every test in the batch, not on the one you doubt"
component: "adlc/proceed"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "review"]
tags: ["inversion", "mutation", "vacuity", "non-vacuity", "REQ-618"]
req: REQ-618
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-618's acceptance suite is four tests, all written the same way, all passing.
The REQ's guard was then reverted — the one line that stops `truncate_to_budget`
dropping an anchored block — to check they would have caught its absence.

Three went red. The fourth did not.

`the_previous_prompt_survives_into_the_next` built a conversation of a prompt
plus twenty-six 2 KB tool results, against a 63,486-byte budget. That is 52 KB —
**under budget**. The gate ran and dropped nothing, so "the ask survived" was a
statement about a turn that had never been pressured. It would have passed on a
build with no anchor rule at all, and it passed for the whole of the run that
wrote it.

Doubling the results to 4 KB and asserting `report.dropped_blocks > 0` fixed it.
All four now redden.

## Lesson

The inversion is not a spot-check on the test you are least sure of. Run it on
**every** test in the batch, and count the reds. The count is the finding: three
out of four is not "the inversion worked", it is one vacuous test you did not
know you had.

This is cheap to do and hard to substitute for. Every one of these four tests had
a non-vacuity assertion in the sibling that shared its shape; the one that failed
to redden was the one whose non-vacuity assertion had been left out, and nothing
about reading it suggested that — the fixture *looked* over budget, because 26 ×
2 KB looks like a lot until you compare it to 63 KB.

Corollary for fixtures sized by arithmetic: assert the arithmetic. A fixture that
depends on crossing a threshold should assert it crossed it, in the same test,
before asserting anything about what happened next. `assert!(report.dropped_blocks > 0)`
is one line and it is the difference between a test and a decoration.

## Related

- LESSON-598 — re-run a derived check's mutation after any change to program
  structure. This is the same rule applied at authoring time rather than at
  change time.
- REQ-592, LESSON-569 — seven green assertions that could not have failed.
