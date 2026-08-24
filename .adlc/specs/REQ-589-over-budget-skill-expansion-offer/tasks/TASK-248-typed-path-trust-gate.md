---
id: TASK-248
title: "Introduce the project-skill trust gate on the typed path"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-244]
---

## Description

BR-6 / ADR-10 / D-10. **New functionality, accepted scope increase.** There is no trust gate on the user-typed `/name` path today — `accept_invocation` (runtime.rs:2904) is synchronous and gates nothing, so a typed project skill runs unacknowledged. Introduce it, before Stage A.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `accept_invocation` (2904) becomes `async`; gate call inserted before Stage A (3601); every caller updated
- `crates/tetond/src/harness/permissions.rs` — reuse `authorize_project_skill_trust` (1490) unchanged

## Acceptance Criteria

- [x] A typed project-sourced skill raises the trust question BEFORE the budget question (AC-9)
- [x] A user-authored skill raises no trust question — the current order stands
- [x] Declining trust yields the trust refusal, not a budget sentence, and no budget offer is made
- [x] The signature change is followed to every caller (compile-time forcing function); no caller silently bypasses the gate
- [x] The model-invoked path's existing acknowledgment is unchanged — a paired test pins it

## Technical Notes

The gate is reused verbatim; `authorize_project_skill_trust` asserts the key is the one this root mints, so do not mint a new key family. This closes a real pre-existing gap uncovered while architecting REQ-589.

## Implementation notes

The gate is called **inside** `accept_invocation`, after the registry resolves
the name and before `skills::expand` — which is before the route, before the
naming duty and therefore before Stage A, not merely before Stage A. That is
what makes `async` a forcing function: a caller that skips the acknowledgment
cannot compile.

Every closed door — `Declined`, `DeniedByLevel`, `Unanswerable`,
`UnrecognizedSubject` — refuses the turn with `CONSENT_DENIED` and a sentence
naming the door (`project_trust_refusal`, runtime.rs). `closed_door` is the
shared reader, so the typed door and the model door cannot come to disagree
about what a decline is.

**Behaviour change worth the verify phase's attention.** The acknowledgment key
is unenumerated, so it takes the level's default, and `plan`'s default is deny.
A typed **project** skill at `plan` is therefore now refused before it expands,
where it used to run with `not run at plan` placeholders in its command slots.
That is REQ-585 BR-4's posture arriving at the door that did not have it; it is
pinned by `at_plan_a_typed_project_skill_is_refused_before_it_expands`
(skill_turn.rs), and REQ-585 AC-9's own `plan` leg now uses a user-authored
skill so it still asserts what it was written to assert.

Tests: `runtime.rs::tests::a_typed_project_skill_is_acknowledged_first` (four
assertions, including the paired typed-door/model-door one). Two existing
suites needed their fixtures taught about the new question — `skill_turn.rs`
(its stand-in client acknowledges the repository and records it separately) and
`provenance_egress.rs` (its skill turn had no addressable connection at all).
