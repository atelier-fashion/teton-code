---
id: REQ-561
title: "Wire the four unreached categories: triage, shell, title, compact"
status: approved
deployable: true
created: 2026-08-07
updated: 2026-08-07
component: "daemon/harness"
domain: "routing"
stack: ["rust", "daemon"]
concerns: ["routing", "cost", "developer-experience"]
tags: ["routing-categories", "call-sites", "duties", "summarization", "context-management"]
---

## Description

REQ-558 declared eleven routing categories and wired six. **Five ship as knobs that
do nothing**, rendered by `teton policy show` as `declared, no call site yet`. This
REQ wires four of them; `redact` is REQ-562, because a model call inside the egress
choke point needs its own spec and its own adversarial review.

| Category | Tier | Today | What it becomes |
|---|---|---|---|
| `triage` | scan | `GrepTool::run` returns the first 200 matches, unranked (`tools/grep.rs`) | rank/filter matches by relevance to the turn before they enter context |
| `shell` | build | `ShellTool::run` returns raw stdout+stderr capped at 8,000 chars (`tools/shell.rs`) | interpret command output — what failed, what it means |
| `title` | reflex | nothing | name a session (and, later, a branch) |
| `compact` | scan | `ContextManager::truncate_to_budget` drops oldest blocks deterministically | decide what to *forget* when the budget is exceeded |

`digest` (REQ-558 TASK-054) is the worked example: a duty that resolves its own
category, may run local or remote, goes through the egress choke point, and
degrades under LESSON-447 when routing fails.

**The honest scope note.** REQ-558's ADR-A justified declaring all eleven on the
grounds that each remaining call site would be "a cheap leaf". Its own Phase-5
review found that half true: the *call site* is cheap, the *plumbing* is not.
`DigestRoute`/`Digester` is ~260 lines of per-category machinery, and `compact` and
`triage` are the same shape. Generalising it is a one-caller refactor today and a
four-caller one after this REQ — so it belongs here, first.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| DutyRoute | category | Category | required; the duty's own category, tagged at the call site |
| DutyRoute | outcome | enum(Serves, Unresolved) | `Serves` carries a `Duty` impl and the resolved provider id |
| DutyRoute | reason | string | the resolver's sentence, verbatim (REQ-558 BR-6) |
| Session | title | Option\<string\> | **new**; set once by the `title` duty, never re-derived |
| CompactionOutcome | dropped_blocks | usize | what compaction removed, for the event surface |
| CompactionOutcome | degraded | bool | true when the duty failed and deterministic truncation ran instead |

No new config surface: all four categories, their tier bindings, and their override
rows already exist and already validate (REQ-558 TASK-049).

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| route_decided | **now also emitted for duties** | gains nothing; the duty emits the same payload the turn does, with its own category |
| cost_recorded | unchanged | already carries `category` (REQ-558) |
| session_titled | **new**; the `title` duty resolved a title (OQ-2, resolved) | `session_id`, `title`. Emitted once per session. New `Event` variant + payload in `teton-protocol` (no such type exists today). The daemon owns the value; this REQ ships it to the wire and no further — see BR-9a on why no renderer is promised. |

## Business Rules

- [ ] BR-1: All four categories are tagged **at their call sites**. No prompt text,
      tool name, or keyword list may assign one — REQ-558 BR-2's rule, which the
      type system already enforces (`JudgmentCategory` cannot name them).
- [ ] BR-2: **Every duty emits `route_decided` — when it performs, not when it
      resolves.** REQ-558 shipped `digest` routable but silent: the one genuinely
      new egress path it opened announced itself nowhere in the event stream. With
      four more duties that becomes five of eleven categories routable and
      unobservable. `digest` is retrofitted here. The timing is load-bearing:
      duties are resolved eagerly once per turn but usually never perform, so
      emitting at resolution would announce model calls that never happen — five
      spurious events per turn once all five duties are wired — and would observe
      a *resolution* rather than the egress this rule exists to make visible. See
      ADR-8, which records the three test failures that established this.
- [ ] BR-3: **Every duty's failure preserves the invariant its call site guards**
      (LESSON-447). `triage` falls back to today's first-200 cap; `shell` to today's
      8k truncation; `compact` to `truncate_to_budget`'s deterministic drop; `title`
      to no title. A routing failure must never become an invariant failure, and the
      degradation must be visible on the outcome, not only in a log.
- [ ] BR-4: **`compact` is the highest-risk duty and fails safe.** It decides what
      to forget, and a bad compaction silently corrupts every later turn. A failed,
      malformed, or over-budget compaction falls back to deterministic truncation —
      never to "keep everything" (which breaks the budget) and never to a partial
      application.
