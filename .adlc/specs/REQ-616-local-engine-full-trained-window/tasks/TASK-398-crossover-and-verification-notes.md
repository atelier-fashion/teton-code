---
id: TASK-398
title: "The crossover test, the reference-workload replay, and the recall-trial record"
status: draft
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-392, TASK-393]
---

## Description

Pin the word/byte crossover as an asserted fact rather than an assumption
(AC-4), replay the 2026-09-04 reference workload against a 262,144 stub (AC-7),
and record the manual recall trial (AC-12).

## Files to Create/Modify

- `crates/tetond/tests/token_corpus.rs` — the crossover test and the reference
  workload replay
- `.adlc/specs/REQ-616-local-engine-full-trained-window/verification-notes.md` —
  the AC-12 dogfood record

## Acceptance Criteria

- [ ] The crossover test computes `budget_bytes / budget_tokens` and asserts it
      is exactly 3.0 at **both** 32,768 and 262,144 — the ratio is scale-free
- [ ] It asserts the byte half is the binding half for all three reference
      contents (prose, code, base64) at both windows, and that this is
      *unchanged* by the raise (AC-4)
- [ ] It pins the effective capacity: ≈522,240 bytes against ≈63,488 today
- [ ] The reference workload replays with no `context_pressure` at 262,144, and
      **does** emit it at 32,768 — the mutation that proves the test can fail
      (AC-7)
- [ ] `verification-notes.md` records the AC-12 recall trial: the KV type used,
      three runs, the three planted marks, and an explicit note that this is a
      dogfood result and not a CI assertion

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-4 | test-case | `crates/tetond/tests/token_corpus.rs::crossover_is_three_bytes_per_word_at_every_window` | no |
| AC-7 | test-case | `crates/tetond/tests/token_corpus.rs::reference_workload_replay_pressured_only_at_32k` | yes |
| AC-12 | structural-check | `.adlc/specs/REQ-616-local-engine-full-trained-window/verification-notes.md`: recall trial recorded | no |

## Technical Notes

- LESSON-565 is the reason AC-4 exists in this shape. The original criterion
  asserted the byte half is *never* binding for prose or code, which is false and
  scale-invariant: both halves scale by the same factor, so the crossover cannot
  move. The test asserts what is true, which is the only version that can fail
  for the right reason.
- ASSUME-022 (invalidated) is the standing caveat: the 2 B/token bridge is not a
  floor, and `numeric_grid.txt` overruns the window with both guards admitting.
  That is unchanged in ratio by this REQ and eight times larger in absolute
  tokens; the engine's typed `context_length_exceeded` remains the only backstop.
  Do not assert a guarantee this REQ does not provide.
