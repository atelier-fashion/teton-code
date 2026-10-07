---
id: TASK-427
title: "ChildDispatcher and the child turn runner: route → assemble → attempt with bounds, deadline, provenance, and the eight statuses"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-425", "TASK-426", "TASK-432"]
repo: teton-code
---

## Description

ADR-2 and ADR-8. `harness/child.rs` defines what the tool needs from the runtime —
`trait ChildDispatcher { async fn run_child(&self, spec: ChildSpec) -> ChildOutcome }`,
`ChildSpec` (task, context, name, tier request, parent turn id, child id, share pool handle,
consent mutex), `PausableDeadline`, and the report bound. `runtime/child_turn.rs` implements
it on `DaemonRuntime` by reusing `resolve_the_route` (with the tier hint), `assemble_harness`
(with `ToolSet::Child`, so no `agent` in the registry) and `run_attempts` from
`runtime/turn.rs`; it skips claim, naming, settle and commit. The child's context is the
system prompt + `context` + `task`; task+context are admitted whole or the child is
`refused(over_budget)`. Bounds (`max_turns` from `child_max_turns`, clamped to the parent's — the config value arrives on `ChildSpec`, threaded by TASK-428) are stamped before the first model call and echoed. The
outcome carries the report (bounded, loud marker if cut), the provenance set, turns used,
route, cost, and exactly one of the eight statuses.

## Files to Create/Modify

- `crates/tetond/src/harness/child.rs` — new: trait, `ChildSpec`, `ChildOutcome`, `PausableDeadline`, `bound_report()`; unit tests for the deadline pause and the report cut
- `crates/tetond/src/harness/mod.rs` — export
- `crates/tetond/src/runtime/child_turn.rs` — new: `impl ChildDispatcher for DaemonRuntime`
- `crates/tetond/src/runtime/mod.rs` — module, `ToolSet` enum threaded to `build_tools`
- `crates/tetond/src/runtime/turn.rs` — `resolve_the_route` passes the child's tier request through (TASK-432); `build_tools` takes `ToolSet`; `run_attempts` is callable with a child `HarnessConfig` (`max_turns` from `child_max_turns` clamped to the parent's)

## Acceptance Criteria

- [x] A child's first provider request holds system prompt, `context`, `task` and no parent block (inspect the captured request) — `child_context_is_system_context_task_only` parses the captured ChatML request: `[system, user]`, the user message is the task verbatim, the system segment is the parent's own captured system prompt followed by the child section carrying `context`
- [x] `ToolSet::Child` registry contains no `agent` — `child_toolset_omits_agent` (the child's registry is the prompt turn's less `agent` and nothing else; `skill` kept, BR-12)
- [x] The outcome's `route` names what TASK-432's resolver returned for the request; a local-only read mid-child pins the remainder local — `ChildResult.route` is `route.route_decided()`'s projection of the `dispatch_route` result (pin first, then `resolve_with_tier_request` / the new `resolve_judgment_with_tier_request`); the pin is the prompt turn's own privacy-reroute arm in `run_attempts`, which a child runs unchanged — its end-to-end proof is TASK-430 AC-10
- [x] `PausableDeadline` does not advance while paused; expiry aborts an in-flight tool and yields `timed_out` — `deadline_pauses_during_consent` (real gate, paused clock), `pauses_nest_and_move_the_wake_time`; `terminal_status_matrix` aborts a model call and a `shell` call in flight (`timed_out` returns before the command's flag is written)
- [x] Each of the eight statuses is produced by a unit or integration test in this task or TASK-430 (list which here) — all eight here, in `terminal_status_matrix`: `completed`, `refused` (`over_budget`), `turns_exhausted`, `budget_exhausted` (local window refusal), `failed` (engine error, with the code), `timed_out` (model call and `shell` in flight), `spend_exhausted` (remote route, zero share), `cancelled` (runner aborted, outcome left in `ChildOutcomeSlot`). TASK-430 still owes AC-14's eight end to end through the `agent` tool with the parent continuing, plus the `refused` flavour this task cannot produce: a project-skill gate refusal (it reaches a child as a typed tool failure, BR-5, and which child ending counts as `refused` is a rule TASK-430 must pin)
- [x] A report of `report_max_bytes + 1` is cut to the bound plus the typed marker; the outcome says `truncated` — `report_cut_is_loud`
- [x] Mutations recorded: drop the whole-or-refused check, drop the pause, drop the clamp — name what reddens. Over the 60 tests matching `child`: whole-or-refused → 1 red (`terminal_status_matrix`, the child ends `budget_exhausted` instead); the pause → 1 red (`deadline_pauses_during_consent`); the clamp → 1 red (`bounds_stamped_before_first_call_and_echoed`). Seventeen more recorded in the tests' doc comments

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::child_context_is_system_context_task_only` | yes |
| BR-2 | test-case | `crates/tetond/src/runtime/turn.rs::tests::child_toolset_omits_agent` | yes |
| BR-7 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::bounds_stamped_before_first_call_and_echoed` | no |
| BR-9 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::outcome_carries_provenance_union` | no |
| BR-10 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::terminal_status_matrix` | yes |
| BR-11 | test-case | `crates/tetond/src/harness/child.rs::tests::report_cut_is_loud` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/child.rs::tests::deadline_pauses_during_consent` | yes |
| AC-2 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::child_context_is_system_context_task_only` | no |
| AC-3 | test-case | `crates/tetond/src/runtime/turn.rs::tests::child_toolset_omits_agent` | yes |
| AC-11 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::bounds_stamped_before_first_call_and_echoed` | no |
| AC-15 | test-case | `crates/tetond/src/harness/child.rs::tests::report_cut_is_loud` | yes |
| AC-19 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::child_shell_starts_in_session_root` | no |

## Technical Notes

- The tools layer must not import `runtime::*` — the trait lives in `harness/child.rs`, the impl in `runtime/`.
- `run_attempts` (`turn.rs:1625`) already handles reroute/refit per REQ-586; a child's reroute emits `context_pressure` through the `for_child` emitter (TASK-425) with bounds unchanged.
- Cancellation: an aborted `JoinHandle` lands inside the loop's existing trim (`turn_loop.rs:1616-1632`); make sure the child's outcome path on abort reports `cancelled` rather than panicking — a `Drop` guard on the child's state is the cleanest.
- Conventions `run_prompt_turn` ≤ 200 lines (REQ-606): do not grow it; add stages beside it.
- LESSON-539: the child re-reads the session root from the registry under the parent's held claim, not from a pre-claim snapshot.
- ASSUME-010: `#[cfg(test)]` last in each new file.
