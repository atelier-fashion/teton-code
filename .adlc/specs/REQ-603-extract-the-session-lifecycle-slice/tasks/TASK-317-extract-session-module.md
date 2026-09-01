---
id: TASK-317
title: "Extract the session-lifecycle slice to runtime/session.rs as one reviewable commit"
status: complete
parent: REQ-603
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

Move the six session-lifecycle items identified in architecture.md ADR-1 out of
`runtime/mod.rs` into a new `runtime/session.rs`, as a second
`impl DaemonRuntime` block. Bodies byte-identical — a relocation, not a
restructure (REQ-603 Out of Scope; REQ-599 ADR-3).

Everything in this task lands as **one commit** (AC-2). The ratchet and map
updates are included because the suite is red without them: the move is not
reviewable in pieces that do not build.

## Files to Create/Modify

- `crates/tetond/src/runtime/session.rs` — new module: header + `impl DaemonRuntime` with the six items + `#[cfg(test)] mod tests` holding the one moved test
- `crates/tetond/src/runtime/mod.rs` — declare `mod session;`, remove the moved run, keep `projects()`
- `crates/tetond/src/runtime/testsupport.rs` — receive `scratch_root`, used by both homes
- `crates/tetond/tests/runtime_visibility.rs` — `PUBLIC` +3, `PUBLIC_DECLARATIONS` 14→17, `CRATE_WIDE` +`store_session_skills`, test rename + doc table
- `.adlc/specs/REQ-599-decompose-the-turn-path/architecture.md` — module-map row for `session.rs`, refreshed `mod.rs` count

## Acceptance Criteria

- [ ] `session.rs` holds `clear_session`, `jail_root`, `session_root_for`, `set_session_cwd`, `store_session_skills`, `drop_grants_expiring_on_root_change`
- [ ] `projects()` stays in `mod.rs`, and line 3504's one-line doc stays with it while 3486–3503 moves with `store_session_skills` (ADR-4)
- [ ] `use super::*;` at the module head, matching `turn.rs`
- [ ] No visibility widened: verified by demote-and-build, not by grep (LESSON-596)
- [ ] `the_session_root_is_probed_from_the_cwd_or_the_daemon_fallback` moves into `session.rs`'s test module; `scratch_root` lifted to `testsupport.rs` as `pub(super)`
- [ ] The module header states which tests stayed and why, naming `conversation_carry`'s membership rule (AC-5 / REQ-599 BR-7)
- [ ] `cargo build --workspace` clean

## Technical Notes

- Take the plain-`//` comment runs above each method, not just the `///` blocks —
  `turn.rs`'s header records losing a 58-line rationale run to exactly this
  (LESSON-594).
- `refused_claim_error` needs no change: `turn.rs` already reaches it as a
  private item of the parent module, and `session.rs` is a sibling.
- `jail_root` and `drop_grants_expiring_on_root_change` stay private. If the
  build demands `pub(super)` for either, that is a finding to record, not a
  qualifier to add silently.

## Verification

Obligations this task carries, by REQ-603 acceptance-criterion ordinal (the
requirement's `## Acceptance Criteria` list is unnumbered, so position is the
addressing):

- **AC-2** (own module, behaviour unchanged, one reviewable commit) — `kind: structural-check`.
  Artifact: the single commit's diff; bodies compared byte-for-byte against
  `origin/main` with `git diff -M`.
- **AC-3** (map + doc-paths green) — `kind: test-case`.
  Artifact: `cargo test -p tetond --test runtime_module_map --test runtime_doc_paths`.
- **AC-4** (visibility ratchet not loosened) — `kind: structural-check`.
  Artifact: `cargo test -p tetond --test runtime_visibility`, plus the
  demote-and-build derivation (`cargo check --workspace --all-targets`, read `E0603`).
- **AC-5** (tests move with subject, or the header says why) — `kind: structural-check`.
  Artifact: `session.rs`'s module header, naming the nine that stayed and
  `conversation_carry`'s membership rule.
