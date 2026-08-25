---
id: TASK-249
title: "Withdraw the expansion when an accepted turn fails at the window"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-239, TASK-246]
---

## Description

BR-14.1 / D-8. An approval must not leave the session hitting the same wall. On the typed context failure, withdraw the expansion so the next turn assembles cleanly.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — the turn's error handling calls `withdraw_block`, mirroring `withdraw_model_expansion` (10860) rather than reusing it

## Acceptance Criteria

- [x] On `context_length_exceeded`, the accepted expansion is withdrawn via `ContextManager::withdraw_block` (context.rs:986)
- [x] The withdrawn block's provenance is absorbed into `DroppedProvenance` — a `local-only` source must not survive the withdrawal (BUG-188)
- [x] The NEXT turn in that session assembles without the expansion — driven by a real second turn, not by inspecting the block list (AC-22)
- [x] Withdrawal fires only on the context failure; other failure classes leave the turn to the ordinary retry machinery
- [x] A test drives a real accepted turn to a window failure and then a real SECOND turn, asserting the expansion is absent from the second turn's assembled prompt — not by inspecting the block list
- [x] A provenance test asserts a `local-only` source in the withdrawn block does not survive into the next turn (BUG-188's own regression shape)
- [x] Mutating the withdrawal call site reddens the suite (LESSON-544)

## Technical Notes

`withdraw_block` already absorbs provenance (context.rs:990-991) — that is why BR-14.1 names it rather than inventing a path. Depends on TASK-239: without the typed outcome there is no reliable trigger.

## Implementation Notes

Implemented in `crates/tetond/src/runtime.rs` only.

- The trigger reads `HarnessError::context_refusal()` (ADR-3's tier-agnostic
  projection), sited beside the privacy-block handler where `result` is still a
  `HarnessError` and the conversation is still writable — not inside the
  two-variant arm below it.
- `withdraw_accepted_expansion` mirrors `withdraw_model_expansion` and calls
  `ContextManager::withdraw_block`, which absorbs the withdrawn block's
  provenance into `DroppedProvenance` (BUG-188).
- `ObservedWindowRejections::mark` is called on the same path, keyed by the
  route that actually refused, so BR-14.2's next offer leads with it.
- `over_budget_accepted: bool` + `accepted_expansion: Option<String>` were
  folded into one `Option<AcceptedExpansion>` carrying the skill name and the
  accepted bytes; the Stage B `Accepted` arm now records this stage's (folded)
  text, which it did not before.

### Deviation, flagged for ratification

**BR-14.1's withdrawal is unobservable under REQ-567's commit protocol.** A
failed turn calls `CarriedTurn::abandon`, which writes nothing, so a withdrawal
applied to that turn's manager dies with it: the next turn would assemble
without the expansion whether or not the withdrawal ran, and no test could tell
the call from its deletion (LESSON-544's shape). So this task **commits** the
withdrawn conversation on exactly one failure path — an over-budget send a human
approved, refused by the tier at the window, whose block was found — and
abandons on every other failure as before. Without that commit, BR-14.1 is dead
code; with it, `BR-6`'s "a failed turn leaves no trace" is narrowed by one path.
What is committed is the conversation it was handed with the expansion replaced
by the refusal, so no history block is dropped (AC-16 holds).

Two further findings for the REQ record:

1. Ordinary context pressure on the *following* turn would drop an oversized
   committed expansion anyway (observed while mutation-testing). BR-14.1's value
   is therefore the **replacement** — the session carries a truthful record of
   what was refused — rather than the removal alone.
2. AC-22's "the next turn assembles without the expansion" is true by
   construction under abandon; the mutation-sensitive assertion is that the
   next turn carries the *refusal that replaced it*.

### Tests (all in `runtime.rs`, module `the_over_budget_offer`)

- `a_window_refusal_withdraws_the_expansion_so_the_next_turn_assembles_without_it`
  — two real turns; asserts against the bytes that reached the engine.
- `the_withdrawn_expansions_local_only_source_is_absorbed_not_shed` — the
  `local-only` boundary case: no retained block names the skill file, the
  identity is in `DroppedProvenance`, and the session stays pinned.
- `a_watched_rejection_leads_the_next_offer_for_the_same_pair` — the memo,
  proved through BR-14.2's real consumer.

Mutation-tested (each reddens the named tests): dropping the
`withdraw_accepted_expansion` call, abandoning instead of committing, matching
`ContextLengthExceeded` alone instead of `context_refusal()`, and dropping the
`ObservedWindowRejections::mark`.
