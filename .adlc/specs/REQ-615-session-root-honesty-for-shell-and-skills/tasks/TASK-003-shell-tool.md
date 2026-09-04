---
id: TASK-003
title: "The shell tool states the cwd contract, notes a cd, and refuses a write"
status: complete
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001, TASK-002]
---

## Description

BR-1, BR-2 and BR-4's shell half, all in `ShellTool`.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — description sentence, the cwd
  note in `render_output`, the write gate in `run`.

## Acceptance Criteria

- [ ] `ShellTool::description()` carries BR-1's sentence verbatim: *"Each command
      starts in the session root; `cd` inside a command does not carry to the
      next one. Only the user can move the root, with `/cd <path>` — say so
      instead of trying."*
- [ ] A command whose output is rendered and which contained a `cd` carries the
      note *"[ran in <root>; the next command starts there again]"* between the
      status line and the body — the slot the REQ-607 withheld advisory uses.
- [ ] The note is **outside `cap_output`**: it does not count toward
      `raw_output_chars`, so it cannot change whether the `shell` duty fires, and
      a chatty command cannot push it out.
- [ ] The note is emitted on **every** root kind (BR-2 is not gated on kind) and
      only when the command carried a `cd`.
- [ ] At a `Home` / `FilesystemRoot` root, a write-gated command is refused
      **before `run_bounded` is called** — assert by inspection that no child
      was spawned, not from the error text (LESSON-519).
- [ ] The refusal names the root display, the kind, and the remedy `/cd <name>`,
      and publishes `write_refused_non_project`.
- [ ] The refusal carries **no provenance and no measurement** — nothing ran, so
      no bytes came off this machine and the `shell` duty must not see it (the
      argument the three existing pre-spawn arms already make).
- [ ] `cargo test -p tetond` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/harness/tools/shell.rs::the_description_states_the_cwd_contract` | no |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_cd_bearing_command_carries_the_cwd_note` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell.rs::the_cwd_note_is_outside_the_cap_and_the_duty_trigger` | no |
| BR-4 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_write_at_a_home_root_is_refused_before_any_child_spawns` | yes |
| AC-2 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_cd_bearing_command_carries_the_cwd_note` | yes |
| AC-3 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_write_at_a_home_root_is_refused_before_any_child_spawns` | yes |

## Technical Notes

Put the write gate immediately after the root canonicalization in `run` and
before the `timeout_ms` computation, so it is unambiguously pre-spawn.

For the "no child spawned" assertion, use a command whose *effect* is
observable — `mkdir -p <tmp>/probe` — and assert the directory does not exist
afterwards. That is AC-3's own instruction: inspect the artifact, do not infer
from the error.

The note goes in `render_output`, which already takes `command`; it needs the
root display, so pass `ctx.root_display()` through. `render_output` is called
from exactly one arm — thread it as a parameter rather than reaching for a
global.
