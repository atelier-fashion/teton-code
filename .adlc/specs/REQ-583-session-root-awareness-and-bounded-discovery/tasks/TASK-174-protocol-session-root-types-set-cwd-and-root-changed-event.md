---
id: TASK-174
title: "Protocol: SessionRoot/RootKind wire types, SessionCreateResult.root, session/set_cwd, session_root_changed"
status: draft
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: []
---

## Description

Add the additive protocol surface every other task builds on: the wire view of
a session root, the `session/set_cwd` method, and the `session_root_changed`
event. No behaviour — types, serde, `RpcMethod`, `Event::name`, and the
enumeration/round-trip tests the crate keeps for every method and event. No
`PROTOCOL_VERSION` bump (additive only). See `architecture.md` ADR-4 and the
"Data model / protocol changes" table.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `RootKind { Project, Home, FilesystemRoot, Plain }` (`#[serde(rename_all = "snake_case")]`, `Copy`), `SessionRoot { display: String, kind: RootKind, project_name: Option<String>, vcs_branch: Option<String> }`; `SessionCreateResult.root: Option<SessionRoot>` (`#[serde(default, skip_serializing_if = "Option::is_none")]` — an older daemon omits it, an older client ignores it); `SessionSetCwdParams { session_id: SessionId, cwd: PathBuf }` / `SessionSetCwdResult { root: SessionRoot, blocks_dropped: u64 }` with `impl RpcMethod { METHOD = "session/set_cwd" }`, placed beside `SessionClearParams` (L196-230). Doc-comment the BR-6/BR-7 contract (absolute, exists, is a directory; the daemon validates). Add `session/set_cwd` to the METHOD pin list test (`request_helper_fills_method_from_trait`, ~L3371-3404) and a `session_set_cwd_round_trips` test beside `session_clear_round_trips` (~L2514) covering both `Some`/`None` optionals; a `session_create_result_without_root_still_deserializes` wire-compat test beside `session_create_without_a_cwd_still_deserializes` (~L2473).
- `crates/teton-protocol/src/events.rs` — `Event::SessionRootChanged(SessionRootChanged { previous_display: String, root: SessionRoot })` (no `session_id` field — the envelope carries it, flatten rule L2042-2045); `Event::name()` arm `"session_root_changed"` (L184-216); a row in `event_names_match_the_spec_events_table` (~L2359-2546); `session_root_changed_round_trips_under_its_wire_name` copied from `context_cleared_round_trips_under_its_wire_name` (L2588-2606), asserting `["event"] == "session_root_changed"` and the envelope's `session_id`.

## Acceptance Criteria

- [ ] `cargo test -p teton-protocol` green; the METHOD pin list and the event-name table both carry the new entries.
- [ ] `SessionCreateResult` JSON without `root` deserializes (wire compat) and one with `root` round-trips including `RootKind` snake_case spellings (`filesystem_root`).
- [ ] `SessionSetCwdParams`/`Result` round-trip; `Event::SessionRootChanged` round-trips under its wire name with the envelope's `session_id` and no payload `session_id`.
- [ ] `cargo build --workspace` still compiles — **the CLI's exhaustive `Event` match in `crates/teton/src/session_ui.rs:592-802` will NOT compile without an arm**: add a minimal placeholder arm there that renders nothing (`Event::SessionRootChanged(_) => {}` with a `// TASK-179 renders this` comment) so the workspace stays green; TASK-179 owns the real rendering. This is the one line this task may touch outside teton-protocol.

## Technical Notes

- Copy the shapes exactly from `SessionClearParams`/`SessionClearResult` and `ContextCleared`. Keep field docs short; the wire is documented in the spec.
- Do not add a `session_id` to the event payload (`events.rs:2042-2045` explains why).
- Commit as `feat(protocol): session root view, session/set_cwd, session_root_changed [TASK-174]`.
