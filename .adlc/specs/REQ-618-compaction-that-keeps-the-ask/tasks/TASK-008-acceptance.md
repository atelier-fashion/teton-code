---
id: TASK-008
title: "Acceptance: the ask survives, across one compaction and across two prompts"
status: draft
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-006, TASK-007]
---

## Description

The end-to-end evidence: the three ACs that are about a whole session rather
than a unit, including the reconstruction of the 2026-09-04 transcript's shape.

## Files to Create/Modify

- `crates/tetond/tests/compaction_keeps_the_ask.rs` — new integration suite

## Acceptance Criteria

- [ ] AC-1: a stub engine at 21,162 tokens, a 25 KB body admitted through BR-4's
      `proceed once`, then 40 KB of tool results; after the compaction the prompt
      block and the body are **byte-identical** to what was pushed and every
      dropped block is a tool result — read from `into_retained`, not inferred
      from an error (LESSON-519).
- [ ] AC-7: across two prompts with a compaction between them, the second
      prompt's request body contains the first prompt's text verbatim; on the
      third it may be summarized.
- [ ] AC-8: a **reconstruction** of the transcript's third and fourth prompts at
      the original 21,162-token budget — the `/analyze` prompt line and *"where
      are the results?"* both appear verbatim in the fourth prompt's request
      body. The test's doc comment states it reconstructs rather than replays,
      and why (ADR-618-8).
- [ ] Each assertion is shown to be able to fail: revert the anchor guard, record
      how many of these go red in the suite's doc comment.
- [ ] `cargo test --workspace --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-1 | test-case | `tests/compaction_keeps_the_ask.rs::the_ask_and_the_body_survive_a_compaction` | yes |
| AC-7 | test-case | `tests/compaction_keeps_the_ask.rs::the_previous_prompt_survives_into_the_next` | yes |
| AC-8 | test-case | `tests/compaction_keeps_the_ask.rs::the_reconstructed_session_keeps_both_prompts` | yes |
| BR-8 | test-case | `tests/compaction_keeps_the_ask.rs::the_anchor_lapses_two_prompts_later` | yes |

## Technical Notes

Assert on the **request body** the transport would send, not on the manager's
internal state, wherever the AC says "request body" — that is what makes AC-7 and
AC-8 evidence about egress rather than about a struct. The egress-capture harness
in `tests/egress_capture.rs` is the precedent.
