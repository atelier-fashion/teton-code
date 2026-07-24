---
id: LESSON-441
title: "A fix pass is new code — re-verify it adversarially, not by test count"
component: "adlc/review"
domain: "process"
stack: ["adlc", "rust"]
concerns: ["reliability", "developer-experience"]
tags: ["re-verify", "regression", "fix-pass", "review-gate", "mutation-check"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

Twice now, across REQ-544 and REQ-547, a Phase-5 fix pass closed every finding,
reported a green suite, and had **introduced new defects of the same or higher
severity**. REQ-544's pass introduced two Majors (a truncated prompt could start
with an assistant message → provider 400; a downed provider could be stranded
permanently). REQ-547's pass introduced a **Critical**: routing a config pin into
the consent proposal was the correct fix, but `decide()` honours a pin with no
RAM-floor test and `Accept` was the one branch that never called
`validate_choice` — so a pinned oversized model could be installed with a single
Enter, bypassing the very confirmation rule the REQ existed to enforce. That path
was *unreachable before the fix*. The same pass also reintroduced a defect class
it had just removed (a multi-GB hash back on a tokio worker) and made a
"fail loudly" refusal invisible by writing it to a nulled stderr.

Every one of these was found by a **re-verify pass**, never by the green suite.

## Lesson

Treat a fix pass as new, unreviewed code — because it is. After remediating
review findings, re-dispatch the reviewers whose dimensions were touched, scoped
to the fix commits, and explicitly ask "what did these fixes break?" rather than
"are the fixes present?". A passing test count proves the old tests still pass;
it says nothing about behaviour the fix newly made reachable. Pay special
attention when a fix **widens an input space** (a pin that previously never
reached a code path now does) — that is where the new-reachability bugs live.
Mutation-check the fix's own tests: break the fix, confirm the test fails.

## Why It Matters

The failure mode is uniquely expensive because it is invisible and
confidence-inverting: the team is *more* trusting after a remediation pass, and
the defects introduced are in exactly the security- and correctness-critical
code the review just flagged. Shipping REQ-547 without the re-verify would have
merged a Critical consent bypass in the feature whose entire purpose was consent.
One extra review round costs minutes; the regression costs the guarantee.

## Applies When

Completing any remediation of review findings; any change that routes a
previously-inert input into a live code path; ADLC Phase 5 Step D (treat
"re-verify" as mandatory when Critical/Major items were fixed, not conditional);
reporting a fix as done on the strength of a test count.
