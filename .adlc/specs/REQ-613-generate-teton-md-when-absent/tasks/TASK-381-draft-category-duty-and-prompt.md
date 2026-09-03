---
id: TASK-381
title: "`Category::Draft` bound to `think`, the `DRAFT_DUTY`, and the draft prompt with bounding"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

ADR-4: the twelfth category and its duty. `teton-core` declares `Draft` with `tier() == Think` and
a configurable counterpart; the resolver handles it; `harness/draft.rs` owns the duty kind, the
prompt builder (fixed section order, stated byte budget) and `bound_answer` (strip and cut as
REQ-612's renderer does, at the cap less the header). No call site yet beyond the duty itself.

## Files to Create/Modify

- `crates/teton-core/src/category.rs` — `Category::Draft`, `ALL` (12), `tier()` arm,
  `origin()` `HarnessKnown`, `ConfigurableCategory::Draft`, `as_str`/parse `"draft"`, every
  `ALL.len()` and exhaustive-match test updated.
- `crates/tetond/src/runtime/duty.rs` — resolve `Draft` through the one resolver; the REQ-558
  ADR-A reached-set test marks it reached.
- `crates/tetond/src/harness/draft.rs` — `DRAFT_DUTY`, `DRAFT_OUTPUT_MAX_BYTES` (= REQ-612's cap),
  `build_prompt(&Evidence) -> String`, `bound_answer(answer, header) -> String`.
- `crates/tetond/src/harness/mod.rs` — `pub mod draft;`.
- `crates/teton/src/main.rs` / `policy` rendering — `teton policy show` lists `draft` with its
  binding.

## Acceptance Criteria

- [ ] `Category::Draft.tier() == Tier::Think`; `/policy set-category draft local` parses and
      binds; `policy show` renders the row; every category test counts 12.
- [ ] `build_prompt` names the five sections in order and the byte budget; a golden.
- [ ] `bound_answer` of cap + 2,000 bytes with a 120-byte header is exactly the cap, cut at a
      line boundary, header first.
- [ ] `cargo test -p teton-core -p tetond --lib --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/teton-core/src/category.rs::draft_is_declared_reached_and_bound_to_think` | no |
| AC-14 | test-case | `crates/tetond/tests/routing.rs::draft_routes_to_the_think_binding_by_default_and_to_local_when_set` | yes |
| AC-8 | test-case | `crates/tetond/src/harness/draft.rs::bound_answer_lands_exactly_at_the_cap_with_the_header_first` | yes |

## Technical Notes

Reuse REQ-612's strip and line-boundary truncation from `repo_context/render.rs` — do not spell
a second cutter (LESSON-456's shape). The prompt says the answer is Markdown for a file a new
contributor reads first; it never asks the model for commands to run.
