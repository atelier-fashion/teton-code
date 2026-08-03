---
id: LESSON-476
title: "A fix pass can create the exposure it closes — re-verify the fix against its own new branches"
component: "adlc/review"
domain: "process"
stack: ["adlc", "rust"]
concerns: ["security", "reliability", "process"]
tags: ["re-verify", "fix-pass", "new-reachability", "adversarial-review", "phase-5"]
req: REQ-554
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

REQ-554's Phase-5 review found a Critical (untrusted content minting real
model control tokens). The fix pass closed it on the ChatML path *and*, in the
same commit, tightened format detection to route ChatML **dialects** the
renderer cannot reproduce (Phi-4's `<|im_sep|>`) to the flat fallback — a
correct, independently-motivated Minor fix.

Composed, those two changes made things worse than either alone: the flat path
had no neutralization, and the new decline clause deliberately steered
ChatML-vocab models onto it. The fix pass created a fresh, reachable instance
of the very Critical it was closing. Only the Step-D re-verify caught it, and
it caught it by asking "which paths reach the tokenizer?" rather than "is the
fix present?" — the same commit's own code (floating markers in both marker
sets, justified by "a flat-serving model can still be ChatML-native") already
contained the argument that refuted its own placement.

This is the third time in this repo's history a remediation introduced a
defect of equal or higher severity (LESSON-441 records the first two).

## Lesson

When a fix pass both **closes** a hole and **widens an input space**, re-verify
the fix against the branches it just created — not only against the original
finding. Concretely, for every fix commit ask:

- Did this add a new arm to a `match`, a new fallback, a new "declined" case?
  What guarantees does the *new* arm inherit, and which did it silently skip?
- Does another change in the same commit route more traffic onto a path the
  fix did not touch?
- Does the commit's own prose argue something the code doesn't do? (Here, the
  scanner comment argued the flat path serves ChatML-native models while the
  renderer assumed it didn't — the contradiction was inside one diff.)

Grep your own commit message for claims and check each one against the code
independently: "the renderer is the single choke point" was true of one arm.

## Why It Matters

Confidence peaks right after a remediation, which is exactly when the newly
reachable code is least examined. Security fixes are the worst case: the fix
lands in code the review just flagged, reviewers have "seen" that area, and
the residual reads as closed. Two independent re-verify agents were needed
here — one framed as correctness, one as security — and both independently
landed on the same unguarded arm.

## Applies When

Completing any Phase-5 remediation, especially one where a Critical/Major was
fixed (treat Step-D as mandatory, not conditional); any commit that mixes a
fix with a scope/behaviour change; reviewing a diff whose comments assert a
global property ("the single choke point", "always", "by construction") —
verify the quantifier.

## Related

- [[LESSON-441]] — the general form: a fix pass is new code.
- [[LESSON-474]] — the specific Critical this pass fumbled and then closed.
