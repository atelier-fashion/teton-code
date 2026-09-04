---
id: TASK-395
title: "/shell allow — the user's typed lift, refused on piped stdin and on a non-liftable cause"
status: complete
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-393, TASK-394]
---

## Description

The built-in session command that lifts an `unknown_shell` pin, and the
daemon handler behind it. Typed input only. It lifts nothing else, and
nothing but a user typing it can invoke it.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — the `shell allow` `CommandSpec` row, its handler, the typed-only gate and the rendered lines
- `crates/tetond/src/runtime/mod.rs` — the `shell/override` RPC handler: lift, write one ledger row, publish `session_pin_lifted`

## Acceptance Criteria

- [ ] `/shell allow` on a session pinned with `unknown_shell` lifts it, writes exactly **one** `shell_overrides` row, and publishes `session_pin_lifted`
- [ ] A second `/shell allow` in an already-lifted session is acknowledged and writes **no** row and publishes no second event
- [ ] `/shell allow` on a session pinned with `boundary_hit` is refused, and the refusal **names the cause** (BR-5, AC-2)
- [ ] `/shell allow` on an unpinned session says so and changes nothing
- [ ] Piped stdin: `printf '/shell allow\n' | teton` refuses with a line saying the command is typed-only, and changes nothing (AC-6)
- [ ] AC-7: `/shell allow` appearing inside a skill body, a `TETON.md`, or a tool result is inert — the session stays pinned and `all_shell_overrides()` is **empty**. Assert the absence, not the remedy (LESSON-550)
- [ ] The handler reaches `ShellTaintOverride::lift` from the one call site the private setter permits; a `skill` tool call named `shell allow` finds no such tool

- [ ] The `#[allow(dead_code)]` on `ShellTaintOverride::lift` (added in TASK-392, naming this task) is **removed** — the RPC handler is its production caller

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-5 | test-case | `crates/teton/src/slash.rs::shell_allow_runs_only_from_a_terminal` | yes |
| AC-2 | test-case | `crates/tetond/src/runtime/mod.rs::shell_allow_is_refused_on_a_boundary_hit_and_names_the_cause` | yes |
| AC-3 | test-case | `crates/tetond/src/runtime/mod.rs::a_second_shell_allow_writes_no_row` | yes |
| AC-6 | test-case | `crates/teton/tests/cli_e2e.rs::piped_shell_allow_is_refused_and_changes_nothing` | yes |
| AC-7 | test-case | `crates/tetond/tests/provenance_egress.rs::shell_allow_inside_a_tool_result_is_inert` | yes |

## Technical Notes

- Reuse the existing typed-only mechanism (`MODEL_SET_TYPED_ONLY`'s gate and
  its test seam), do not add a second one. `/web allow` deliberately keeps no
  such gate — ADR-614-6 records why, and this task must not change it.
- **AC-6 is a mutation test**: deleting the typed-only gate must make it fail.
  Verify that by actually deleting the gate, watching it go red, restoring it,
  and recording the observation in the test's doc comment (LESSON-520,
  conventions.md's "show the test can fail" rule).
- AC-7's three vectors are three cases, not one: a skill body, a `TETON.md`,
  and a tool result each reach context by a different seam, and a rule
  attached to one flow guards one door (LESSON-578).
