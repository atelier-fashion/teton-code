---
id: TASK-427
title: "ChildDispatcher and the child turn runner: route → assemble → attempt with bounds, deadline, provenance, and the eight statuses"
status: draft
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-422", "TASK-425", "TASK-426"]
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
`refused(over_budget)`. Bounds are stamped before the first model call and echoed. The
outcome carries the report (bounded, loud marker if cut), the provenance set, turns used,
route, cost, and exactly one of the eight statuses.

## Files to Create/Modify

- `crates/tetond/src/harness/child.rs` — new: trait, `ChildSpec`, `ChildOutcome`, `PausableDeadline`, `bound_report()`; unit tests for the deadline pause and the report cut
- `crates/tetond/src/harness/mod.rs` — export
- `crates/tetond/src/runtime/child_turn.rs` — new: `impl ChildDispatcher for DaemonRuntime`
- `crates/tetond/src/runtime/mod.rs` — module, `ToolSet` enum threaded to `build_tools`
- `crates/tetond/src/runtime/turn.rs` — `resolve_the_route` accepts an optional tier request; `build_tools` takes `ToolSet`; `run_attempts` is callable with a child `HarnessConfig` (`max_turns` from `child_max_turns` clamped to the parent's)
- `crates/tetond/src/router.rs` — tier-request resolution: binding if configured, else the category's default route, then the boundary pin
- `crates/tetond/src/harness/permissions.rs` — the gate exposes its ask-await so the deadline can pause around it

## Acceptance Criteria

- [ ] A child's first provider request holds system prompt, `context`, `task` and no parent block (inspect the captured request)
- [ ] `ToolSet::Child` registry contains no `agent`
- [ ] A `tier: build` request under a Think parent resolves to the Build binding when one exists, else the category default, and the outcome names it; a local-only read mid-child pins the remainder local
- [ ] `PausableDeadline` does not advance while paused; expiry aborts an in-flight tool and yields `timed_out`
- [ ] Each of the eight statuses is produced by a unit or integration test in this task or TASK-430 (list which here)
- [ ] A report of `report_max_bytes + 1` is cut to the bound plus the typed marker; the outcome says `truncated`
- [ ] Mutations recorded: drop the whole-or-refused check, drop the pause, drop the clamp — name what reddens

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::child_context_is_system_context_task_only` | yes |
| BR-2 | test-case | `crates/tetond/src/runtime/turn.rs::tests::child_toolset_omits_agent` | yes |
| BR-6 | test-case | `crates/tetond/src/router.rs::tests::tier_request_binding_default_then_pin` | yes |
| BR-7 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::bounds_stamped_before_first_call_and_echoed` | no |
| BR-9 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::outcome_carries_provenance_union` | no |
| BR-10 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::terminal_status_matrix` | yes |
| BR-11 | test-case | `crates/tetond/src/harness/child.rs::tests::report_cut_is_loud` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/child.rs::tests::deadline_pauses_during_consent` | yes |
| AC-2 | test-case | `crates/tetond/src/runtime/child_turn.rs::tests::child_context_is_system_context_task_only` | no |
| AC-3 | test-case | `crates/tetond/src/runtime/turn.rs::tests::child_toolset_omits_agent` | yes |
| AC-9 | test-case | `crates/tetond/src/router.rs::tests::tier_request_binding_default_then_pin` | yes |
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
