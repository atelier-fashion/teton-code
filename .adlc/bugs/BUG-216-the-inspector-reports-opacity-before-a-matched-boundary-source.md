---
id: BUG-216
title: "The egress inspector reports the unknown-provenance sentinel before a matched boundary source, so a session that read a protected file is recorded with the liftable cause"
status: open
severity: low
created: 2026-09-05
updated: 2026-09-05
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "developer-experience"]
tags: ["inspector", "unknown-provenance", "boundary-hit", "taint", "cause", "req-614", "req-619"]
introduced_by: ["REQ-544"]
attribution: manual
---

## Description

`egress::inspector::inspect` checks `provenance.is_unknown()` **before** it
walks `provenance.sources()` against the boundary globs. A block that is both
opaque and names a boundary file — a `read` of `secrets/prod.env` plus any
opaque `shell` result in the same context, or, since REQ-619, one skill
expansion whose preambles are `sh probe.sh` and `cat secrets/prod.env` — is
refused against `<unknown-provenance>`. `taint::cause_of` reads the block's
**path** to choose the pin's cause, so the session is recorded as
`unknown_shell`: liftable, with the client printing the `/shell allow` remedy
for a session that did ingest protected bytes.

Found during REQ-619's verify pass (its e2e mutation record documents the
shape; the correctness reviewer confirmed the easier two-preamble route).

## Reproduction Steps

1. Builtin boundaries in force, `build` routed to a remote provider.
2. A user skill with the preambles `!\`sh probe.sh\`` and
   `!\`cat secrets/prod.env\``; type `/name`.
3. Read `privacy_block.path` and `session_pinned.cause`.

## Expected Behavior

The block names `secrets/prod.env` and the cause is `boundary_hit`: a proved
boundary read is strictly more specific than "we could not prove anything",
exactly as ADR-614-3 already puts `boundary_touch` ahead of the unknown arm
("the more specific reading is the true one").

## Actual Behavior

The block names `<unknown-provenance>` and the cause is `unknown_shell`. The
user is invited to type `/shell allow`; the lift releases only the opacity, the
next send is refused naming the file and escalates to `boundary_hit`
(pinned by `shell_pin_shape::a_boundary_read_after_a_lift_escalates_the_pin_and_nothing_later_leaves`).
**No content leaves** — the defect is the cause, the remedy sentence and one
wasted turn.

## Environment

- Platform: any; daemon 0.1.31 and the REQ-619 branch.

## Root Cause

Arm order in `inspect` (`crates/tetond/src/egress/inspector.rs`): malformed →
boundary_touch → unknown → sources. The unknown arm predates the idea that a
block could be both opaque and source-bearing; REQ-614 gave the sentinel a
liftable meaning without moving it behind the source loop.

## Resolution

(open) Run the source loop before the unknown arm. This changes the reported
path for pre-existing `read` + opaque-`shell` sessions, which is why REQ-619
did not fold it in. REQ-619 pins the current behaviour with
`e2e::skill_provenance::a_preamble_that_is_both_opaque_and_boundary_reading_pins_and_survives_the_lift`;
when the order changes, legs (a) and (c) of that test flip to naming the file
at the first block and the test becomes the record of the change.

## Files Changed

- `crates/tetond/src/egress/inspector.rs` — arm order; tests
