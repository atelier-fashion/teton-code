---
id: TASK-130
title: "Server: dispatch handlers with may_drive gates and rejection events"
status: draft
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-129"]
---

## Description

Wire the three setup methods into `server.rs` dispatch, gated exactly like
`web/override` (`conn.may_drive(&session_id)`), with the BR-4 defense-in-depth
rejection: a gated-out `preview`/`commit` answers `NOT_ATTACHED` **and**
publishes `WebSetupRejected { origin }` so the refusal is visible in front of
the user (LESSON-505), not only in the RPC error the attacker received.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `handle_web_setup_plan`, `handle_web_setup_preview`, `handle_web_setup_commit` following `handle_web_override` (server.rs:2123): parse params, `may_drive` check, delegate to the runtime methods, map errors. The rejection event publishes from the gate-failure arm of preview and commit (plan is read-only: gate it, no event).
- `crates/tetond/src/server.rs` — dispatch table entries for the three method names.
- `crates/tetond/src/server.rs` — unit/integration tests in the crate's existing server-test pattern: a second connection (attached to a different session / not attached) calling commit gets `NOT_ATTACHED` and the owning session's subscribers see `WebSetupRejected`; a mutation check note — the may_drive predicate call in the commit arm gets a dedicated test that fails when the check is deleted (LESSON-508: the seam is redundant with the structural no-model-path property, which is exactly why it needs its own test).

## Acceptance Criteria

- [ ] All three methods dispatch and answer over a real socket in the existing e2e harness style
- [ ] Commit from a non-driving connection: `NOT_ATTACHED` error + `WebSetupRejected` event delivered to the session's subscriber — asserted at a client, not via logs
- [ ] Deleting the `may_drive` check in the commit handler fails a named test (comment in the test states why it exists — LESSON-508 rule 2)
- [ ] No new daemon-wide method: none of the three appears in `refuse_daemon_wide`'s list (they are session-scoped)

## Technical Notes

Request/response only — these are not consent prompts, so `PendingConsents`
id minting is NOT involved (architecture ADR-1 removed the pending-state
surface). `origin` in the rejection event is a coarse string ("connection
without session access") — never connection internals.
