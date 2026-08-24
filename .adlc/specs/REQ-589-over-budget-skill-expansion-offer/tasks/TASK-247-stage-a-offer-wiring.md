---
id: TASK-247
title: "Wire the offer into Stage A for the typed caller"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-243, TASK-244, TASK-259]
---

## Description

BR-2 + BR-11. Stage A (runtime.rs:3601) becomes offer-capable for `SkillCaller::User` only. The model-invoked path (`skill_append_fit`) keeps refusing and is never offered a choice.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — Stage A Body (3601) and Stage B WithDynamicContext (3681); the reroute guard (10894) stays refusal-only

## Acceptance Criteria

- [x] A model-invoked over-budget call is refused with the Model arm's sentence and reaches no offer (BR-2, AC-5)
- [x] Every not-sent path reaches no provider, emits no context_pressure, changes no health, and does not spend the session-naming duty (BR-11, AC-18) — asserted by egress capture, not by inspection
- [x] The naming duty stays deferred below the gate (runtime.rs:3641)
- [x] Declining produces byte-identical output to today's refusal under the same -32023 (AC-3)
- [x] Accepting dispatches the expansion whole, byte-for-byte what skill_fit measured (AC-1, BR-1)
- [x] **Publishes all three events TASK-241 added** — `skill_over_budget_offered`,
  `skill_over_budget_accepted`, `skill_over_budget_remedy_applied`. Assigned here after
  TASK-244 reported that no task's acceptance criteria claimed them; unassigned, they would
  have shipped dead. Each is driven end-to-end from a real turn and mutating its emit line
  must redden the suite (LESSON-544 — a wire fact whose producer is untested is exactly how
  REQ-585/587 shipped Criticals past a green suite)
- [x] Maps the protocol `SkillStage` to `tetond::harness::budget::SkillStage` explicitly —
  they collide by name and the daemon type could not be re-exported (ADR-13)

## Technical Notes

> **Pass `.window`, never `.cap` (TASK-259).** On a `UserCap` route,
> `budget_inputs_for(..).window` is the RAW declaration (e.g. 200,000), not the user's cap
> (e.g. 40,000). That is ADR-15 applied consistently — the cap is *this daemon's* policy
> exactly as the reservation is — but it means a measurement can be over budget, over the
> cap, and still legitimately `FitsWindow`. Passing `.cap` would collapse the `UserCap` +
> `FitsWindow` row and produce a false `ExceedsWindow` warning.
> `the_inputs_carry_the_declared_window_the_budget_does_not` pins it.

> **Map the two `SkillStage` types.** The protocol's and
> `tetond::harness::budget::SkillStage` collide by name; the daemon type carries the refusal
> clause it words, so it could not be re-exported (ADR-13).


`the_two_refusals_bracket_the_consent_seam_and_precede_the_seed` (skill_turn.rs:3357) and `the_budget_check_runs_in_the_loop_and_the_tool_measures_nothing` (:3447) are structural tests that WILL break on any ordering change — update them deliberately, do not delete.

## Implementation notes (2026-08-24)

**ADR-16 landed as `PermissionSubject::SkillOverBudget { sentence: String }`.**
Required, not `#[serde(default)]` — no daemon that can emit this `kind` predates
the field, and a default would only hide a daemon that stopped wording its own
question. The daemon composes it with `OverBudgetOffer::question(source, prior)`
and the client renders it verbatim; the surrounding structure stays for layout,
for which option rows to draw, and for the `WindowVerdict::Unknown` hedge, which
is a claim about the *client's* vocabulary and so cannot come out of a sentence
the daemon wrote.

**Two seams, one helper.** `DaemonRuntime::offer_or_refuse_over_budget` is called
from Stage A **and** Stage B, and `SkillStageVerdict` is what the call sites read.
Stage B was wired too — the task file listed it, the wire carries `stage`
precisely so it can, and leaving it a hard refusal would have refused a turn the
user had just approved at Stage A. It is guarded by a **carry**: an expansion the
dynamic-context fold left byte-identical is the same question, already answered
one screen up, and is not asked twice. Compared as *text*, not as a measured pair
— two different expansions can measure the same figures, and this decides whether
a human is asked about bytes they have not seen. Nothing survives the invocation,
so BR-10 is untouched.

**`skill_fit` is no longer called at either typed stage.** It consumes the `Fit`
and returns only a sentence, and the offer needs the pair. The estimator is
unchanged (`ContextManager::would_seed_fit`, one call), and the decline sentence
is `OverBudgetOffer::decline_refusal()`, which reaches the identical
`skill_refusal` arm with identical arguments — AC-3 holds by construction rather
than by two sentences being kept in step. The model path and both reroute arms
still call `skill_fit`/`skill_append_fit` and are refusal-only.

