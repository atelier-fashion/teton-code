---
id: TASK-057
title: "End-to-end, egress-capture, and the mutation checks"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-053, TASK-054, TASK-056]
---

## Description

The verification tier. Three claims are only demonstrable end to end — the
privacy override, the classifier bypass, and the one-resolver rule — and two are
claims tests must make rather than prose.

## Files to Create/Modify

- `crates/tetond/tests/e2e/routing_categories.rs` — **new**: AC-1, AC-5, AC-6,
  AC-8, AC-11 end to end
- `crates/tetond/tests/e2e.rs` — register the module
- `crates/tetond/tests/e2e/harness.rs` — a fixture helper for tier/category config
- `docs/manual-verification.md` — anything that could not be automated, at the
  strength it was actually verified

## Acceptance Criteria

- [ ] **AC-6, egress-capture**: a session tainted by boundary content stays on the
      local tier for every subsequent turn with `think` bound to a remote provider
      and a `design`-classified prompt. Zero remote payloads contain boundary
      content, asserted by capture (BR-7, REQ-544 AC-5 posture).
- [ ] **The boundary test is provably non-vacuous.** Assert the turn genuinely
      *would* have gone remote — that the pre-taint turn produced a
      `route_decided` naming the remote provider — so a future change that quietly
      routes it local cannot leave this test green and meaningless.
- [ ] **AC-8**: `route_decided` carries a category, tier, provider, and non-empty
      reason across a scripted session covering at least one harness-known
      (`digest`) and one intent-classified (`design`) category.
- [ ] **AC-11 / BR-6**: `route_decided`, `policy show`, and the turn-failure
      sentence agree byte-for-byte on provider, category, tier, and reason for one
      deliberately-unset binding. A second call site computing its own answer makes
      this red.
- [ ] **Mutation A (AC-10)**: reintroducing a keyword match for any harness-known
      category makes at least one test red.
- [ ] **Mutation B (AC-10)**: removing the taint override (BR-7) makes at least one
      test red.
- [ ] **Mutation C**: un-screening the category resolver's provider usability
      (ADR-E) makes at least one test red.
- [ ] Each mutation is run by hand, confirmed red, and reverted — and any mutation
      that comes back **green** is reported as a finding, not silently fixed.

## Technical Notes

**Run every mutation and report the green ones.** BUG-155 found two mutations that
left the whole suite green, and the value was entirely in noticing. A mutation
returning green is a finding about the tests — record it, then close it with a test
that fails on the mutation alone.

**Guard specifically against mutually-masking guards.** BUG-155 also found two
guards that each caught the other's mutation, so neither had independent coverage.
Where this REQ has layered protection — taint override *and* category screening
both prevent a tainted session going remote — mutate each **in isolation** and
confirm a test fails for that one alone. If not, the inner guard needs its own
test at its own layer.

**The vacuity assertion is not optional.** BUG-155's near-miss was a boundary test
whose fixture prompt contained an `AUXILIARY_SIGNALS` word, so the turn never went
remote and "nothing leaked" was trivially true. Deleting the keyword list removes
that specific trap, but the general one — a boundary test where the turn never
approaches the boundary — survives any routing change. Assert the route, not just
the absence of bytes.

**Record honestly what could not be automated.** REQ-557's wrapup deferred a leg
to `docs/manual-verification.md` and initially overstated why; the corrected entry
is the model — name what *is* covered, name the actual gap, and say plainly if it
was not run.
