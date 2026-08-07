---
id: REQ-561
title: "Wire the four unreached categories: triage, shell, title, compact"
status: draft
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

## Business Rules

- [ ] BR-1: All four categories are tagged **at their call sites**. No prompt text,
      tool name, or keyword list may assign one — REQ-558 BR-2's rule, which the
      type system already enforces (`JudgmentCategory` cannot name them).
- [ ] BR-2: **Every duty emits `route_decided`.** REQ-558 shipped `digest` routable
      but silent: the one genuinely new egress path it opened announced itself
      nowhere in the event stream. With four more duties that becomes five of
      eleven categories routable and unobservable. `digest` is retrofitted here.
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
      applies to every duty that reads a remote response.
- [ ] BR-9: `title` runs **once per session**, not per turn, and never re-derives an
      existing title. It is `reflex`-tier and therefore local (REQ-558: `reflex`
      inherits the local tier and never `default_provider`).
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
      provider returning an unbounded response yields a result no larger than the
      duty's declared ceiling. Asserted per duty, with a mock that deliberately
      overruns.
- [ ] AC-12: **Every duty is answerable off-script** (BR-10): a scripted-engine
      test asserts each of the four duties consumes **no** block, and that the turn
      sequence after a duty fires is unchanged. REQ-558 shipped this fix twice
      reactively — the classifier consumed a block and desynchronised two
      `cli_e2e` tests before anyone noticed, and `summarize_if_large` carried the
      same latent exposure. With four more duties it stops being a surprise and
      becomes a checklist item.
- [ ] AC-9: Mutation checks — for each duty, (a) removing the taint override and
      (b) making the failure path return its input unchanged each turn at least one
      test red. **A mutation that comes back green is reported as a finding**
      (LESSON-485).

## External Dependencies

- **REQ-558 must be merged.** It is (`2a2f47b`). The categories, tiers, resolver,
  config schema, and `policy show` surface all exist; this REQ adds call sites only.
- No new crates.

## Assumptions

- The four call sites are all inside `tetond`'s harness and tool layer, so no
  protocol or config change is needed. `route_decided` for duties (BR-2) is the one
  wire-visible addition, and its payload type already exists.
- `triage` and `shell` are lower-risk than `compact` because their fallbacks are the
  current behaviour verbatim; `compact` replaces a deterministic algorithm with a
  model call and is the one that warrants the most adversarial review.
- Ranking grep matches (`triage`) is useful at the scale the tool already caps at
  (200). If dogfooding shows the cap is the real constraint rather than the ordering,
  `triage` may deserve a different job than the one specced here.

## Open Questions

- [ ] OQ-1: Does `shell` interpretation run on **every** command result, or only on
      failure / above a size threshold? Every result is the simplest rule and the
      most expensive; failure-only is cheap but blind to a command that succeeded
      and did something surprising.
- [ ] OQ-2: Does `title` need a wire surface? A session title nobody can read is a
      cost with no benefit — but the client-side rendering may belong to REQ-560's
      status line rather than here.
- [ ] OQ-3: `compact` currently has no trigger of its own — `truncate_to_budget` is
      called when the budget is exceeded. Does the duty replace that call, or run
      earlier at a soft threshold so compaction is not always an emergency?
- [ ] OQ-4: Should `triage` and `compact` share the `scan` tier's binding, given
      REQ-558 made `scan` inherit the **local** tier by default? A user who binds
      `scan` remotely for cheap long-context summarisation also gets remote
      conversation compaction, which is a different privacy posture.

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
