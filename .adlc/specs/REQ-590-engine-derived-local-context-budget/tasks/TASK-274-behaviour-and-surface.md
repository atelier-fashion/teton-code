---
id: TASK-274
title: "The byte-band regression and the bound's account of itself"
status: complete
parent: REQ-590
created: 2026-08-25
updated: 2026-08-26
dependencies: [TASK-270, TASK-271]
---

## Description

Two user-visible consequences of the new pair.

**BR-7 / AC-7 — the byte half fell**, 32,768 → 30,720. Byte-dense local content in that
2,048-byte band is newly over budget. D-4 accepts this: the outcome is an over-budget *offer*
(REQ-589 shipped that surface immediately before this REQ), not a hard refusal. This task pins
the chosen behaviour.

**BR-12 / AC-16 — the bound accounts for itself.** `LocalEngine`'s meaning narrows from "a fixed
pair" to "derived from the engine's window". A user whose budget changed must be able to see
where the number came from, and must not be told to set a `capabilities.max_context` that does
not exist for this route.

## Files to Create/Modify

- `crates/tetond/tests/context_pressure.rs` — the byte-band turn
- `crates/tetond/src/harness/budget.rs` — the `LocalEngine` bound's rendered clause
- wherever bound strings are asserted today

## Acceptance Criteria

- [ ] AC-7: a local turn of byte-dense content sized inside 30,721–32,768 raises **exactly one**
      over-budget offer and no silent elision. Paired with a just-under turn on the same fixture
      that serves untouched — a test pinning only the improving direction is what let this
      regression go unnoticed in the spec's first draft
- [ ] AC-16: the rendered `LocalEngine` clause names the window and the reservation that produced
      the pair, asserted on the string a user sees rather than the fields behind it
- [ ] AC-6: that clause offers **no** `capabilities.max_context` remedy. Paired against a remote
      `Window` bound, which does. Mutation: give the local bound the remote clause; this must
      redden

## Technical Notes

Seam for the byte band: `filler(words)` at `context_pressure.rs:81` builds content at exactly
4 B/word, so 7,681–8,192 words lands in the band. Verify the actual bytes rather than trusting
the multiplier — the point of this test is an exact byte count.

The offer surface is REQ-589's. Read `skill_over_budget_offer.rs` for how an offer is asserted
before writing a new harness; do not build a parallel one.

`bound_clause` is at `budget.rs:1210-1221` and renders the `max_context` remedy for
`provider_id: None`. That is the sentence AC-6 forbids on this route.


## Outcome (2026-08-26)

**AC-7 is void and was not implemented.** D-4 was reversed mid-implementation (ADR-9, TASK-276):
the byte half stays 32,768, so the 30,721–32,768 band this task was written around does not
exist, and no byte-dense local content is newly over budget. Nothing was written against it, and
nothing asserts it. What carries the AC's actual intent — *do not pin only the improving
direction* — is named in the rewritten AC-7 in `requirement.md`: AC-12's one-byte-apart legs, and
AC-11's 8 and 20 B/word rows, which now assert `after == before` where under D-4 they asserted
`after <= before`.

**AC-16 and AC-6 are implemented, in `bound_clause` (`harness/budget.rs`).** The `LocalEngine`
arm renders:

```
bound: local engine — the word half comes from the engine's 16,384-token window,
less the 1,024 reserved for the reply; the byte half is fixed
```

Both figures are interpolated from `LOCAL_ENGINE_N_CTX` and `LOCAL_GENERATION_RESERVATION`, never
restated. The byte half is named as *not* derived, which is the honest half of the sentence after
ADR-9 — a clause implying both halves came from the window would send a reader to divide 33 KB by
something and get an answer that does not reconcile.

Asserted on the string in
`budget::tests::the_local_bound_accounts_for_the_window_and_the_reservation_it_derived_from`,
including that the numbers it names really produce the pair beside them.

**AC-6's pairing, and where it differs from the criterion as written.** AC-6 pairs the local bound
against "a remote `Window` bound, which does" offer a `capabilities.max_context` remedy. Two
surfaces carry that remedy, and they are pinned separately:

- the **bound clause**, where only `DefaultUnknown` renders one — which is also the arm the local
  route falls into if `derive`'s local branch is deleted (BR-2), so it is the pairing that
  matters here;
- the **remedy clause**, where a `Window`-bound route really is told to raise
  `capabilities.max_context` — already pinned by
  `skill_over_budget_offer.rs::every_bound_offers_exactly_the_remedy_the_table_names`.

**Mutation, run and confirmed:** folding `LocalEngine` into `bound_clause`'s `DefaultUnknown`
condition reddens both `the_local_bound_accounts_for_the_window_and_the_reservation_it_derived_from`
and `the_default_configs_budget_is_still_bound_by_the_local_engine`. Reverted by re-editing the
line.
