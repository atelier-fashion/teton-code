---
id: TASK-382
title: "The evidence gatherer: one walk, two closed tables, a rendered tree, exclusion, and the priority cut"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

ADR-3 as a pure-as-possible module over REQ-612's `RepoFileReader` and the walker: gather the
full tree under the tool walk's budget, render it breadth-first with per-directory extension
counts, read the manifests and README whole (bounded) and the entry-point headers, exclude
boundary-covered files, and assemble under the byte budget in priority order with the cut
recorded. Covers BR-3 and the exclusion half of BR-4.

## Files to Create/Modify

- `crates/tetond/src/repo_context/evidence.rs` — `EVIDENCE_FILES`, `ENTRY_POINTS`,
  `EvidenceBudget { max_bytes }`, `Tree`, `Evidence { body, provenance: ToolProvenance,
  excluded: usize, entries: usize, stop: Option<WalkStop>, cut: Option<Cut> }`, `gather(..)`.
- `crates/tetond/src/repo_context/mod.rs` — `pub mod evidence;`.

## Acceptance Criteria

- [ ] BR-3: a planted six-level tree renders to its leaves with counts; a tree over a small
      injected `WalkBudget` stops with `stop` set; the skip set and symlink rule hold; the
      reader records zero calls before `gather` is called.
- [ ] Both tables are exercised by name; present members contribute whole text to 16 KiB or
      4 KiB; absent members cost one `stat`; nothing outside the tables is read (the reader's
      path log is the assertion).
- [ ] BR-4: a covered `Cargo.toml` is absent from `body` and `provenance`, `excluded == 1`;
      listing names of a covered directory still appear in the tree.
- [ ] The priority cut drops entry points before README before manifests before the tree, and
      the tree is cut by depth with `cut.depth` recorded; the body never exceeds `max_bytes`.
- [ ] Rendering is order-independent: two listings of the same entries in different orders
      render identically (LESSON-540).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/src/repo_context/evidence.rs::the_full_tree_is_listed_to_its_leaves_and_a_budget_stop_is_recorded` | yes |
| BR-3 | test-case | `crates/tetond/src/repo_context/evidence.rs::the_two_tables_are_read_by_name_and_nothing_else_is_opened` | yes |
| BR-4 | test-case | `crates/tetond/src/repo_context/evidence.rs::a_covered_evidence_file_is_excluded_and_counted_and_its_directory_name_still_lists` | yes |
| AC-4 | test-case | `crates/tetond/src/repo_context/evidence.rs::the_full_tree_is_listed_to_its_leaves_and_a_budget_stop_is_recorded` | yes |
| AC-5 | test-case | `crates/tetond/src/repo_context/evidence.rs::the_two_tables_are_read_by_name_and_nothing_else_is_opened` | yes |

## Technical Notes

`walk::visit` yields paths; derive depth from component count. Sort entries before rendering.
Provenance ids are minted with `ProvenanceId::from_resolved` on the resolved path, and the
matcher is `BoundaryMatcher::match_path` (LESSON-623). Never call `projects::scan`.
