---
id: TASK-124
title: "Regression coverage for taint reached via a non-canonical spelling"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-9. This adds no pinning behavior — BUG-156 already delivered the
failover/retry pin and is resolved. It proves the existing pin is now actually
*reached* when a session is tainted through a spelling that previously matched
nothing, and that the second-hop web-lookup channel closes with it.

## Files to Create/Modify

- `crates/tetond/tests/provenance_egress.rs` — add the AC-8 cases.

## Acceptance Criteria

- [x] AC-8: a session tainted via a non-canonical spelling (absolute-inside-root, and separately `..`-traversing) reaches the existing local-tier pin.
- [x] AC-8: a subsequent model-composed `web_fetch` in that session is refused.
- [x] A control asserts a session tainted via the canonical spelling behaves identically — the two paths must not diverge.
- [x] The test names BUG-156 and states that it verifies existing behavior is reached, not new behavior added, so a future reader does not mistake it for duplicate coverage.
- [x] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

`context_is_sensitive` (`crates/tetond/src/runtime.rs:6112-6124`) calls the same
`inspect` + `BoundaryMatcher` path, so it inherits the TASK-119 fix with no
production change here. The web gate is at
`crates/tetond/src/egress/lookup.rs:882-893`, keyed on
`Authorship::ModelComposed` + `taint.is_tainted`.

If this task requires a production change, that is a signal TASK-119 is
incomplete — investigate there rather than patching the taint path.

## Implementation Notes (as landed)

**No production change was needed.** The three new tests passed on their first
run against TASK-119's tree, which is the outcome the task predicted: the pin
and the web gate were already correct and the only thing missing was a session
that could reach them.

- **The pin is asserted through `CarriedTurn`, not around it.**
  `runtime::context_is_sensitive` is `pub(crate)`, so an integration test cannot
  call it — and neither can anything but the commit seam. `carry::CarriedTurn`
  is public and is the *only* path to it, in production and in the test alike,
  so the fixture seeds a real `SessionRegistry` session, runs the scripted turn
  over `turn.ctx_mut()`, and commits. The `SessionTaint` handed to that turn is
  minted per probe and given to nothing else, and the fixture asserts the
  session is un-pinned immediately before `commit()` — so an observed pin can
  only have come from the seam under test.
- **Committing after a privacy-blocked turn is the production path, not a
  liberty.** `run_prompt_turn` does not abandon a privacy-blocked turn: it
  marks, reroutes to the local tier, and re-runs, and *that* attempt commits.
  The fixture has no local engine to re-run against, so it commits the manager
  the blocked attempt left — the same blocks through the same seam.
- **One comparable value, three spellings.** Each probe returns a
  `SessionProbe` (provenance ids, whether the turn blocked, the `privacy_block`
  count, whether the commit pinned, and the outcome + packet count of a
  model-composed and a user-pasted `web_fetch` afterwards). AC-8's two
  non-canonical spellings are asserted equal to a literal expectation, and the
  control asserts canonical == absolute == `..`-traversing, so the requirement's
  "must not diverge" is an equality rather than three prose claims.
- **The falsification leg rides in the same tests.** Each AC-8 test also probes
  a session that read a *non-boundary* file: nothing blocks, nothing pins, and
  both authorships put a packet on the wire — with the public file's bytes
  asserted present in a captured provider body (LESSON-479). Each test mints its
  own control rather than sharing one (LESSON-502).
- **`drive_scripted_turn` was split out of `run_touching_tool`.** The three
  REQ-544 tests now run through the same function on a context they own, so the
  new cases extend the existing harness instead of forking it.
- **The web view is re-composed, not stubbed.** `runtime::SessionTaintView` has
  private fields, so the test implements `TaintView` over the same two public
  types (`SessionTaint` + `WebTaintOverride`) the daemon composes it from.
  Neither flag can be written through that handle.
