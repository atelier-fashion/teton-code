---
id: TASK-275
title: "Measure what the full window costs, and write the runbook"
status: complete
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
  section holding the numbers (appended; no existing ADR touched)
- `docs/manual-verification.md` — the AC-14 dogfood leg
- `crates/tetond/tests/compaction_cadence.rs` — **new.** AC-11 as a CI test, since its half of
  the measurement needs no engine
- `crates/teton-inference/examples/local_budget_cost.rs` — **new.** The AC-10 harness, behind
  `--features llama`, so the recorded timings can be re-taken rather than merely trusted

## Acceptance Criteria

- [x] AC-10(a): **taken.** 6,164 tokens → 3,111 ms; 15,410 tokens → 13,548 ms. Token ratio
      2.50×, **prefill time ratio 4.35×** (second run 4.43×). Prefill is **not** linear — a
      five-point sweep shows per-token cost nearly doubling across the range. That is the
      finding; see architecture.md § Measurements
- [x] AC-10(b): **taken, and the duty does NOT still pass.** Short prompts: 151 ms / 100.27
      tok/s → Pass. Behind a full-budget context: 12,885 ms / 9.65 tok/s → **Fail**, on
      `max_first_token_ms` (1,000 ms), *not* on `min_tokens_per_sec`. Decode alone holds at
      79–82 tok/s (vs 135–139 short), ~16× above the 5.0 floor. The measurement is discharged;
      the pass condition is not met, and architecture.md records why and what it does and does
      not imply
- [x] AC-11: **asserted in CI** — `crates/tetond/tests/compaction_cadence.rs`, driving the
      production turn loop over a scripted local engine. 4 B/word 9→14 turns (1.56×); 6 B/word
      9→10 (1.11×); 8 B/word 8→8 (1.00×); 20 B/word 4→4 (1.00×). The word threshold moves
      2,867 → 7,168 as expected, but the **byte** threshold moves 22,937 → 21,504 — down — and
      it is the byte half that binds all real content after this REQ
- [x] AC-14: **written** — `docs/manual-verification.md`, "REQ-590 AC-14 (the engine-derived
      local budget)", five legs and a sign-off block. **Not run**: its checkbox in
      `requirement.md` stays unticked until a person fills the block in, which is the point
- [x] Every number recorded with the machine it was taken on — Apple M5 Max / 48 GiB /
      macOS 26.6.2, `qwen3-coder-30b-a3b.gguf` Q4_K at `n_ctx = 16,384`, 2026-08-25, two runs

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
