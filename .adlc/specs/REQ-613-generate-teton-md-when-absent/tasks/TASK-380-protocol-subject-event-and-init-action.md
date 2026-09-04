---
id: TASK-380
title: "Protocol: the generation subject, the generation event, `Init { force }`, and the shared key predicate"
status: complete
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The wire half (ADR-2, ADR-6): a `PermissionSubject::RepoContextGeneration { root, path, replace }`
variant, one `Event::RepoContextGeneration` with its outcome enum, `ContextAction::Init { force }`
on REQ-612's `session/context`, an `origin` on `RepoContextState`, and
`is_repo_context_generate_key` beside `is_project_skill_key` so the daemon and the CLI expire the
same keys (ASSUME-017).

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — the subject variant (before `Unrecognized`), the
  event and `GenerationOutcome { Offered, Declined, RefusedUnattended, DeniedLevel, Suppressed,
  Walking, Drafted, Written, Replaced, Failed }`, `name()` arm, spec-table row;
  `RepoContextState.origin: Option<RepoContextOrigin>` (additive).
- `crates/teton-protocol/src/methods.rs` — `ContextAction::Init { force: bool }`,
  `SessionContextResult` gains `origin` and `generation: Option<GenerationOutcome>`;
  `is_repo_context_generate_key`, `repo_context_generate_key(root)` (one spelling, tested).

## Acceptance Criteria

- [ ] Every new type round-trips; an older reader of `PermissionSubject` yields `Unrecognized`
      for the new variant (assert the fail-closed arm, not merely deserialization).
- [ ] `is_repo_context_generate_key` accepts exactly the minted spelling and rejects
      `skill:project:*`, `web_*`, and a root-less prefix.
- [ ] Additivity in both directions for the event and the result fields.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton-protocol/src/methods.rs::the_generation_key_predicate_matches_only_its_own_spelling` | yes |
| AC-2 | test-case | `crates/teton-protocol/src/events.rs::an_older_client_reads_the_generation_subject_as_unrecognized` | no |

## Technical Notes

`root` in the subject is the home-relative display bounded by `bounded_field`, as
`ProjectSkillTrust` should have been (REQ-591 BR-11); the key uses the durable mint, never the
display.
