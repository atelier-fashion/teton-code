---
id: TASK-425
title: "SessionEvents::for_child and child-tagged transcript records"
status: draft
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

- [ ] A `tool_started` published through `for_child` carries `child_id`/`parent_turn_id`; one published through the parent emitter carries neither
- [ ] The transcript line for a child record has the ids in the body and the reserved key set unchanged
- [ ] Reserved-key test in `record.rs` still passes
- [ ] `cargo test -p tetond transcript` green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-13 | test-case | `crates/tetond/tests/transcript.rs::child_records_tagged_in_body_not_reserved_keys` | yes |
| AC-17 | test-case | `crates/tetond/tests/transcript.rs::one_file_holds_parent_and_children` | no |

## Technical Notes

- `SessionEvents` is at `turn_loop.rs:752`; it already carries bus, session id and optional sink — `for_child` clones those and adds the two ids.
- LESSON-501: the ids are stamped where the event is made; no re-tagging in the tap.
- REQ-611 BR-8's denied-prefix rule is untouched.
