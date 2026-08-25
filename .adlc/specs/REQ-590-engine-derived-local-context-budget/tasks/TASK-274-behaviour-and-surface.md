---
id: TASK-274
title: "The byte-band regression and the bound's account of itself"
status: draft
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
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
