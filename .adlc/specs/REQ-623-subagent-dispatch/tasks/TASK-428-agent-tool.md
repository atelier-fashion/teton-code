---
id: TASK-428
title: "AgentTool: schema, caps, JoinSet fan-out, consent serialisation, the as_agent() loop arm, registration"
status: draft
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-427", "TASK-422"]
repo: teton-code
---

## Description

ADR-1, ADR-5, ADR-7. The tool itself: validates `tasks` (1..=`max_children_per_call`, unique
names, non-empty task, per-turn cap) and refuses typed before any child starts; builds a
`SharePool` and a per-call consent `Mutex`; spawns one `run_child` per task into a
`JoinSet` on the runtime `Handle`; publishes `agent_call_started`/`agent_child_started`/
`agent_child_finished`/`agent_child_share_released`/`agent_call_finished` through the
parent emitter; assembles `ChildResult[]` as `UntrustedData` with the provenance union on the
block. The loop gains `Tool::as_agent()` and an `.await` arm in `run_the_allowed_tool` before
the `block_in_place` dispatch. Registered in `build_tools` behind `agent.enabled`, cap-exempt
with its reason, `Allowance::Twice` in the repeat ledger, `Allow` at every permission level.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/agent.rs` — new: `AgentTool`, schema, validation, `dispatch`, result assembly, `run` returning `agent_requires_async_dispatch`; unit tests
- `crates/tetond/src/harness/tools/mod.rs` — `as_agent`, `CAP_EXEMPT_TOOLS` row, export, `AGENT_TOOL_NAME`
- `crates/tetond/src/harness/turn_loop.rs` — the `as_agent` arm in `run_the_allowed_tool`; cancellation reaching the arm aborts the set
- `crates/tetond/src/harness/repeat.rs` — `agent` classified write-capable
- `crates/tetond/src/harness/permissions.rs` — default row; `permission_request` carries `child_id` when asked from a child
- `crates/tetond/src/runtime/turn.rs` — `register_agent_tool` in `build_tools` for `ToolSet::Prompt` only
- `crates/tetond/tests/repeat_refusal.rs` — `agent` refused on the third identical call, not the second

## Acceptance Criteria

- [ ] Six tasks → `too_many_children` naming 6 and 5, no `agent_child_started`; a second call pushing past `max_children_per_turn` → `child_cap_reached`
- [ ] Duplicate names and an empty task refuse typed
- [ ] Three children spawn concurrently (rendezvous via TASK-423's fixture in TASK-430; here a unit test with a stub dispatcher that counts overlap)
- [ ] A child ask takes the call mutex; a second child's ask waits; the first grant is visible to the second without a re-ask
- [ ] `agent.enabled = false` → not in the registry, model call gets the unknown-tool refusal naming the key
- [ ] `CAP_EXEMPT_TOOLS` parity test (REQ-587 AC-17) passes with the new row
- [ ] The parent emitter stays live while children run (unit: events published by a stub child reach a subscriber before `dispatch` returns)
- [ ] Mutations recorded: remove the per-call cap check, remove the mutex — name what reddens

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/src/harness/tools/agent.rs::tests::caps_refuse_whole_and_typed` | yes |
| BR-4 | test-case | `crates/tetond/src/harness/tools/agent.rs::tests::children_overlap_and_parent_waits_for_all` | no |
| BR-5 | test-case | `crates/tetond/src/harness/tools/agent.rs::tests::asks_serialise_and_grants_are_shared` | yes |
| BR-12 | test-case | `crates/tetond/src/harness/tools/agent.rs::tests::child_registry_has_skill_not_agent` | no |
| BR-14 | test-case | `crates/tetond/src/runtime/turn.rs::tests::agent_disabled_is_absent_from_registry` | yes |
| AC-1 | test-case | `crates/tetond/src/runtime/turn.rs::tests::agent_roster_schema_and_disabled` | yes |
| AC-4 | test-case | `crates/tetond/src/harness/tools/agent.rs::tests::caps_refuse_whole_and_typed` | yes |
| BR-3 | test-case | `crates/tetond/tests/repeat_refusal.rs::agent_is_write_capable_third_identical_refused` | yes |

## Technical Notes

- Mirror `register_skill_tool` (`skill.rs:2336`): takes the registry, `Arc<PermissionGate>`, `Handle`, plus `Arc<dyn ChildDispatcher>` and `AgentConfig`.
- The `as_agent` arm goes **before** line 1956's `block_in_place_if_multithread`; BUG-226's fix must remain the path for every other tool.
- Tool result disposition is `UntrustedData` — a report is content about the repo, not instructions (REQ-587's framing rule).
- The REQ-617 repeat ledger keys on name+args; identical `tasks` twice is the "repeat" case.
- ASSUME-010: `#[cfg(test)]` after `impl Tool for AgentTool`.
