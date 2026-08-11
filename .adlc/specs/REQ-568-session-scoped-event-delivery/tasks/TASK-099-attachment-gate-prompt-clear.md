---
id: TASK-099
title: "Daemon: attachment gate on session/prompt and session/clear"
status: draft
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-098"]
---

## Description

Refuse `session/prompt` and `session/clear` with `NOT_ATTACHED` (-32009) when
the issuing connection is not attached to the target session (BR-4), enforced
inside the dispatch seam — not in `handle_client` — so the direct-RPC test
surface exercises the gate (LESSON-484 / BUG-155 pattern, ADR-B).

## Files to Create/Modify

- `crates/tetond/src/server.rs` — (1) `dispatch` gains the connection context (`&ConnState` or the attached-set handle from TASK-098); before routing `session/clear`, check membership → `NOT_ATTACHED` if absent. (2) `spawn_prompt_turn` (which bypasses `dispatch`) performs the same check before spawning the turn task. (3) Ordering: attachment check runs BEFORE the runtime touches the session, but a nonexistent session must still surface `UNKNOWN_SESSION` — since an unattached caller can't distinguish paths that never reach the registry, resolve as: if attached-set lacks the id, return `NOT_ATTACHED` regardless of existence (no new existence oracle; ADR-B rationale). (4) Update the existing direct-`dispatch` unit tests (`dispatch_routes_session_clear_and_tells_empty_from_unknown`, `dispatch_lists_created_sessions`) to thread a ConnState; the clear-vs-unknown test now covers three states: attached+live → clears, attached-set-missing → `NOT_ATTACHED`, attached-but-ghost → `UNKNOWN_SESSION` (create-then-attach semantics make this reachable only via a stale id after daemon state loss — if unreachable in practice, assert the two reachable states and document why).
- `crates/tetond/tests/multi_client.rs` — RPC-surface test (BUG-155/AC-8-of-spec pattern): client B calls `session/prompt` and `session/clear` against A's session id without attaching → both refused `-32009`; after `session/attach` both succeed. Driven over the raw socket, not the CLI.

## Acceptance Criteria

- [ ] Unattached `session/prompt` → `-32009` before any turn work starts (no turn task spawned, no runtime mutation).
- [ ] Unattached `session/clear` → `-32009`; attached clear behavior unchanged (idempotent zero for untouched sessions).
- [ ] After `session/attach`, the same calls succeed (AC-4 of the spec).
- [ ] Gate verified at the dispatch/raw-RPC surface, not through the CLI (BUG-155).
- [ ] `cargo test -p tetond` passes.

## Technical Notes

- `session/create`, `session/list`, `session/attach` remain ungated (spec Permissions table).
- Mutation-test the gate in isolation (LESSON-484): temporarily inverting the membership check must fail the new tests — verify the test actually fails before finalizing (LESSON-479 "see it fail").
- Error text follows conventions: no session content, no paths in the message.
