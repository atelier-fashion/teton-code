---
id: TASK-385
title: "The pipeline: gather → draft → bound → write → load, with typed failure and one event stream"
status: complete
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-381, TASK-382, TASK-383]
---

## Description

ADR-6's function without the offer: `run(ctx, consent, force) -> GenerationOutcome` joins the
gatherer, the draft duty, the bounder, the writer and REQ-612's loader, publishing a
`RepoContextGeneration` event per stage, writing one cost row (`category: Draft`), and returning a
typed `Failed { stage, reason }` on any error with the file absent. Covers BR-4's call, BR-5,
BR-7, BR-9.

## Files to Create/Modify

- `crates/tetond/src/repo_context/generate.rs` — `run`, `GenerationOutcome`, the stage events,
  the cost row through the duty's existing recording, the `load` call after the write.
- `crates/tetond/src/repo_context/mod.rs` — `pub mod generate;`.
- `crates/tetond/tests/repo_context_generation.rs` — pipeline tests over a fake duty and a
  tempdir root.

## Acceptance Criteria

- [x] BR-7: after a successful run the loader's state is `Loaded` with `origin: Generated` and
      the block bytes equal the file's rendered block.
- [x] BR-5: exactly one `CostRecord` with `category == Some(Draft)` and the serving provider.
- [x] BR-9: a duty error, a privacy block, an over-window answer, and a write error each yield
      `Failed` with the stage named, no file on disk, and the provider's health unchanged;
      a walk-budget stop with a usable tree is **not** a failure.
- [x] BR-4: the duty receives `Provenance` equal to the evidence's `Sources` (fake duty records
      it); `excluded` rides the `drafted` event.
- [x] Both doors: `run` with `force` replaces; without it an existing file is `Failed {
      AlreadyExists }` and nothing changes.
- [x] `cargo test -p tetond --test repo_context_generation --no-fail-fast` green; every
      Verification row below resolves to a real, executed case.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/tests/repo_context_generation.rs::the_draft_duty_receives_the_evidence_provenance_and_the_excluded_count_rides_the_event` | yes |
| BR-5 | test-case | `crates/tetond/tests/repo_context_generation.rs::one_cost_row_names_the_draft_category_and_the_serving_provider` | no |
| BR-7 | test-case | `crates/tetond/tests/repo_context_generation.rs::a_written_file_is_loaded_the_same_run_with_origin_generated` | yes |
| BR-9 | test-case | `crates/tetond/tests/repo_context_generation.rs::every_stage_failure_is_typed_leaves_no_file_and_keeps_provider_health` | yes |
| AC-7 | test-case | `crates/tetond/tests/repo_context_generation.rs::one_cost_row_names_the_draft_category_and_the_serving_provider` | no |
| AC-9 | test-case | `crates/tetond/tests/repo_context_generation.rs::every_stage_failure_is_typed_leaves_no_file_and_keeps_provider_health` | yes |

## Technical Notes

Compose the failure sentence at the CLI from `{ stage, reason }` (LESSON-557); the daemon carries
facts. A privacy block on the duty is the existing `privacy_block` path — do not add a second.
