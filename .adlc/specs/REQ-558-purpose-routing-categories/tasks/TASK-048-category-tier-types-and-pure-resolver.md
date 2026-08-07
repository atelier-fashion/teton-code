---
id: TASK-048
title: "Category, Tier, and the pure resolution function in teton-core"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: []
---

## Description

The foundation every other task builds on: the category/tier vocabulary and the
single pure function that answers "where does this category go" (BR-12, ADR-D).

Mirrors `teton_core::policy::evaluate`'s existing shape — pure, health injected as
a closure, returns a reason naming the signal that fired — so this inherits a
proven pattern rather than inventing one.

## Files to Create/Modify

- `crates/teton-core/src/category.rs` — **new**: `Category` (11),
  `ConfigurableCategory` (10, no `redact`), `Tier` (4), `JudgmentCategory` (4),
  `category_for_phase`, and `resolve()`
- `crates/teton-core/src/lib.rs` — export the module
- `crates/teton-core/src/policy.rs` — no signature change; `RouteOutcome` is
  re-used by `resolve()` rather than duplicated

## Acceptance Criteria

- [ ] `resolve()` is pure — no I/O, no clock, health injected as `Fn(&str) ->
      ProviderHealth`, exactly as `policy::evaluate` does it.
- [ ] Table-driven test over **all eleven** categories × (per-category override /
      tier inheritance / unresolvable), asserting provider, tier, outcome, and a
      non-empty reason for each (AC-2, BR-12).
- [ ] No path produces a synthesized provider id. Removing a tier binding makes
      the corresponding category **name itself** in the failure reason — asserted
      per category, not once (BR-8).
- [ ] `resolve(Category::Redact, …)` returns the local tier even when every tier
      is bound to a remote provider, via a match arm that consults no config
      (AC-4, ADR-B).
- [ ] `ConfigurableCategory` has no `Redact` variant, and
      `From<ConfigurableCategory> for Category` is total.
- [ ] `JudgmentCategory` has exactly four variants and a total conversion into
      `Category`. There is **no** conversion from prompt text or from `&str` into
      `Category` (AC-3 — this is the type-level guarantee, see notes).
- [ ] `Category::origin()` is a `const fn` returning `harness_known` /
      `intent_classified`, and a test asserts the seven/four split matches the
      requirement's table exactly.
- [ ] `category_for_phase` is total over the (post-REQ, five-variant) `Phase`.

## Technical Notes

**`redact`'s pin is a type, not a check** (ADR-B / LESSON-443). Do not write
`if binding.is_none() { local }`. `redact` is absent from `ConfigurableCategory`,
so a binding for it cannot be represented, so `resolve` has no condition to get
wrong. LESSON-443's rule is that a guard whose condition is "the feature does not
exist yet" is a time bomb with your own roadmap as the fuse.

**AC-3 is discharged by the classifier's return type, not by a grep.** The
classifier (TASK-053) returns `JudgmentCategory`. Because there is no path from
text to `Category`, assigning `digest` from prompt text does not compile. Do not
add a `FromStr for Category` "for convenience" — it is exactly the hole this
closes.

**Re-use `RouteOutcome`.** Two vocabularies for one concept is how surfaces drift
(LESSON-456). If a variant genuinely does not fit, extend the shared enum rather
than forking it.

**`resolve()` must screen provider usability** (ADR-E) the same way REQ-557's
router does — an unroutable provider is not a valid resolution. BUG-155's Critical
finding was three config-reading paths that each bypassed that screen; a new
dispatch axis is a fourth unless it is screened by construction.
