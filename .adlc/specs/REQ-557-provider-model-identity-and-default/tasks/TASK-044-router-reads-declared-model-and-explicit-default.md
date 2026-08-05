---
id: TASK-044
title: "Router reads the declared model and an explicit default; delete billing_model and both fallbacks"
status: draft
parent: REQ-557
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-043]
---

## Description

Make the router consume the declared `model` and the explicit
`default_provider`, and delete the two derivation paths that made this REQ
necessary: `billing_model()`'s provider-id fallback and `build_router`'s
positional default selection with its literal-`"local"` tail.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `build_router` (:2669) reads `p.model`
  instead of `billing_model(prices, &p.id)`; `default_provider` is read from
  `config.default_provider` with **no** positional `.find` and **no**
  `local_provider` / `"local"` tail; **delete** `billing_model` (:2772) and its
  doc comment; wire the migration's legacy resolver closure at the config load
  site; add the missing-default branch to `unserved_turn_error`
- `crates/tetond/src/router.rs` — `Router`'s default provider becomes
  `Option<ProviderId>`; `model_of` returns the provider's declared model;
  `resolve_freeform` / `resolve_structured` handle an absent default by
  returning the typed no-default outcome rather than a synthesized id

## Acceptance Criteria

- [ ] `route_decided` for a turn carries the provider's **declared** `model`,
      asserted against a provider whose `id` appears **nowhere** in the price
      table. This test fails against the pre-REQ binary (AC-3).
- [ ] `Router`'s default provider is an `Option` at the type level — a unit test
      asserts `None` when no `default_provider` is configured, and the code
      contains no string literal `"local"` as a provider-id fallback (AC-4).
- [ ] With no `default_provider` and no matching policy, a turn fails with a
      message naming the missing default and the `teton provider` remedy, routed
      through `unserved_turn_error`'s existing precedence — **not** a new
      classifier (BR-5).
- [ ] A provider marked **unusable** by TASK-043's usability pass (remote kind,
      `model: None` after migration) is not routable: a turn that would resolve
      to it fails naming that provider and the `--model` remedy, while other
      providers keep working. This is the router half of ADR-E — the daemon
      started, so the refusal has to happen here.
- [ ] `billing_model` no longer exists anywhere in the workspace (grep-level
      assertion in the test or a compile failure if referenced).
- [ ] Existing router policy tests pass unmodified where they do not construct
      providers; those that do are updated to declare a `model`.
- [ ] Table-driven tests cover (default configured / absent) ×
      (policy match / no match).

## Technical Notes

**Both halves of the fallback go.** `build_router` currently does:

```rust
let local_provider = …find(Local).map_or_else(|| "local".to_owned(), |p| p.id.clone());
let default_provider = …find(is_remote).map_or_else(|| local_provider.clone(), |p| p.id.clone());
```

Removing only the outer `.find` leaves the literal `"local"` reachable through
`local_provider`. BUG-146's root cause #1 is this doubled chain — see ADR-D.
`local_provider` is still needed for local-tier routing, but it must carry
`Option` rather than minting a plausible id.

**Reuse the classifier, do not add one.** `unserved_turn_error` already
distinguishes six unserved-turn states with a precedence shared by the lifecycle
stream (BUG-152's fix). The missing-default case is a seventh branch **in that
function**, so the turn-failure sentence and the lifecycle replay cannot drift.
LESSON-456 is the governing rule.

**Error code.** The missing-default condition is settled, not transient — it
needs a user action. It therefore keeps `UNKNOWN_PROVIDER`, **not**
`TIER_WARMING` (which BUG-152 reserved for the one state that resolves itself).

**The legacy resolver closure** from TASK-043 is constructed here, at the config
load site, from the price table. Keep it private to the load path — it must not
become a general-purpose model lookup.
