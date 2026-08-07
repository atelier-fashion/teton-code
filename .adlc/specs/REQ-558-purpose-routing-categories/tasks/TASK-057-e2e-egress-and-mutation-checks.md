---
id: TASK-057
title: "End-to-end, egress-capture, and the mutation checks"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-06
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

- [x] **AC-6, egress-capture**: a session tainted by boundary content stays on the
      local tier for every subsequent turn with `think` bound to a remote provider
      and a `design`-classified prompt. Zero remote payloads contain boundary
      content, asserted by capture (BR-7, REQ-544 AC-5 posture).
- [x] **The boundary test is provably non-vacuous.** Assert the turn genuinely
      *would* have gone remote — that the pre-taint turn produced a
      `route_decided` naming the remote provider — so a future change that quietly
      routes it local cannot leave this test green and meaningless.
- [x] **AC-8**: `route_decided` carries a category, tier, provider, and non-empty
      reason across a scripted session covering at least one harness-known
      (`digest`) and one intent-classified (`design`) category.
- [x] **AC-11 / BR-6**: `route_decided`, `policy show`, and the turn-failure
      sentence agree byte-for-byte on provider, category, tier, and reason for one
      deliberately-unset binding. A second call site computing its own answer makes
      this red.
- [x] **Mutation A (AC-10)**: reintroducing a keyword match for any harness-known
      category makes at least one test red.
- [x] **Mutation B (AC-10)**: removing the taint override (BR-7) makes at least one
      test red.
- [x] **Mutation C**: un-screening the category resolver's provider usability
      (ADR-E) makes at least one test red.
- [x] Each mutation is run by hand, confirmed red, and reverted — and any mutation
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

## Verification record

Written at completion, because a mutation check is only worth what its record
says. Every mutation below was applied by hand to the working tree, run against
`cargo test --workspace --no-fail-fast`, and reverted.

| # | Mutation | Result | Caught by |
|---|---|---|---|
| A | A keyword match in `dispatch_route` assigning `Category::Digest` from prompt text (the `AUXILIARY_SIGNALS` shape) | **red** | `routing_categories::a_freeform_turn_reads_the_category_override_not_a_keyword_list` (e2e), `runtime::dispatch::a_freeform_design_prompt_reaches_the_think_binding_not_the_local_tier`, `an_unavailable_local_tier_bypasses_classification_with_no_call`, 2 CLI e2e |
| B | The BR-7 taint override deleted from `dispatch_route` (turn path only) | **red** | 7 tests, incl. `routing_categories::a_tainted_session_stays_local_…` and `…does_not_fail_over_…`, `model_identity`, all three `privacy_fixes` |
| B2 | The BR-7 taint override deleted from `digest_route` (duty path only) | **red** | `runtime::dispatch::digest::a_tainted_session_digests_on_the_local_tier` — **and nothing else**, which is the isolation result: B and B2 have disjoint coverage, so neither guard is masking the other |
| C1 | `Router::is_usable` → `\|_\| true` (ADR-E screen removed) | **red** | `router::an_unregistered_provider_is_never_selected_by_the_category_chain` |
| C2 | `Router::is_usable` → `is_routable` alone (local-tier arm removed) | **red** | 8 tests across `ac_matrix`, `consent_matrix`, `router`, `runtime::dispatch` |
| D | `Router::fallback_for` re-reads the configured table for a route with no resolution (main's BUG-156 logic) | **red** | `routing_categories::a_tainted_session_does_not_fail_over_to_the_tiers_remote_fallback` — **and, at first, only that.** See the finding below |
| E | `classify::plan`'s bypass removed (classify regardless of the `route` resolution) | **red** | `classify::a_bypassed_turn_issues_no_call_at_all`, `routing::an_unavailable_local_tier_bypasses_to_the_declared_default`, 2 `runtime::dispatch` |
| F | `category::resolve`'s by-construction pin removed for `route` (reads its tier binding) | **red** | `category::route_never_resolves_to_a_remote_provider` + 4 — disjoint from E, so the bypass and the pin each have coverage of their own |
| G | The turn-failure sentence composes its own answer instead of carrying the resolver's | **red** | `routing_categories::one_resolver_answers_policy_show_and_the_turn_failure_alike` |
| H | `policy show`'s projection composes its own reason instead of carrying the resolver's | **red** | `routing_categories::one_resolver_answers_policy_show_and_the_turn_failure_alike` |

No mutation came back green.

### Two findings the run produced

**AC-11 was genuinely violated, and the test found it.** The turn-failure
sentence for an unresolvable category was computed entirely by
`unserved_turn_error` — the resolver's own sentence, which names the category,
the binding it read and the `teton policy set-*` remedy, was discarded. The
`digest` duty path already carried it verbatim; the *turn* path, the one a user
reads, did not. Fixed by `runtime.rs::unserved_turn_sentence`, which prefixes
the resolution's sentence when (and only when) the resolver is what declined.
Mutation G is the revert of that fix.

**The router-level BUG-156 test did not pin BUG-156.**
`a_tainted_session_cannot_fail_over_to_a_remote_provider` stayed green under
mutation D: its fixture bound no tier to `local`, and the defect only fires when
the failed provider is some row's primary. Its fixture was rebuilt in the bug's
own shape with a non-vacuity leg; both layers now fail on D alone. Recorded in
BUG-156.
