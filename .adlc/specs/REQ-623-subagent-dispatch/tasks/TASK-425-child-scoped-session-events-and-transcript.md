---
id: TASK-425
title: "SessionEvents::for_child and child-tagged transcript records"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-421"]
repo: teton-code
---

## Description

ADR-3. `SessionEvents::for_child(child_id, parent_turn_id)` returns an emitter on the same
bus, session and sink that stamps both ids into every payload carrying the fields. The
transcript sink writes the ids into the **bodies** of `ToolCallInput`/`ToolResult` and the
new agent records — never among the reserved keys. One session, one file.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — `SessionEvents::for_child`, stamping in the publish helpers for the five payloads
- `crates/tetond/src/transcript/record.rs` — body fields, record kinds for the seven agent events
- `crates/tetond/src/transcript/writer.rs` — pass-through
- `crates/tetond/tests/transcript.rs` — a child-scoped `tool_call_input` line carries both ids in its body and none in reserved keys; parent and child lines share one file
- `docs/transcript-format.md` — the two body fields and the agent record kinds

## Acceptance Criteria

- [x] A `tool_started` published through `for_child` carries `child_id`/`parent_turn_id`; one published through the parent emitter carries neither
- [x] The transcript line for a child record has the ids in the body and the reserved key set unchanged
- [x] Reserved-key test in `record.rs` still passes
- [x] `cargo test -p tetond transcript` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-13 | test-case | `crates/tetond/tests/transcript.rs::child_records_tagged_in_body_not_reserved_keys` | yes |
| AC-17 | test-case | `crates/tetond/tests/transcript.rs::one_file_holds_parent_and_children` | no |

## Technical Notes

- `SessionEvents` is at `turn_loop.rs:752`; it already carries bus, session id and optional sink — `for_child` clones those and adds the two ids.
- LESSON-501: the ids are stamped where the event is made; no re-tagging in the tap.
- REQ-611 BR-8's denied-prefix rule is untouched.

## Implementation Notes

- `SessionEvents::for_child(&self, child_id: ChildId, parent_turn_id: TurnId) -> SessionEvents`
  clones bus, session id and sink and sets a private `child: Option<ChildScope>`; `new`/`with_sink`
  leave it `None`. Stamped in `emit` (every `session_update`: tool start, tool finish, text chunk),
  `context_pressure`, and the `tool_input`/`tool_result` sink hand-offs. `child_scope()` exposes the
  pair for the payloads built outside this type (`permission_request` in the gate — TASK-428;
  `cost_recorded` in the ledger — TASK-424), so they stamp the ids the emitter was made with.
- `transcript::record::ChildScope { child_id, parent_turn_id }` is `#[serde(flatten)]`ed as
  `Option` into `ToolCallInput`/`ToolResult`: two body keys when `Some`, none when `None`.
- The seven `agent_*` events need no sink-local kind: they are bus events and are recorded as
  envelopes under their wire names. `every_agent_event_kind_is_documented_in_the_format_doc` reads
  them off `Event::name` and requires each in `docs/transcript-format.md`.
- Reserved-key collision: `Record::body` now *displaces* a payload field named like a line key
  (`truncated`, `kind`, `n`, `ts`, `original_bytes`) under `event_fields` instead of dropping it;
  only the envelope's frame keys (`session_id`, `seq`, `event`) are lifted and removed.
  `agent_child_finished.truncated` reaches the file as `event_fields.truncated`; the same rule
  restores `context_pressure.kind`, `repo_context_state.truncated` and `web_lookup.kind`, which
  were silently dropped before.
- The two `tests/transcript.rs` tests build the real sink, bus tap and emitter in-process (the
  binary cannot start a child until TASK-427/428); TASK-430 re-asserts AC-17 through the daemon.
- Mutations (run 2026-10-05, restored): `emit` stamp dropped → only the turn_loop unit test
  reddens; `context_pressure` stamp dropped → unit + both transcript tests; `tool_input`/
  `tool_result` scope dropped → both transcript tests; displacement reverted → record.rs unit +
  `one_file_holds_parent_and_children`; parent emitter born child-scoped → all three; `flatten`
  dropped → record.rs / writer.rs unit tests. Each test's doc comment names its assertion.
