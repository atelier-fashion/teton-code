---
id: REQ-588
title: "A spend cap, and an event vocabulary a future kind cannot break"
status: complete
deployable: true
created: 2026-08-20
updated: 2026-08-23
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
      **Scoped to enums *inside* a payload.** The top-level `Event` enum is a
      larger, separate hole — an unknown event *kind* drops the whole frame,
      which is why `ProjectMatch`, `SkillRefused` and `SkillInvoked` are all
      invisible to older clients — and it is tracked on its own rather than
      absorbed here, because widening `Event` touches every match on it.
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
      and bound behaviour is unchanged where no ceiling binds. (BR-1..BR-4
      regression)
- [ ] AC-5: **BR-5's half that is not code.** A fresh install's ceiling
      behaviour — that there is none until it is configured (OQ-3) — is stated
      in `teton_docs` and in the release notes, and a test asserts the
      `teton_docs` sentence exists rather than trusting it was written. The
      release-notes half is a wrapup checklist item, not a test.
- [ ] AC-6: **The unpriced case (OQ-2).** A provider the price table cannot
      price, with a ceiling configured, is refused at the choke point with a
      typed outcome naming the missing price — not waved through uncapped, and
      not charged a guessed rate. (egress-capture)

## External Dependencies

- Builds on REQ-586 (merged, `c9e9265`) — the budget, the `bound` fact, and
  the `context_pressure` vocabulary.
- REQ-587 **shipped** (2026-08-22, `f2066b0`) and added no
  `ContextPressureKind` after all — only REQ-586 has ever touched that enum.
  The speculation this line used to carry is retired; BR-4 stands on its own
  merits rather than on a coming variant.
- BUG-186 (2026-08-22) already applied BR-4's pattern to two sibling enums
  (`DynamicOutcome`, `NotRunReason`), so the shape and its four-leg skew test
  exist to copy. Verified at validation: `ContextPressureKind` (4 variants) and
  `BudgetBound` (5) are **still closed**, so BR-4 has real work left.

## Assumptions

- A-1: **The cost ledger's prices are the ceiling's arithmetic.** A currency
  cap (OQ-2) is only as correct as `prices.toml`, and a stale price enforces
  the wrong number *silently* — the exact failure class this REQ exists to
  close. This is why OQ-2 resolved to "refuse when unpriced" rather than
  best-effort: an absent price is detectable and refusable, whereas a *wrong*
  price is neither. The residual stands: a price that is present but stale
  still yields a wrong ceiling, and nothing here detects that. The post-switch
  price-page re-verify is the mitigation and is tracked separately.
- A-2: A refusal mid-prompt is acceptable to a user who set a ceiling. Setting
  one is an explicit act (OQ-3 keeps it opt-in), so the wall is a consequence
  they chose. This is the assumption that would be wrong if the cap were ever
  defaulted on — see OQ-3.
- A-3: Per-prompt is the unit a user reasons in (OQ-1). A prompt is what they
  initiate; a turn is an implementation detail of the loop, and a session is
  long enough that a ceiling on it would bind at an arbitrary moment days
  later.
- A-4: The egress choke point sees every spend. BR-1 depends on it: a route
  that reached a provider without passing it would be uncapped, and the same
  property REQ-562 relies on for redaction is what makes the ceiling total.
- A-5: The ledger is written before the refusal, so AC-1's "the cost ledger
  showing what was spent" is available at the moment of refusal. If the write
  happens after the forward point, the refusal cannot name the figure and BR-3
  needs a different source.

## Open Questions

_All four resolved at validation, 2026-08-22. OQ-1 and OQ-3 adopt their leans
(consistent with precedent, and closable by ADR). **OQ-2 and OQ-4 were product
decisions and were taken by the user**, not inferred — they determine what
happens to someone's money and what they see when it happens._

- [x] OQ-1: Per turn, per prompt, or per session? **Adopted: per prompt** — it is
      the unit the user initiates and the one the ≈25M figure is stated in.
- [x] OQ-2: Tokens or currency? **DECIDED: currency, and refuse when
      unpriced.** The ledger prices every call and a token ceiling means
      different money per provider. The addition over the original lean is the
      unpriced case: if the table cannot price a provider the cap is
      unenforceable, so the call is refused rather than waved through — a
      missing price must not become a missing ceiling (AC-6, A-1).
- [x] OQ-3: Default on or opt-in? **Adopted: opt-in**, matching `[privacy] redact`,
      with the REQ-586 notice pointing at it.
- [x] OQ-4: At the ceiling — refuse, or degrade to a cheaper tier?
      **DECIDED: refuse, naming the spend and the ceiling.** A silent tier
      downgrade answers with a model the user did not choose and was not told
      about — the shape BUG-156's failover pin and REQ-586's "nothing is
      clamped in silence" both reject. Offering the cheaper tier as an
      *accepted* recipe was considered and left out of scope: it needs a new
      consent surface, and refusing plainly is the smaller correct thing.

## Out of Scope

- Changing REQ-586's budget derivation or its bounds.
- Per-user or hosted billing of any kind (out of MVP scope entirely).

## Wrapup Checklist (BR-5, the half that is not a test)

TASK-238 pins the `teton_docs` sentence with a test, because a bundled page is
something the build can check. The release-notes half cannot be, and is
recorded here as an obligation rather than dressed up as one:

- [ ] The release note for the version carrying REQ-588 states that a fresh
      install has **no spend ceiling**, names `[cost] prompt_ceiling_usd`, and
      states the one-call overshoot. A user who reads "spend ceiling" in a
      changelog and infers a default cap has been told the opposite of the
      truth by omission — which is the same failure BR-5 exists to prevent, one
      surface over.

## Retrieved Context

Filed at REQ-586 wrapup from its own review findings; not produced by a
`/spec` retrieval pass. Run `/spec` retrieval before `/architect` if this REQ
is picked up, so it inherits the corpus the pipeline expects.
