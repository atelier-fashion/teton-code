---
id: TASK-221
title: "One sentence about who runs what, measured with the roster in place"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-211]
---

## Description

BR-8 and AC-9, plus ADR-9 — the margin tests must **see** the tool, or they pass
while the prompt grows.

## Files to Create/Modify

- `crates/tetond/src/harness/self_config.md` — line 4, amended a third time
- `crates/tetond/src/harness/turn_loop.rs` — the pinning test's needles
- `crates/tetond/src/egress/redact.rs`, `crates/tetond/src/harness/tools/web.rs` — both margin tests register a worst-case-roster `skill` tool
- `docs/manual-verification.md` — the headroom table, re-measured

## Acceptance Criteria

- [ ] The sentence keeps **five** needles, not three: `/help` (exactly one line), both `.claude/` paths, "only the user runs" — which BR-8 re-words, so the anchor moves *with* the sentence — plus REQ-585's `loads skills and commands from` and `no CLAUDE.md, agents or hooks`. BR-8's illustrative wording in the spec fails the second of those; the shipped sentence must not.
- [ ] The `asking`-line count is still 1; no `teton …` form; position before step 1; resident in **both** harness shapes.
- [ ] **Both margin tests register a `skill` tool carrying the at-cap roster.** `the_total_cap_clears_the_harness_context_budget_with_margin` builds `with_builtins()` only, so a conditionally-registered tool is invisible to it and it would keep passing while the real prompt grew — LESSON-481, in the one test guarding a budget three REQs contend for.
- [ ] The margin is counted with BR-8's sentence **and** BR-2's roster **together**, never one at a time. Today: 826 margin, 48 floor, **778 usable**. Measure, do not estimate — the tests print only on failure, so pad the guide by a known amount, read the figure the failure names, and subtract.
- [ ] If `REDACT_BODY_OVERHEAD_BYTES` moves 10 → 11 KiB, **both** arithmetic claims are re-stated: the chunk count stays 4, *and* `REDACT_SCANNABLE_CONTEXT_BYTES` drops 89,127 → 88,196 — a 931-byte cut to every `redact = true` route's byte budget, which is the budget BR-7's `bound: redact scan` refusal measures against. The existing test passes either way, so the cut is silent unless asserted.
- [ ] AC-3's "byte-identical" is **two registries compared in one test**, not a checked-in golden — nothing in the tree pins rendered tool docs byte-for-byte.
- [ ] Mutation: reverting the sentence, and omitting the skill tool from either margin test, each fail a named test.

## Technical Notes

- REQ-584 contends for the same constant. Whichever lands first moves it once; the second re-measures rather than moving it again.
