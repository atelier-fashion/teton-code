---
id: TASK-312
title: "Repair the derived checks the move breaks, and document the new module"
status: draft
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-310, TASK-311]
---

## Description

AC-6. This codebase derives guarantees by scanning its own source, and REQ-599
broke seventeen such checks by moving code (LESSON-594). The blast radius here
was **measured** — by planting a probe module under `runtime/` and running the
guards — rather than predicted.

| guard | new module under `runtime/` | why |
|---|---|---|
| `runtime_module_map.rs` | **fails** | demands every module appear in REQ-599's map table |
| `runtime_visibility.rs` | passes | corpus enumerated from disk (REQ-602) |
| `runtime_doc_paths.rs` | passes | same |
| `traceability_sweep.rs` | passes | recursive since REQ-602 |

Three of four absorb a new module because REQ-602 landed first. That is the
return on it.

## Files to Create/Modify

- `.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md` — add `turn.rs`
  to the module map table
- `crates/tetond/src/runtime/mod.rs` — the `offer_or_refuse_over_budget`
  call-site assertion
- `crates/tetond/src/runtime/taint.rs`, `crates/tetond/src/projects/scan.rs` —
  content assertions, if the move invalidates them
- `crates/tetond/tests/suppression_ratchet.rs` — only if TASK-311 changed the count

## Acceptance Criteria

- [ ] `turn.rs` is added to the module map table in REQ-599's architecture doc
      with its measured production count, in the parsed row format
      ``| `name.rs` | <production> | <holds> |``. `runtime_module_map.rs` passes
      in **both** directions — no module undocumented, no documented module absent.
- [ ] `mod.rs:25940`'s assertion that `.offer_or_refuse_over_budget(` has exactly
      two call sites is **repaired, not deleted**. Its message says both are
      "`run_prompt_turn`'s own budget stages"; after decomposition that is still
      the claim, and the assertion must still be able to see them.
- [ ] `taint.rs:1064` (exactly one setter call site, in the `web/override`
      handler) and `projects/scan.rs:518` (`pub(crate) fn store_session_skills`
      exists under `runtime/`) both still pass, or are repaired with their
      subject re-located. **Neither may be made vacuous**: `projects/scan.rs`
      went silently dead once in this line by reading a path that no longer
      existed and taking an early return that asserted nothing.
- [ ] Every repaired check is **shown to still fail** when its subject is broken
      — a repair that quietly turns a guard into a no-op is worse than the
      breakage, and this exact failure has shipped here before.
- [ ] `runtime_doc_paths.rs` passes: no comment is left citing a `runtime::…`
      path that the move invalidated.
- [ ] Suite green, grepped for `FAILED`.

## Technical Notes

`traceability_sweep.rs`'s `BASE` and `TOUCHED` are **deliberately not
repointed** (AC-6). `BASE` is REQ-599's pre-split commit `17c39ec`; repointing
it at this REQ's base makes the sweep compare the split tree against itself,
which proves nothing about the split. REQ-602 recorded the same decision with
the same reason.
