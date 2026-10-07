---
id: TASK-428
title: "AgentTool: schema, caps, JoinSet fan-out, consent serialisation, the as_agent() loop arm, registration"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-07
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

- [x] Six tasks → `too_many_children` naming 6 and 5, no `agent_child_started`; a second call pushing past `max_children_per_turn` → `child_cap_reached` — `caps_refuse_whole_and_typed` (6 refused 6/5, 5 pass, then 5+4 refused `{started: 5, requested: 4, cap: 8}`, 5+3 pass, 8+1 refused; every refusal publishes `agent_call_refused`, no `agent_call_started`, and the dispatcher is never reached). The per-turn count is a counter on the tool, which `build_tools` rebuilds per prompt turn; a refused call reserves nothing
- [x] Duplicate names and an empty task refuse typed — same test: `duplicate_name` (an explicit name, and a name colliding with a `child-<n>` default), `empty_task {index}` (whitespace counts as empty), and a fifth code the protocol lacked, `name_too_long {name, max: 40}` (added to `AgentRefusal`; the protocol test now enumerates five — renamed `agent_refusal_codes_are_the_five_the_tool_raises`). A call with no tasks, or one that does not parse, is an argument error, not a typed refusal
- [x] Three children spawn concurrently (rendezvous via TASK-423's fixture in TASK-430; here a unit test with a stub dispatcher that counts overlap) — `children_overlap_and_parent_waits_for_all`: three children park on a barrier sized three (a sequential dispatch never passes it), peak overlap 3, nothing running when the result arrives, every `agent_child_finished` before `agent_call_finished`, results and tally in task order. Parent cancellation: `parent_cancel_aborts_every_child_and_each_reports_cancelled` (the parent's task aborted mid-call; both children dropped; each one's runner-left `cancelled` outcome and the call's end still published)
- [x] A child ask takes the call mutex; a second child's ask waits; the first grant is visible to the second without a re-ask — `asks_serialise_and_grants_are_shared` (real gate: one `permission_request` out, carrying `child_id`/`parent_turn_id` and labelled by `agent_child_consent_requested`; no second while it is open; both children's clocks paused — the asker's by the gate's observer, the queued one's by its queue; "allow for this session" answers the sibling with no second question). Benign: a child whose question a grant or the level already answers never queues, even with its call's queue held. The mutex is taken in the gate's `settle` after the level and grant steps, the grant re-read once the turn is held
- [x] `agent.enabled = false` → not in the registry, model call gets the unknown-tool refusal naming the key — `agent_disabled_is_absent_from_registry`, `agent_roster_schema_and_disabled` (the roster's schema parsed and asserted against the spec's literals; off: absent from names, docs and every cap, and `dispatch("agent")` answers `unknown tool \`agent\`; … (… agent.enabled = false)` via `ToolRegistry::note_absent`, while any other unknown tool gets the bare answer)
- [x] `CAP_EXEMPT_TOOLS` parity test (REQ-587 AC-17) passes with the new row — `the_cap_exempt_table_is_the_registrys_exempt_set` (the daemon-shaped fixture now registers `agent` too)
- [x] The parent emitter stays live while children run (unit: events published by a stub child reach a subscriber before `dispatch` returns) — `the_parent_emitter_stays_live_while_children_run`; and at the loop, `the_loop_awaits_agent_and_never_runs_it` (the `as_agent` arm awaits `dispatch` ahead of `block_in_place_if_multithread(|| tools.dispatch(..))`; `Tool::run` answers `agent_requires_async_dispatch`)
- [x] Mutations recorded: remove the per-call cap check, remove the mutex — name what reddens. Over the 2,360 tests of the `tetond` lib plus `repeat_refusal`, `cost_attribution`, `provenance_egress` and `boundary_coverage`: per-call cap check removed → 1 red (`caps_refuse_whole_and_typed`); the consent mutex removed → 1 red (`asks_serialise_and_grants_are_shared`; a mutex per child, no grant re-check, and no `child_id` on the request each the same single red); the `as_agent` arm removed, so the call reaches `Tool::run` after `block_in_place` → 3 red (`the_loop_awaits_agent_and_never_runs_it`, `repeat_refusal::agent_is_write_capable_third_identical_refused`, `provenance_egress::an_agent_childs_boundary_read_blocks_the_parents_next_remote_turn`). Seventeen more recorded in the tests' doc comments, including one hang: dispatching children sequentially hangs `parent_cancel_aborts_every_child_and_each_reports_cancelled` (killed) besides its 3 reds

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
- 2026-10-07 (Phase-4 fix): the `agent` docs line was in neither prompt-size sweep. Both now register `turn_loop::AgentToolDocs::worst_case()` (both caps `u32::MAX` — `validate_agent` bounds them from below only, and they render as digits) with a `- agent: ` self-check; the stand-in renders from the extracted `agent::describe`/`agent::schema`, pinned by `the_doc_only_agent_tool_and_the_real_one_render_one_set_of_prompt_bytes`. Measured: the line is 1,301 B at the defaults, 1,328 B at the ceiling; `spent` 24,064 → 25,392 (opted-out) and 24,017 → 25,345 (web), 816 / 769 over 24 KiB. Decision: `REDACT_BODY_OVERHEAD_BYTES` 24 → 25 KiB (the one KiB that clears it); chunk count holds at 4, `REDACT_SCANNABLE_CONTEXT_BYTES` 183,334 → 182,403 (−931 on every redact route; `router` golden digest 67,138 → 66,797); margins 512 → 208 and 559 → 255, gap 47. Red-proofs: dropped registration → both self-checks red; constant back at 24 KiB → 6 lib tests red (both sweeps' `spent <` assertion, the re-stating test, budget's two literals, the router golden).

