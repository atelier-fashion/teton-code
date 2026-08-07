---
id: TASK-056
title: "teton policy set-tier / set-category, and a show that admits what is unreached"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-06
dependencies: [TASK-052, TASK-055]
---

## Description

The user-facing surface: bind a tier or a category, and read back the effective
mapping — including which categories have no call site yet (ADR-A, ADR-H, OQ-4).

## Files to Create/Modify

- `crates/teton/src/main.rs` — `PolicyAction::SetTier`, `PolicyAction::SetCategory`;
  `CliCategory` / `CliTier` value enums; `run_policy_show` rendering
- `crates/teton-protocol/src/methods.rs` — replace `ConfigUpdate::SetRoutingRule`
  and the `RoutingRule` type (it carries a `phase` field, which AC-9 reaches) with
  the tier/category equivalents
- `crates/tetond/src/runtime.rs` — the handlers

## Acceptance Criteria

- [x] `teton policy set-tier think anthropic` and
      `teton policy set-category review sonnet` both apply and persist.
- [x] `teton policy show` renders, per category: the category, its tier, the
      effective provider, whether that came from an override or from tier
      inheritance, and — for the six with no call site — a
      `declared, no call site yet` marker (ADR-A).
- [x] **A test asserts the unreached set matches reality** rather than a
      hand-maintained list: enumerate the categories with call sites and assert the
      marker agrees. Adding a call site later must fail this test until the marker
      is updated.
- [x] `teton policy set-category redact <provider>` is rejected, naming the pin
      (BR-4). The CLI enum should not offer `redact` as a value at all.
- [x] The BR-9 judgment default is visible in `policy show` (AC-12).
- [x] Setting a tier or category to an unregistered or unusable provider is
      rejected before anything is written, naming the provider (REQ-557 BR-6,
      BUG-155 M4's shape).
- [x] Arg-parsing tests cover both new subcommands.
- [x] `RoutingRule` and `ConfigUpdate::SetRoutingRule` no longer exist — the
      protocol carries no phase-keyed routing type (AC-9).

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

## Implementation Notes

**The unreached set is five, not six.** The AC and the architecture's table both
say six, listing `route` among them — but that table was written before TASK-053
built the classifier, and `route` now has a call site
(`DaemonRuntime::classify_freeform`). The derived test says five: `redact`,
`title`, `compact`, `triage`, `shell`. This is exactly the rot ADR-A predicted,
caught on the first run of the mechanism designed to catch it, so the count was
corrected rather than the marker.

**`Router::table_report` exists because the scan found a hole in the first
draft.** `policy show` resolves all eleven categories, so a naive loop over
`Router::resolution_for` in the snapshot builder would have read as eleven call
sites and marked everything reached. The reporting surface is now one named
method, excluded by name in `call_sites`' scan, with the exclusion stated rather
than silent.

**`CategoryResolution` gained a public `source` field.** `policy show` has to say
whether a provider came from an override or from tier inheritance, and the only
alternatives were recomputing it (a second resolver) or pattern-matching the
reason sentence (a second resolver in disguise).
