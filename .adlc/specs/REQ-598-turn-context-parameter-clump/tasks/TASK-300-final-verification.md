---
id: TASK-300
title: "Final verification sweep and PR body numbers"
status: complete
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-297, TASK-298, TASK-299]
---

## Description

Run every acceptance criterion end to end against the finished branch and write
the measured numbers into the PR body.

## Files to Create/Modify

- `.adlc/specs/REQ-598-turn-context-parameter-clump/requirement.md` — tick the
  acceptance criteria that verifiably hold

## Acceptance Criteria

- [ ] AC-3: `cargo clippy --workspace --all-targets` clean, with the known
      coverage limit restated (it does not compile the `llama` block).
- [ ] AC-4: `cargo test --workspace --no-fail-fast`, output **grepped for
      `FAILED`** rather than trusting a summed pass count — an interrupted
      fail-fast run reports a floor, not a total (conventions.md, LESSON-533).
- [ ] AC-1: the final suppression count is measured and recorded in the PR body,
      **split into vestigial and earned**.
- [ ] AC-10: the event-ordering fixture from TASK-293 still matches.
- [ ] Every AC in the requirement is either ticked with evidence or explicitly
      called out as unmet, with a reason. No AC is ticked because it "should"
      hold.
- [ ] The PR body names anything found and deliberately not fixed, per the
      requirement's Out of Scope rule that incidental fixes get filed, not
      folded in.

## Technical Notes

`cargo test --workspace --no-fail-fast` on this workspace runs ~4,000 tests and
takes several minutes. Run it once, capture to a file, and grep that file —
re-running to double-check is slower than reading the capture.

Report honestly on the preservation criteria. AC-6 and AC-7 are regression
guards for hazards this REQ does not introduce; saying so is more useful than
implying they proved something about this diff. "I could not find a problem" and
"there is no problem" are different claims — say which one applies.
