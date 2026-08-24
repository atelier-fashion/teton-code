---
id: TASK-261
title: "The trust prompt must name who actually asked"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-248]
---

## Description

**Created mid-Phase-4 from TASK-255's PTY finding — a user-facing defect this REQ introduced.**

D-10/ADR-10 gave the typed `/name` path the project-skill trust door. That door's prompt says:

> the model wants to run this repository's skills as instructions: /path/to/proj

On the typed path **no model asked for anything** — the user typed the name. The prompt makes
a false statement about who is acting, on a security prompt whose entire job is to let a
human decide whether to trust a repository.

`PermissionSubject::ProjectSkillTrust` carries no `invoked_by`, so the client cannot tell the
two callers apart. `PermissionSubject::SkillDynamicContext` — in the same enum, rendered by
the same function — **does** carry it, and `invoker_clause` exists for exactly this reason:
REQ-585 BR-5 holds that a human at `guarded` is entitled to know which caller is on screen.
This REQ now has one door that honors that and one that does not.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `invoked_by` on `PermissionSubject::ProjectSkillTrust`
- `crates/tetond/src/runtime.rs` — pass the real invoker from the typed path
- `crates/tetond/src/harness/tools/skill.rs` — pass it from the model path
- `crates/teton/src/session_ui.rs` — render the caller-correct clause

## Acceptance Criteria

- [x] A **typed** `/name` trust prompt says the user asked; it never says "the model wants to"
- [x] A **model-invoked** trust prompt is unchanged, byte for byte — a test pins it
- [x] The clause comes from the existing `invoker_clause`, not a second vocabulary (LESSON-456)
- [ ] A PTY leg pins the typed wording at a real terminal (TASK-255's fixture is the pattern)
- [x] Mutating the invoker away reddens

## Technical Notes

Follow `SkillDynamicContext`'s existing shape exactly — it already solved this. The defect is
that the trust door was written when only one caller could reach it, and D-10 added a second.

**Run `cargo build --workspace` before any targeted `-p teton --test pty_e2e` run.** TASK-255
confirmed live that a targeted run tests a STALE daemon: a mutation it applied looked survived
until the workspace was rebuilt.

## Implementation notes

**Two files outside the listed four had to change, and one AC is left open.**

`PermissionSubject::ProjectSkillTrust` has exactly **one** production mint site,
inside `PermissionGate::authorize_project_skill_trust`
(`crates/tetond/src/harness/permissions.rs`). Rust has no defaulting for
enum-variant literals, so an added field is a hard `E0063` there and the invoker
cannot reach the client without one parameter and one field in that function.
The change is two production lines plus three of its own call sites; the key the
answer is remembered under is untouched, so ADR-7's "one answer per root, shared
by both callers" still holds. `crates/tetond/tests/skill_consent_matrix.rs`
needed the same mechanical repair at three fixture sites.

The PTY leg is **not** done — `crates/teton/tests/pty_e2e.rs` was outside this
task's file ownership. The typed wording is pinned at the client's rendering
seam (`session_ui`) and the invoker at both producers (`runtime`, `skill`)
instead; a terminal leg would add the real-tty layer those three do not cover.

**Pre-existing, not from this task:** `cli_e2e`'s
`a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget`
fails at HEAD without any of this change — a piped session cannot answer the
trust door TASK-248 put on the typed path, so the turn is refused. Verified by
running it in a clean worktree at `53f1c71`.
