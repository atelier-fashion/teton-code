---
id: REQ-603
title: "Extract the session-lifecycle slice REQ-599 planned and never shipped"
status: draft
deployable: false
created: 2026-08-31
updated: 2026-09-01
component: "daemon/runtime"
domain: "refactoring"
stack: ["rust", "daemon"]
concerns: ["maintainability", "developer-experience"]
tags: ["decomposition", "god-module", "req-599-followup", "deferred-slice"]
---

## Description

REQ-599's ADR-4 planned seven extraction steps. Step 7 was **session lifecycle
-> `runtime/session.rs`, ~900 lines**. Seven commits landed; none of them was
that one. The slot was taken by provider transport, and nothing recorded the
substitution — the plan table still named `session.rs` as shipped work while no
such module existed.

REQ-602 TASK-306 reconciled that table and filed this REQ so the deferral has a
tracked home rather than living in a paragraph of a closed spec.

The reason it did not ship is not that it stopped being worth doing. The seven
steps were taken cheapest-seam-first from the impl structure (ADR-2), session
lifecycle was the most entangled of the candidates, and the REQ ran out of steps
before it ran out of seams.

**The baseline, measured at `4a2238b`.** `crates/tetond/src/runtime/mod.rs` is
**7,420 production lines** — not the 10,306 REQ-599 closed on. REQ-600 moved the
turn path out to `runtime/turn.rs`, and REQ-599's own module map already records
the correction: *"Was 10,306 at REQ-599's close; REQ-600 moved the turn path
out."* **Counting rule:** everything above the first column-0 `#[cfg(test)]`,
which is the rule `runtime_module_map.rs` enforces against that table and the
rule every figure in this REQ uses.

The architecture doc's target is unchanged — no module above 2,000, `mod.rs`
under 1,000 — so REQ-599's AC-1 is still recorded NOT MET, and `mod.rs` is still
more than seven times the target. The slice has lost none of its value; if
anything the ~900 lines ADR-4 estimated are now a larger fraction of what is
left, which is a reason to re-measure the slice rather than to trust the
estimate.

This is deliberately **not** REQ-600. REQ-600 restructures `run_prompt_turn`'s
control flow; this relocates session-lifecycle code without changing behaviour.
REQ-599's own reason for splitting those apart applies unchanged: a behaviour
change buried in a relocation diff is not reviewable.

## Acceptance Criteria

- [ ] The session-lifecycle surface is identified by **reading the impl
      structure**, not by searching rationale ids — ADR-1 of REQ-599 measured
      that ids do not locate seams, and LESSON-593 records the correction that
      they are a weak *positive* signal only. State the counting rule beside
      any figure this produces.
- [ ] The slice moves to its own module under `runtime/`, behaviour unchanged,
      as one reviewable commit.
- [ ] `crates/tetond/tests/runtime_module_map.rs` and
      `crates/tetond/tests/runtime_doc_paths.rs` stay green — the new module is
      documented in the map, and no comment is left citing a path that moved.
- [ ] `crates/tetond/tests/runtime_visibility.rs`'s ratchet is not loosened:
      anything the move makes cross-module is `pub(super)`, not `pub(crate)`,
      unless a caller outside `runtime/` genuinely exists (REQ-602 AC-1).
- [ ] Tests move with their subject, or the module header says why they stayed
      (REQ-599 BR-7, as enforced from REQ-602 TASK-304 onward).
- [ ] `mod.rs`'s production line count is reported before and after, with the
      counting rule stated.
- [ ] Suite green, grepped for `FAILED`; clippy and `fmt --check` clean.

## Assumptions

- The slice is still coherent as a unit. REQ-599 asserted this from the plan
  side and never tested it against the code; this REQ must confirm it before
  committing to a module, and say so if it turns out the lifecycle code is
  genuinely entangled rather than merely large.

## Out of Scope

- `run_prompt_turn`'s control flow (REQ-600).
- Any behaviour change. This is a relocation.

## External Dependencies

None.
