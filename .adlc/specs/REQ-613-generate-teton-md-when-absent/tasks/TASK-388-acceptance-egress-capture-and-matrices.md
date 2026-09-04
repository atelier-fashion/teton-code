---
id: TASK-388
title: "Acceptance: egress capture for excluded evidence, the walk and routing matrices, and the end-to-end legs"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-386, TASK-387]
---

## Description

The charter-level claims proven the way the conventions demand, run after every surface they
measure: egress capture for BR-4 / AC-6 (a covered manifest's marker never leaves), the routing
matrix for AC-14 on the real router, and the end-to-end legs that drive the daemon through both
doors.

## Files to Create/Modify

- `crates/tetond/tests/egress_capture.rs` — AC-6: a `local-only` glob covering `Cargo.toml`
  with a marker in its bytes; a generation run on a remote-routed `draft`; no request body carries
  the marker; `excluded == 1`; a mutation that skips exclusion fails.
- `crates/tetond/tests/routing.rs` — AC-14 on the real resolver with three policy fixtures.
- `crates/tetond/tests/cost_attribution.rs` — AC-7's row: DELIVERED INSTEAD by TASK-385's
  `repo_context_generation.rs::one_cost_row_names_the_draft_category_and_the_serving_provider`,
  which asserts exactly one `draft`-category ledger row naming the serving provider (no second
  test written; the obligation row below points at the existing artifact).
- `crates/tetond/tests/repo_context_generation.rs` — AC-8 (cap + 2,000 written at the cap; a
  file created between consent and write → no write), AC-4's real-walker leg on a tempdir.

## Acceptance Criteria

- [ ] AC-6 both legs green, mutation recorded in the doc comment (the marker lives only in the
      file's bytes — LESSON-624).
- [ ] AC-14 three fixtures green; AC-7 through TASK-385's ledger assertion; AC-8 both halves (done: cap + race, mutations recorded).
- [ ] `cargo test --workspace --no-fail-fast` green, output grepped for `FAILED`.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/tests/repo_context_generation.rs::the_real_walker_lists_a_deep_tempdir_and_stops_at_an_injected_budget` | yes |
| BR-4 | test-case | `crates/tetond/tests/egress_capture.rs::a_boundary_covered_manifest_never_reaches_the_draft_provider_and_is_counted_excluded` | yes |
| BR-6 | test-case | `crates/tetond/tests/repo_context_generation.rs::a_file_created_between_consent_and_write_stops_the_write_and_a_long_answer_lands_at_the_cap` | yes |
| AC-6 | test-case | `crates/tetond/tests/egress_capture.rs::a_boundary_covered_manifest_never_reaches_the_draft_provider_and_is_counted_excluded` | yes |
| AC-8 | test-case | `crates/tetond/tests/repo_context_generation.rs::a_file_created_between_consent_and_write_stops_the_write_and_a_long_answer_lands_at_the_cap` | yes |
| AC-14 | test-case | `crates/tetond/tests/routing.rs::draft_routes_to_the_think_binding_by_default_and_to_local_when_set` | yes |
| AC-4 | test-case | `crates/tetond/tests/repo_context_generation.rs::the_real_walker_lists_a_deep_tempdir_and_stops_at_an_injected_budget` | yes |

## Technical Notes

When the egress assertion fires, print the request indices carrying the marker beside the
ordered event names before touching the choke point (conventions).