- [ ] BR-4a: **`compact` runs at a soft threshold; `truncate_to_budget` stays the
      hard backstop** (OQ-3, resolved). The duty fires at a soft fraction of the
      budget and `truncate_to_budget` still fires unconditionally at 100%. This is
      what makes BR-4 *structural* rather than a code path someone has to remember
      to take: the deterministic floor is not a fallback branch inside the duty, it
      is a separate gate the duty runs ahead of and cannot disable. A duty that
      hangs, returns garbage, or is never routed at all still ends with a context
      under budget, because the thing that enforces the budget was never the duty.
      Corollary: the duty runs with headroom instead of stalling a turn at the
      exact moment context is already full.
- [ ] BR-4b: **`shell` interprets on failure or on oversized output, not on every
      result** (OQ-1, resolved). The duty fires when the command exits non-zero
      **or** when its output would be truncated by the existing 8,000-char cap —
      the two cases where the raw bytes are either alarming or unreadable. A short,
      successful command is returned verbatim with no model call. `shell` is the
      highest-frequency tool call in a coding session, so "every result" is the one
      rule whose cost scales with session length rather than with incident count.
- [ ] BR-5: Session taint overrides every duty binding, on all four, as it does for
      the turn path and `digest` (REQ-558 BR-7). Verified by egress capture.
- [ ] BR-6: **The duty plumbing is generalised before the fourth caller lands.**
      `DigestRoute`/`Digester` becomes a shared `DutyRoute`/`Duty` seam that
      `digest`, `triage`, `shell`, and `compact` all use. One resolution path, one
      egress scoping rule, one LESSON-447 fallback shape.
- [ ] BR-7: **A duty's egress is scoped by the content it sends**, not by the turn's
      context — the rule `digest` established. `triage` sends match text, `shell`
      sends command output, `compact` sends conversation blocks; each is scoped by
      that content's own provenance, so a `local-only` source is refused while the
      rest of the turn proceeds.
- [ ] BR-8: **A duty's output is bounded by the harness, not by the provider.**
      REQ-558 capped the remote digest's accumulator for this reason; the same bound
      applies to every duty that reads a remote response. **Each duty declares an
      explicit ceiling as a named constant** — a bound that lives only in a
      reviewer's head is not a bound, and AC-11 has nothing to assert against
      without one. The ceiling is per-duty because the duties differ by an order of
      magnitude in what a legitimate response looks like: a `title` is a handful of
      words, a `compact` is a conversation.
- [ ] BR-9: `title` runs **once per session**, not per turn, and never re-derives an
      existing title. It is `reflex`-tier and therefore local (REQ-558: `reflex`
      inherits the local tier and never `default_provider`).
- [ ] BR-9a: **`title` reaches the wire in this REQ** (OQ-2, resolved) via
      `session_titled`. The reasoning: a title no consumer *can* observe is a model
      call bought for nothing, and no downstream renderer can ever show a value
      that was never sent. So this REQ ships the data and the event, and stops
      there. **It does not commit any other REQ to rendering it.** REQ-560's spec
      does not currently mention a session title and its AC-7 pins the status-line
      matrix to `(level × effort)`; its BR-8 input tuple ends in `…`, so adding a
      title later is *possible* there, but that is REQ-560's decision to make and
      not a dependency of this REQ. REQ-561 is complete and verifiable whether or
      not anything ever renders the title — AC-15 asserts the event on the wire,
      not a pixel.
