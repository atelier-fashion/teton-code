---
id: TASK-264
title: "Cherry-pick the five trust commits onto origin/main"
status: complete
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-263]
---

## Description

ADR-3. Land REQ-591's code on this worktree's branch, cut from `origin/main`. The five are at positions 4, 5, 8, 11 and 22 of the 33 — interleaved, so they are picked individually in chronological order, each building on the last.

## Files to Create/Modify

- the worktree at `/Users/brettluelling/Documents/GitHub/teton-code/.worktrees/REQ-591`

## Acceptance Criteria

- [ ] Cherry-picked in this order: `b4e4b01`, `b071da5`, `4be0c34`, `37a2e6c`, `bda079d`
- [ ] Conflicts resolved toward the TRUST side — an offer hunk arriving in a conflict is a signal the pick is too wide; narrow it rather than accepting it
- [ ] `cargo build --workspace` clean and `cargo test --workspace --no-fail-fast` green
- [ ] The three mutation-verified properties are RE-VERIFIED here, not assumed to survive the move: the AC-9 ordering test (skip the trust block → it must fail), and the TOCTOU attack reproduction
- [ ] **Spec AC-1, AC-2, AC-3, AC-4, AC-5, AC-6, AC-7, AC-8, AC-9 are satisfied by tests that
  TRAVEL with these commits** — `a_root_re_pointed_after_discovery_cannot_spend_the_listed_trees_trust`,
  `an_unattended_session_at_a_root_nobody_listed_still_refuses`,
  `a_project_skills_trust_question_is_put_before_its_budget_question`,
  `the_typed_door_says_the_user_asked_not_the_model` and their siblings. Verify each is PRESENT
  and PASSING here, by name. A green suite is not the same claim as "these specific tests moved"
- [ ] No offer symbol is present: grep for `offer_or_refuse_over_budget`, `OverBudgetOffer`, `window_verdict`, `PressurePolicy` returns nothing outside pre-existing main code

## Technical Notes

Order matters: `b4e4b01` introduces the gate, `b071da5` adds `invoked_by` to it, `4be0c34` adds the allowlist, `37a2e6c` tests the ordering, `bda079d` fixes the TOCTOU. Picking out of order will conflict.

## Outcome

Four of the five picked; the branch is green at 3,697 tests, clippy and fmt clean.

**`37a2e6c` was SKIPPED — it cannot land here.** Both tests it adds live in
`crates/tetond/tests/skill_over_budget_offer.rs`, a file created by an offer commit
(`53f1c71`), and both assert facts about the **budget** question:
`a_project_skills_trust_question_is_put_before_its_budget_question` asserts the raw prompt
log reads `["project trust", "over-budget offer"]`, and
`a_user_authored_skill_is_asked_only_the_budget_question` asserts the log reads
`["over-budget offer"]`. Neither claim exists on a branch with no budget question; ported
here they would be vacuous, which is the failure mode the AC was written against.

The ordering rule is **not** unwitnessed here. `b4e4b01` carried
`declining_the_repository_refuses_the_turn_and_asks_no_budget_question` into `runtime.rs`,
which asserts the same ordering from the engine's prompt list (empty ⇒ the classifier and
therefore Stage A were never reached), and it reddens under the AC-9 mutation. What is
missing on this branch is the *raw permission-log* leg. REQ-591's own AC-1 asks for that
leg — "the ordering is asserted from the raw prompt log, not a filtered view" — and it must
be authored here against the two gates this branch actually has (trust, then
`authorize_skill`). It has no task. **TASK-268 should take it.**

**Two offer-introduced seams had to be carried, narrowed, because `4be0c34` compiles
against them** (ADR-4 checked for trust code in offer commits, not for offer code the
trust commits depend on):

- `Question` (from `e8b1bfb`) — `4be0c34` constructs `Question::ProjectTrust` and reads
  `Question::durable_project_root`. Carried with **two** variants, `Standard` and
  `ProjectTrust`; `OverBudget`, `consults_grants` and `remedy_offered` left behind.
  `settle`/`interpret`/`decide`/`options_for` take the enum in place of `Option<WebTier>`.
- the addressed-route test double and `wired` (from `e8b1bfb`), which `4be0c34`'s eight
  D-13 tests are written through, plus the `grant_keys` test accessor (from `a23c9f2`).
  Renamed `OverBudgetRoute` → `AddressedRoute`; its two unused variants dropped.

Both are shapes `e8b1bfb` will re-introduce on REQ-589's rebuilt branch, so the eventual
merge of the two REQs will see them defined twice. That is a merge-time reconciliation, not
a split defect — but it is not what ADR-3 predicted, and **AC-10 should check it.**

REQ-589's own task docs (`TASK-248`, `TASK-261`, `TASK-262`) arrived in the picks as
modify/delete conflicts and were dropped: three of REQ-589's twenty-four task files under
a `.adlc/specs/REQ-589-*/tasks/` tree this branch does not otherwise have would be worse
than none.
