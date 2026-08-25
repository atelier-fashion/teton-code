---
id: TASK-272
title: "The 4,097-word refusal becomes a 4,097-word success"
status: draft
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-270]
---

## Description

ADR-7. `turn_loop.rs:3365-3367` asserts that a 4,097-word local turn is **refused** against a
4,096-word budget. That is the exact field report that motivated REQ-589 and this REQ, currently
pinned as passing behaviour.

AC-12 requires that turn to serve. So this test's premise is deleted, not renumbered — and a
refusal case at the **new** boundary is added, so the refusal path keeps a witness.

Then sweep every other test pinning the old pair.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — invert the `4_097` case (line ~3365); update the
  `5_000 / 4_096` case (line ~2770) to the new boundary
- `crates/tetond/src/router.rs` — `:2566`, `:2575` assert `(4_096, 32_768)`; `:2678-2679` pin the
  whole `RouteBudget` Debug rendering including `budget_tokens: 4096, bound: LocalEngine`
- any further sites the sweep finds

## Acceptance Criteria

- [ ] AC-12: 4,097 words on the local tier is **not** refused and raises **no** over-budget offer.
      Asserted on both halves — a turn that serves but silently offers would pass a weaker test
- [ ] A refusal case exists at the new boundary (just over 10,240 words), so
      `ContextRefusalOrigin::LocalEngine` keeps a witness. Paired with a just-under case on the
      same fixture
- [ ] `router.rs:2678-2679`'s Debug pin updated — including `digest_threshold_*`, which move to
      3,750 / 11,250 as a consequence of the budget moving (D-3, accepted)
- [ ] AC-13: full suite green, `cargo audit` clean, no new clippy warnings, `cargo fmt --check`
      clean. **This task owns AC-13** because it is the one that lands the test sweep — the last
      point at which "green" means anything about the change as a whole
- [ ] A grep for the literals `4_096` / `4096` / `32_768` / `32768` in tests returns only sites
      that are genuinely about the **constants** (which do not move), not about the local route

## Technical Notes

**Do not mechanically renumber.** An exploration pass listed line 3367 as a `4,096 → 10,240`
substitution; that keeps the refusal and leaves AC-12 unwitnessed while the suite goes green.
Read each site and ask which fact it is pinning: the *constants* (unchanged) or the *local
route's derived pair* (changed).

`assert_eq!(LOCAL_BUDGET_BYTES, 32_768)` at `budget.rs:3202` stays true and must not be touched.

The digest thresholds moving is expected and accepted (D-3). Note the asymmetry so it is not
mistaken for a bug: words go **up** 1,500 → 3,750 while bytes go **down** 12,000 → 11,250,
because `digest_thresholds` scales each half by its own constant and the byte half of the budget
fell.
