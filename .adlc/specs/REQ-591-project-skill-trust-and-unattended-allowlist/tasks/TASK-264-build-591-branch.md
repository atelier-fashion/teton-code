---
id: TASK-264
title: "Cherry-pick the five trust commits onto origin/main"
status: draft
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