## Implementation notes (TASK-428)

- **Wire shape** — an accepted call's tool result is `serde_json::to_string_pretty(&Vec<ChildResult>)` in task order, `ResultDisposition::UntrustedData` (the loop wraps it in `<tool-result tool="agent" trust="untrusted">…</tool-result>` plus the untrusted-data sentence), with `ToolProvenance::from_bits` of the union of every child's `ChildOutcome::provenance` on the block (ADR-8). A refusal is an error result `"<code>: <why, numbers, config key> No child was started."`, unframed.
- **Call id** is `<parent turn id>:<tool call id>` (e.g. `turn-4:call-2`), not the loop's bare `call-N`: those restart every prompt turn, and the entity table requires a call id unique within the session. Child ids are `<call id>/<name>`.
- **BR-10 "refused by a gate"** (the rule TASK-427 left open): a child that ends with an empty final text and whose every attempted call the gate denied, none having run, ends `refused` / `gate_denied:<first tool denied>`; any other denial is a tool failure the child answered itself — `gate_denial_refuses_only_a_child_that_had_nothing_else`. The loop notes denials and runs on the child's task-local `ChildToolCalls`.
- **The parent's own spend** (TASK-424's open item): `TurnContext::for_turn` in `run_attempts`; `Egress::with_turn` on the turn's and the duties' choke points, `LocalEngineSource::under_turn` on the local tier — `cost_attribution::a_parent_turns_own_calls_are_stamped_at_its_choke_point`, and the daemon path in `agent_is_write_capable_third_identical_refused` (`/cost` shows the turn's four own calls beside its two children). `CostRecord.parent_turn_id`'s doc no longer says it is `None` exactly when `child_id` is.
- **Boundary coverage** — `AgentTool` joins `boundary_coverage`'s table, covered by `provenance_egress::an_agent_childs_boundary_read_blocks_the_parents_next_remote_turn`.
- **Collateral, each forced by the change**: the CLI's refusal renderer gained the `name_too_long` arm; `runtime_visibility`'s crate-wide list gained the test-only `testsupport::turn_registry` (the test was renamed off its count); the REQ-599 module map's counts were refreshed; `skill_over_budget_offer`'s redact-scan fixture argument went 130,000 → 128,000 because `agent`'s ~1.6 KB roster entry now rides every prompt turn's system prompt; `web_tool_wiring` asserts `web` is last before the two conditional tools; `deadline_pauses_during_consent` skips the new label event.
