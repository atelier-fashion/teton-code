---
id: TASK-051
title: "Retire Phase from routing signatures and remove Phase::Freeform"
status: draft
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
- [ ] `Phase` still appears in `CostRecord` and `LedgerRow`, and a cost-attribution
      test proves per-phase rollups still work for a structured session (BR-11).
- [ ] `phase_from_wire("freeform")` returns `None` via an **explicit arm above the
      catch-all**, with a comment naming it as the retired variant, and a test
      asserts it (ADR-G).
- [ ] `Phase::ALL.len() == 5` and its test is updated rather than deleted.
- [ ] The structured machine's initial state is a deliberate choice, documented in
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
