---
id: REQ-606
title: "Collapse the turn-path parameter bundles that carry no invariant"
status: approved
deployable: false
created: 2026-09-01
updated: 2026-09-01
component: "daemon/session"
domain: "refactoring"
stack: ["rust", "daemon"]
concerns: ["maintainability"]
tags: ["refactor", "parameter-bundle", "req-600-followup", "turn-path"]
---

## Description

REQ-600 decomposed `run_prompt_turn` into eight stages and introduced **fourteen**
parameter bundles to keep their signatures under clippy's limit without adding
`too_many_arguments` suppressions — which `suppression_ratchet.rs` refuses by
design ("a new suppression is a new unnamed parameter cluster; name it instead").

**The set, enumerated.** Review reported "thirteen"; the diff of `9ec2a17`
against `9232fac` introduces fourteen. The set is listed here rather than left
as a count, because AC-1's deliverable is a classification and a classification
of an unnamed set cannot be checked:

| # | Type | Module |
|---|------|--------|
| 1 | `ClaimedTurn` | `runtime/turn.rs` |
| 2 | `AssembledHarness` | `runtime/turn.rs` |
| 3 | `AttemptInputs<'a>` | `runtime/turn.rs` |
| 4 | `AttemptState` | `runtime/turn.rs` |
| 5 | `ResolvedRoute` | `runtime/turn.rs` |
| 6 | `PreparedAttempts` | `runtime/turn.rs` |
| 7 | `SessionFacts<'a>` | `runtime/turn.rs` |
| 8 | `TurnRequest<'a>` | `runtime/turn.rs` |
| 9 | `ExpansionInputs<'a>` | `runtime/turn.rs` |
| 10 | `TurnProducts` | `runtime/turn.rs` |
| 11 | `LoopContext<'a>` | `harness/turn_loop.rs` |
| 12 | `ToolCallSite<'a>` | `harness/turn_loop.rs` |
| 13 | `ModelReply<'a>` | `harness/turn_loop.rs` |
| 14 | `TurnLatches` | `harness/turn_loop.rs` |

`SkillToolDocs` is deliberately excluded: it is `pub(crate)`, it carries bundled
documentation rather than a call's parameters, and it is not a signature-width
device. If `/architect` judges any of the fourteen out on the same grounds, it
says so and why — the set shrinks on a stated rule, never on a recount.

Naming them was right. Fourteen is more than the job needs. Review judged that
roughly five earn a name and the rest are transport:

- **`PreparedAttempts`** is constructed on the last line of
  `prepare_the_attempts` and destructured on the first line of its only call
  site. It exists because Rust returns one value, and carries no invariant.
- **`TurnProducts`** is named as an output but is an *input* bundle, built from
  four loose locals at the call site and destructured on the callee's first line.
- **`ToolCallSite`** is a borrowed re-projection of `ModelReply`: four of its
  five fields come straight from it.
- **`SessionFacts`** and **`TurnRequest`** together re-spell six values that
  `TurnContext` already carries, so a reader must know which spelling is in force
  at which line. REQ-600 ADR-3 gives a real reason — they exist before the pivot,
  where no `TurnContext` can — so this one may well be correct as it stands.

`route` appears as a field in four bundles, `probed` in three, `turn_id` in three.

## Acceptance Criteria

- [ ] AC-1: Each of the fourteen named in the Description's table is
      classified: **carries an invariant** (keep), **transport** (collapse), or
      **deliberate duplication with a stated reason** (keep, and say the reason
      in the type's doc). The classification is the deliverable; the count that
      results is not a target.
- [ ] AC-2: No `#[allow(clippy::too_many_arguments)]` is added.
      `suppression_ratchet.rs` stays green at its recorded figure, or the figure
      moves deliberately with what collapsed named.
- [ ] AC-3: `run_prompt_turn`'s body stays under 200 lines (REQ-600 AC-1) and
      `run_session_turn_with_pressure_policy` stays at brace depth 5 or below
      (REQ-600 AC-3), both under the rules those ACs state.
- [ ] AC-4: Behaviour unchanged: the REQ-598 event fixture replays unregenerated, and
      **each of REQ-600 BR-3's three testable ordering invariants — 1, 3 and 5 —
      still fails on its inversion, re-run rather than re-asserted.** REQ-600
      shipped a guard that silently stopped covering its subject when code
      moved, and only re-running the mutation found it.
  - **Why three and not five.** REQ-600's own verification records **its** AC-4
    as *four of five*, and the two that are not pinned by inversion cannot be: invariant
    2's ordering is enforced by the compiler (`accept_invocation` takes the gate
    as a parameter, so the test pins the adjacent property that the gate is
    constructed exactly once inside the memoizing `permission_gate_for`), and
    invariant 4 "has no inversion test either and cannot have one on this path"
    — there is no presence gate to park in, and its substitute pins that no
    blocking wait is introduced. An AC that demands five inversions cannot be
    met, and an unmeetable AC is the shape that gets ticked without checking.
  - **What covers 2 and 4 instead.** Their substitutes are re-run under the same
    rule: the gate-construction count for invariant 2, the no-blocking-wait
    assertion for invariant 4. If this REQ's collapse changes a signature such
    that invariant 2's ordering stops being compiler-enforced, that is a finding
    to record, not a substitution to make quietly.
- [ ] AC-5: Suite green, grepped for `FAILED`; clippy 0 under `deny`; fmt clean.

## Assumptions

- The bundles can be collapsed without pushing any signature back over the
  argument limit. If one cannot, that is a finding to record — it would mean the
  cluster is real and the bundle earns its name after all.
- **The same applies to AC-3's body-length budget, which is tighter than it
  looks.** `run_prompt_turn` is at **188** lines against AC-3's 200 — twelve
  lines of headroom, re-derived at this REQ's base rather than taken from
  REQ-600's record. Collapsing an *input* bundle moves its fields back to the
  call site, and for the input bundles that call site is `run_prompt_turn`'s
  body. If a collapse that is right on the classification cannot be had without
  pushing the body over 200, that is the same kind of finding as the argument
  limit: record it, and keep the bundle. AC-1's classification is the
  deliverable; neither the resulting count nor the resulting line count is a
  target to be hit by weakening the other criterion.

## Out of Scope

- Further decomposition of the turn path. REQ-603 re-measures this impl for the
  session-lifecycle slice and can absorb anything structural.

## External Dependencies

- None.