**Sharp edges, both handled.** `budget_inputs_for(..).window` (never `.cap`) —
pinned end-to-end by `a_capped_route_reads_the_declared_window_and_its_remedy_clears_the_cap`,
which drives a 200,000-token window under a 6,000-token cap and asserts
`FitsWindow`; swapping in `.cap` reddens it. The two `SkillStage` types are
mapped by `wire_skill_stage`, an exhaustive `const fn` at the publishing surface,
so a third stage is a compile error rather than a silent `Unknown` on a record.

**All three events are live**, each driven from a real turn, each mutation-checked:

| Mutation | Reddens |
|---|---|
| drop the `skill_over_budget_offered` publish | 3 tests, incl. `a_declined_offer_is_todays_refusal_and_no_provider_sees_the_turn` |
| drop the `skill_over_budget_accepted` publish | `an_accepted_offer_dispatches_the_expansion_whole` |
| drop the `skill_over_budget_remedy_applied` publish | `a_capped_route_reads_the_declared_window_and_its_remedy_clears_the_cap` |
| `inputs.window` -> `inputs.cap` | the capped-route test |
| `sentence: offer.question(..)` -> `String::new()` | `the_offer_carries_the_daemons_own_words_and_the_figures_it_measured` |
| the `invoker: None` arm -> `Accepted` | `an_offer_with_nobody_to_ask_is_todays_refusal_and_announces_nothing` |
| `SuspendedForAcceptedTurn` -> `Enforced` | `an_accepted_offer_dispatches_the_expansion_whole` |

**BR-11 is asserted by egress capture, not inspection.** On a local route the
engine *is* the provider, so the fixture's recording engine is the capture: after
a declined offer no prompt beyond the classifier (which runs above Stage A by
ADR-3 and is legitimately paid for) reaches it, no `context_pressure` is on the
bus, `health_snapshot()` is empty, the conversation has zero blocks, and
`sessions.claim_title()` still answers `true` — the naming duty is *spent*
synchronously, so claiming it afterwards is a race-free proof the turn left it
alone.

## Decisions taken here that the task file did not specify — flagged for verify

1. **Stage B offers too** (above). The alternative refuses a turn the user just
   approved.
2. **The one-question-per-expansion carry** (above).
3. **`PressurePolicy::SuspendedForAcceptedTurn` is wired here**, through
   `run_one_attempt`. TASK-245 built the suspension and left it with **no
   production caller**; the accept path is the only place that knows, so it is
   wired here rather than left dead (LESSON-544). A fresh policy value is built
   per attempt, so a fallback re-assembling the same consented prompt is owed the
   same suspension.
4. **The three single-write remedies are applied here** (`RaiseWindow`,
   `DeclareWindow`, `RaiseCap`), through `apply_config_update` — `config/set`'s
   own body, ADR-4's one durable-write path — because `skill_over_budget_remedy_applied`
   otherwise had no producer at all. `BindTierRemote`'s **ordered pair** and
   ADR-12's provider choice are explicitly left to TASK-250, with a `TODO` on the
   match arm; a half-applied version here would leave a newly-bound remote tier
   with no window, which is the exact circle BR-9 exists to close.
5. **`RaiseCap` writes `context_budget_cap = 0`.** The label offers to "raise or
   clear" it; the cap is the user's own number and the daemon has no second
   opinion about what it should be, so clearing is the one write it can make
   without inventing the user's policy.

## Gaps found and NOT closed here — for verify

- **`OverBudgetOffer::accepted_record` has no wire surface.** ADR-16 gave the
  *question* one; BR-5's third sentence has none — `SkillOverBudgetAccepted` is
  typed and holds no prose, and an accepted turn has no refusal frame to carry
  it. It is written to the daemon's stderr record channel here, beside the taint
  pin and the degraded-duty lines, which is honest but is not a user surface.
  Whoever owns the client rendering should decide whether it deserves one.
- **A window remedy with no `ProposedWindow` promises a write it cannot make.**
  `window_write`'s no-proposal arm renders `write capabilities.max_context for
  \`x\` to that provider's real window — this daemon ships no figure for it and
  will not invent one`, and that string becomes an option **label**. Selecting it
  applies nothing. That is precisely the `enable_permanent` failure ADR-1 cites.
  The fix belongs in `budget.rs` (drop the remedy options when no value can be
  written) or in ADR-1's label rule, neither of which this task owns.
