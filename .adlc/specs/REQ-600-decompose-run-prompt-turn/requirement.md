---
id: REQ-600
title: "Decompose run_prompt_turn into a stage sequence, and slice the god-impl"
status: draft
deployable: true
created: 2026-08-31
updated: 2026-08-31
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability", "extensibility"]
tags: ["refactor", "god-impl", "run-prompt-turn", "turn-path", "req-599-followon"]
---

## Description

REQ-599's deferred step 8, filed as its own REQ by decision on 2026-08-30.

REQ-599 delivered seven seams and took `runtime.rs` from 14,183 production lines
to 10,306 in `mod.rs`. It did **not** reach its own AC-1 target ("no module
above 2,000, `mod.rs` under 1,000"), and the arithmetic says why: six of its
seven steps moved *top-level* items — types, free functions, constants — and
only one took methods out of `impl DaemonRuntime`. That impl is still **~6,540
production lines**, and it is the whole remaining problem.

Within it, `run_prompt_turn` is ~1,084 lines carrying session claiming, skill
expansion, routing, budget checks, consent, dispatch and commit in one `async
fn`.

**Why this was split off rather than done.** REQ-599's steps *relocate* code;
this one *changes control flow* on the path every prompt runs through. With tests
at 61% of the file, landing them together buries the one genuine behavior risk
inside a diff several times its size. This REQ's diff should be readable as a
change, because that is what it is.

## Inherited from REQ-599

- **AC-2**: `run_prompt_turn` reduced to a stage sequence, body under 200 lines,
  each stage independently nameable and testable.
- **AC-3**: `turn_loop.rs::run_session_turn_with_pressure_policy` (762 lines,
  8–9 levels of nesting) drops to depth 5 or below. It was REQ-599's OQ-3 and
  sits in a different module with a different parameter cluster (REQ-598 ADR-2).
- **BR-3's ordering invariants**, which are the actual risk: the three
  typed-outcome arms stay ordered before the generic remote arm; gates stay
  before the parses they guard (LESSON-520); the claim stays before the
  session-state read (LESSON-539); presence gates keep their reader-loop freedom
  (LESSON-518, BUG-184); and the `TurnContext` construction point stays after
  the REQ-580 warming hold (REQ-598 BR-2.1).

## What REQ-599 learned that this REQ should start from

- **Rationale ids do not locate seams** (LESSON-593, REQ-599 ADR-1). Measured:
  1 of 19 clustered, 13 scattered file-wide. Do not re-propose that method.
- **Seams are created, not only discovered** (REQ-599 step 7). `provider`
  measured as scattered across 10,366 lines at Phase 2 and was 375 contiguous
  lines after four unrelated slices left. Re-measure cohesion after each step
  rather than planning the whole order up front.
- **Adjacency is not membership.** Two items inside extraction ranges belonged
  elsewhere and were checked rather than assumed.
- **Every derived check is part of the change** (LESSON-594). Seventeen
  source-scanning tests broke across REQ-599's first two steps, one of them from
  a `#[cfg(test)] mod` declaration placed at the top of a file.
- **Visibility passes can narrow the API** (LESSON-595). Re-export by name.

## Acceptance Criteria

- [ ] AC-1: `run_prompt_turn`'s body is under 200 lines, a sequence of named
      stages.
- [ ] AC-2: `impl DaemonRuntime`'s production line count drops below a target
      recorded in the architecture doc.
- [ ] AC-3: `run_session_turn_with_pressure_policy`'s maximum nesting depth is 5
      or below, measured and recorded.
- [ ] AC-4: Every BR-3 ordering invariant above has a test that fails when the
      ordering is inverted — not a comment asserting it holds.
- [ ] AC-5: The REQ-598 event-ordering fixture replays identically.
- [ ] AC-6: REQ-599's traceability sweep and module-map guard both still pass,
      with `BASE` and `TOUCHED` repointed at this REQ's base.
- [ ] AC-7: Delivered as independently-green commits, each reviewable as a
      change rather than as a relocation.

## Out of Scope

- Further module extraction that does not serve the decomposition of the turn
  path. REQ-599's seven seams are enough surface to work against.

## Open Questions

- [ ] OQ-1: Do the stages become free functions taking `TurnContext`, methods on
      a `TurnStages` type, or an enum-driven sequence? REQ-598's ADR-3 argued
      against a typestate for `route`; the same argument may or may not apply to
      the stage sequence.
- [ ] OQ-2: Does `turn_loop.rs` (AC-3) belong in this REQ or its own? It shares
      the ordering invariants but not the parameter cluster.
