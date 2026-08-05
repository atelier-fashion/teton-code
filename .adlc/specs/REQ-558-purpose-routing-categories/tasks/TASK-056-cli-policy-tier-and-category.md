---
id: TASK-056
title: "teton policy set-tier / set-category, and a show that admits what is unreached"
status: draft
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-052, TASK-055]
---

## Description

The user-facing surface: bind a tier or a category, and read back the effective
mapping — including which categories have no call site yet (ADR-A, ADR-H, OQ-4).

## Files to Create/Modify

- `crates/teton/src/main.rs` — `PolicyAction::SetTier`, `PolicyAction::SetCategory`;
  `CliCategory` / `CliTier` value enums; `run_policy_show` rendering
- `crates/teton-protocol/src/methods.rs` — the `ConfigUpdate` variants
- `crates/tetond/src/runtime.rs` — the handlers

## Acceptance Criteria

- [ ] `teton policy set-tier think anthropic` and
      `teton policy set-category review sonnet` both apply and persist.
- [ ] `teton policy show` renders, per category: the category, its tier, the
      effective provider, whether that came from an override or from tier
      inheritance, and — for the six with no call site — a
      `declared, no call site yet` marker (ADR-A).
- [ ] **A test asserts the unreached set matches reality** rather than a
      hand-maintained list: enumerate the categories with call sites and assert the
      marker agrees. Adding a call site later must fail this test until the marker
      is updated.
- [ ] `teton policy set-category redact <provider>` is rejected, naming the pin
      (BR-4). The CLI enum should not offer `redact` as a value at all.
- [ ] The BR-9 judgment default is visible in `policy show` (AC-12).
- [ ] Setting a tier or category to an unregistered or unusable provider is
      rejected before anything is written, naming the provider (REQ-557 BR-6,
      BUG-155 M4's shape).
- [ ] Arg-parsing tests cover both new subcommands.

## Technical Notes

**The unreached marker is only honest if a test derives it.** A hardcoded list of
"these six are unreached" is stale the moment someone wires a call site, and it
goes stale silently — which is the failure mode ADR-A exists to prevent. Derive the
reached set from the code (a const the call sites register into, or an exhaustive
match that must be updated) and assert the rendering matches.

**`policy show` must read from the same resolver as `route_decided`** (BR-6, AC-11).
This is the surface most likely to drift, because it is the one a human reads and
therefore the one most tempting to "just format nicely" with its own logic.

**BUG-155 M4 is the precedent for rejecting before writing.** `provider add` now
refuses a duplicate id before reading a credential; `policy set-*` should likewise
refuse an unusable provider before persisting anything.
