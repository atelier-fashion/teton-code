---
id: TASK-092
title: "Conversation store in the session registry + ContextManager replay/commit seams"
status: complete
parent: REQ-567
created: 2026-08-10
updated: 2026-08-10
dependencies: []
repo: teton-code
---

## Description

The foundation: give the session registry ownership of each session's
conversation, and give `ContextManager` the two seams dispatch needs —
extracting its post-turn blocks for commit, and replaying committed blocks
into a fresh manager under a freshly built system head.

## Files to Create/Modify

- `crates/tetond/src/sessions.rs` — `Conversation` (ordered
  `Vec<ContextBlock>`, no system head — architecture D-1/data model) and
  registry methods `conversation_snapshot`, `commit_conversation`
  (whole-vector replace, the BR-6 atomic unit), `clear_conversation`
  (returns blocks dropped), and `try_begin_turn`/`TurnClaim` (atomic claim
  in one lock, `claim_title` discipline; releases on drop) — architecture
  D-3.
- `crates/tetond/src/harness/context.rs` — `ContextManager::into_blocks()`
  (move out the block vector) and `ContextManager::replay_blocks(blocks)`
  (append committed blocks preserving role + provenance via the same push
  paths, before `push_user` of the new message). Keep `request` retention
  semantics correct when replay precedes `push_user`.

## Acceptance Criteria

- [ ] Unit tests in `sessions.rs`: snapshot/commit round-trip; commit is
  whole-vector replacement (a failed turn's never-committed mutation is
  invisible); `clear_conversation` empties and returns the count;
  interleaved session ids never see each other's blocks (BR-2 isolation);
  `try_begin_turn` refuses a second claim while one is live and re-admits
  after drop (BR-5 seam).
- [ ] Unit tests in `context.rs`: `replay_blocks` + `push_user` produces an
  assembled context whose block order equals the committed conversation
  followed by the new user message; per-block provenance survives
  round-trip (`into_blocks` → `replay_blocks` → `context_provenance`
  unchanged); replay respects budgets (an over-budget replay still passes
  through the existing compaction/truncation gates, never panics).
- [ ] `cargo test -p tetond` green.

## Technical Notes

Registry lock discipline: check-and-act inside one lock (`claim_title`
precedent, `sessions.rs:154-170`); never hold the registry lock across a
turn (LESSON-448). `ContextBlock` already carries role + provenance —
`Conversation` stores it verbatim; no parallel type. The system head is
NEVER stored (spec assumption: heads are rebuilt per prompt). `into_blocks`
must exclude the system head by construction.
