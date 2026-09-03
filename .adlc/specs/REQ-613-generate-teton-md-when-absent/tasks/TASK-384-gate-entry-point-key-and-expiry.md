---
id: TASK-384
title: "`authorize_repo_context_generation`: the fourth gate entry point, its key, and expiry in both stores"
status: draft
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-380]
---

## Description

ADR-2's gate half: a new entry point shaped like `authorize_project_skill_trust`, the key minted
from the durable root, the level table, and expiry on `session_root_changed` in the daemon's
gate **and** the CLI's `SessionGrants` through the shared predicate (ASSUME-017).

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `expires_on_session_root_change` gains `is_repo_context_generate_key` as a
  third disjunct (the one predicate both stores read).
- `crates/tetond/src/harness/permissions.rs` — `authorize_repo_context_generation(key, root:
  TrustRoot, replace, addressee) -> GenerationConsent`; `repo_context_generate_key` use;
  `drop_project_skill_grants` (`permissions.rs:2610`) is asserted to drop the new key through
  the shared predicate; denial note.
- `crates/teton/src/session_ui.rs` — `SessionGrants::forget_root_scoped_grants` (`:107`) is
  asserted to forget the new key through the same predicate; no new code expected.
- `crates/tetond/tests/skill_consent_matrix.rs` — the generation subject joins the matrix.

## Acceptance Criteria

- [ ] BR-2: at `guarded` and `edits` a prompt is published with the subject naming root, path
      and `replace`; `plan` returns `Denied` with no prompt (and no event — the short-circuit is
      the caller's, but the gate's own `plan` arm is asserted too); `full` returns `Allowed` with
      no prompt and `pending_count() == 0` (LESSON-524's test shape, with the timeout guard).
- [ ] `AllowAlways` under `repo_context:generate:<root A>` does not answer root B; a `/cd`
      drops the key in the daemon **and** the CLI memo (drive both, ASSUME-017).
- [ ] A `Refused(NoTerminal)` answer yields `RefusedUnattended` and draws nothing further.
- [ ] `replace: true` renders a different sentence than `replace: false` (the human sees which
      question is on screen).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/tests/skill_consent_matrix.rs::the_generation_offer_asks_at_guarded_and_edits_denies_at_plan_and_allows_at_full_without_a_prompt` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/permissions.rs::a_generation_grant_is_keyed_by_root_and_expires_in_both_stores_on_cd` | yes |
| AC-2 | test-case | `crates/tetond/tests/skill_consent_matrix.rs::the_generation_offer_asks_at_guarded_and_edits_denies_at_plan_and_allows_at_full_without_a_prompt` | yes |

## Technical Notes

Mint the key from the same durable root REQ-591's `TrustRoot::durable` provides; a root that will
not canonicalize mints no key and the offer is `Suppressed` with a reason (fail closed, REQ-591
BR-6). Drive the derivation end to end in the test — never hand the minter its key (LESSON-552).
