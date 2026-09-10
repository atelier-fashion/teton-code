---
id: TASK-406
title: "The shell tool description states the grammar to the model, and the prompt margin is raised to pay for it"
status: complete
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: []
repo: teton-code
---

## Description

Add the one-paragraph contract (`SHELL_REACH_CONTRACT`, ≤ 420 bytes) to
`ShellTool::description()`, read by the description and by its test. Raise
`REDACT_BODY_OVERHEAD_BYTES` from 23 KiB to 24 KiB, re-derive the four figures that hang
off it, and re-record the prompt margin (ADR-620-5). Record the assumption that the
paragraph changes model behaviour often but not always.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — `SHELL_REACH_CONTRACT` and its inclusion in `description()`; a test that the constant names `/dev/null`, `2>&1`, quotes, globs, `$`, `~/`, "local tier" and is ≤ 420 bytes
- `crates/tetond/src/egress/redact.rs` — `REDACT_BODY_OVERHEAD_BYTES = 24 * 1024`; `REDACT_TOTAL_CAP_CHUNKS`, `REDACT_INPUT_MAX_BYTES`, `REDACT_SCANNABLE_CONTEXT_BYTES`, `REDACT_MAX_CHUNKS` re-derived; `RECORDED_PROMPT_MARGIN_BYTES` and `RECORDED_WEB_PROMPT_MARGIN_BYTES` re-recorded; the ledger doc comment gains the REQ-620 row
- `crates/tetond/src/harness/completion.rs` — the prompt-margin test asserts the contract is in the serialised tool spec for a typed and a model-invoked turn
- `.adlc/knowledge/assumptions/ASSUME-048-a-tool-description-steers-but-does-not-bind.md` — new (id via `adlc_alloc_id assume`)

## Acceptance Criteria

- [x] `exposed_tool_specs()` output for the shell tool contains `SHELL_REACH_CONTRACT` verbatim, and the same bytes for a typed and a model-invoked turn
- [x] `the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound`, `the_scannable_bound_plus_overhead_and_escaping_fits_under_the_cap`, `the_total_cap_clears_the_harness_context_budget_with_margin` green at 24 KiB with the re-derived figures
- [x] The recorded margin after the raise is positive and asserted, not narrated
- [x] The paragraph is description of the daemon's behaviour, not an instruction about repository text (REQ-612 framing)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/tetond/src/harness/tools/shell.rs::tests::the_reach_contract_names_every_pinning_class_and_fits_its_budget` | no |
| AC-8 | test-case | `crates/tetond/src/harness/completion.rs::tests::the_shell_reach_contract_reaches_the_provider_for_typed_and_model_turns` | no |
| AC-8 | test-case | `crates/tetond/src/egress/redact.rs::tests::the_overhead_raise_restates_the_chunk_count_and_the_scannable_bound` | no |

## Technical Notes

- Wording draft (adjust to fit 420 bytes): "Commands are checked before they run. A
  command stays on the current model when it uses ordinary read verbs (ls, cat, grep, git
  status…) on paths inside the session root; `2>/dev/null` and `2>&1` are fine. Quotes,
  other redirects, globs, `$`, `~/` paths, interpreters, network clients, or an unknown
  verb pin the rest of this session to the local model; the pin is announced, and only
  the user can lift it."
- ASSUME-043's resolution is the precedent for raising rather than shortening: two REQs
  borrowing from one margin in one sprint went 85 bytes over. Raise once, by a whole KiB,
  and re-derive.
- LESSON-542: a grammar taught to the model must be read on every path it can answer
  through — the contract must not contradict `shell_provenance`'s tables; the test cross-
  checks each verb the paragraph names against `READS_NOTHING`/`NAME_ONLY`/`READS_CONTENT`.
