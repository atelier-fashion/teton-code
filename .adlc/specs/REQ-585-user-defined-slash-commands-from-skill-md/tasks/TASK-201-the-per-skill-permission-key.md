---
id: TASK-201
title: "One consent per invocation, under the skill's own key — and project grants die at /cd"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-195, TASK-196]
---

## Description

BR-6's gate half and ADR-6. The permission gate keys remembered grants by
exactly the string it asked about, so a skill's dynamic context must ask under
a key that is per skill and is **not** `shell`.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — `is_skill_permission_key`, `authorize_skill`, the `debug_assert!` widening, `drop_project_skill_grants`
- `crates/tetond/tests/skill_consent_matrix.rs` — the grant-isolation matrix

## Acceptance Criteria

- [ ] `permission_key_for(skill)` is `skill:<source>:<name>` (`source ∈ {user, project}`), and `is_skill_permission_key` sits beside `is_web_permission_key` (`permissions.rs:995`) with a doc that carries the LESSON-495 argument, as that one does.
- [ ] `table_for` and `READ_ONLY_TOOLS` are **not modified**. The key rides the level default — `guarded` ask, `edits` ask, `plan` deny, `full` allow. `an_unknown_server_supplied_tool_is_classified_by_the_levels_default` (`permissions.rs:1188`) is the existing proof of that posture; extend it with a skill key rather than replacing it. `expected_rows` (`:1078`) must **not** gain a skill row.
- [ ] `authorize_skill(key, commands, skill, source)` asks **once per invocation**, publishing one `PermissionRequest` whose `subject` is `PermissionSubject::SkillDynamicContext { skill, source, commands }` with every command listed verbatim in document order. Never one prompt per command (REQ-560 BR-2's anti-pattern).
- [ ] **The request is addressed to the connection that sent the invocation, and only that connection may answer it.** Today `permission_request` reaches every connection attached to the session and any driver may answer — so a pre-REQ-585 client attached alongside a new one would see a request it cannot recognize, understand no `subject`, and call `prompter.ask`: on a pipe the next stdin line becomes a `y` that authorizes shell commands. This is the guard, not a refinement; BUG-177 established connection-targeted delivery for the replay path and this reuses that shape. Two assertions: an unaddressed attached connection never receives it, and an answer from an unaddressed connection is refused.
- [ ] `PermissionOutcome::Refused { reason }` (TASK-196) is distinguished from `Cancelled` and from a decline, so the fold can produce AC-9's "no human could be asked" placeholder rather than a decline placeholder.
- [ ] Grant isolation, each its own assertion (copy the `web_consent_matrix.rs:917` shape — "a grant on key A does not un-ask key B"):
  - a prior `shell` allow-always does **not** answer a skill request;
  - a skill allow-always does **not** answer a model-issued `shell` call;
  - an allow-always on `skill:user:status` does **not** answer `skill:user:canary`;
  - an allow-always on `skill:project:x` does **not** answer `skill:user:x`.
- [ ] "For this session" lasts to session end and not beyond (`web_consent_matrix.rs:719` shape).
- [ ] `drop_project_skill_grants` removes every remembered grant whose key begins `skill:project:`, and is called on `/cd`. A grant remembered in one repo must not authorize another repo's commands after the root moves (ADR-6, LESSON-501).
- [ ] `authorize`'s `debug_assert!(!is_web_permission_key(...))` (`permissions.rs:721`) still fires for a web key and does **not** fire for a skill key — asserted in both directions, because a guard whose precondition is untested is a guard whose claim is untested (LESSON-504).
- [ ] `options_for` returns the standard four for a skill key; the fifth (`enable_permanent`) is web-only and stays web-only — there is no `[skills] tier` to write.
- [ ] Mutation table: keying on `shell` instead of the skill key, dropping the source from the key, and skipping `drop_project_skill_grants` each fail a named test.

## Technical Notes

- The key's shape is illustrative to the client and load-bearing to the gate. BR-11 forbids the client from parsing it — the client selects on `PermissionSubject`, never on the string (TASK-207).
- One prompt, many commands: the description string cannot carry them (`Surface::line` destroys newlines), which is why they ride `subject.commands` as a list. See ADR-7.
