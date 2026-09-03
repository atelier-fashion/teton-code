---
id: TASK-374
title: "Runtime wiring: load at create and `/cd`, refresh in `assemble`, the switch, and `session/context`"
status: draft
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-369, TASK-370, TASK-373]
---

## Description

ADR-3 and ADR-6's daemon half: the session record holds the `RepoContextState` and the session
switch; the loader runs at the three sites; the `assemble` stage stamps the block onto the route
and seeds the manager's sources; `session/context` and `SetRepoContextEnabled` are handled; the
one event is published. Covers BR-1 (when), BR-2 (the switches), BR-6 (reload), and the
ordering half of AC-2.

## Files to Create/Modify

- `crates/tetond/src/sessions.rs` — `repo_context: Arc<RepoContextState>` and
  `context_switch: Option<bool>` on the record; `set_repo_context`, `repo_context`,
  `set_context_switch`, `context_switch` beside `set_skills`/`skills`.
- `crates/tetond/src/runtime/session.rs` — `store_session_repo_context` beside
  `store_session_skills`; called at create (line 411 region) and inside `set_session_cwd`
  **before** the `context_cleared` / `session_root_changed` publish; `session_context(&params,
  &events)` for the method (`Status` reads, `On`/`Off` set the switch and re-load at once).
- `crates/tetond/src/runtime/turn.rs` — in `assemble`, after `session_root_for` and before
  `build_system_prompt`: `refresh` against the stored state, store a changed one, publish the
  event on change, stamp `route.harness.repo_context`, seed `system_sources`.
- `crates/tetond/src/runtime/mod.rs` — `SetRepoContextEnabled` persistence through the same
  write path as `SetTranscriptEnabled`; `config/get` shows the posture.
- `crates/tetond/src/server.rs` — `handle_session_context` shaped as `handle_session_transcript`
  (`may_drive` gate); dispatch arm.

## Acceptance Criteria

- [ ] BR-1 / AC-2: a `session/create` from a project root with `TETON.md` stores a `Loaded`
      state and publishes `repo_context_state` once; a `/cd` into another project stores the
      new state **before** `session_root_changed` is published (assert order on the bus:
      `context_cleared`, `session_root_changed`, and the new state visible to a second client
      by the time it reads `session_root_changed`); a `/cd` to a `home` root drops the block.
- [ ] BR-6 / AC-8: an edit between prompts is resident on the next prompt with one event; a
      touch that changes neither `len` nor `mtime` reads nothing; a mid-turn edit is not
      resident until the next prompt (drive a two-iteration tool loop).
- [ ] BR-2 / AC-10: `Off` drops the block from the next turn and writes nothing to
      `config.toml`; `On` re-loads at once; `repo_file = false` in config yields `WithheldOff`
      with zero reader calls (the injected reader's counter).
- [ ] The method refuses an unattached connection with `NOT_ATTACHED`, as `session/transcript`
      does; the model has no tool that reaches it (grep the registry).
- [ ] OQ-4: a boundary configured mid-session that covers the file yields `WithheldBoundary` at
      the next refresh with the event.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/tests/repo_context.rs::create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed` | yes |
| BR-2 | test-case | `crates/tetond/tests/repo_context.rs::the_session_switch_and_the_durable_switch_withhold_without_opening_the_file` | yes |
| BR-6 | test-case | `crates/tetond/tests/repo_context.rs::an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not` | yes |
| AC-2 | test-case | `crates/tetond/tests/repo_context.rs::create_loads_the_file_and_cd_rebuilds_it_before_session_root_changed` | yes |
| AC-8 | test-case | `crates/tetond/tests/repo_context.rs::an_edit_between_prompts_is_resident_on_the_next_and_a_mid_turn_edit_is_not` | yes |
| AC-10 | test-case | `crates/tetond/tests/repo_context.rs::the_session_switch_and_the_durable_switch_withhold_without_opening_the_file` | yes |

## Technical Notes

**Already landed by the orchestrator (commit after TASK-370):** the `ConfigUpdate::SetRepoContextEnabled`
arms in `runtime/turn.rs` (restates: None), `runtime/mod.rs::reject_unusable_binding` (Ok) and
`runtime/mod.rs::apply_update` (`config.context.repo_file = enabled`). Do not re-add them; build on them.

The refresh runs on the claiming turn only (REQ-583 ADR-4: re-read after the claim). Do not put
the event on `EventBus::publish` from inside the sessions mutex — publish after the store, as
`set_session_cwd` does. Never use `block_in_place` for a `stat`; it is one syscall.
