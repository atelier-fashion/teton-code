---
id: TASK-215
title: "A third gate door for the project-skill acknowledgment, and a grant key that follows its arguments"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-210]
---

## Description

ADR-7. BR-4's acknowledgment cannot ride `authorize_skill`, and BR-5's
digest-bearing grant key breaks one of that function's own guards.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — `authorize_project_skill_trust`, the digest-keyed grant, `READ_ONLY_TOOLS`
- `crates/tetond/tests/skill_consent_matrix.rs` — the new door's matrix

## Acceptance Criteria

- [ ] `authorize_project_skill_trust(key, root, addressee) -> …` is a **third entry point**, not a widened `authorize_skill`. That function `debug_assert!`s the key is a skill key **and** equals `skill_permission_key_for(source, name)`; an acknowledgment key is neither. Widening those asserts would loosen a guard that is pinned in both directions.
- [ ] BR-5's digest: whenever any command in the body interpolates `$ARGUMENTS`/`$N`, the remembered grant's key carries a digest of the **substituted** command set — for **both** callers, so a user-typed and a model-issued invocation of the same skill with different arguments do not share an answer. The minting function and `authorize_skill`'s second `debug_assert!` move **in lockstep**, or every debug build fires.
- [ ] `authorize`'s narrow web guard is untouched: it fires for web keys and deliberately admits skill keys, asserted both directions. Do not widen it.
- [ ] `READ_ONLY_TOOLS` gains `skill` (BR-11: no level ever raises an "allow `skill`?" prompt). The level table stays unenumerated — an unknown key still falls to the level default, and a **skill-name** row is never added.
- [ ] The acknowledgment is session-scoped (OQ-3) and expires on `/cd` on **both** stores, using TASK-210's shared predicate. The daemon-side drop and the client's `SessionGrants` drop are the same moment.
- [ ] The matrix asserts what must *not* happen: a `shell` grant does not answer an acknowledgment; an acknowledgment does not answer dynamic context; a grant for root A does not answer root B.
- [ ] Mutation: reusing `authorize_skill` for the acknowledgment, dropping the digest, and skipping the `/cd` expiry each fail a named test.

## Technical Notes

- `skill_consent_matrix.rs` **invents** a `ConnectionId` and hands it to the gate, so it passes whether or not production has one to pass. That is fine for *this* task — the gate is what is under test — but it is why TASK-217 must assert the addressee separately, from inside the loop.
