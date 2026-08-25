---
id: TASK-248
title: "Introduce the project-skill trust gate on the typed path"
status: complete
parent: REQ-591
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-244]
---

## Provenance

**Moved from REQ-589 during the REQ-591 carve-out (2026-08-25).** Its commit `b4e4b01` left with
the trust work; this file survived the drop only because a KEPT commit (`65a66a3`) created it and
the dropped one merely edited it. Left behind, it would have described a gate that is no longer
on REQ-589's branch.

## Description

BR-6 / ADR-10 / D-10. **New functionality, accepted scope increase.** There is no trust gate on the user-typed `/name` path today — `accept_invocation` (runtime.rs:2904) is synchronous and gates nothing, so a typed project skill runs unacknowledged. Introduce it, before Stage A.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `accept_invocation` (2904) becomes `async`; gate call inserted before Stage A (3601); every caller updated
- `crates/tetond/src/harness/permissions.rs` — reuse `authorize_project_skill_trust` (1490) unchanged

## Acceptance Criteria

- [ ] A typed project-sourced skill raises the trust question BEFORE the budget question (AC-9)
- [ ] A user-authored skill raises no trust question — the current order stands
- [ ] Declining trust yields the trust refusal, not a budget sentence, and no budget offer is made
- [ ] The signature change is followed to every caller (compile-time forcing function); no caller silently bypasses the gate
- [ ] The model-invoked path's existing acknowledgment is unchanged — a paired test pins it

## Technical Notes

The gate is reused verbatim; `authorize_project_skill_trust` asserts the key is the one this root mints, so do not mint a new key family. This closes a real pre-existing gap uncovered while architecting REQ-589.
