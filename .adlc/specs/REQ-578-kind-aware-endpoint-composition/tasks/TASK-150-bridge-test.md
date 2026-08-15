---
id: TASK-150
title: "Catalog↔composition bridge test (AC-7 mutation vehicle)"
status: complete
parent: REQ-578
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-148"]
repo: teton-code
---

## Description

Pin the composition module against the REQ-577 recipe catalog without
touching any AC-6-protected file (ADR-2): idempotence over every recipe
endpoint, base→full agreement, and the Anthropic default identity.

## Files to Create/Modify

- `crates/tetond/tests/endpoint_composition_bridge.rs` — new test file: for
  every `recipe_catalog()` entry assert (a)
  `compose_endpoint(kind, Some(endpoint))` returns it unchanged
  (`changed: false`); (b) composing the endpoint's origin — and its
  bare-`/v1` form where the canonical path starts with `/v1` — yields
  exactly the recipe endpoint; (c) `ANTHROPIC_DEFAULT_ENDPOINT` equals the
  Anthropic recipe's endpoint. Non-vacuity floor: assert ≥ 6 recipes swept.

## Acceptance Criteria

- [ ] All three legs green against the real catalog; failure messages state
  which spelling moved and that BOTH the module and the catalog (or seam
  test) must be reconciled, never one deleted.
- [ ] AC-7 mutation demonstrated: with `compose_endpoint` stubbed to
  identity, leg (b) fails on the missing canonical path — record the
  demonstration in the commit body, revert byte-identically.
- [ ] AC-6 audit: `git diff` shows zero changes to
  `crates/tetond/src/provider_recipes.rs`,
  `crates/teton-providers/tests/conformance.rs`, and
  `crates/tetond/tests/web_setup_contracts.rs`.
- [ ] `cargo test -p tetond --test endpoint_composition_bridge` green;
  clippy + fmt clean.

## Technical Notes

- Derive origins from the recipe endpoints themselves (strip after the
  authority) rather than hand-writing base URLs — the test then keeps
  working when a recipe's host changes, and only genuine rule drift fails.
