---
id: REQ-604
title: "The turn-ordering fixture covers a plain turn only — capture the skill and consent scenarios REQ-599 AC-6 named"
status: draft
deployable: false
created: 2026-08-31
updated: 2026-08-31
component: "daemon/runtime"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["reliability", "maintainability"]
tags: ["golden-fixture", "event-ordering", "req-599-followup", "lesson-569", "lesson-591"]
---

## Description

REQ-599 AC-6 claimed an event-sequence fixture recorded before the split replays
identically after it, "for a turn that exercises: skill expansion, a routing
decision, a consent prompt, and a successful dispatch".

The fixture — `crates/tetond/tests/fixtures/req598_turn_event_order.txt` —
records a plain typed prompt turn:

```
route_decided
session_titled
route_decided
session_update
prefix_cache
```

No skill expansion. No consent prompt. The criterion was ticked on the fixture
holding across all seven steps, which it did, for the scenario it contains.
REQ-602 TASK-306 narrowed AC-6 to what shipped and filed this REQ for the rest.

**Why this could not simply be fixed at the time.** The fixture's whole value is
its provenance: captured at `17c39ec`, before any `TurnContext` existed.
Regenerating it at today's tip would produce a golden file computed by the
subject it checks, which is not an oracle (LESSON-569, and the fixture's own
header says so). So the missing scenarios cannot be added by recording them now.

**Why it is nonetheless doable.** `17c39ec` is still in history and still
carries the `carry_runtime` and `prompt` helpers. A capture harness can be built
at that commit and the sequences recorded there, which is a genuine pre-split
recording rather than a re-derivation.

**Why it is worth the cost.** Ordering on this path has already failed once, and
failed in the way that is hardest to catch: LESSON-591's detached-naming race
passed 40/40 locally and on `ubuntu-latest`, and went red only on
`macos-latest`. The two uncovered scenarios both add events to the same
sequence. The behaviours are covered — skill-turn routing in
`crates/tetond/tests/skill_turn.rs`, consent isolation in
`crates/tetond/tests/skill_consent_matrix.rs` — but neither pins *where* their
events fall in a turn.

## Acceptance Criteria

- [ ] Two sequences are captured **at `17c39ec`**, not at tip: one turn that
      expands a skill and dispatches, one that raises a consent prompt, is
      answered, and dispatches. The capture commit is recorded in each
      fixture's header, as the existing fixture records its own.
- [ ] Each new fixture replays identically against the current tree.
- [ ] Detached events are excluded **by discriminator, not by position** —
      LESSON-591: `session_titled` and the title duty's own `route_decided` are
      both published from inside a `tokio::spawn` and their arrival order is a
      race. Any new detached event these scenarios introduce is identified the
      same way.
- [ ] Non-vacuity: each fixture asserts a positive count of the events it exists
      to pin, so a filter that ate everything cannot pass (the existing test's
      "exactly ONE route decision survives" assertion is the model).
- [ ] A transposition of two adjacent distinct events still fails, per scenario
      — the normalizer must not have been widened into an excuse.
- [ ] Suite green, grepped for `FAILED`; clippy and `fmt --check` clean.

## Assumptions

- `17c39ec` still builds. If it does not, this REQ becomes "record that the
  scenarios cannot be captured and say what covers them instead" — which is a
  real outcome, not a failure to be papered over.

## Out of Scope

- Regenerating the existing fixture. It is correct for its scenario and its
  provenance is the reason it is worth anything.
- Changing turn-path behaviour.

## External Dependencies

None.