- [ ] BR-11: **`policy show` states what content each category sends** (OQ-4,
      resolved). The `scan` tier carries both `triage` (grep match text — file
      content) and `compact` (conversation blocks), so a user who binds `scan`
      remotely for cheap long-context summarisation also moves conversation
      compaction off the machine. Re-splitting the tier→category bindings is Out of
      Scope (that is REQ-558's decision), so the mitigation is legibility, not
      re-binding: each category's row names the content class it transmits, making
      the remote binding an informed choice rather than a surprise. BR-7's
      per-content egress scoping remains the enforcement — a `local-only` source is
      refused whatever the binding says. Legibility is not a substitute for that
      enforcement, and this BR does not claim to be one.
- [ ] BR-10: The `ScriptedFileEngine` duty-recognition seam gains an arm per duty.
      REQ-558 established the pattern — one constant that both writes a duty's
      output contract and recognises it — because a duty that consumes a scripted
      block silently shifts every fixture's turn sequence by one.

## Acceptance Criteria

- [ ] AC-1: `teton policy show` renders **no** `declared, no call site yet` marker
      for `triage`, `shell`, `title`, or `compact`. The REQ-558 derived-marker test
      fails until the marker is updated, which is the intended prompt — and this AC
      is that prompt being answered.
- [ ] AC-2: Each of the four emits `route_decided` naming its category, tier,
      provider, and a non-empty reason. `digest` does too (BR-2's retrofit).
- [ ] AC-3: A table-driven test over all four duties × (resolves / unresolvable /
      provider error / tainted session) asserts the call site's invariant still
      holds on **every** failure path (BR-3). For `compact`, "holds" means the
      context is under budget afterwards.
- [ ] AC-4: Egress capture — with each duty bound to a remote provider and a
      `local-only` boundary configured, no boundary content appears in any captured
      payload, and each test proves the duty **would** have sent when the content is
      clean (the non-vacuity pairing REQ-558 TASK-054 established). The scoping rule
      is asserted too (BR-7): a duty whose *own* content is clean still sends while
      the turn's wider context contains boundary material, so the scope is
      demonstrably the content sent rather than the turn.
- [ ] AC-5: A tainted session runs all four duties on the local tier regardless of
      binding, asserted by captured bytes (BR-5).
- [ ] AC-6: `title` is requested exactly once for a multi-turn session, asserted by
      call count, and an existing title is never overwritten (BR-9).
- [ ] AC-7: `compact` under a forced failure leaves the context under budget, with
      the degradation reported on the outcome (BR-4). A test asserts the "keep
      everything" fallback is not taken.
- [ ] AC-8: One `DutyRoute`/`Duty` seam serves all four plus `digest`; a
      grep-level or type-level assertion pins that no per-category duty plumbing
      survives (BR-6).
- [ ] AC-10: **Each duty is tagged at its call site** (BR-1): a compile-level or
      grep-level assertion that no duty's category is produced from prompt text,
      tool name, or any string comparison. The type system already forbids the
      judgment path from naming these four; this pins that the duty path does not
      reintroduce it.
- [ ] AC-11: **A duty's output is bounded by the harness** (BR-8): a remote
      provider returning an unbounded response yields a result no larger than that
      duty's declared ceiling constant. Asserted per duty, with a mock that
      deliberately overruns. The assertion reads the declared constant rather than
      a literal, so raising a ceiling cannot silently un-test the bound.
- [ ] AC-12: **Every duty is answerable off-script** (BR-10): a scripted-engine
      test asserts each of the four duties consumes **no** block, and that the turn
      sequence after a duty fires is unchanged. REQ-558 shipped this fix twice
      reactively — the classifier consumed a block and desynchronised two
      `cli_e2e` tests before anyone noticed, and `summarize_if_large` carried the
      same latent exposure. With four more duties it stops being a surprise and
      becomes a checklist item.
- [ ] AC-13: **`shell` fires only on failure or oversize** (BR-4b): a table-driven
      test over (exit 0, small output), (exit 0, output **exactly at** the 8k cap),
      (exit 0, output over the cap), (exit≠0, small output), (exit≠0, large output)
      asserts the duty is invoked in exactly the last three and **not** in the
      first two, by call count. The zero-call cases are the load-bearing ones —
      they are the whole cost argument.
      **The boundary row is not decoration.** This AC originally listed four rows
      and claimed a mutation reading the post-truncation length would turn the
      *oversize* row red. It does not: truncation clamps the body to exactly the
      cap and then appends a notice, so a rendered oversize result is still over
      the cap and the size arm fires either way. Only the **exactly-at-the-cap**
      row discriminates. Verified by applying that mutation — it turns the
      boundary row red and leaves the oversize row green. The general shape: an
      off-by-truncation guard is caught by a boundary case, never by an extreme
      one.
- [ ] AC-14: **`compact`'s soft threshold does not weaken the hard backstop**
      (BR-4a): with the duty stubbed to never return, to return garbage, and to be
      entirely unrouted, the context is under budget after each — proving the
      budget is enforced by `truncate_to_budget` and not by the duty. A mutation
      that removes the unconditional 100% gate turns at least one of these red.
- [ ] AC-15: **`session_titled` reaches the wire once** (BR-9a): a multi-turn
      session emits exactly one `session_titled` carrying a non-empty title, and a
      session that already has a title emits none. Asserted on captured events, not
      on daemon-internal state.
- [ ] AC-16: **`policy show` names the content class per category** (BR-11): the
      rendered output states, for each of the eleven categories, what content that
      category transmits — and a test pins that `triage` and `compact` disclose
      distinct content classes despite sharing the `scan` tier. This AC asserts
      disclosure only; the enforcement assertion is AC-4. Declaring a content class
      for a still-unreached category (`redact`, REQ-562) **describes intent, not a
      call site** — it does not wire that category and does not intrude on
      REQ-562's scope. A category that transmits nothing today says so.
- [ ] AC-9: Mutation checks — for each duty, (a) removing the taint override and
      (b) making the failure path return its input unchanged each turn at least one
      test red. **A mutation that comes back green is reported as a finding**
      (LESSON-485).

## External Dependencies

- **REQ-558 must be merged.** It is (`2a2f47b`). The categories, tiers, resolver,
  config schema, and `policy show` surface all exist; this REQ adds call sites only.
- No new crates.

## Assumptions

- The four call sites are all inside `tetond`'s harness and tool layer, so **no
  config change is needed** — the schema is stable and this REQ must not migrate.
- **A protocol change *is* needed, for exactly one thing.** There are two
  wire-visible additions, not one. `route_decided` for duties (BR-2) reuses the
  existing `RouteDecided` payload, so it costs nothing new. `session_titled`
  (BR-9a) does **not** exist — verified: no `SessionTitled` variant or payload
  struct anywhere in `crates/`. It needs a new `Event` variant and payload in
  `teton-protocol`, whose no-`teton-core`-dependency rule the payload must
  respect (`session_id` + `title` are both plain strings, so it does).
- `triage` and `shell` are lower-risk than `compact` because their fallbacks are the
  current behaviour verbatim; `compact` replaces a deterministic algorithm with a
  model call and is the one that warrants the most adversarial review.
- Ranking grep matches (`triage`) is useful at the scale the tool already caps at
  (200). If dogfooding shows the cap is the real constraint rather than the ordering,
  `triage` may deserve a different job than the one specced here.

## Open Questions

- [x] OQ-1: **RESOLVED — failure or oversize.** `shell` interprets when the command
      exits non-zero **or** when its output would hit the 8k cap; a short successful
      command is returned verbatim with no model call. "Every result" was rejected
      on cost: `shell` is the highest-frequency tool call, so that rule's spend
      scales with session length rather than with incident count. "Failure only" was
      rejected as blind to the 40k-line successful build log that gets truncated to
      8k — the size arm is what covers it. See BR-4b, AC-13.
- [x] OQ-2: **RESOLVED — yes, in this REQ.** `title` reaches the wire via
      `session_titled`; REQ-560's status line renders it. Deferring the wire would
      have left BR-9 paying for a model call whose output no consumer could observe,
      and would have blocked REQ-560 on data that was never sent. See BR-9a, AC-15.
- [x] OQ-3: **RESOLVED — soft threshold, hard backstop retained.** The duty fires at
      a soft fraction of the budget; `truncate_to_budget` still fires
      unconditionally at 100%. This is the choice that makes BR-4 structural: the
      deterministic floor is a separate gate the duty runs ahead of, not a fallback
      branch inside it, so a duty that hangs or was never routed still ends under
      budget. See BR-4a, AC-14.
- [x] OQ-4: **RESOLVED — keep the binding, disclose the content class.**
      Re-splitting tier→category bindings is Out of Scope (REQ-558's decision), so
      the mitigation is legibility: `policy show` names what content each category
      transmits, making a remote `scan` binding an informed choice. Enforcement is
      unchanged and remains BR-7's per-content egress scoping — legibility is a
      disclosure, not a control. **Revisit trigger:** a user who binds `scan`
      remotely and is then surprised that conversation blocks left the machine.
      That report, not the abstract asymmetry, is what would reopen REQ-558's
      binding decision. See BR-11, AC-16.

## Out of Scope

- `redact` — REQ-562. It is a model call inside the egress choke point.
- Per-category reasoning effort (REQ-559) and permission levels (REQ-560).
- Any new configuration surface: the schema is stable and this REQ must not migrate.
- Changing the tier→category default bindings REQ-558 established.

## Retrieved Context

- REQ-558 (spec, merged `2a2f47b`) — the categories, the resolver, and `digest` as
  the worked example of a routed duty
- LESSON-447 — a best-effort step that guards an invariant must enforce it by
  degraded means on failure, not skip it (written about `summarize_if_large`, the
  very function `digest` now routes)
- LESSON-485 — a fixture that cannot reach the discriminating state is not a test
- LESSON-484 — enforce the rule where the decision is made
- BUG-156 — a privacy pin bypassed by a recovery path that re-derived its target
