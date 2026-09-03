---
id: TASK-375
title: "The reviewed ceiling move: overhead 14→23 KiB (measured; 22 was short), sweeps at the cap, margins re-pinned, guide sentence amended"
status: complete
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

- [x] AC-4: both sweeps clear the new overhead by at least `MIN_PROMPT_HEADROOM_BYTES` with the
      block at the cap; removing the block from either sweep fails a test naming the reason;
      the chunk-arithmetic test re-derives and is green; both margin pins equal the measured
      values (run the sweep, read the number, pin it — record the mutation).
- [x] AC-11: the guide test passes with its re-worded needles; exactly one `/help` line; the
      `cli_rows.rs` cross-check finds no shell form; the sentence still precedes step 1 and is
      present in both harness shapes.
- [x] BR-8: a needle for `repository notes from` and one for `TETON.md` are asserted
      separately, so the next amendment re-words rather than deletes (BUG-181's rule).
- [x] LESSON-570: the sentence is true of the product after TASK-374 lands and after this
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

## Implementation note — the ceiling landed at 23 KiB, not 22

ADR-5 and this task's own description named `22 * 1024`, which is `14 KiB + the
8,192-byte block cap`. **Measured, it does not clear.** Both sweeps now build
with `RepoContextBlock::worst_case()`; the widest opted-out prompt is 16,462
bytes and `spent` (prompt + escaping) is 22,810 — 282 bytes *over* 22,528,
before `MIN_PROMPT_HEADROOM_BYTES` is even consulted. The prompt pays 8,603 for
this REQ, not 8,192: 8,192 of capped file text, 331 for the block's own frame
(opening tag, attribution line, truncation marker, closing tag, closing
sentence) and 80 for BR-8's sentence. 23 KiB is the smallest whole-KiB
assumption that clears, and it leaves 742 / 789 on the two shapes.

This is LESSON-593/597 doing its job: the 22 was arrived at by adding the cap to
the old ceiling, which is exactly the move ADR-5 forbids for the margins. AC-4
("clear the overhead by at least `MIN_PROMPT_HEADROOM_BYTES`") is the criterion
that decided it.

**Consequence the architecture did not predict.** The raise takes the chunk
count `REDACT_TOTAL_CAP_CHUNKS` 3 → 4 (three chunks hold twice a body only while
the overhead is ≤ 21,353, so *either* figure would have moved it), the total cap
169,683 → 226,244, `REDACT_MAX_CHUNKS` 4 → 5, and therefore the scannable bound
**up** 141,224 → 184,265 rather than down: a scanned route's byte budget widens
by 43,041, and the cost lands on calls instead. Full account in
`REDACT_BODY_OVERHEAD_BYTES`'s ledger and re-stated by
`the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`.

Collateral, all from the bound moving, all re-derived rather than re-pinned by
hand: `harness::budget`'s two RedactScan fixtures (the cap that makes the clamp
bite tracks the bound: 80k → 160k), `router`'s golden `redact_scan` row
(184265 / 67479) and `tests/skill_over_budget_offer.rs`'s RedactScan route
(arguments 96,000 → 130,000 B, sized to sit above the clamp and below the
declared window, since the same route is that suite's `FitsWindow` cell).

`docs/manual-verification.md:2214` still quotes the pre-REQ capability sentence.
Left for TASK-378, which owns the docs surface.
