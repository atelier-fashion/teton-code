---
id: REQ-588
title: "A spend cap, and an event vocabulary a future kind cannot break"
status: draft
deployable: true
created: 2026-08-20
updated: 2026-08-20
component: "daemon/cost-ledger"
domain: "cost"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["cost", "reliability", "extensibility", "developer-experience"]
tags: ["spend-cap", "context-budget", "cost-ceiling", "forward-compatibility", "serde", "context-pressure", "req-586", "follow-up"]
---

## Description

Two residuals REQ-586 named rather than closed, filed together because both
are about a ceiling that does not exist yet.

**1. There is no spend cap.** REQ-586 made the context budget follow the route,
which was the point — but its own review established that *the context budget
is the only per-turn input-token bound Teton has*. With the shipped recipes
declaring 1M-token windows for four of six vendors, a `Native` route derives
≈666k words per call and runs up to 25 iterations, so one prompt can carry
≈25M input tokens. The product decision (REQ-586 OQ-6, amended) was
deliberate: the declaration is the consent, and TASK-194 added a notice when a
big window is recorded. A notice is not a bound. The product's headline promise
is cost control, and the only knob today is a per-provider
`context_budget_cap` the user must know to set.

What this REQ should decide: whether the cap is per turn, per prompt, or per
session; whether it counts input tokens or currency (the cost ledger already
prices every call, so currency is available); what happens at the ceiling —
refuse the next call with a typed outcome, or degrade the route to a cheaper
tier; and whether a default exists at all or it stays opt-in like `[privacy]
redact`.

**2. `ContextPressureKind` is not forward-compatible.** It is a plain
snake_case serde enum, so a client that meets a kind it does not know
*refuses the frame* rather than degrading — a lost line, not a mis-rendered
one. No released client is affected today (the event and all four kinds ship
in one release, and a client predating the event drops the envelope at
`classify`), which is why REQ-586 recorded it instead of fixing it. It becomes
real the day a fifth kind is added to a shipped enum — and REQ-587's work is
likely to want one.

The fix is a custom `Deserialize` with an `Unknown(String)` catch-all, and the
same question should be asked of `Event` itself and of `BudgetBound`: the
project's additive-protocol rule (REQ-573) promises older peers degrade, and
for enums carried inside a payload that promise is currently unbacked.

## Business Rules

- [ ] BR-1: A spend ceiling exists and is enforced where the spend happens —
      the egress choke point, not the caller — so no route can bypass it.
- [ ] BR-2: The ceiling's unit and scope are one decision with one home, and
      the surface names which one is binding (the REQ-586 `bound` pattern).
- [ ] BR-3: Reaching the ceiling is a **typed outcome**, not a generic error,
      and it never degrades a provider's health (the REQ-586 ADR-8 posture).
- [ ] BR-4: An unknown enum variant inside an event payload degrades to a
      rendered-but-unrecognised line, never a dropped frame; pinned in both
      directions for `ContextPressureKind`, `BudgetBound`, and any sibling.
- [ ] BR-5: Whatever default is chosen, a fresh install's behaviour is stated
      in `teton_docs` and in the release notes — a ceiling that appears
      silently is the mirror of the window that was recorded silently.

## Acceptance Criteria

- [ ] AC-1: A session that would exceed the ceiling is refused at the choke
      point with the typed outcome, health unchanged, and the cost ledger
      showing what was spent. (egress-capture)
- [ ] AC-2: The binding ceiling is named on `/verbose` and in the refusal.
      (`cli_e2e`)
- [ ] AC-3: A payload carrying an unknown `ContextPressureKind` renders a
      sane line rather than dropping the frame; a payload carrying a known one
      is byte-identical to today. (protocol contract, both directions)
- [ ] AC-4: `cargo test --workspace --no-fail-fast` green; the REQ-586 budget
      and bound behaviour is unchanged where no ceiling binds.

## External Dependencies

- Builds on REQ-586 (merged, `c9e9265`) — the budget, the `bound` fact, and
  the `context_pressure` vocabulary.
- REQ-587 (spec PR #193) will likely add a `ContextPressureKind`; if it lands
  first, BR-4 is the thing that makes that safe.

## Open Questions

- [ ] OQ-1: Per turn, per prompt, or per session? *Lean:* per prompt — it is
      the unit the user initiates and the one the ≈25M figure is stated in.
- [ ] OQ-2: Tokens or currency? *Lean:* currency, since the ledger prices
      every call and a token ceiling means different money per provider.
- [ ] OQ-3: Default on or opt-in? *Lean:* opt-in, matching `[privacy] redact`,
      with the REQ-586 notice pointing at it.
- [ ] OQ-4: At the ceiling — refuse, or degrade to a cheaper tier? *Lean:*
      refuse; a silent tier downgrade is the shape this project keeps rejecting.

## Out of Scope

- Changing REQ-586's budget derivation or its bounds.
- Per-user or hosted billing of any kind (out of MVP scope entirely).

## Retrieved Context

Filed at REQ-586 wrapup from its own review findings; not produced by a
`/spec` retrieval pass. Run `/spec` retrieval before `/architect` if this REQ
is picked up, so it inherits the corpus the pipeline expects.
