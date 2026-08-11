---
id: TASK-114
title: "Resolve effort once at route time; report the clamped level on route_decided and feed both TurnRequest sites"
status: pending
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-111, TASK-112]
---

## Description

ADR-G's single-resolution wiring. The daemon calls `resolve_effort` **once** per
call, at the point the route is decided, and the resulting `ResolvedEffort` flows
to three consumers that must not be able to disagree: the `route_decided` event
(AC-4), the `TurnRequest` handed to the adapter (BR-1), and — via TASK-116 — the
`teton effort` / `/effort` surfaces (AC-8).

This is also where BR-6 and BR-7 become true without any per-category
configuration: the local tier's per-kind default shape is `none`, so a
local-routed call resolves to `Omit(ShapeNone)` no matter how high the global
setting is.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — add `effort: Option<ResolvedEffort>` to
  `RouteDecided` (:308)
- `crates/tetond/src/runtime.rs` — resolve at route time; store on the remote
  source / duty construction; read `Config.effort` and the session override
- `crates/tetond/src/harness/completion.rs` (:484) — pass the stored value
- `crates/tetond/src/harness/duty.rs` (:832) — pass the stored value
- `crates/tetond/tests/routing.rs` — AC-4, AC-5, AC-6

## Acceptance Criteria

- [ ] `RouteDecided.effort: Option<ResolvedEffort>` with
      `#[serde(skip_serializing_if = "Option::is_none", default)]`. `Option` is
      for **wire additivity only** — the daemon always populates it. A frame from
      a daemon predating the field reads `None`, and a client predating it ignores
      a key serde does not require it to know, so this moves neither
      `PROTOCOL_VERSION` nor `PROTOCOL_VERSION_MIN` (same posture as
      `PrivacyBlock::cause`, REQ-562 ADR-7). Asserted against literal JSON in
      both directions.
- [ ] Effort is resolved **exactly once** per call. A test asserts that the value
      in `RouteDecided.effort` and the value in the captured `TurnRequest` are the
      same value — not merely equal-looking. Two computations of one fact is the
      LESSON-456 drift this ADR exists to prevent.
- [ ] **AC-4**: with the session at `xhigh` and a provider whose ladder is
      `{low, high, max}`, `route_decided` reports `Effort(High)` — the **clamped**
      level, not the requested one.
- [ ] **AC-5**: a call routed to the local tier produces `Omit(ShapeNone)`, and
      the captured request carries no effort field and no thinking flag.
- [ ] **AC-6**: with the session at `max`, a `route`-category call still resolves
      to the local tier and carries no effort field. The cap comes from the local
      provider's per-kind empty ladder / `none` shape (BR-7) — assert that **no
      per-category effort configuration exists** by grepping the config surface
      for any category-keyed effort key and finding none (BR-2).
- [ ] Resolution order is `session_override.or(config.effort)` with
      `EffortLevel::default()` (`High`) as the floor — never an absent value
      (BR-1). A test with no `effort` key in config and no session override
      asserts the request carries `high`.
- [ ] **ADR-I**: the effective effort is snapshotted at turn start. A test changes
      the effort mid-turn and asserts the in-flight turn's request and its
      `CostRecord` reflect the level in force when the turn began, and the next
      turn reflects the new one.
- [ ] **AC-11 (egress)**: raising effort to `max` on a session with a `local-only`
      boundary produces zero remote calls containing boundary content. Effort
      changes nothing about egress (BR-13) — the existing egress-capture harness
      is reused, not reimplemented.

## Technical Notes

**Where "route time" is.** The provider and its `ModelProvider` (hence `kind` and
`capabilities`) are both in scope where the route is decided in `runtime.rs`;
that is the only place all four `resolve_effort` inputs exist together. Resolving
later — e.g. inside `completion.rs` from `self.provider.capabilities()` — would
put the value out of reach of the event, which is exactly the split AC-4 is
guarding against.

**Store the resolved value, not the inputs.** Put `ResolvedEffort` on the remote
source / duty struct (it is `Copy`). Storing `EffortLevel` + capabilities and
re-resolving at the `TurnRequest` site would be a second call to the resolver,
which is a second opportunity to drift.

**The local tier does not go through a `Provider` adapter at all.** `LocalEngineSource`
serves local turns directly. Its `route_decided` must still report
`Omit(ShapeNone)` — BR-6 requires the no-op be *declared*, not merely true by
omission. Do not leave `effort: None` on a local route; `None` means "a daemon
that predates this field", which is a different claim.

**BR-13 is a non-change, and must be asserted as one.** Do not add any
effort-conditional branch to the egress path. AC-11 passes trivially if nothing
was touched — which is the point; the test is a regression guard for a future
change, not a check on this one.
