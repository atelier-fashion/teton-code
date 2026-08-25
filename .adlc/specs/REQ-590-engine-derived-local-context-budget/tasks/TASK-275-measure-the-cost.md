---
id: TASK-275
title: "Measure what the full window costs, and write the runbook"
status: draft
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-270]
---

## Description

BR-11, AC-10, AC-11, AC-14. D-3 took the full window and accepted that a local session now holds
~2.5× more conversation before compaction fires. **The measurements report; they do not gate** —
but they must be numbers, and this REQ is the last chance to take them cheaply.

REQ-586 deferred this decision on exactly this cost, and REQ-589's AC-15 runbook — which its own
spec named as the first data point REQ-590 would need — was never written. This task ends that.

## Files to Create/Modify

- `.adlc/specs/REQ-590-engine-derived-local-context-budget/architecture.md` — a Measurements
  section holding the numbers
- `docs/manual-verification.md` — the AC-14 dogfood leg

## Acceptance Criteria

- [ ] AC-10(a): wall-clock prefill for a full-budget local prompt (~15,360 tokens) against the
      same at today's budget (~6,144). Prefill is ~linear in prompt tokens so ~2.5× is expected;
      a materially worse ratio means something other than linear cost and **is the finding**
- [ ] AC-10(b): the REQ-544 BR-8 duty (`min_tokens_per_sec: 5.0`, `benchmark.rs:43`) re-run with
      a full-budget context resident. **Pass = the duty still passes.** Note the gap this closes:
      that duty measures *generation* on a *short* prompt, so as it stands it can see neither
      prefill cost nor generation under a large resident context
- [ ] AC-11: turns-until-`under_pressure` on a real multi-turn local session, before and after.
      Expected 2,867 → 7,168 words; the interesting number is how many turns that is in practice
- [ ] AC-14: a `docs/manual-verification.md` leg — a large local turn by hand, confirming the
      reported budget matches the window and that the turn serves
- [ ] Every number recorded with the machine it was taken on

## Technical Notes

**The honest constraint: none of AC-10/AC-11 can run in default CI.** The real engine is behind
`#[cfg(feature = "llama")]` (`runtime.rs:12011`), and building it compiles llama.cpp from source
with cmake. `ScriptedEngine` (`context_pressure.rs:98`) can exercise the *logic* but measures
nothing real.

So these are **recorded measurements, not CI assertions**. Say so plainly in the architecture
doc — a criterion dressed as a test that never runs is worse than a number with a date on it
(LESSON-499's coverage-boundary point). AC-11's compaction-trigger count *can* be asserted
against `ScriptedEngine` in CI; do that part as a test and keep only the timings manual.

If the machine cannot run the real engine at all, record that as the outcome rather than
inventing figures. An absent measurement that says it is absent is fine; a fabricated one is not.
