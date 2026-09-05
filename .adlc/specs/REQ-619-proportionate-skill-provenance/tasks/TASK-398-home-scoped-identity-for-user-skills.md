---
id: TASK-398
title: "A `~`-scoped ProvenanceId for user skills — the home constructor, the repo-scope reservation, and discovery's branch on source"
status: complete
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: []
---

## Description

Give a user skill an identity (BR-3, BR-4, ADR-619-3). `teton-core` gains
`ProvenanceId::from_home_resolved(home, resolved)` minting `~/<home-relative>`,
and `from_resolved` refuses a remainder whose first segment is `~`
(`ProvenanceError::ReservedScope`) so the repo scope and the home scope can
never produce the same string. `skills::provenance_of` branches on
`Skill::source`: `Project` as today, `User` through the home constructor.
The jail and `from_resolved`'s out-of-root refusal are untouched.

## Files to Create/Modify

- `crates/teton-core/src/provenance_id.rs` — `from_home_resolved`, `ReservedScope` error variant and its `Display`, module docs naming the two scopes; tests
- `crates/teton-core/src/boundary.rs` — a test that `**/.ssh/**` and `**/.claude/skills/**` match `~/…` spellings through `match_path` (no code change expected; the test pins the glob claim ADR-619-3 rests on)
- `crates/tetond/src/skills/discovery.rs` — `provenance_of` branches on source; home resolved through `session_root::home()`; canonicalize both sides as today; tests
- `crates/tetond/src/skills/mod.rs` — `Skill` carries its user root (or `provenance_of` re-derives it from `session_root::home()`); doc updates
- `crates/tetond/src/harness/tools/skill.rs` — the two unit tests `a_user_skill_is_unknown_and_a_project_skill_mints` and `a_roster_holding_a_user_skill_is_unknown_because_one_row_will_not_mint` flip to assert a `~`-scoped id (the model-invoked provenance *mapping* is TASK-401's; here only the minting expectation changes)

## Acceptance Criteria

- [ ] `ProvenanceId::from_home_resolved(home, home/.claude/skills/x/SKILL.md)` mints `~/.claude/skills/x/SKILL.md`; a `resolved` outside `home` is `NotUnderRoot`
- [ ] `ProvenanceId::from_resolved(root, root/~/x)` is `Err(ReservedScope)`; every existing canonical-form test still passes
- [ ] `BoundaryMatcher::match_path("~/.claude/skills/x/SKILL.md")` matches a `**/.claude/skills/**` glob and none of the thirteen `DEFAULT_BOUNDARIES`
- [ ] `provenance_of` returns `Some(~/…)` for a discovered user skill and `Some(<repo-relative>)` for a project skill; a user skill whose canonical path is outside the home returns `None`
- [ ] `cargo test -p teton-core -p tetond --lib` green; `runtime_visibility`, `runtime_doc_paths`, `traceability_sweep` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/teton-core/src/provenance_id.rs::tests::a_home_resolved_file_mints_a_tilde_scoped_id` | yes |
| BR-3 | test-case | `crates/teton-core/src/provenance_id.rs::tests::the_repo_scope_refuses_a_leading_tilde_segment` | no |
| BR-3 | test-case | `crates/teton-core/src/boundary.rs::tests::a_tilde_scoped_id_is_matched_by_a_user_glob_and_by_no_builtin` | yes |
| BR-4 | test-case | `crates/tetond/src/skills/discovery.rs::tests::provenance_of_mints_by_source_and_refuses_a_file_under_neither_root` | yes |
| BR-10 | test-case | `crates/tetond/src/skills/discovery.rs::tests::provenance_of_mints_by_source_and_refuses_a_file_under_neither_root` | yes |

## Technical Notes

- `mint` already accepts a leading `~` segment; the change is the **reservation** in `from_resolved` (strip the root, then refuse if the first segment is `~`). Put the check after `strip_prefix` so `NotUnderRoot` keeps precedence.
- `session_root::home()` returns `Option<PathBuf>` from `HOME`; a missing home yields `None` → `unknown`, fail-closed.
- Discovery follows a symlinked user root (dogfood machine); canonicalize `home` too, or a `/tmp`-style link on macOS makes every user skill fail to strip (the same trap `provenance_of` already documents for the project root).
- Mutation records to write into the test docs: drop the `~` reservation and `the_repo_scope_refuses…` goes red; make `from_home_resolved` strip nothing and the tilde test goes red.
