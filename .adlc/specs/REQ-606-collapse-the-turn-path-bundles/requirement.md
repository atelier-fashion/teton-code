---
id: REQ-606
title: "Collapse the turn-path parameter bundles that carry no invariant"
status: draft
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

REQ-600 decomposed `run_prompt_turn` into eight stages and introduced **thirteen**
parameter bundles to keep their signatures under clippy's limit without adding
`too_many_arguments` suppressions — which `suppression_ratchet.rs` refuses by
design ("a new suppression is a new unnamed parameter cluster; name it instead").

Naming them was right. Thirteen is more than the job needs. Review judged that
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

- [ ] Each of the thirteen is classified: **carries an invariant** (keep),
      **transport** (collapse), or **deliberate duplication with a stated
      reason** (keep, and say the reason in the type's doc). The classification
      is the deliverable; the count that results is not a target.
- [ ] No `#[allow(clippy::too_many_arguments)]` is added.
      `suppression_ratchet.rs` stays green at its recorded figure, or the figure
      moves deliberately with what collapsed named.
- [ ] `run_prompt_turn`'s body stays under 200 lines (REQ-600 AC-1) and
      `run_session_turn_with_pressure_policy` stays at brace depth 5 or below
      (REQ-600 AC-3), both under the rules those ACs state.
- [ ] Behaviour unchanged: the REQ-598 event fixture replays unregenerated, and
      every BR-3 ordering guard still fails on its inversion — **re-run, not
      re-asserted.** REQ-600 shipped a guard that silently stopped covering its
      subject when code moved, and only re-running the mutation found it.
- [ ] Suite green, grepped for `FAILED`; clippy 0 under `deny`; fmt clean.

## Assumptions

- The bundles can be collapsed without pushing any signature back over the
  argument limit. If one cannot, that is a finding to record — it would mean the
  cluster is real and the bundle earns its name after all.

## Out of Scope

- Further decomposition of the turn path. REQ-603 re-measures this impl for the
  session-lifecycle slice and can absorb anything structural.

## External Dependencies

- None.
