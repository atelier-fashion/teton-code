---
id: TASK-257
title: "Session-recovery suite: withdrawal, the observed-rejection memo, and the closed circle"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-249, TASK-250]
---

## Description

The second half of the integration coverage, split from TASK-253 so neither suite
carries more than three dependencies and each has one subject. This one owns D-8's
promise: **an approval must not leave the session hitting the same wall.**

## Files to Create/Modify

- `crates/tetond/tests/skill_over_budget_recovery.rs` (new)

## Acceptance Criteria

- [x] **AC-22**: an accepted turn that fails at the window withdraws its expansion, and a
  real second turn in the same session assembles without it. **Restated during
  implementation, and the restatement is load-bearing** — see *Findings* below: what is
  asserted is that the next turn carries the **refusal that replaced** the expansion, in
  the committed conversation and on the next turn's wire. "Assembles without it" is
  asserted nowhere, because it was **measured to be vacuous**
- [x] **AC-23**: after an observed rejection, the next offer for the same skill on the
  same route names the prior rejection and leads with the remedy. Two negative
  assertions guard BR-10's boundary: the record must not suppress the offer, and must not
  pre-answer it
- [x] **AC-24**: after a `BindTierRemote` remedy is applied, an identical second
  invocation reaches **no offer at all**, because the route now fits — the end-to-end
  proof that the reported `/analyze` circle is closed
- [x] The observed-rejection record is asserted to live in one store, daemon-side. **This is
  a STRUCTURAL claim, not a behavioural one** (corrected after TASK-246): the record never
  crosses the wire — only the sentence composed from it does — so there is no client half to
  drive. Assert the absence: no `SkillOverBudget` field on the CLI's `SessionGrants`.
  Attempting a behavioural test here would produce a vacuous pass (ASSUME-017, LESSON-520)
- [x] Every assertion is driven from real turns, never struct literals (LESSON-544/552)

## Technical Notes

AC-24 is the criterion that matters most to the user who filed this: it is the difference
between a feature that explains the dead end and one that removes it. Build it against the
same spawned-daemon fixture `context_pressure.rs:1095` uses, since that is the only
existing pattern that constructs a real local-engine route through a skill refusal.

**Deviation, flagged.** The suite drives `DaemonRuntime::run_prompt_turn` **in process**
rather than the spawned daemon `context_pressure.rs:1095` uses, and it is not a shortcut:
the spawned-daemon `Client` auto-answers every permission prompt with `allow_once`, which
`interpret_over_budget` **denies** (every unrecognized id denies, by design — REQ-585
ADR-7). No test in this suite could select `over_budget_proceed_once` or
`over_budget_remedy_only` through that harness. The in-process fixture is still a real
turn against a real config file on disk, a real `AddressedPermissionDelivery`, a real
`MockProvider` on a real socket, and — for AC-24 — a real config write re-parsed by a
second `DaemonRuntime`. It is the same fixture shape TASK-253 arrived at independently.

## Findings

**1. AC-22 as written is a vacuous assertion, and this was measured rather than argued.**
"The next turn assembles without the expansion" is satisfied by three independent
mechanisms, only one of which is the withdrawal:

- `run_prompt_turn`'s ordinary failure arm calls `CarriedTurn::abandon` and writes nothing
  (TASK-249's own finding);
- REQ-586 BR-10's budget re-assertion drops the oversized block **at the commit**, so it
  is gone from the session even on the path where the accepted turn *succeeded*;
- ordinary context pressure drops it again on the turn after.

`an_accepted_turn_that_serves_still_loses_the_expansion_at_the_commit` pins the second and
third on a serving turn where nothing withdrew anything, and the mutation run confirmed the
first. So the absence assertion is green with `withdraw_accepted_expansion` deleted. It is
therefore asserted **nowhere** in the suite, and both the module doc and the test carry a
note saying why, so nobody re-adds it believing it means something.

What is asserted is the positive claim TASK-249 made observable: the refusal **is** a block
in the committed conversation, and it **is** in the next turn's assembled prompt.

**2. Mutation-verified.** Against a clean tree at `97d2c8f` with the
`withdraw_accepted_expansion` call removed, `the_next_turn_after_a_window_refusal_carries_
the_refusal_that_replaced_the_expansion` **fails** (on the conversation assertion first,
then the wire one); AC-23, AC-24 and the structural test still pass, which is correct —
they do not depend on the withdrawal. This makes AC-22 genuinely mutation-sensitive.

**3. `Window` + `ExceedsWindow` needs a declaration above ~13,000.** A declared 8,000
derives a *floored* pair, which is a true and reachable state but a different sentence
about a different thing. The fixture declares 20,000.

**4. AC-24 result: the circle is closed.** On the reported route (`bound: local engine`,
one configured remote, unbound), `over_budget_remedy_only` writes both halves of ADR-5's
ordered pair — `capabilities.max_context = 1000000` for `frontier` and the tier binding —
verified on disk *and* by re-loading the document into a fresh `DaemonRuntime`. An
identical second invocation of the same skill then reaches **no offer at all** and serves,
with the whole expansion on the wire.
