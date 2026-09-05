---
id: TASK-403
title: "End-to-end acceptance suite through the real daemon, the flipped BUG-214 claim, and the documentation"
status: draft
parent: REQ-619
created: 2026-09-05
updated: 2026-09-05
dependencies: [TASK-401, TASK-402]
---

## Description

AC-1 through AC-13 as egress-capture tests through the daemon binary
(`tests/e2e/skill_provenance.rs`), the flip of
`shell_pin_shape::a_typed_user_skill_pins_liftably_and_is_announced` to
AC-1's shape, and the prose: README's skills and privacy sections, the doctor
and skills topics, and the architecture Key Pattern. Every claim's mutation
record is written into its doc comment.

## Files to Create/Modify

- `crates/tetond/tests/e2e/skill_provenance.rs` — new: AC-1..AC-13 (see the REQ), each an egress-capture claim with the leak marker only in `secrets/prod.env`
- `crates/tetond/tests/e2e.rs` — register the module and its doc bullet
- `crates/tetond/tests/e2e/harness.rs` — a `Workspace::user_skill(name, body)` helper writing under the fixture HOME, and a `Client::skill(session, name, args)` helper for `session/prompt` with a `skill` invocation, if absent
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — flip the user-skill claim to "leaves, no pin"; keep the announcement claims on the opaque-shell tests
- `README.md` — skills section: what pins and what does not; `/shell allow` for an opaque preamble
- `crates/tetond/src/harness/docs/*.md` — the skills / privacy topics that state the retired rule
- `.adlc/context/architecture.md` — Key Pattern amended (if TASK-401 did not already)

## Acceptance Criteria

- [ ] Every AC in the REQ has one named e2e test; each asserts on captured request bodies (presence or absence by count), never on `route_decided` alone (LESSON-650)
- [ ] The BUG-214 shape (AC-13) — `sh <script>`, `cat`, `cat` — pins `unknown_shell` from the `sh` alone and leaves after `/shell allow`
- [ ] Mutation record per claim: disabling preamble classification (force `Unknown`) reddens AC-3/AC-11's leave halves; dropping the user-skill identity reddens AC-1/AC-2; the `Rooted` controls stay green
- [ ] `cargo test -p tetond --test e2e` green on both CI legs

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-1 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_user_skill_with_no_preambles_leaves_under_the_builtins` | yes |
| AC-2 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_model_invoked_user_skill_leaves_too` | yes |
| AC-3 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::rooted_preambles_leave_with_their_output` | yes |
| AC-4 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_boundary_reading_preamble_pins_permanently_and_nothing_later_leaves` | no |
| AC-5 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::an_opaque_preamble_pins_liftably_and_shell_allow_restores_routing` | yes |
| AC-6 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::the_exit_code_channel_is_closed_by_the_verdict` | no |
| AC-7 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_user_glob_naming_the_skills_directory_refuses_the_skill_by_name` | yes |
| AC-8 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_user_skills_identity_survives_compaction_and_reattach` | no |
| AC-9 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_read_of_a_user_skill_file_is_still_refused_by_the_jail` | no |
| AC-10 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::with_no_boundaries_an_opaque_preamble_is_sent_and_nothing_pins` | yes |
| AC-11 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::a_project_skill_leaves_with_a_rooted_preamble_and_is_refused_with_a_boundary_one` | yes |
| AC-12 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::skill_invoked_carries_each_commands_reach_and_nothing_more` | yes |
| AC-13 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::the_bug_214_shape_pins_liftably_from_the_sh_alone` | yes |
| BR-8 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::an_opaque_preamble_pins_liftably_and_shell_allow_restores_routing` | yes |
| BR-9 | test-case | `crates/tetond/tests/e2e/skill_provenance.rs::with_no_boundaries_an_opaque_preamble_is_sent_and_nothing_pins` | yes |

## Technical Notes

- The harness gives every daemon a fixture HOME only when a test sets it (`DaemonOptions::env("HOME", …)`, as `shell_pin_shape` does); a `Workspace::user_skill` helper should create `<root>/home/.claude/skills/<name>/SKILL.md` and return the HOME path to pass.
- Preamble consent: the harness Client auto-approves `permission_request`; the `sh`/`cat` preambles will ask once under the skill's own key.
- AC-8's mid-session glob: use `config/set` through the client (presence-gated; the harness sets `TETON_PRESENCE_ACCEPT=1`), or split into "survives compaction" (a second prompt after a forced compaction) and "survives reattach" (second client) if the mid-session path is too heavy — say which in the test doc.
- AC-9 is a `read` tool call the mock model issues with the absolute `~/.claude/skills/x/SKILL.md` path; assert the tool result is the jail's refusal and no `~/` id appears in any request.
- Rebuild the binary before running (memory: stale binaries pass e2e mutations). Run the two mutation inversions with a scratch edit and restore, and write the counts into the module doc.
