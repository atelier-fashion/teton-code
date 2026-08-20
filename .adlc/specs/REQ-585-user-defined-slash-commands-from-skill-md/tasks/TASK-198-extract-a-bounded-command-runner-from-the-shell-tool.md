---
id: TASK-198
title: "Extract the shell tool's spawn body so dynamic context is a second caller, not a second implementation"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-197]
---

## Description

BR-6 runs a skill's `` !`cmd` `` under "the `shell` tool's jail, timeout and
output cap". Copying that logic would put the jail, the PATH floor, the
process-group kill and the output cap in two places, and the copy would drift —
the LESSON-528 shape one layer down. Extract it once; `ShellTool::run` becomes
its first caller.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — `pub(crate) fn run_bounded(root: &Path, command: &str, timeout_ms: u64) -> BoundedRun`; `ShellTool::run` refactored to call it

## Acceptance Criteria

- [ ] `run_bounded` owns: `canonicalize` of the root and `.current_dir`, `scrub(env::vars())` + `apply_path_floor` + `.env_clear().envs(...)`, `.process_group(0)`, `.stdin(Stdio::null())`, the `recv_timeout` wait, the `libc::kill(-(pid), SIGKILL)` on timeout, and the `MAX_OUTPUT_CHARS` cap.
- [ ] `BoundedRun` is a typed outcome — `Completed { status, output }` / `TimedOut` / `SpawnFailed(reason)` — so a caller can distinguish "ran and failed" from "never ran" without parsing a message (BR-6 needs both, with different placeholder text).
- [ ] `ShellTool::run` behaviour is **unchanged**: the existing tests pass without edits, including `timeout_kills_a_runaway_command` (`shell.rs:634`) and `a_timeout_from_a_home_kind_root_hints_at_the_consent_dialog` (`shell.rs:650`). The `TIMEOUT_CONSENT_HINT` and the `.with_unknown_provenance()`/`.measuring(NO_OUTPUT_CAPTURED)` decoration stay in `ShellTool::run` — they are the tool's presentation, not the runner's.
- [ ] `Tool::refine` is untouched and stays on the tool. It fires the `shell` duty, which is a model call; BR-4 forbids a model call at expansion time.
- [ ] `ShellTool::with_timeouts(200, 500)` (`shell.rs:115`) still works and is the seam TASK-205's timeout test uses.
- [ ] Mutation: deleting `apply_path_floor` or `process_group(0)` from `run_bounded` fails an existing shell test — i.e. the extraction did not move logic out from under its coverage.

## Technical Notes

- Pure refactor. No behaviour change, no new tests beyond what proves the extraction is faithful. If a test has to change, the extraction is wrong.
- Keep it `pub(crate)` in the `shell` module rather than promoting it to `tools::mod` — the caller set is exactly two and the jail semantics are the shell tool's.
- **Sequenced behind TASK-197 on purpose.** TASK-197 changes `CarriedTurn::begin`'s signature across six production and three test call sites. Parallel implementers share one worktree (LESSON-541), so a concurrent `tetond` task would see a workspace that does not compile through no fault of its own. TASK-197 is not a functional dependency — it is a compile-stability one, and it is the only such edge in this REQ.
