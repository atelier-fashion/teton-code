---
id: TASK-414
title: "Docs, changelog, the architecture-context pattern, and the mutation re-run over the widened suite"
status: complete
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: ["TASK-413"]
repo: teton-code
---

## Description

Land AC-12 and the REQ-619 rule that the last task to add tests owns the mutation
counts: re-run the `frame` tick mutation and the withdraw default mutation after
the pty legs exist, rewrite the recorded numbers naming what reddened, and touch
the mutated files so cargo rebuilds. Add the "live row is owned by the pump"
pattern to the context architecture.

## Files to Create/Modify

- `README.md` — "In the session": the activity row, its phases, cost so far, the stall annotation, the verbose summary line; piped use shows none of the row
- `CHANGELOG.md` — `## [Unreleased]` entry
- `docs/` — the interactive-session page if one exists (cross-reference REQ-556's loading indicator as the pre-turn counterpart)
- `.adlc/context/architecture.md` — Key Patterns entry from the REQ-621 architecture's "Proposed additions"
- `crates/teton/src/activity.rs` — mutation record updated with the pty red count
- `crates/teton/src/render.rs` — mutation record for the withdraw default

## Acceptance Criteria

- [x] README describes every phase sentence the row can show, verbatim from `activity.rs`, and the stall wording
- [x] Mutation table in `activity.rs` names the unit test and the pty leg that reddened, with counts, after `cargo build -p tetond -p teton`
- [x] `cargo test --workspace --no-fail-fast 2>&1 | grep -c FAILED` is 0
- [x] `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-4 | test-case | `crates/teton/src/activity.rs::tests::the_frame_advances_with_the_tick` (mutation re-run recorded) | no |
| AC-12 | structural-check | `README.md`, `CHANGELOG.md`: phase sentences present verbatim (grep against `activity.rs` constants) | no |

## Technical Notes

- LESSON-652: the count is owned here, not in TASK-409; rewrite, do not append.
- Revert each mutation with the same targeted edit that applied it, never `git checkout -- <file>` on a file with uncommitted work.
