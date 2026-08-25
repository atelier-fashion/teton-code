---
id: TASK-260
title: "Name the provider in the rebind remedy"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-250]
---

## Description

**Created mid-Phase-4 (ADR-18 item 2).** BR-9 requires the `BindTierRemote` offer to name
the tier, the provider, and the cost consequence. It cannot: `Remedy::BindTierRemote { tier }`
carries no provider, so the sentence and the option label both say "a remote provider".

That is the vagueness ADR-1 exists to forbid — its rule is that a label names the concrete
write, never a generic gesture, on the `enable_permanent` precedent. And at one configured
provider the daemon *does* know the name, because it binds by name.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — a provider slot on `Remedy::BindTierRemote`; composer + label
- `crates/tetond/src/runtime.rs` — pass the chosen provider into the remedy

## Acceptance Criteria

- [x] With exactly one configured remote provider, the offer sentence AND the option label
      name it (e.g. "bind `think` to `kimi` and declare `capabilities.max_context = 1000000`")
- [x] The provider named is the one that will actually be bound — a test asserts the name in
      the sentence equals the id in the applied write
- [x] With 0 or 2+ providers the option stays withheld (TASK-250's behaviour is unchanged)
- [x] The remedy is still never addressed to the provider the route is LEAVING — that is why
      `for_bound` dropped it, and the fix must not reintroduce that confusion
- [x] The cost consequence still appears (BR-9); mutating it away reddens

## Technical Notes

Keep `Remedy::for_bound`'s existing guarantee intact — the slot must be filled by the
*target* provider, chosen by the planner, not inherited from the route's current one.

## Outcome

`Remedy::BindTierRemote` gained a `target: Option<RebindTarget>` slot. `Remedy::for_bound`
still leaves it `None` — the only provider id a bound-keyed classifier holds names the route
being *left* — and the one door into it is `OverBudgetOffer::name_rebind_target`, called from
`offer_or_refuse_over_budget` with the target `plan_tier_rebind` chose. The plan's
`provider_id`, both writes and the offer's wording now come off one constructed value, so
ADR-1's "the label names the concrete write" is a property of the construction rather than of
two lookups agreeing.

`RebindTarget` carries the window as a `RebindWindow` — `Catalogued(ProposedWindow)` or
`Declared(u32)` — so the figure is reachable only through the variant that says where it was
read. That is what let the sentence and both labels carry ADR-7's date, which the rebind clause
could not do before (TASK-254's blocked assertion).

**Before:** `bind the \`build\` tier to a remote provider and declare that provider's
\`capabilities.max_context\` in the same change`

**After (one configured remote):** `bind the \`build\` tier to \`frontier\` and declare its
\`capabilities.max_context = 1000000\` in the same change (Moonshot (Kimi)'s own published
window, read 2026-08-19)`

At ADR-12's zero and two-or-more counts the wording is unchanged and the remedy options stay
withheld.

**Two expected-string repairs in files this task did not own**, both forced by the wording
change and both a single constant:

- `crates/tetond/tests/skill_over_budget_offer.rs` — the `LocalEngine` row of
  `every_bound_offers_exactly_the_remedy_the_table_names`.
- `crates/teton/tests/pty_e2e.rs` — `REMEDY_WRITE`. This one now copies a vendor window and its
  `verified_on` date into a `crates/teton` test, which `recipe_window_one_home.rs` (a `-p tetond`
  sweep over `src/`) cannot see. The CLI crate is the thin client and cannot read the recipe
  catalog, so a literal was the only option there. **Flagged for Phase 5.**
