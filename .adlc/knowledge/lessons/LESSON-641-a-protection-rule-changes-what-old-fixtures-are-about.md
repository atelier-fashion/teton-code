---
id: LESSON-641
title: "A protection rule changes what every existing fixture is a test of"
component: "tetond/harness"
domain: "testing"
stack: ["rust"]
concerns: ["testing", "review", "maintainability"]
tags: ["anchors", "fixtures", "test-adaptation", "superseded-ac", "REQ-618"]
req: REQ-618
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-618 made the user's prompt undroppable. Thirteen existing tests went red —
none of them about anchors, all of them about something else that had quietly
depended on the prompt being droppable:

- Fixtures that built a conversation out of four `push_user` calls, because that
  was the shortest way to get four blocks. Two of the four are now anchored, so
  "three blocks were dropped" became "two were".
- `a_truncated_context_whose_oldest_survivor_is_assistant_still_starts_with_user`,
  which needed the oldest user block evicted to reach the state it guards. It now
  takes three prompts to reach, because the newest two are protected.
- BUG-182's reply-reserve test, whose whole mechanism (the clamp fills the byte
  budget exactly, the next append overflows) no longer applies to the block it
  was written about.
- REQ-590 AC-12, which said the reported `/analyze` turn must serve *silently* —
  a claim REQ-618's own Description sets out to overturn.

Each one had to be handled differently, and the difference is the point.

## Lesson

When a rule takes something out of the reach of a mechanism, the tests that break
are not "tests of the new rule that need updating". They are tests of *other*
things whose fixtures happened to rely on the reach. Sort them before touching
any of them:

1. **The fixture was a convenience.** Four `push_user` calls meant "four blocks",
   not "four prompts". Move the fixture to blocks the rule does not protect —
   model turns, tool results — and the original claim is untouched. Most of them
   are this.
2. **The state is now reachable only by a longer path.** Keep the claim, extend
   the fixture, and say in a comment why it takes three prompts now. The test
   gets better: it reaches the state the way a session does.
3. **The mechanism is gone.** Assert the new outcome, and put the *old* one in
   the comment with what replaced it. `newest_user_elided` can no longer be set
   by any path; a test asserting it would have to be deleted, and a test
   asserting its absence beside the refusal that replaced it is worth keeping.
4. **A prior acceptance criterion is superseded.** This is the one that must not
   be quietly edited. REQ-590 AC-12 and REQ-618 BR-4 disagree about one turn, on
   purpose, and the later REQ says so in its Description. Record the supersession
   *at the assertion*, with both REQ numbers and the sentence from the newer spec
   that authorizes it — so the next reader meets an argument rather than a
   changed expectation.

The failure mode is treating all four as case 1 and mechanically resizing
fixtures until the suite is green. Cases 3 and 4 lose real coverage that way, and
case 4 loses a decision.

## Related

- REQ-618 BR-1/BR-2, REQ-590 AC-12, REQ-587 BR-6/BR-7.
- LESSON-640 — the inversion that catches a fixture resized into vacuity.
