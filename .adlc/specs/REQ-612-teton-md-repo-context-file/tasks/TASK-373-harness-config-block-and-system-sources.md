---
id: TASK-373
title: "`HarnessConfig.repo_context`, the prompt's tail, and `ContextManager::system_sources`"
status: complete
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-371]
---

## Description

ADR-1 and ADR-2 in the harness, still with no runtime wiring: the config field, the composer
appending the block last, and the manager-level provenance source that `context_provenance`
unions and the carry seams re-state. Also the `prefix_cache_session` case for AC-9, since the
harness is where the prompt bytes are decided.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — `RouteBudget.repo_context_cap = min(REPO_CONTEXT_MAX_BYTES, budget_bytes / 4)`,
  derived in `derive` beside the pair (ADR-5); a table test over local, floored, 128k and 1M routes.
- `crates/tetond/src/harness/turn_loop.rs` — `HarnessConfig.repo_context: Option<RepoContextBlock>`
  (default `None`); `build_system_prompt` appends `block.text` after the tool docs; doc comment
  on the order (ADR-1).
- `crates/tetond/src/harness/context.rs` — `system_sources: BTreeSet<ProvenanceId>`,
  `with_system_sources`, `system_sources()`; `Clone`/`Debug` derive unaffected.
- `crates/tetond/src/harness/completion.rs` — `context_provenance` unions `system_sources`
  through `tool_result_provenance(&ToolProvenance::Sources(..))`.
- `crates/tetond/src/carry.rs` — `CarriedTurn::begin` and the reroute `rebudget` path
  set `system_sources` from the route's `repo_context` (never from `RetainedContext`).
- `crates/tetond/tests/prefix_cache_session.rs` — AC-9 case.

## Acceptance Criteria

- [x] AC-1: with `repo_context = Some(block)` the prompt ends with the block for both
      `HarnessConfig::default()` and `for_strong_model()`; with `None` it is byte-identical to
      the pre-task prompt.
- [x] BR-5: `context_provenance` of a manager with a system source contains that id; a mutation
      that drops the union fails the test; the empty set contributes nothing (`is_unknown`
      false, `len` unchanged).
- [x] LESSON-501: the source is present after `begin`, after `rebudget`, and after a replay
      from `RetainedContext` — three seams, three assertions.
- [x] AC-9: two prompts with the same block hit the prefix cache; a changed block misses once.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-1 | test-case | `crates/tetond/src/harness/turn_loop.rs::the_repo_context_block_is_the_last_region_of_both_harness_shapes` | yes |
| BR-5 | test-case | `crates/tetond/src/harness/completion.rs::context_provenance_unions_the_system_sources` | yes |
| BR-5 | test-case | `crates/tetond/src/carry.rs::system_sources_are_restated_at_begin_rebudget_and_replay` | no |
| AC-9 | test-case | `crates/tetond/tests/prefix_cache_session.rs::an_unchanged_repo_context_block_keeps_the_prefix_cache_and_a_changed_one_misses_once` | yes |
| BR-3 | test-case | `crates/tetond/src/harness/budget.rs::the_repo_context_cap_is_a_quarter_of_the_byte_budget_up_to_the_pinned_max` | yes |

## Technical Notes

`ContextManager::new(system, ..)` keeps its signature; the sources are set by the builder that
already knows the route. The union spelling is the skill-expansion arm's (`completion.rs:903`),
so one mapping decides what a repository file means to egress.

## Implementation notes (TASK-373, on completion)

- `CarriedTurn` lives at `crates/tetond/src/carry.rs`, not under `harness/`; the two
  references above are corrected to the real path. The test names are unchanged.
- `RouteBudget` gained a field, so the router's `BUDGET_FOR_GOLDEN` Debug snapshot
  (`router.rs`) gained a column. All five bounds carry `repo_context_cap: 8192` — the
  narrowest byte half in that table is 32,768, whose quarter is exactly the pin — so the
  addition moved no derived figure. The golden's doc records that.
- `StampedRoutes::record` (`server.rs`) rebuilds a `RouteBudget` from a `route_decided`
  event; its `repo_context_cap` is derived through `budget::repo_context_cap` rather than
  zeroed, because `budget_bytes` is on that wire and the quarter rule has one home.
- No runtime stamping: the `route` stage still does not set `repo_context`, and the two
  resident-prompt ceiling sweeps and their recorded margins are untouched (TASK-374/375).
