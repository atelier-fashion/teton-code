---
id: TASK-392
title: "Taint gains a cause, the lift gains a type, and the route predicate is composed once"
status: draft
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

`SessionTaint` becomes cause-carrying, a `ShellTaintOverride` sibling holds
the per-session lift, and the seven route sites that force a local tier stop
reading the raw taint bit and read a composed predicate instead — so the lift
is honored by construction rather than by seven remembered conjunctions
(ADR-614-4).

Also rewords the pin reason so a liftable pin names its remedy (BR-4).

## Files to Create/Modify

- `crates/tetond/src/runtime/taint.rs` — `TaintCause`, cause-carrying `SessionTaint`, `ShellTaintOverride`, `RoutePin`
- `crates/tetond/src/runtime/duty.rs` — six route sites read `RoutePin`
- `crates/tetond/src/runtime/turn.rs` — `dispatch_route` reads `RoutePin`
- `crates/tetond/src/runtime/mod.rs` — `taint_pin_reason` becomes cause-aware; runtime holds the override
- `crates/tetond/src/carry.rs` — `context_is_sensitive` marks with a cause

## Acceptance Criteria

- [ ] `TaintCause` is `BoundaryHit` / `UnknownShell` / `MalformedProvenance` / `McpUntrusted`, with `liftable()` returning `true` for `UnknownShell` alone
- [ ] `SessionTaint` records the **first** cause that pinned a session; a later cause does not overwrite it
- [ ] `ShellTaintOverride::lift` is **not** `pub` — reachable from the module and its children only, exactly as `WebTaintOverride::lift` is, so a model-issued lift does not compile (the AC-12-of-REQ-563 property, preserved)
- [ ] `RoutePin::pins(session)` returns `false` when the recorded cause is `UnknownShell` and the session is lifted; `true` otherwise
- [ ] All seven sites — `turn.rs` `dispatch_route` and the six `*_route` fns in `duty.rs` — call `RoutePin::pins`; a source region check enumerates them and fails if a site reverts to the raw bit
- [ ] A `boundary_hit` pin is **never** lifted, whatever the override set contains (BR-3)
- [ ] `taint_pin_reason` names the cause and, when liftable, the remedy: the reason for an `unknown_shell` pin contains ``/shell allow`` (BR-4)
- [ ] AC-8: an `unknown` block carried into a later turn of a **lifted** session is still refused at egress with `privacy_block.path == "<unknown-provenance>"`, the turn is rerouted local, and the session's recorded cause and lift are both unchanged — no re-pin

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/tetond/src/runtime/taint.rs::a_boundary_hit_pin_is_never_liftable` | yes |
| BR-4 | test-case | `crates/tetond/src/runtime/taint.rs::an_unknown_shell_pin_names_its_remedy` | yes |
| BR-6 | test-case | `crates/tetond/tests/provenance_egress.rs::a_lift_does_not_untaint_the_blocks_that_caused_it` | yes |
| AC-8 | test-case | `crates/tetond/tests/provenance_egress.rs::a_carried_unknown_block_in_a_lifted_session_is_still_refused` | yes |

## Technical Notes

- The seven-site sweep is the point of the task. A **region** check, not a
  count: relocating a call keeps a count identical (LESSON-568, REQ-592).
- Preserve the design argument in taint.rs's docs for why the override is a
  separate type — folding it into `SessionTaint` would let anything that can
  mark taint unmark its consequence. `RoutePin` is a read-only composer over
  two `Arc`s, the shape `SessionTaintView` already uses.
- `try_mark`'s poison-tolerant path must keep its fail-closed direction when
  it gains a cause: a poisoned lock still inserts.
- Mutation to record: make `RoutePin::pins` ignore the lift and count what
  fails; write the number in the doc comment.
