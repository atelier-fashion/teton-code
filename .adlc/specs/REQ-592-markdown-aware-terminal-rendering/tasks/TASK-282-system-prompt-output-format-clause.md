---
id: TASK-282
title: "The daemon's output-format clause, measured against both prompt budgets"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: []
---

## Description

Tell the model where its words land. A named clause const, inserted by `build_system_prompt`, and
re-measured against **both** resident-prompt ceilings. Covers BR-1 and BR-2.

**No dependencies — this is the whole daemon half and runs in parallel with TASK-277..281.**

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — the clause const with its why-each-sentence doc
  comment, and its insertion point in `build_system_prompt` (~2489–2534); AC-1's test beside the
  existing prompt pins (~4906–5020).
- `crates/tetond/src/egress/redact.rs` — re-measure; touch only if a constant must move.
- `crates/tetond/src/harness/tools/web.rs` — the mirrored budget sweep re-measures.

## Acceptance Criteria

- [x] AC-1: `build_system_prompt` output contains the clause, and contains it **exactly once** —
      filter for its anchor phrase, assert the count is 1, with a message saying a second sentence
      about output format is a decision rather than an accident. Mutation-checked: remove the
      `push_str` and the test fails.
- [x] AC-2: `the_total_cap_clears_the_harness_context_budget_with_margin` (redact.rs:2276) is green.
- [x] `min_budget_bytes_holds_the_harnesss_own_system_prompt` (budget.rs:4016) is green — the
      **second** ceiling, which the spec's BR-2 did not name.
- [x] `a_harness_authored_system_prompt_is_byte_identical` (harness/render.rs:825) is green — so
      the clause carries no flush-left `User:` or `Assistant:` label.
- [x] If either constant moved: (n/a — neither moved; redact margin 476 -> 129, default prompt 6,411 -> 6,758)
- [ ] ~~If either constant moved:~~ the PR shows the re-stated chunk count and scannable bound
      (`the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`), and the new margin is
      recorded in the constant's doc-comment ledger beside the REQ-577/BUG-181/REQ-587 entries.

## Technical Notes

**Both budgets, measured on this branch before any edit:**

| Budget | Shape | Ceiling | Current | Slack |
|---|---|---|---|---|
| `MIN_BUDGET_BYTES >= 2 × prompt` (budget.rs:4016) | default config | 8,192 | 6,411 | 1,781 |
| `REDACT_BODY_OVERHEAD_BYTES` (redact.rs:2276) | worst case + skill roster | 11 KiB | — | 710 |

Redact binds first. Candidate wordings measure 184–322 bytes, so **neither constant is expected to
move** — but measure, do not trust these figures.

**The clause is a const beside `WEB_OFF_AVAILABLE_CLAUSE`, not a line in `self_config.md`.** The
guide's lines are pinned by whole-line and per-segment assertions tuned by REQ-579's live A/B, and
the spec's Out of Scope forbids re-opening them to pay for bytes. Follow `web_capability_clause` /
`effective_web_clause`: the words live in the clause, `build_system_prompt` decides only its place.

AC-1's shape already exists — `the_system_prompt_states_what_the_session_can_run_and_from_where`
(turn_loop.rs:5015) pins BUG-181's capability sentence by presence *and* uniqueness. Copy it,
including the "if reworded deliberately, update this expectation; do not delete the assertion"
failure message.

Content: a plain terminal that renders no Markdown; prefer short paragraphs and bullet lists;
tables only when genuinely clearest, at most three short columns, never a sentence in a cell.
Prompt-adjacent behaviour is chaotic under byte-level changes (BUG-168), so treat the exact wording
as unverified until AC-13's live check.

**Editing `tetond` trips the staleness guard** (`tests/common/mod.rs:46`): run
`cargo build --workspace` before any `cargo test -p teton`, or the CLI suite silently tests a daemon
without this clause (BUG-164, [[LESSON-510]]).
