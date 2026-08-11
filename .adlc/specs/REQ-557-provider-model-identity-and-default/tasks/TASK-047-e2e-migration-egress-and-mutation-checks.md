---
id: TASK-047
title: "End-to-end migration, egress-capture, and mutation checks"
status: complete
parent: REQ-557
created: 2026-08-05
updated: 2026-08-11
dependencies: [TASK-044, TASK-045, TASK-046]
---

## Description

The cross-cutting verification tier. Three claims in REQ-557 are only
demonstrable end-to-end and one (BR-8) is explicitly a claim that tests must
make rather than prose: the two-leg migration, the unchanged egress posture, and
the mutation checks proving both deleted fallbacks are actually gone.

## Files to Create/Modify

- `crates/tetond/tests/e2e/` — migration e2e covering both legs (resolvable and
  unresolvable) and the second-start idempotence check
- `crates/tetond/tests/` — egress-capture leg asserting boundary enforcement is
  unchanged by this REQ
- `crates/teton/tests/cli_e2e.rs` — the `provider add --model` legs and the
  missing-`--model` refusal
- `docs/manual-verification.md` — record anything that could not be automated,
  at the strength it was actually verified

## Acceptance Criteria

- [x] **Migration, both legs in one test** (AC-6): a config written in the
      pre-REQ shape with two providers — one resolvable through the legacy price
      lookup, one not — loads with the resolvable provider migrated to a declared
      model, the unresolvable one reported by id and unusable, and a second start
      re-running nothing.
- [x] **Egress-capture** (BR-8): a session with a `local-only` boundary produces
      zero remote calls containing boundary content, and a tainted session stays
      pinned to the local tier — with a `default_provider` configured and a
      declared model on every provider. Verified by mock-transport capture, not
      by code inspection, per conventions.md.
- [x] **Mutation check A** (AC-8): restoring `billing_model`'s provider-id
      fallback makes at least one test red.
- [x] **Mutation check B** (AC-8): restoring the positional default-provider
      `.find` — **or** its `local_provider` / literal-`"local"` tail — makes at
      least one test red. Both halves must be pinned separately; a single test
      covering only the outer `.find` leaves the literal reachable.
- [x] **The daemon starts on a pre-REQ config** (ADR-E): a config in the old
      shape — every provider `model: None` — loads and the daemon starts, so
      migration can run at all. Pinned separately from the migration test,
      because this is the precondition migration depends on.
- [x] **The daemon starts with one unresolvable provider** (ADR-E, BR-7): a
      config where migration could resolve one provider and not the other starts,
      reports the unresolvable one by id, serves turns on the usable provider, and
      refuses turns routed to the unusable one. A daemon that refuses to start
      here is the regression this criterion exists to catch.
- [x] **Mutation check C**: moving the model requirement into `Config::validate()`
      — the obvious-but-wrong design ADR-E rejects — makes at least one of the two
      startup tests above red.
- [x] Both existing e2e suites (`cli_e2e`, `tetond/tests/e2e`) pass with only
      the provider-construction updates the new required field forces — no test
      is edited to accommodate a behavior change.

## Technical Notes

**The mutation checks are the point of this task, not a formality.** This REQ is
mostly *deletions* of derivation paths. A deletion is verified only by proving
that restoring it breaks something — LESSON-441 ("a fix pass is new code —
re-verify it adversarially, not by test count"). Run each mutation by hand,
confirm red, revert.

**Mutation check B has two halves for a reason.** `build_router`'s fallback is a
chain: `default_provider` falls back to `local_provider`, which falls back to the
literal `"local"`. A test that only pins the outer `.find` stays green while the
inner literal is reachable — the one-directional-guard shape of BUG-151 and
LESSON-479.

**Egress-capture is not optional and not inspectable.** conventions.md: "Privacy
boundary (BR-1) claims require egress-capture integration tests (mock transport
asserting no boundary content in any remote payload) — code inspection is not
acceptance." REQ-557 BR-8 claims this REQ leaves egress unchanged; that claim
needs the same evidence standard as the original guarantee. LESSON-432 is why:
the coverage gap and the security gap are the same gap.

**Record honestly what could not be automated.** REQ-556's wrapup deferred two
ACs to `docs/manual-verification.md` rather than closing them by assertion.
Follow that precedent — a criterion with no harness is aspirational and should
say so.
