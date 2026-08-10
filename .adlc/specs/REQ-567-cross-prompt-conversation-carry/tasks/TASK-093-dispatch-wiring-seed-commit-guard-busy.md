---
id: TASK-093
title: "Dispatch wiring: seed from conversation, commit on completion, cancel guard, busy refusal"
status: draft
parent: REQ-567
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-092]
repo: teton-code
---

## Description

Wire `run_prompt_turn` to the conversation store: claim the session (typed
busy refusal on contention), replay the committed conversation into the
fresh manager before `push_user`, and commit the post-turn blocks per the
D-1 protocol — explicit commit on success, no commit on error, commit-on-
drop guard for task abort (OQ-1: retain prose, drop incomplete tool work).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — in `run_prompt_turn`
  (`runtime.rs:2051`, ctx built at 2139-2141): `try_begin_turn` before any
  work, mapping `InFlight` to the new typed session-busy error naming the
  in-flight turn id (LESSON-456 — never a generic turn failure);
  `replay_blocks(conversation_snapshot(id))` between `ContextManager::new`
  and `push_user`; wrap the manager in the commit-on-drop guard armed after
  `push_user`; on `Ok` disarm + `commit_conversation(id, ctx.into_blocks())`;
  on `Err` disarm without commit.
- `crates/tetond/src/harness/mod.rs` or `runtime.rs` — the guard type
  (owns the manager + registry handle + session id; commits current blocks
  on armed drop).
- `crates/teton-protocol/src/jsonrpc.rs` or error surface — the typed
  session-busy error code/shape (follow BUG-152's `TIER_WARMING` precedent:
  daemon classifies, client renders).

## Acceptance Criteria

- [ ] e2e (scripted engine, beside `crates/tetond/tests/prefix_cache_session.rs`
  patterns): 3-prompt session — prompt 3's engine-received context contains
  prompt 1's user message and kept reply (AC-1); fails against the
  pre-change dispatch (AC-10's red half).
- [ ] Atomicity test: a scripted turn erroring after a completed tool call
  leaves the next prompt's context byte-identical to the failed turn never
  having run (AC-5).
- [ ] Cancellation test: aborting the turn task mid-loop (drop the future
  after a scripted generation completed) commits the streamed prose blocks;
  a pending unanswered tool call is absent (OQ-1 / D-1).
- [ ] Concurrency test: two concurrent `session/prompt` on one session —
  second gets the typed busy error naming the in-flight turn; conversation
  afterward has no interleaved blocks; a session whose turn was refused can
  prompt again after the first completes (AC-4).
- [ ] Cut-tail test: a scripted reply with a fabricated continuation
  carries only the kept text into the next prompt's context (BR-1's
  kept-view rule / LESSON-500 — the fabricated tail never enters the
  conversation; spec AC-2, the privacy test, lives in TASK-096).
- [ ] `cargo test --workspace` green.

## Technical Notes

The guard must ride the spawned turn task (`server.rs` abort path drops the
future; the guard's Drop is the only code that runs). Blocks in the manager
are complete by construction (pushes happen on completion), so armed-drop
commit implements retain-prose/drop-incomplete without filtering. Do NOT
hold any registry lock across the turn (LESSON-448). The busy error is
transient-shaped, not terminal (BUG-152).
