---
id: TASK-009
title: "Measure the composed prompt, re-pin the margins, and replay the transcript"
status: complete
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-003, TASK-004, TASK-005, TASK-006, TASK-007, TASK-008]
---

## Description

The task that measures a composed artifact, and it runs after every task that
writes to it (architecture ADR-8, REQ-612's rule, LESSON-541). Also carries the
end-to-end replay (AC-8) and the BR-9 regression sweep.

## Files to Create/Modify

- `crates/tetond/src/egress/redact.rs` — `RECORDED_PROMPT_MARGIN_BYTES`,
  `RECORDED_WEB_PROMPT_MARGIN_BYTES`, and a ledger line on
  `REDACT_BODY_OVERHEAD_BYTES` naming REQ-615.
- `crates/tetond/tests/` — the AC-8 replay.

## Acceptance Criteria

- [ ] Both margin pins are **re-measured** from the built prompt, not derived by
      subtracting this REQ's sentences from 733/780.
- [ ] The margin still clears `MIN_PROMPT_HEADROOM_BYTES` (48). If it does not,
      shorten a sentence — do **not** raise `REDACT_BODY_OVERHEAD_BYTES`, which is
      a whole-KiB move with a scannable-bound consequence belonging to its own
      REQ.
- [ ] `REDACT_BODY_OVERHEAD_BYTES`'s ledger gains one line saying which bytes
      REQ-615 spent and on what.
- [ ] AC-8 replay: the 2026-09-04 tool sequence, replayed call-for-call against a
      stub model, is answered by the harness with — every `cd`-bearing shell
      result carrying the BR-2 note, the `mkdir -p .adlc/context …` refused by
      BR-4 creating nothing, and the `/analyze` invocation refused by BR-5. The
      assertions are on the harness's outputs, never on the stub's call count.
- [ ] BR-9 sweep: with `kind == Project`, the full suite is green with no test
      edited to accommodate this REQ, and the environment block is
      byte-identical to `main`'s.
- [ ] `cargo test --workspace --no-fail-fast`, output grepped for `FAILED`
      (conventions.md: a summed count from a fail-fast run is a floor, not a
      total).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/egress/redact.rs::the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound` | no |
| BR-3 | test-case | `crates/tetond/src/egress/redact.rs::the_recorded_prompt_margin_is_what_the_prompt_actually_leaves` | no |
| BR-9 | test-case | `crates/tetond/tests/session_root_honesty.rs::a_project_session_is_unchanged_by_req_615` | yes |
| AC-8 | test-case | `crates/tetond/tests/session_root_honesty.rs::the_2026_09_04_sequence_is_answered_by_the_gates` | no |
| AC-2 | test-case | `crates/tetond/src/egress/redact.rs::the_recorded_prompt_margin_is_what_the_prompt_actually_leaves` | no |

## Technical Notes

**Re-measure, do not reason** — that correction is what reasoning about this cost
last time (`RECORDED_PROMPT_MARGIN_BYTES`'s own doc comment). Run the test, read
the actual number out of the failure, write it down, and add the ledger line in
the same diff.

**REQ-617 moves these same two pins concurrently.** If this branch rebases onto a
merged REQ-617, re-run the measurement after the rebase: a pre-rebase figure is
stale by construction, and the pin is an `assert_eq!` that will say so.

AC-1's live three-of-three trial on the shipped local model is a manual step and
is **not** automatable here; record its outcome in the PR body rather than
pretending a test covers it.
