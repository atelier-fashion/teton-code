---
id: TASK-370
title: "Protocol: `session/context`, `repo_context_state`, and `SetRepoContextEnabled`"
status: draft
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The wire half, additive in both directions (ADR-6): the `session/context` method's params and
result, the one `repo_context_state` event, the state-kind enum both carry, and the
`ConfigUpdate` struct variant for the durable switch. No daemon or CLI behaviour yet; the
contract tests that pin additivity and the ends-turn table land here so TASK-374 and TASK-376
build against a fixed shape.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `SessionContextParams { session_id, action:
  ContextAction }`, `ContextAction { On, Off, Status }`, `SessionContextResult { state:
  RepoContextStateKind, source: Option<RepoContextSource>, file: Option<String>, bytes_on_disk,
  resident_bytes, cap, truncated }`, `RepoContextStateKind { Loaded, Truncated, Absent,
  WithheldBoundary, WithheldOff, Unreadable }`, `RepoContextSource { TetonMd, AgentsMd }`,
  `ConfigUpdate::SetRepoContextEnabled { enabled: bool }` (struct variant — the reason at
  line 2091), the `ENDS_TURN` table row for `session/context` (false).
- `crates/teton-protocol/src/events.rs` — `Event::RepoContextState(RepoContextState)`, `name()`
  arm `repo_context_state`, the spec-table test row; payload: session id, state kind, source,
  bytes on disk, resident bytes, truncated, and an optional bounded `reason` for
  `Unreadable`.

## Acceptance Criteria

- [ ] Every new type round-trips through serde; an older client's `ConfigUpdate` set still
      deserializes; a payload with the new event is ignored by the pre-REQ event reader
      (the REQ-573 additive rule, asserted the way `route_decided_budget_fields_are_additive_in_both_directions` does).
- [ ] The `ENDS_TURN` sweep sees `session/context` and it does not end a turn.
- [ ] `SetRepoContextEnabled` is a struct variant; the test that pins the transcript variant's
      shape gains the twin row.
- [ ] `cargo test -p teton-protocol --no-fail-fast` green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton-protocol/src/methods.rs::session_context_params_and_result_round_trip_and_do_not_end_a_turn` | no |
| AC-2 | test-case | `crates/teton-protocol/src/events.rs::repo_context_state_is_additive_in_both_directions` | no |

## Technical Notes

One event, not two: the spec's `repo_context_loaded` / `repo_context_withheld` are the two halves
of `state` (ADR-6). `reason` is bounded at the daemon with `bounded_field` before it is put on the
wire — a filesystem error string is repository-adjacent text (REQ-591 BR-11's rule).
