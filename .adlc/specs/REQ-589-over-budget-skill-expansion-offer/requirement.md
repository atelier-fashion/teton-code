---
id: REQ-589
title: "Offer to proceed when a skill expansion exceeds the route's context budget"
status: approved
deployable: true
created: 2026-08-24
updated: 2026-08-24
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability", "routing", "security"]
tags: ["skills", "context-budget", "consent", "over-budget", "skill-expansion", "local-engine"]
---

## Description

A user typed `/analyze` and the turn died:

```
error: prompt failed: `/analyze` does not fit this route's context budget: the body
alone, with the system prompt, comes to about 4,097 words / 31 KB, and the budget is
4,096 words / 33 KB (bound: local engine). Nothing was sent and no provider saw this
turn — a skill expansion is carried whole or refused, never shortened into something
you did not invoke.
```

One word over 4,096. The byte half had ~1.7 KB to spare. Nothing the user could type
would change the outcome: there is no way to say "send it anyway", and no way to raise
the budget — `context_budget_cap` only ever *lowers* a budget, and only on remote
routes, because `derive` short-circuits to the fixed local pair before the cap is
consulted.

REQ-585 BR-8 made the refusal **honest** — an expansion is carried whole or refused,
never silently shortened — and that invariant is correct and stays. What it did not
give the user is an **exit**. This REQ adds one: the daemon asks instead of refusing,
tells the user what it expects to happen (including "this will blow your declared
window"), and offers to raise the limit so the question stops recurring.

The governing stance, settled with the product owner (see Decisions): **a prediction of
failure is a thing to say, not a reason to refuse to ask.** The daemon's job here is to
be the best-informed party in the room, not the one that decides for the user.

Two things this REQ is careful about, because both were nearly got wrong:

- **Proceeding is not shortening.** BR-8 forbids *middle-eliding* an expansion into a
  partial procedure. Carrying it whole while over the policy budget does not violate
  that invariant, and the 142 B truncation surcharge in `would_seed_fit` — which exists
  precisely to stop a candidate being admitted and then elided — is untouched.
- **The budget is a policy bound, not a physical one.** The route's *window* is the
  physical bound, and the two failures are not the same event: over-budget is this
  daemon's own policy refusing, while over-window is the provider refusing. The daemon
  knows which one it is looking at and must say so (BR-3) — but per D-1 it says it and
  still asks, rather than deciding on the user's behalf.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `OverBudgetOffer` | `skill` | string | the registered skill name; already validated `^[a-z0-9][a-z0-9_-]{0,63}$` |
| | `stage` | enum | `Body` \| `WithDynamicContext` — the existing `SkillStage` |
| | `measured` | (words, bytes) | the pair `would_seed_fit` produced; never re-measured for display |
| | `budget` | (words, bytes) | the pair the router stamped; never re-derived |
| | `bound` | `BudgetBound` | the stamped bound, verbatim |
| | `overrun_words` | int | `max(0, measured.words − budget.words)` |
| | `overrun_bytes` | int | `max(0, measured.bytes − budget.bytes)` |
| | `window_verdict` | enum | `FitsWindow` \| `ExceedsWindow` \| `WindowUnknown` (BR-3) |
| | `remedy` | `Remedy` | what a durable fix would be on this route (BR-7). **One representation only** — absence is `Remedy::NotOffered`, never a separate `Option`; two ways to say "no remedy" is LESSON-545's shape |
| `Remedy` | `kind` | enum | `DeclareWindow` \| `RaiseCap` \| `RaiseWindow` \| `BindTierRemote` \| `NotOffered` |
| | `provider_id` | string? | present only for `DeclareWindow` / `RaiseCap` / `RaiseWindow`, sanitized |
| | `tier` | string? | present only for `BindTierRemote` |
| `OfferAnswer` | `proceed` | bool | send this turn's expansion whole — per-invocation, never persisted (BR-10) |
| | `apply_remedy` | bool | write the going-forward remedy through `config/set` (BR-7, BR-8) |

The two answers are **independent**: proceed-only, remedy-only (fix it but don't send
this turn), both, or neither. A remedy-only answer is legitimate and must be supported —
it is the correct choice for a user who wants the limit fixed but does not want this
particular oversized turn to run.

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `skill_over_budget_offered` | the offer is put to a human | skill, stage, measured, budget, bound, window_verdict, remedy.kind |
| `skill_over_budget_accepted` | the human said proceed | skill, stage, measured, budget, window_verdict |
| `skill_over_budget_remedy_applied` | the human took the going-forward remedy | remedy.kind, provider_id, old value, new value |
| `SkillInvoked::refused` (reason `over_budget`) | declined, unanswerable, or never offered | today's payload, unchanged |

`SkillInvoked::refused` keeps `OVER_BUDGET_REASON = "over_budget"` as the single reason
token for every not-sent outcome. The *record* distinguishes declined from unanswerable
via the offer events above, not by minting a second refusal token (this is REQ-585
AC-9's "nobody was asked and nobody decided" distinction, kept where it already lives).

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| answer an over-budget offer | the connection that submitted the turn, and only it (the REQ-587 ADR-3 addressee) |
| write a durable window/cap remedy | whoever `config/set` already admits — no new authority is minted (BR-8) |

## Business Rules

- [ ] **BR-1: Proceeding carries the expansion whole.** Accepting an offer sends the
  same bytes `skill_fit` measured, unshortened. No path introduced by this REQ may
  middle-elide, truncate, or summarize an expansion, and the `truncated = true`
  surcharge in `would_seed_fit` is not relaxed to widen the offer band. REQ-585 BR-8
  is preserved in full, not amended. (informed by REQ-585)

- [ ] **BR-2: The offer is the user's alone.** A model-invoked expansion measured by
  `skill_append_fit` (`SkillCaller::Model`) continues to refuse with today's sentence
  and is never offered a choice. There is no human in a mid-loop tool call to answer
  per-invocation, and the Model arm's existing instruction — say what you tried to run
  and that it did not fit — remains the whole of its behavior. (informed by REQ-587)

- [ ] **BR-3: The offer is always made; the *sentence* changes with the window
  verdict.** The daemon never withholds the choice on the grounds that it expects
  failure — it says what it expects and lets the human decide. The verdict selects which
  true thing is said:
  - `ExceedsWindow` — the route declares a window and the expansion exceeds it. The
    offer **states plainly that this will blow the declared window and that proceeding
    without raising it will very likely be rejected by the provider**, and — *where BR-7
    gives this bound a remedy* — leads with that remedy rather than the one-time
    override. Both choices remain available; the user is informed, not overruled.
    **Where the bound has no remedy** (`RedactScan`, BR-7b), the offer still states the
    window consequence and presents the one-time override alone; it must not imply a
    durable fix exists. This cell is reachable and is not an oversight — see the
    reachability table below.
  - `FitsWindow` — over budget but inside the declared window: offer. **Superseded in part
    by ADR-15/ADR-17:** "expected to serve" is too strong on `Window`/`UserCap`, where that
    band IS the generation reservation — the sentence says the prompt fits the declared
    window but may leave the reply little room. On `RedactScan` the band is the egress byte
    clamp, and the sentence claims neither fact.
  - `WindowUnknown` — no window fact exists (the local tier, or a remote provider with
    `max_context = 0`): offer, stating that the daemon **cannot promise** the send will
    fit, with the typed `context_length_exceeded` outcome as the backstop.
    **Correction (architecture phase):** that outcome is produced today only on the
    *remote* path (`completion.rs:535`/`:1259`); the local engine's failure becomes
    `HarnessError::Engine` → `INTERNAL_ERROR`. The backstop this rule relies on must
    therefore be **built** for the local tier, not assumed — see architecture ADR-3. It
    is the head of the task DAG, because BR-14.1's trigger depends on it and the
    alternative (string-matching the engine's sentence) is LESSON-528's shape.

  There is no overrun ceiling. A prediction of failure is a thing to say, not a reason
  to refuse to ask.

  **Reachability — the verdict and the bound are not independent axes.** A window
  verdict exists only where a window was declared, so most of the 5 × 3 cross product is
  unreachable. Only these pairs occur, and a test written for any other cell would pass
  vacuously (LESSON-520):

  | Bound | Reachable verdicts |
  |---|---|
  | `LocalEngine` | `WindowUnknown` only |
  | `DefaultUnknown` | `WindowUnknown` only |
  | `Window` | `FitsWindow`, `ExceedsWindow` |
  | `UserCap` | `FitsWindow`, `ExceedsWindow` |
  | `RedactScan` | `FitsWindow`, `ExceedsWindow` |

- [ ] **BR-4: A declined or unanswerable offer is exactly today's refusal.** Where the
  offer cannot be put to a human — no terminal, non-interactive client, `Unanswerable`,
  or `Refused(RefusalReason)` — the turn refuses with the existing sentence under
  `SKILL_EXPANSION_TOO_LARGE` (`-32023`). **Silence is never consent.** The absence of
  an answer must never resolve to proceed, on any path, including timeout. (informed by
  LESSON-524)

- [ ] **BR-5: The accepted path does not reuse the refusal sentence.** The refusal's
  clause *"Nothing was sent and no provider saw this turn"* becomes false the moment a
  user proceeds, and that clause is what distinguishes `-32023` from `-32022`. The offer
  question, the decline refusal, and the acceptance record are distinct sentences
  composed in the **one existing composer** (`skill_refusal`'s module) by adding arms —
  not by forking a second composer. Only integers this daemon measured, two literal key
  names, the skill's name, and a sanitized provider id may reach any of them; no
  provider response body is an input, because none is in scope. (informed by REQ-586)

- [ ] **BR-6: A project skill's trust question comes first — and must first exist.**
  *(Cross-REQ rule references in this document are always qualified with their REQ —
  this document has its own BR-4 and BR-10 meaning different things.)*

  **Correction (architecture phase).** This rule was drafted believing Stage A ran
  before **REQ-585's** BR-4 acknowledgment on the typed path. It does not: the only
  production caller of `authorize_project_skill_trust` is `harness/tools/skill.rs:1572`,
  the **model-invoked** tool. The user-typed path's `accept_invocation`
  (`runtime.rs:2904`) is a synchronous `fn` whose only check is name validity — **there
  is no trust gate on the typed path at all.** A user who types `/name` today runs a
  project-authored skill body with no acknowledgment.

  **D-10 resolves this by building the gate** rather than dropping the rule: the
  acknowledgment is introduced on the typed path as part of this REQ, `accept_invocation`
  becomes `async`, and the gate call is inserted before Stage A. This is new
  functionality, not the reordering the original text described, and it is a deliberate
  scope increase the product owner accepted with that stated. See ADR-10. For a **project-sourced**
  skill that ordering must invert: the user is asked whether they trust the repository
  *before* they are asked whether to send an oversized body from it. Asking the budget
  question first would have a user authorize an over-budget send of bytes from a
  repository they have not yet said they trust — a file on disk would be choosing when
  it gets a consent prompt. For a **user-authored** skill the current order stands.
  Where trust is declined, the trust refusal wins and no budget offer is made.

- [ ] **BR-7: Every offer that has a durable remedy offers it, as a *going-forward*
  choice beside the one-time override.** The two questions are asked together — "send it
  this time?" and "raise the limit so this stops happening?" — because a user who has to
  answer the same question on every invocation has not been given a fix. Keyed off the
  stamped `BudgetBound`:

  | Bound | Remedy offered | Why |
  |---|---|---|
  | `DefaultUnknown` | `DeclareWindow` — set `capabilities.max_context` for the named provider (value per BR-7c) | the window is undeclared; declaring it is the real fix |
  | `UserCap` | `RaiseCap` — raise or clear `context_budget_cap` | the user set this ceiling and may raise it |
  | `Window` | `RaiseWindow` — raise `capabilities.max_context` (value per BR-7c) | **the user's declaration is the user's to correct.** The daemon cannot distinguish an accurate declaration from a conservative one, so it states the risk (BR-7a) and asks rather than deciding |
  | `LocalEngine` | `BindTierRemote` (see BR-9) | the local *pair* has no lever (that is REQ-590), but the route does: bind the tier to a remote provider and declare its window |
  | `RedactScan` | **none** (see BR-7b) | the byte clamp is an egress-privacy guarantee |

- [ ] **BR-7c: A proposed window value is looked up, never invented (D-5).** Where the
  provider matches a shipped vendor recipe, the proposed `max_context` is that recipe's
  value — `provider_recipes.rs` already carries one per vendor, read off the vendor's own
  documentation and dated in a comment beside it (`kimi` = 1,000,000, verified
  2026-08-19). Where the provider matches **no** recipe, the offer asks for the value and
  proposes nothing. A silently invented window would be the same defect class as the
  silently defaulted one REQ-586 BR-3 exists to name, and it would be worse here, because
  this one gets written to disk.

- [ ] **BR-7a: A `RaiseWindow` offer states what it risks.** Raising `max_context` above
  the provider's *real* window does not enlarge that window — it makes the daemon send
  requests the provider will reject, converting a local refusal into a remote error. The
  offer must say so in the same breath as it is made. This is an informed choice, not a
  silent knob.

- [ ] **BR-7b: The `RedactScan` clamp is not offered as a remedy.** Unlike a window or a
  cap, that byte ceiling is what bounds the egress scanner's reach; raising it to fit a
  skill would trade a privacy guarantee for a convenience, and the user asking to send a
  skill is not the same as the user asking to weaken redaction. A user who wants that
  can change the `[web]`/redaction posture deliberately, on a surface that is about
  redaction. *(This is the one bound left remedy-less after D-2 opened the
  `Window` arm. It was raised for the product owner explicitly and confirmed as-is by
  D-6 — left so on privacy grounds, not by omission.)*

- [ ] **BR-8: The durable remedy writes through `config/set` and mints no new
  authority.** It inherits that method's existing posture exactly, including the known
  one: `config/set` is a **REQ-570** BR-10(b) daemon-wide commitment, the `presence` feature is
  non-default, and on a shipped build `refuse_unattested_commitment` degrades to allow
  with a stderr line. The offer must not claim an attestation the running build does not
  perform. (informed by REQ-576, LESSON-519)

- [ ] **BR-9: A local-engine route offers the two-part remedy, both halves or neither.**
  There is no config lever for the local *pair*, so the sentence must not imply one.
  What the route does have is a binding: the remedy is to bind the tier to a remote
  provider **and** declare that provider's window. Either half alone leaves the user
  exactly where they started — a provider with `max_context = 0` derives the *same*
  default pair under `bound: unknown window` — so a remedy that offered only the tier
  binding would send the user in a circle, which is exactly the circle the reported
  `/analyze` failure was already sitting in.

  **The daemon performs both writes (D-9), gated behind the cost sentence.** Naming a
  two-command remedy and leaving the user to run it is precisely what produced the
  circle the reported failure was sitting in, so the remedy is applied, not recited.

  **This remedy moves a whole category's work to a paid provider, and must say so.**
  Rebinding `think` is not a context-budget setting; it changes where every design,
  debug and review turn is served and what it costs. The offer names the tier, the
  provider, and the cost consequence in the same sentence, and BR-8's `config/set`
  posture applies to both writes. This is the one remedy whose blast radius exceeds the
  problem it solves, and the user is told that before answering.

- [ ] **BR-10: "Going forward" raises the budget; it never remembers a consent.** The
  one-time override is per-invocation and is not persisted: the measurement is
  route-specific, and a remembered "yes" would be applied to a different route's budget
  on a later turn — a grant recorded against a question that was never asked. The
  durable half of the offer (BR-7) works the honest way instead, by changing the
  *derivation*: once the window or cap is raised, later turns simply fit and no prompt
  is reached. That is why there is no "don't ask me again" for the override itself —
  the fix for being asked repeatedly is the remedy, not a stored bypass. The consent is
  asked under a `skill:` permission key so it cannot be smuggled through another key
  family. (informed by BUG-161, LESSON-501)

- [ ] **BR-11: A refused turn still reaches no provider and spends no model call.** The
  property Stage A exists to hold is unchanged on every not-sent path: no dispatch, no
  `context_pressure`, no health change, no degradation, no retry, and the session-naming
  duty stays deferred below the gate so a refused turn never spends it.

- [ ] **BR-12: An accepted over-budget turn does not drop history to make room.** The
  top-of-loop gate's ordinary answer to pressure is to shed older turns (REQ-567 BR-4).
  On the turn a user knowingly accepted over budget, that behavior is **suspended**:
  the user consented to sending an oversized expansion, not to losing their
  conversation, and silently deleting history to accommodate the first consent would be
  a second loss they were never asked about. The turn runs with history intact; if the
  assembled context then does not fit at the engine or provider, it fails with the typed
  `context_length_exceeded` outcome. **A visible, recoverable error is the correct
  outcome here, and is strictly preferable to a turn that silently succeeds by
  discarding the conversation that gave it meaning.**

  The suspension is scoped to that turn. Ordinary pressure handling resumes afterwards,
  which means a later turn may drop the expansion like any other block — that is
  REQ-567 doing its job on a turn nobody made promises about, and it must be stated in
  the offer's aftermath rather than discovered.

- [ ] **BR-13: The refusal is reachable before it happens.** A user should be able to
  learn that a skill will not fit *without* typing it and being refused. `/doctor`
  reports, for the current route, which registered skills exceed the budget — with the
  same figures, the same bound, and the same remedy sentence the offer would carry —
  and `/verbose` shows the route's budget and bound beside that count. The figures come
  from the same measurement and the same stamped budget as the live path; this surface
  derives nothing of its own (LESSON-456's one-classifier rule).

  **Stated limitation:** only the `Body` stage can be pre-measured. A skill whose
  dynamic-context output pushes it over (`WithDynamicContext`) cannot be known before it
  runs, so the pre-flight answer is a floor — "these definitely will not fit" — never a
  guarantee that the rest will. The surface must say which claim it is making.

- [ ] **BR-14: An approval must not turn the session into a dead end (D-8).** The
  product owner's rule is that a user who approved once does not keep hitting the same
  wall. Two mechanisms, both using seams that already exist:

  1. **Withdraw on context failure.** When an accepted over-budget turn fails with
     `context_length_exceeded`, the expansion is withdrawn from the session context via
     `ContextManager::withdraw_block` and replaced with the refusal, exactly as BUG-188
     does for a mid-loop reroute. The turn is over, but the *session* returns to
     something usable instead of carrying an oversized block that re-fails every
     subsequent turn. The withdrawn block's provenance is absorbed into
     `DroppedProvenance` — that function already does this, and it is what stops a
     `local-only` source leaking through a block that is no longer visible (BUG-188).
  2. **Remember the observed failure, and say it next time.** The session records that
     *this skill on this route* was actually rejected at the window. The next offer for
     the same pair leads with that fact — "this was rejected by the provider last time"
     — and with the remedy. The offer is still made (D-1: always ask), but the user is
     no longer choosing blind.

  **This is not a remembered consent, and BR-10 still holds.** What is recorded is a
  *measurement the daemon observed* — the provider rejected this — not an authorization
  the user granted. The distinction matters: a stored consent would let a later turn
  send something nobody approved, whereas a stored observation can only ever make the
  next question better informed. Recording it must not shorten, skip, or pre-answer the
  next offer.

## Acceptance Criteria

- [ ] AC-1: `/analyze` on the reported route (measured 4,097 words / 31 KB against
  4,096 words / 33 KB, `bound: local engine`) presents an offer rather than a bare
  refusal, and accepting it **dispatches** the expansion whole — byte-for-byte what
  `skill_fit` measured. Whether the turn then *completes* is BR-12's and AC-15's
  question, not this one; asserting success here would make the criterion untestable on
  the very route that motivated the REQ.
- [ ] AC-2: The offer sentence quotes the same figures the measurement produced — no
  second estimator, no re-derived budget — and names the bound verbatim.
- [ ] AC-3: Declining produces byte-identical output to today's refusal, under the same
  `-32023` code.
- [ ] AC-4: A non-interactive client (no terminal / `Unanswerable`) receives today's
  refusal. A test asserts that no code path maps an unanswered offer to proceed —
  deleting the fallback must redden the suite.
- [ ] AC-5: A model-invoked over-budget `skill` call is refused with the Model arm's
  sentence and is never offered a choice (BR-2).
- [ ] AC-6: All three window verdicts produce an offer (BR-3) — none refuses outright.
  The `ExceedsWindow` sentence states that the declared window will be blown and that
  proceeding without raising it will very likely be rejected; the `WindowUnknown`
  sentence states the daemon cannot promise the send will fit; the `FitsWindow` sentence
  claims neither. Each arm pins its own wording.
- [ ] AC-7: Remedy offers follow BR-7's table exactly — each of the five bounds has a
  test. `Window` asserts a remedy **is** offered (the arm opened by D-2); `LocalEngine`
  asserts the `BindTierRemote` pair (BR-9, AC-8); `RedactScan` is the **only** bound
  asserting that no durable write is offered (BR-7b).
- [ ] AC-7a: A `RaiseWindow` offer carries BR-7a's risk sentence. A test asserts the
  offer cannot be rendered without it.
- [ ] AC-7b: `proceed` and `apply_remedy` are independently honored: all four
  combinations are tested, including **remedy-only** (the limit is raised on disk and
  the oversized turn still does not run) and **proceed-only** (the turn runs and the
  config is byte-identical afterwards).
- [ ] AC-8: On a `LocalEngine` route the remedy names both halves — bind the tier *and*
  declare the window — and carries the cost consequence of rebinding the tier (BR-9).
  Tests assert: the sentence never offers a `capabilities.max_context` write *for the
  local tier itself*; the two writes are applied together or not at all (a half-applied
  remedy that leaves `max_context = 0` reproduces the original circle and must be
  impossible); and the cost sentence cannot be omitted.
  **Correction (architecture phase):** "applied together or not at all" is not
  achievable through `config/set`, which persists one update per call and which
  `architecture.md:169-172` explicitly forbids generalizing. ADR-5 satisfies this
  criterion's *intent* instead by **ordering the writes so the forbidden state is
  unreachable** — `max_context` first, tier binding second. A partial failure then
  leaves a declared window on an unbound tier, which is harmless; only the reverse
  order can produce the circle.
- [ ] AC-9: A project-sourced over-budget skill asks the trust question before the
  budget question; a user-authored one does not (BR-6). Declining trust yields the trust
  refusal, not a budget sentence.
- [ ] AC-10: Accepting twice in one session prompts twice — no grant is persisted
  (BR-10). A test asserts no `skill:` grant survives the invocation.
- [ ] AC-11: The accepted path never emits the "no provider saw this turn" clause
  (BR-5), asserted negatively as `a_skill_refusal_carries_no_provider_response_body`
  already does for its own invariant.
- [ ] AC-12: Every new wire fact is driven **end to end from a real turn**, not from a
  struct literal: the producer of each new event field is exercised, and mutating the
  producer line reddens the suite. (informed by LESSON-544, LESSON-552)
- [ ] AC-13: The durable remedy path is verified by **reading the config file on disk**
  after the write, not by inspecting a return code, and is paired with a refusal-path
  test on the same fixture proving nothing was written. (informed by LESSON-519,
  LESSON-520)
- [ ] AC-14: Where the offer prompt is rendered to a terminal, a PTY leg pins the actual
  bytes — the question, the figures, and the remedy line. (informed by BUG-191)
- [ ] AC-15: Dogfood runbook: reproduce the original `/analyze` failure on a
  local-engine route, accept the offer, and record whether the turn completes or hits
  `context_length_exceeded`. **This is the first real data point REQ-590 needs**, and
  the runbook must record the measured pair, the verdict, and the outcome — not just
  pass/fail.
- [ ] AC-16: On an accepted over-budget turn, **no history block is dropped** (BR-12).
  Tested with a session carrying enough history to trigger the gate: the block list
  before and after is compared, and a turn that cannot fit surfaces
  `context_length_exceeded` rather than a silently shortened conversation. Deleting the
  suspension must redden this test.
- [ ] AC-21: A proposed window value matches the vendor recipe for a recognized
  provider, and **no value is proposed at all** for an unrecognized one (BR-7c). A test
  asserts the offer cannot render a number the recipe table does not contain.
- [ ] AC-22: An accepted turn that fails with `context_length_exceeded` withdraws the
  expansion from session context, and the **next** turn in that session assembles
  without it (BR-14.1). A test drives a real second turn rather than inspecting the
  block list alone, and asserts the withdrawn block's provenance was absorbed into
  `DroppedProvenance` — a `local-only` source must not survive the withdrawal (BUG-188).
- [ ] AC-23: After an observed window rejection, the next offer for the **same skill on
  the same route** names that prior rejection and leads with the remedy (BR-14.2). Two
  negative assertions guard the BR-10 boundary: the recorded observation must not
  suppress the offer, and it must not pre-answer it.
- [ ] AC-24: The `BindTierRemote` remedy is **applied**, not recited (BR-9, D-9): after
  acceptance, the config on disk carries both the tier binding and the declared window,
  and a subsequent identical invocation reaches no offer at all because the route now
  fits. This is the end-to-end proof that the reported `/analyze` circle is closed.
- [ ] AC-18: **BR-11's not-sent invariant is tested, not assumed.** On every not-sent
  path (declined, unanswerable, never offered, trust-declined) an egress-capture test
  asserts no provider was reached, no `context_pressure` was emitted, no health change
  or degradation occurred, and the session-naming duty was not spent. This is the
  invariant that makes the refusal `-32023` rather than `-32022`; without a test it is a
  comment. (informed by BUG-188)
- [ ] AC-19: `/verbose` shows the route's budget and bound beside the count of skills
  that would not fit (BR-13) — the `/doctor` half is AC-17, and this is the half that
  would otherwise ship untested.
- [ ] AC-20: The offer's attestation wording matches what the **running build** actually
  performs (BR-8): on a build without the `presence` feature the sentence must not claim
  a verified human. Tested on both feature configurations, using the existing
  `TETON_PRESENCE_ACCEPT=fail` seam rather than a second mechanism. (informed by
  REQ-576, LESSON-519)
- [ ] AC-17: `/doctor` names the skills that would not fit on the current route, with
  figures and bound matching the live path exactly (BR-13), and labels the answer as a
  floor — `Body`-stage only, dynamic-context skills not pre-measurable. A test asserts
  the pre-flight figures equal the figures the live refusal produces for the same skill
  on the same route (one classifier, not two).

## External Dependencies

- None. Every seam this REQ needs already exists: `PermissionGate::authorize_skill` for
  the question, `config/set` for the durable write, `skill_fit`/`skill_append_fit` for
  the measurement, and the typed `context_length_exceeded` outcome for the backstop.

## Assumptions

- [ ] ASSUME-A: The word half of the local pair has genuine headroom — 4,096 words at
  the 3/2 safety ratio claims ≈6,144 provider tokens against a 16,384-token engine
  window — so a small word-half overrun is very likely to serve. This is the assumption
  AC-15 exists to test, and it is *not* symmetric: the byte half is the whole window
  (32,768 B = 16,384 × 2 B/token) with **no** generation reservation subtracted, unlike
  every remote pair. A byte-half overrun is materially more dangerous than a word-half
  one. (informed by REQ-586 OQ-3)
- [ ] ASSUME-B: `PermissionGate::authorize_skill`'s existing arms (`Declined`,
  `Unanswerable`, `Refused`) are sufficient to express this question without widening
  the consent vocabulary.
- [ ] ASSUME-C: The shipped build has no presence mechanism, so BR-8's durable write is
  gated by connection standing alone (**REQ-570** BR-10(a)). This is the status quo for
  `/provider setup` and `web/setup_commit`, not a regression introduced here.

## Decisions (product owner, 2026-08-24)

Four questions were put to the product owner during drafting and answered. They are
recorded here rather than left as open, and the business rules above already reflect
them.

- **D-1 — no overrun ceiling; say what you expect and ask anyway.** The offer is always
  made. Where the daemon expects the send to blow the declared window it *says so* and
  asks for approval to expand the limit, rather than withholding the choice.
  → BR-3, BR-7. *(Reverses the draft's lean, which suppressed the offer on
  `ExceedsWindow`.)*
- **D-2 — `Window` is a remediable bound.** The draft left it remedy-less on the grounds
  that raising `max_context` past a real window lies to the provider; overruled. The
  user's declaration is the user's to correct, so the daemon states the risk and offers
  the raise. → BR-7, BR-7a.
- **D-3 — an accepted over-budget turn does not drop history.** The draft's lean was to
  let the gate shed older turns; overruled. Consent to send an oversized expansion is
  not consent to lose the conversation, so the turn fails visibly instead. → BR-12.
- **D-4 — the refusal is reachable before it happens.** Pre-flight surfacing is in
  scope, not a follow-up. → BR-13.

A second round of five questions was answered on the same day, after the first draft
was validated. **D-8 reversed the draft's lean; the other four confirmed it.**

- **D-5 — a proposed window value is looked up, never invented.** Vendor recipe where
  the provider is recognized; ask where it is not. → BR-7c.
- **D-6 — `RedactScan` stays remedy-less.** Left as drafted: that byte ceiling is a
  privacy guarantee, and wanting to run a large skill is not wanting weaker redaction.
  → BR-7b unchanged.
- **D-7 — ordinary pressure resumes after the accepted turn.** BR-12's suspension is
  scoped to the turn that was consented to, and the aftermath says so. → BR-12.
- **D-8 — an approval must not leave the user hitting the same wall.** *This reversed
  the draft's lean, which was to note the dead-end risk and defer it.* An approved turn
  that fails at the window withdraws its expansion from session context, and the next
  offer for that skill on that route names the rejection it already observed. → BR-14.
- **D-9 — the daemon performs the `BindTierRemote` remedy.** Reciting a two-command fix
  is what produced the reported circle. Applied, gated behind the cost sentence. → BR-9.

**Third round — decisions taken during the architecture phase, 2026-08-24.**

- **D-10 — build the project-skill trust gate on the typed path.** Exploration proved
  BR-6's premise false: no trust gate exists on the user-typed `/name` path, so the rule
  as drafted was a no-op. The product owner chose to **build** the gate rather than drop
  the rule and file the gap. This is an accepted scope increase: `accept_invocation`
  becomes `async` and its signature change reaches every caller. → BR-6, ADR-10.
- **D-11 — the local tier's typed context outcome is built, not assumed.** → BR-3
  correction, ADR-3.
- **D-12 — ordering replaces atomicity for the two-write remedy.** → AC-8 correction,
  ADR-5.

**Fourth round — product owner, 2026-08-24, mid-implementation.**

- **D-13 — give the trust gate an unattended path.** D-10's gate blocked piped/unattended
  sessions from running any typed project skill. The owner chose to preserve automation over
  accepting the refusal. → TASK-262, built on `[web] permission_allow`'s precedent: a human
  still decides, durably and out of band; the unattended path only *consults* that decision.
- **D-14 — the two remaining decisions go to Phase 5 review first.** The remedy's daemon-wide
  gate gap (ADR-18 item 3) and BR-14.1's unobservable withdrawal are left for the review agents
  to reach independently before the owner rules, rather than being settled on the
  orchestrator's summary.

## Open Questions

Every question from the first two rounds is now decided (D-1 … D-9). Answering them
opened two new ones, both narrow and both created by D-9's decision to *perform* the
tier rebind rather than recite it.

- [x] OQ-1 *(decided in architecture — ADR-12: ask when two or more, propose by name
  when exactly one; a provider-enumeration helper is new code)*: **Which provider does
  `BindTierRemote` bind to when more than one is configured?** The reported machine has exactly one remote provider, which hides the
  question. With two or more healthy providers the daemon would be choosing where a
  whole category's spend goes. *Lean:* **ask, and never pick silently** — D-9 authorized
  performing the remedy, not choosing the vendor. A single configured provider may be
  proposed by name; two or more must be presented as a choice.
- [x] OQ-2 *(decided in architecture — ADR-7: `ProviderRecipe` gains a `verified_on`
  field, promoting the comment-only date to data; the write records it)*: **What happens
  when a vendor recipe's window goes stale?** BR-7c writes a
  recipe value to disk, and recipes carry a verification date precisely because vendors
  move (REQ-577 already documents this decay). A stale recipe means a wrong
  `max_context` persisted into the user's config, where it outlives the recipe that
  produced it. *Lean:* record the recipe's verification date in the same write — as a
  comment or an adjacent field — so a later `/doctor` can tell a declared window that
  was measured from one that was inherited. Do not block the write on freshness.

## Out of Scope

- **Raising the local tier's context budget.** This is REQ-586's recorded OQ-3
  ("Derive the local budget from the engine's `n_ctx`?") and is deliberately deferred.
  It needs the engine window plumbed up from the loader — `EngineLoadReport` carries
  only `benchmark` and `duty` today, so the router cannot read `n_ctx` at all — plus a
  measured decision on prefix-cache and prompt-processing cost. **Filed as a follow-up
  REQ against REQ-586 OQ-3; this REQ must not pre-empt it.**
- Shortening, eliding, chunking, or summarizing an oversized expansion (BR-1 forbids it).
- Any change to how typed non-skill prompts handle pressure — those still elide loudly
  (REQ-586 OQ-4), and this REQ does not revisit that.
- Weakening the `RedactScan` byte clamp (BR-7b; confirmed by D-6).
- Changing `context_budget_cap` **semantics**. A `RaiseCap` remedy writes a larger
  number into the existing field; the rule that `derive` takes `min(cap, window)` is
  untouched, so a cap raised above the window still has no effect. Raising the value is
  not the same as changing what the value means.
- Offering the choice to model-invoked calls (BR-2).
- Deciding what `n_ctx` the local tier should derive from — REQ-590.

## Retrieved Context

- LESSON-518 (lesson, score 11): Reader-loop freedom needs a parked verifier
- LESSON-519 (lesson, score 11): "Inspect, not infer" needs the real artifact
- LESSON-520 (lesson, score 11): A gate before parse makes an invalid-payload test vacuous
- LESSON-551 (lesson, score 9): The instrument is the defect
- LESSON-552 (lesson, score 9): Test the derivation, not the minter
- BUG-184 (bug, score 9): Skill discovery runs on the connection's reader loop
- BUG-188 (bug, score 9): A reroute ends the turn where a relayable refusal was promised
- BUG-189 (bug, score 9): A refusal that names no registered skill is silent on the surface
- BUG-191 (bug, score 9): No PTY leg for the acknowledgment prompt bytes
- LESSON-539 (lesson, score 9): Claim first, then re-read session state
- LESSON-524 (lesson, score 9): Exposure is not callability
- BUG-161 (bug, score 9): Permission request ids collide across concurrent sessions
- LESSON-501 (lesson, score 9): Carried state sheds its invariants silently
- LESSON-544 (lesson, score 8): A hand-built wire value leaves its producer unguarded
- LESSON-545 (lesson, score 8): Splitting one decision into two fields repoints its callers
