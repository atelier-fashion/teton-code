---
id: TASK-399
title: "The preamble verdict is taken inside `run_all`, once per command, before it spawns"
status: draft
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: []
---

## Description

BR-1, BR-2, BR-7 (ADR-619-1). `skills::dynamic::run_all` takes a `Reach`
input (`root`, `root_kind`, `boundaries`, `denied_prefixes`) and, for each
command, calls `shell_provenance::classify` **before** `run_bounded`,
returning `PreambleRun { verdict, outcome }` per command. `Verdict`,
`VerdictKind` and `RootKind` are re-exported from `harness::tools` so the
skills module names REQ-614's grammar rather than copying it. Existing
callers keep compiling through a thin shim only if a test needs it; the
production callers move in TASK-401.

## Files to Create/Modify

- `crates/tetond/src/skills/dynamic.rs` — `Reach` struct, `PreambleRun`, `run_all(…, &Reach)` classifying before each spawn; `outcome_view` takes the verdict (fields added in TASK-402 stay `None` until then); tests
- `crates/tetond/src/harness/tools/mod.rs` — `pub(crate) use shell_provenance::{classify, Verdict, VerdictKind}` (and `RootKind` if not already reachable), with a doc line naming the second consumer
- `crates/tetond/src/harness/tools/shell_provenance.rs` — module doc gains the second consumer; no grammar change
- `crates/tetond/tests/runtime_visibility.rs` / `crates/tetond/tests/runtime_module_map.rs` — only if the re-export trips a pinned count; adjust the pin with the reason in the test's doc

## Acceptance Criteria

- [ ] `run_all` calls `classify` exactly once per command and before that command's `run_bounded`; a `NotRun` command (door closed) is still classified and still not spawned
- [ ] A `Rooted` command that times out and a `Rooted` command that succeeds carry the same verdict; an `Unknown` command that prints nothing still carries `Unknown` (BR-2)
- [ ] The verdict's `reason` is a `&'static str`; nothing in `PreambleRun` carries output or command text beyond what `DynamicOutcome` carried before
- [ ] `cargo test -p tetond --lib skills::dynamic` green; guard suites green

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::the_verdict_is_taken_once_per_command_before_it_spawns` | yes |
| BR-1 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::an_opaque_verb_is_unknown_and_a_name_only_verb_is_rooted` | yes |
| BR-2 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::exit_status_and_output_never_change_the_verdict` | yes |
| BR-2 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::an_unrun_command_is_classified_but_not_spawned` | yes |
| BR-7 | test-case | `crates/tetond/src/skills/dynamic.rs::tests::a_preamble_reason_is_static_and_content_free` | no |

## Technical Notes

- Copy the shape of `shell.rs::the_verdict_is_computed_before_measurement`: count `classify` calls through a seam (a counting wrapper or a recorded reason list) and assert it equals the command count, taken before any spawn.
- `classify` needs the session root as cwd; `run_all` already runs commands with the session root as cwd (REQ-585 BR-6) — pass the same path.
- Do not touch consent: `authorize_skill` runs before `run_all` and lists the same substituted `Command::as_str()` the classifier reads (REQ-585 BR-6, REQ-591).
- Keep `DynamicOutcome::spawned` (tests reference it) but nothing in production should read it for provenance after TASK-401; leave a doc note that its provenance use was retired by REQ-619.
