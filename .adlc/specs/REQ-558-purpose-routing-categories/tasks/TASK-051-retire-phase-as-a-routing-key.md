---
id: TASK-051
title: "Retire Phase from routing signatures and remove Phase::Freeform"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-048]
---

## Description

`Phase` stops being a routing input and keeps its cost-attribution and ADLC-gating
roles (BR-11, AC-9). `Phase::Freeform` is removed — the freeform/structured
distinction already lives in `Session.mode`.

## Files to Create/Modify

- `crates/teton-core/src/phase.rs` — remove `Freeform`; `ALL` becomes `[Phase; 5]`
- `crates/teton-protocol/src/lib.rs` — same, on the wire enum
- `crates/teton/src/main.rs` — `CliPhase::Freeform` removed
- `crates/teton/src/session_ui.rs` — the freeform display arm
- `crates/tetond/src/structured/machine.rs` — a new initial state (see notes)
- `crates/tetond/src/cost/ledger.rs` — explicit `"freeform" => None` arm

## Acceptance Criteria

- [ ] `Phase` appears in **no** routing signature — not `resolve_*`, not the
      configured table, not `route_decided`'s dispatch input (AC-9). Enforced by
      compilation plus a test that the router's public API mentions no `Phase`.
- [x] `Phase` still appears in `CostRecord` and `LedgerRow`, and a cost-attribution
      test proves per-phase rollups still work for a structured session (BR-11).
- [x] `phase_from_wire("freeform")` returns `None` via an **explicit arm above the
      catch-all**, with a comment naming it as the retired variant, and a test
      asserts it (ADR-G).
- [x] `Phase::ALL.len() == 5` and its test is updated rather than deleted.
- [x] The structured machine's initial state is a deliberate choice, documented in
      the code, not the first variant that compiles.

## Technical Notes

**The ledger reattribution is a decision, not an accident.** Validation W2 flagged
that retiring the variant silently moves historical rows. Exploration narrowed it:
`resolve_freeform` already sets `phase: None`, so freeform turns have **always**
recorded a NULL phase. The only rows carrying the literal string come from a
structured session explicitly created at `Phase::Freeform`. Accepting the
reattribution is right — but via an explicit arm and a test, so a reader can tell a
human decided it. A value that falls through a catch-all looks like an oversight
six months later.

**`structured/machine.rs:113` initializes at `Phase::Freeform`.** This is the one
place where this task changes behavior rather than types. Decide what a structured
session's initial phase should be and say why in a comment; do not pick whatever
compiles.

## Implementation Notes

**The first AC is left unticked on purpose — it is not wholly this task's to
close.** `Router::resolve_structured(&self, phase: CorePhase)` and the
`RoutingPolicy` table it reads are removed by TASK-050 and TASK-055
respectively; TASK-051 owns the enum and every non-router call site. The half of
AC-9 that *is* closed here: `Phase` no longer reaches routing through
`category_for_phase`'s freeform arm, and `route_decided`'s dispatch input
(`Route::resolution`) never mentioned it. The "router's public API mentions no
`Phase`" test belongs with TASK-050, which is the change that makes it pass.

**The freeform-routing rejection moved outward rather than disappearing.**
`ConfigError::FreeformRoutingPolicy` and its `Config::validate` check became
unreachable the moment `Phase` lost the variant — serde rejects
`phase = "freeform"` at deserialization, before validation runs — so the check,
the error variant, and the test that constructed `RoutingPolicy { phase:
Phase::Freeform }` are deleted. The behaviour is pinned by
`a_freeform_routing_entry_is_still_rejected_after_the_schema_change`, which drives
the config through `Config::load` as text and now asserts `LoadError::Parse` with
a message naming the offending value.

**The freeform machine holds `None`, not a nominated phase.**
`PhaseMachine.phase` became `Option<Phase>`: `Some(Phase::Spec)` for a structured
session (unchanged), `None` for a freeform one. Naming a substitute lifecycle
phase would have made `PhaseMachine` the only type in the workspace claiming a
freeform session sits somewhere in the ADLC flow — `Session.phase`,
`Route.phase`, `RouteDecided.phase`, and the ledger's `phase` column are all
`Option` and have always held `None` for freeform work — and it would silently
bill freeform turns to a lifecycle bucket the moment the machine gains a
cost-attributing caller. `next_phase()` now derives from `self.phase` alone so
`Mode` cannot disagree with it. Rationale is recorded on
`PhaseMachine::freeform`.
