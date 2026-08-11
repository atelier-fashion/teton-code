---
id: ASSUME-006
title: "A declared ModelProvider.model and a real Option<default_provider> are a usable foundation for REQ-558 and REQ-559"
status: validated
req: REQ-557
created: 2026-08-05
resolved: 2026-08-11
---

## Assumption

REQ-557's Assumptions section stated that REQ-558 (purpose-routing categories)
and REQ-559 (global reasoning effort) were sequenced after it and "may assume a
declared `ModelProvider.model` and a real `Option<default_provider>`."

This was an assumption about *sufficiency*, not just ordering: that replacing the
price-table-derived model string with a declared field, and the positional
default with an explicit `Option`, would give downstream routing work everything
it needed — no second lookup, no supplementary identity, no reintroduced
fallback.

## Context

REQ-557 justified its own existence largely by what it unblocked. Its Description
calls itself "the **blocking prerequisite** for REQ-558 and REQ-559: a category
cannot bind to a model that cannot be named, and an effort level cannot be
clamped against a model whose identity is inferred from a price table."

If the foundation had been insufficient, the cost would have landed on the
consumers — REQ-558 would have needed a supplementary identity mechanism, and
REQ-557's narrow scope (field + CLI + default + migration) would have been the
wrong cut.

## Resolution

**Validated by REQ-558 shipping on it.** REQ-558 (`status: complete`) bound
purpose categories to providers on top of the declared model with no
supplementary identity mechanism and no reintroduced price-table lookup —
`build_router` reads `config.default_provider` directly (`runtime.rs:5782`) with
no positional `.find` and no literal-`"local"` tail, which is the shape REQ-557
ADR-D specified.

Partially outstanding for REQ-559: that REQ is still `draft`, so the clamping
half of the assumption ("an effort level clamped against a declared model") has a
consumer on paper but not yet in code. REQ-559's External Dependencies still name
REQ-557 as a hard prerequisite, and that prerequisite is now met — the remaining
risk is in REQ-559's own design, not in this foundation.

Recorded because REQ-557's narrow scope was a deliberate bet, and the bet paid.
A future REQ tempted to widen scope "so the next one has what it needs" can cite
this: shipping the minimum declared fact was enough.
