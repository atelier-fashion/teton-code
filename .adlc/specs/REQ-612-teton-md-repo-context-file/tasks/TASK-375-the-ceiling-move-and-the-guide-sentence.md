---
id: TASK-375
title: "The reviewed ceiling move: overhead 14→22 KiB, sweeps at the cap, margins re-pinned, guide sentence amended"
status: draft
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-373]
---

## Description

The measuring task, which runs after the composer has the block (LESSON-541): both resident-
ceiling sweeps build the worst-case prompt with `RepoContextBlock::worst_case()`, the overhead
constant moves once with its chunk arithmetic re-derived and re-stated, the two recorded margins
are re-pinned to what the sweeps now measure, and the guide's capability sentence is amended
with its five needles re-worded (BR-3's ceiling half, BR-8, AC-4, AC-11).

## Files to Create/Modify

- `crates/tetond/src/egress/redact.rs` — `REDACT_BODY_OVERHEAD_BYTES = 22 * 1024`; the doc
  ledger gains the REQ-612 entry with the re-derived chunk window, `REDACT_TOTAL_CAP_CHUNKS`,
  `REDACT_INPUT_MAX_BYTES` and the scannable bound (compute; do not add 8,192 to the old
  numbers — LESSON-593); `the_total_cap_clears_the_harness_context_budget_with_margin`
  builds its prompt with the worst-case block; `RECORDED_PROMPT_MARGIN_BYTES` re-pinned to the
  measured value.
- `crates/tetond/src/harness/tools/web.rs` — `the_web_tool_docs_clear_the_outbound_body_overhead`
  builds the block too; `RECORDED_WEB_PROMPT_MARGIN_BYTES` re-pinned.
- `crates/tetond/src/harness/self_config.md` — line 4 becomes: *Teton loads skills and commands
  from `.claude/` and `~/.claude`, and the repository notes from `TETON.md` (or `AGENTS.md`) at
  the session root, but nothing else there (no CLAUDE.md, agents or hooks); …* — the rest of
  the sentence unchanged.
- `crates/tetond/src/harness/turn_loop.rs` — `the_system_prompt_states_what_the_session_can_run_and_from_where`:
  the two REQ-585 needles re-worded (`loads skills and commands from`, `no CLAUDE.md, agents or
  hooks` both survive; add `repository notes from` and `TETON.md`); every other anchor unchanged.
- `crates/tetond/src/harness/tools/docs.rs` — the headroom sentence in the module docs.

## Acceptance Criteria

- [ ] AC-4: both sweeps clear the new overhead by at least `MIN_PROMPT_HEADROOM_BYTES` with the
      block at the cap; removing the block from either sweep fails a test naming the reason;
      the chunk-arithmetic test re-derives and is green; both margin pins equal the measured
      values (run the sweep, read the number, pin it — record the mutation).
- [ ] AC-11: the guide test passes with its re-worded needles; exactly one `/help` line; the
      `cli_rows.rs` cross-check finds no shell form; the sentence still precedes step 1 and is
      present in both harness shapes.
- [ ] BR-8: a needle for `repository notes from` and one for `TETON.md` are asserted
      separately, so the next amendment re-words rather than deletes (BUG-181's rule).
- [ ] LESSON-570: the sentence is true of the product after TASK-374 lands and after this
      task alone (it describes loading; loading exists once TASK-374 merges — order this task
      after TASK-374 in the merge sequence, or land them together).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/src/egress/redact.rs::the_total_cap_clears_the_harness_context_budget_with_margin` | no |
| BR-3 | test-case | `crates/tetond/src/harness/tools/web.rs::the_web_tool_docs_clear_the_outbound_body_overhead` | no |
| BR-8 | test-case | `crates/tetond/src/harness/turn_loop.rs::the_system_prompt_states_what_the_session_can_run_and_from_where` | no |
| AC-4 | test-case | `crates/tetond/src/egress/redact.rs::the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap` | no |
| AC-11 | test-case | `crates/tetond/src/harness/turn_loop.rs::the_system_prompt_states_what_the_session_can_run_and_from_where` | no |

## Technical Notes

The overhead is a production input to every redact-scanning route's budget (REQ-586 verify (b));
say so in the ledger entry. Measure headroom before writing the sentence (LESSON-543). The
`AGENTS.md` parenthetical is ADR-7's OQ-1 decision; if product overturns it, drop the
parenthetical and its needle together.
