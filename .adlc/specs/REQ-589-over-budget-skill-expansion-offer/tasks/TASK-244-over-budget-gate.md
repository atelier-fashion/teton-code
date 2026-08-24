---
id: TASK-244
title: "authorize_skill_over_budget on the permission gate"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-241]
---

## Description

BR-10 + BR-4. A new gate method beside `authorize_skill` (1365), asked under a `skill:` key so it cannot be smuggled through another key family (ADR-7 of REQ-587). Silence is never consent.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs` — `authorize_skill_over_budget`; option set built conditionally like `options_for` (2055)

## Acceptance Criteria

- [x] The question is asked under a `skill:` key; `is_skill_permission_key` (2019) rejects anything else — a hard return, not a debug_assert
- [x] No-route, connection-refused, and disconnected-channel paths all resolve to `Unanswerable`, never to proceed (BR-4, AC-4)
- [x] Nothing is persisted: no grant survives the invocation (BR-10, AC-10)
- [x] Remedy-bearing options appear only when BR-7 grants that bound a remedy
- [x] A seam-level unit test asserts the non-persistence guard's removal reddens (LESSON-508)

## Technical Notes

Reuse `settle`/`Addressed` and the existing `SkillConsent` arms (ASSUME-B). The addressee is the connection that submitted the turn (REQ-587 ADR-3).

## Implementation notes (2026-08-24)

**Signature.**

```rust
pub async fn authorize_skill_over_budget(
    &self,
    key: &str,
    subject: PermissionSubject,          // must be ::SkillOverBudget
    labels: OverBudgetOptionLabels,      // composed by TASK-243, never here
    addressee: ConnectionId,
) -> OverBudgetAnswer                    // { consent: SkillConsent, apply_remedy: bool }
```

`OverBudgetAnswer`'s fields are private with no public constructor, so
`apply_remedy()` cannot be `true` unless a human picked a remedy id that was
actually offered — BR-4's "silence is never consent" extended to the durable
write, made structural rather than checked.

**ASSUME-B holds.** No `SkillConsent` arm was added. `remedy_only` narrows to
`Declined` (a human decided this turn does not run) and the remedy rides beside
the consent, not inside it.

**Non-persistence is two guards, not one.** `Question::consults_grants()` is
`false` for the offer — the offer asks under the *same* `skill:<source>:<name>`
key `authorize_skill` remembers a dynamic-context answer under, so without it one
"allow for this session" on `/deploy`'s commands would auto-send every later
oversized `/deploy` expansion with no prompt on any screen. And
`interpret_over_budget` is a **free function** with no `&self`, so no answer can
be recorded at any scope.

**Six mutations verified to redden exactly the intended test and nothing else:**

| mutation | reddens |
|---|---|
| `consults_grants()` → always `true` | `a_remembered_skill_grant_cannot_settle_an_over_budget_offer` |
| `self.remember(...)` on the accept arm | `no_over_budget_answer_is_remembered_and_accepting_twice_asks_twice` (+ the above) |
| `remedy_may_be_offered_on(RedactScan)` → `true` | `remedy_options_appear_only_where_the_bound_has_a_remedy`, `a_redact_scan_offer_cannot_authorize_a_write` |
| drop the `is_skill_permission_key` hard return | `the_over_budget_door_refuses_a_key_that_is_not_the_skills_own` |
| drop the subject-variant guard | `the_over_budget_door_refuses_a_subject_that_is_not_the_offer` |
| `LevelAllow::DoesNotSettle` → `Settles` | `the_level_still_denies_but_an_allow_row_no_longer_settles` |

**Two decisions taken here that the task file did not specify** — flagged for
verify:

1. `LevelAllow::DoesNotSettle`, so a `full` session still asks. `deny` still
   denies, so `plan` still refuses; the knob's whole range is "ask anyway".
2. BR-3's "leads with the remedy" is implemented as **option order** when
   `window_verdict == ExceedsWindow`. Both choices stay on the prompt.

**Gap reported:** nothing publishes `skill_over_budget_offered` /
`_accepted` / `_remedy_applied` (added by TASK-241). No task's ACs claim them.
The gate does not publish them — it has the subject but not the wiring task's
context. TASK-247 is the natural owner.
