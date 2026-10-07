---
id: REQ-623
title: "Subagent dispatch — an `agent` tool runs bounded child turn-loops and hands their results back to the parent turn"
status: approved
deployable: true
created: 2026-10-05
updated: 2026-10-07
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "llm-providers"]
concerns: ["cost", "security", "privacy", "reliability"]
tags: ["subagent", "agent-tool", "child-turn-loop", "dispatch", "fan-out", "skills", "proceed", "sprint", "analyze"]
---

## Description

Today a Teton Code turn is one model loop with one context. The model can
read, edit, grep, glob, run the shell, look up docs, and — since REQ-587 —
expand a registered skill into its own turn. What it cannot do is **hand a
task to a second loop and get a result back**. Every ADLC skill that does
real work assumes it can: `/proceed` Phase 4–5 dispatches implementers and
reviewers, `/sprint` dispatches `pipeline-runner`s, `/analyze` and `/review`
fan out four or five auditors and consolidate their reports. REQ-587 scoped
this out explicitly and recorded the consequence in its AC-15: `/proceed`
stalls at the first "dispatch an agent" step. The 2026-10-05 `/analyze` run
that prompted this spec did the same — the orchestrator ran four audit
dimensions itself, sequentially, in one context, because there was nothing
to hand them to.

This REQ adds an **`agent` tool**: a model-callable tool that runs one or more
**child turn-loops** on the model's behalf and returns each child's final
report to the parent as a typed tool result. A child is a real turn — the
same `run_session_turn_with_pressure_policy` loop, the same tool registry
(minus `agent` itself), the same permission level, the same session root and
privacy boundaries, the same single egress point — started with a **fresh
context** holding only the task the parent wrote, run under **bounds that
are the parent's to pay for** (turns, context budget, spend, wall clock), and
ended by returning text. Several children may run **concurrently** in one
call, which is what the fan-out skills need and what ETHOS #3 ("Parallel by
Default") expects.

Why this shape and not another:

- **Why a child is a turn, not a thread of the parent's context.** Context is
  the scarce thing. A reviewer that reads twenty files should not leave those
  twenty files in the orchestrator's window; a fresh child context holds them,
  and only the report comes back. That is the whole value of fan-out, and it
  is also what keeps the parent's compaction (REQ-618) from having to reason
  about which child's reads are still load-bearing.
- **Why children may run on a different tier.** The product's core promise is
  that frontier-model money goes only where frontier intelligence matters.
  `/proceed`'s Phase 4 wants a Think-tier parent handing task files to
  Build-tier implementers. If every child inherited the parent's route, the
  fan-out would multiply the most expensive tier by N and the cost meter would
  have nothing to show for it. The caller may *request* a tier; the router
  still decides, and the privacy boundary still pins.
- **Why the bounds are hard, typed, and visible.** BUG-185 is the shape of
  the failure to avoid: one consent bought an unbounded number of commands
  with no deadline of its own. A child loop is a far larger lever than a
  skill body — it can call `shell` as many times as its turn cap allows. So a
  child carries a turn cap, a context budget, a spend share, and a deadline,
  each one a named number the parent gets back in the result, and exhausting
  any of them is a typed status the parent can reason about, never a silent
  stop.

This is the harness-level primitive. It does not make `/proceed` work by
itself — that needs the skill to find the tool under the name it expects and
the companion-file reads REQ-587 also deferred — but it removes the stall
REQ-587 AC-15 recorded, and it is the gate the `pipeline-runner`,
`/analyze`, and `/review` fan-outs are all waiting on.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| AgentCall | `tasks` | array of ChildTask | required; 1 ≤ len ≤ `max_children_per_call` |
| AgentCall | `call_id` | string | daemon-minted, unique within the session; echoed on every event and result |
| ChildTask | `task` | string | required, non-empty; the child's user-role prompt, verbatim |
| ChildTask | `name` | string | optional; ≤ 40 chars; defaults to `child-<n>`; must be unique within the call |
| ChildTask | `tier` | enum `reflex`/`scan`/`build`/`think` | optional; a *request*, not a binding — see BR-6 |
| ChildTask | `context` | string | optional; extra text the parent chooses to pass; counts against the child's context budget |
| ChildTurn | `child_id` | string | daemon-minted; `<call_id>/<name>` |
| ChildTurn | `parent_turn_id` | string | the parent prompt turn's id; immutable |
| ChildTurn | `route` | ResolvedRoute | the route the router actually chose, after boundary pinning |
| ChildTurn | `bounds` | ChildBounds | the four numbers the child ran under; fixed before the first model call |
| ChildBounds | `max_turns` | u32 | ≤ parent's `max_turns`; default `agent.child_max_turns` |
| ChildBounds | `context_budget_bytes` | u64 | derived per the child's route exactly as a prompt turn's is (REQ-586) |
| ChildBounds | `spend_ceiling_micro_cents` | Option<u64> | the child's *initial* share of the session ceiling — see BR-8 |
| ChildBounds | `deadline_secs` | u64 | whole-child wall clock; default `agent.child_deadline_secs` |
| ChildResult | `name` | string | the ChildTask name |
| ChildResult | `status` | enum | `completed` / `refused` / `cancelled` / `turns_exhausted` / `budget_exhausted` / `spend_exhausted` / `timed_out` / `failed` |
| ChildResult | `report` | string | the child's final assistant text; ≤ `agent.report_max_bytes` (BR-11); empty unless `completed` or `turns_exhausted` |
| ChildResult | `refusal` | string | typed refusal code when `status = refused`; absent otherwise |
| ChildResult | `error` | string | *added 2026-10-07 (verify)* — `<code>: <message>` when `status = failed`; absent otherwise (BR-10) |
| ChildResult | `turns_used` | u32 | model calls the child made |
| ChildResult | `route` | ~~string~~ object `{tier?, provider_id, model}` *(amended 2026-10-07)* | ~~tier + provider + model the child ran on~~ the route the child **ended** on, after any privacy pin or fallback (BR-6) — a structure rather than a sentence, so a client selects on its parts; absent only for a child that ended before its route was resolved |
| ChildResult | `bounds` | ChildBounds | *added 2026-10-07 (verify)* — the bounds stamped before the first call, echoed (BR-7, AC-11); absent exactly where `route` is |
| ChildResult | `cost_micro_cents` | u64 | sum of the child's `CostRecord`s |
| ChildResult | `spend_ceiling_final_micro_cents` | Option<u64> | the child's ceiling when it ended — ≥ the stamped share, raised only by sibling releases (BR-8) |
| ChildResult | `provenance` | ProvenanceId set | every provenance the child's context touched — carried onto the result block (BR-9) |
| AgentConfig | `max_children_per_call` | u32 | default 5; `[agent]` config table |
| AgentConfig | `max_children_per_turn` | u32 | default 8; counts every child started by one parent prompt turn across all `agent` calls |
| AgentConfig | `child_max_turns` | u32 | default 12 (the local profile's own cap); clamped to the parent's `max_turns` |
| AgentConfig | `child_deadline_secs` | u64 | default 600 |
| AgentConfig | `report_max_bytes` | u64 | default 32 KiB |
| AgentConfig | `enabled` | bool | default true; `false` leaves `agent` out of the registry entirely |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `agent_call_started` | the `agent` tool accepted a call after validation | `call_id`, `parent_turn_id`, `children: [{name, requested_tier}]` |
| `agent_child_started` | a child's first model call is about to be made | `child_id`, `parent_turn_id`, `name`, `route`, `bounds` |
| `agent_child_consent_requested` | a child hit an ask-gate at an attended session | `child_id`, `tool`, the ordinary consent payload — see BR-5 |
| `agent_child_share_released` | a child ended with unspent share while siblings still run | `child_id`, `released_micro_cents`, `recipients: [{child_id, new_ceiling_micro_cents}]` |
| `agent_child_finished` | a child reached any terminal status | `child_id`, `status`, `turns_used`, `cost_micro_cents`, `report_bytes`, `truncated: bool` |
| `agent_call_finished` | every child in the call is terminal and the tool result is being returned | `call_id`, per-child `status`, `total_cost_micro_cents`, `elapsed_ms` |
| child-scoped tool events | every `tool_started` / `tool_finished` / `context_pressure` / `cost_recorded` a child emits | the existing payloads, plus `child_id` and `parent_turn_id` |
| `agent_call_refused` | the call failed validation or a cap (BR-2, BR-3, BR-12) | `call_id`, typed `refusal`, the number that refused it |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| call `agent` | the model, at every permission level, when `agent.enabled`; what a child may then do is decided per tool by BR-5, never up front |
| run a tool inside a child | the child, at the parent session's level, through the same `authorize` path as the parent — `agent` itself never |
| grant consent to a child's ask | the user, through the session's ordinary consent surface, labelled with the child's name |
| cancel a call | the user (cancelling the parent turn), the deadline, the spend ceiling |
| read a child's transcript | whoever can read the session's transcript (REQ-611) — children write into the same file |

## Business Rules

- [ ] BR-1: **A child is a real turn with a fresh context.** A child runs the
  same turn loop as a prompt turn, with the session's tool registry
  (rebuilt per child, as `Runtime::build_tools` does per turn), the
  session's permission level, root, denied prefixes, and privacy boundaries.
  Its context holds exactly: the session's system prompt (with the same
  `system_sources` provenance), the ChildTask `context` string if given, and
  the `task` as the user-role prompt. **None of the parent's conversation
  history is visible to a child.** What the parent wants the child to know,
  it writes into `task` or `context`. The `task` and `context` strings are
  admitted **whole or refused**: if together they do not fit the child's
  context budget the child ends `refused` with `over_budget` naming `size`,
  `budget`, and `bound` (`BudgetBound::words()`), so the parent can shorten
  them — never digested, never middle-elided (informed by REQ-589).
  (informed by REQ-567, REQ-618)
- [ ] BR-2: **Depth is one.** The `agent` tool is registered for prompt turns
  only; a child's registry never contains it. A child that asks for `agent`
  gets the registry's ordinary unknown-tool refusal. This is not a cap to be
  raised by config in v1 — nesting is Out of Scope, and the reason is the
  same one BUG-185 taught: every added level multiplies the amount of work
  one consent can buy.
- [ ] BR-3: **Fan-out is bounded by two caps and both are typed.** A call
  with more than `max_children_per_call` tasks is refused whole with
  `too_many_children` naming the count and the cap; a call that would push
  the parent turn past `max_children_per_turn` is refused whole with
  `child_cap_reached` naming both numbers. A refusal starts no child. Refused
  calls count against the parent's `max_turns` like any tool call, and
  against REQ-617's repeat ledger, where `agent` is a **write-capable** call
  (its children can write) — the third identical call is refused there
  before it reaches `agent`.
- [ ] BR-4: **Children in one call run concurrently and the parent waits for
  all of them.** The tool result is returned once every child is terminal;
  there is no partial return. Children in one call share nothing but the
  session: each has its own context, own route, own bounds, own ledger
  entries. The parent's turn claim stays held for the whole call, so a second
  prompt on the session is refused busy exactly as it is during any tool call
  today (informed by LESSON-539). While children run, the parent's event
  forwarder must keep draining: `agent_child_*` and child-scoped tool events
  reach the client live, not in a burst when the call finishes (informed by
  BUG-226 — a child loop that blocks inside `Tool::run` on the parent's
  worker reproduces that bug at N× the duration).
- [ ] BR-5: **A child's permissions are the session's permissions, asked
  through the session's surface.** A child authorizes every tool by name
  before `run` through the same gate as the parent. At an attended session a
  child that reaches an ask-gate prompts the user through the ordinary
  consent surface, labelled with the child's `name`, and concurrent asks from
  different children are presented one at a time; the other children keep
  running. Time a child spends parked on a consent prompt does not count
  against its `deadline_secs` — the clock covers work, not waiting for the
  user, so an unattended minute cannot turn a child `timed_out`. A grant
  given to a child's ask is session-scoped like any other
  grant and is visible to the parent and to sibling children. At an
  unattended session the gate's unattended decision applies (informed by
  REQ-591): an unlisted gate denies, the child's tool call fails typed, and
  the child continues or ends on its own — a child never invents consent. At
  `plan`, children are read-only like the parent. Nothing a child does can
  raise the session's level.
- [ ] BR-6: **A child's tier is requested, routed, and pinned — in that
  order.** `tier` is a hint to `Router`, which resolves the child's route the
  way it resolves a prompt turn's: the requested tier's binding if one is
  configured, else the session's default route for the child's category, and
  then the privacy pin. A child whose context touches `local-only` provenance
  routes local regardless of the request, and the result's `route` says so.
  A request the router cannot honour is **not** a refusal; the child runs on
  the route the router chose and `ChildResult.route` names it. The parent's
  own route is unaffected by anything a child does.
- [ ] BR-7: **Every bound is fixed before the child's first model call and
  returned in the result.** `max_turns`, `context_budget_bytes`,
  `spend_ceiling_micro_cents`, and `deadline_secs` are derived once from the
  child's resolved route and the config, stamped into `ChildBounds`,
  published on `agent_child_started`, and echoed in `ChildResult`. A mid-child
  reroute refits the child's context exactly as REQ-586 refits a prompt
  turn's — loudly, with `context_pressure` events carrying `child_id` — and
  never raises the bounds. The one bound that may move after stamping is
  the spend share, and only upward, by BR-8's release rule. (informed by REQ-586, LESSON-501: the bounds
  travel with the child, they are not re-derived later where the route that
  produced them is gone.)
- [ ] BR-8: **Children spend from the session's ceiling and the parent pays.**
  Every remote call a child makes flows through the single egress point and
  writes a `CostRecord` attributed to `child_id` **and** `parent_turn_id`, so
  `/cost` can show the parent turn's total with per-child lines beneath it.
  The REQ-588 session spend ceiling is one number shared by the parent and
  every child. When a call starts, each child's initial share is the
  remaining session headroom divided equally among the call's children
  (floor), stamped as `ChildBounds.spend_ceiling_micro_cents`. **When a child
  ends with unspent share while siblings still run, the unspent amount is
  released to the running siblings in equal parts** (floor; the remainder
  stays in the session pool), each sibling's ceiling rises by its part, and
  `agent_child_share_released` names the amount and every recipient's new
  ceiling. A ceiling only ever rises; a child's final ceiling is echoed as
  `spend_ceiling_final_micro_cents`. A child whose next call would exceed its
  current ceiling ends `spend_exhausted`. A session with no ceiling gives
  children no ceiling and releases nothing. The parent turn's own next model
  call still checks the session ceiling after the call returns — children
  can leave the parent with no headroom, and that is the existing
  spend-exhausted path, not a new one.
  *Amended 2026-10-07 (verify):* "a child whose next call **would exceed**
  its current ceiling" is checked the way REQ-588's prompt ceiling is —
  before the call, against what is already spent, because a call's cost is
  not known until its response is metered. A child's call is refused once
  its spend has **reached** its current ceiling (`spent >= share`); the call
  that carries it past the ceiling is allowed, so a child can overshoot its
  share by **at most one call**, and an overshoot is never charged to its
  siblings. And a `timed_out` child still inside a blocking tool keeps its
  share until that work has actually ended — the tool could still draw a
  metered call — then releases it; `agent_child_share_released` follows the
  result.
- [ ] BR-9: **Provenance flows up, never sideways.** The result block the
  parent receives carries the union of every `ProvenanceId` the child's
  context touched. If that union
  pins under a configured boundary, the parent's next model call is pinned
  by the existing mechanism — a child that read `local-only` content pins
  its parent local, exactly as the parent's own `read` would have. A child
  never sees another child's context or result. The egress check runs on the
  child's own calls and on the parent's next call; there is no third place.
  (informed by LESSON-501: a carried value that sheds its taint is the
  failure mode; REQ-611's transcript rules for provenance apply to child
  records unchanged.)
- [ ] BR-10: **Every terminal status is typed and the parent always gets a
  result.** A child ends in exactly one of the eight statuses in the entity
  table. `turns_exhausted` returns whatever final text the child had
  produced, marked; `budget_exhausted`, `spend_exhausted`, and `timed_out`
  return an empty report and the number that ended the child; `refused`
  carries the typed refusal (a project-skill gate, an unattended deny of a
  tool the task needed, a child-level `over_budget` on its own task text
  per REQ-589's whole-or-refused rule); `cancelled` means the parent turn was
  cancelled; `failed` is a provider or engine error after the child's own
  retry/reroute path is exhausted, with the error code. A child's failure
  never fails the parent turn: the parent receives the result and decides.
  Cancelling the parent turn cancels every running child, and the cancel
  reaches a child's blocking tool call the way it reaches the parent's today.
  *Amended 2026-10-07 (verify):* (1) **The gate's `refused`**: a child ends
  `refused` with `gate_denied:<tool>` exactly when its final text is empty
  **and** every tool call it attempted was denied — by the session gate, an
  unattended deny, or a tool's own consent gate (`skill`'s project-skill
  acknowledgment, `web`'s consent, a skill command's consent) — and none ran.
  Any other denial is a typed tool failure the child read (BR-5): a report
  saying why, or another tool that ran, makes its ending `completed`.
  (2) **"Reaches" means abandons, not kills**: when a child is cancelled or
  times out, the in-flight call is abandoned — its result never reaches a
  model; the tool process itself is not killed (follow-up — see Deferred), so
  a blocking `shell` runs to its own timeout and may outlive the parent's
  turn, exactly as the parent's own cancelled `shell` does today.
- [ ] BR-11: **A report is bounded, and over-bound is loud.** A child's final
  text longer than `report_max_bytes` is cut at the bound with a typed
  marker naming the kept and dropped byte counts, `truncated: true` on
  `agent_child_finished`, and the full text in the transcript. This is a
  deliberate departure from REQ-589's whole-or-refused rule for *skill
  expansions*: a skill body is a procedure that cannot survive elision; a
  child's report is a summary the parent asked for, and a bounded summary
  beats a refused one. The child's system prompt states the bound so a
  well-behaved child never hits it.
- [ ] BR-12: **A child may invoke skills; it may not escape them.** `skill` is
  in a child's registry under REQ-587's rules — flat expansion, the per-turn
  invocation cap applied per child, project-skill acknowledgment taken from
  the session's existing grants and asked through BR-5 when missing. A
  user-skill expansion in a child carries `unknown` provenance exactly as it
  does in a prompt turn, so it pins the child — and through BR-9, the parent.
  *Amended 2026-10-07 (verify):* since REQ-619 a `~/.claude` user skill
  carries a `~`-scoped identity, **not** `unknown`. What pins the child — and
  through BR-9 the parent — is a configured boundary covering it (for
  example `**/.claude/skills/**`), exactly as for a prompt turn; AC-16's
  "with a boundary configured" is that boundary.
- [ ] BR-13: **Children are visible wherever the parent is.** The REQ-621
  activity line shows the call's running children by name; the REQ-611
  transcript records every child turn in the session's file with `child_id`
  and `parent_turn_id` so a reader can reconstruct the tree; `/cost` shows
  per-child lines under the parent turn. A child that is running and
  invisible is BUG-226's symptom, and this REQ must not reintroduce it.
- [ ] BR-14: **`agent.enabled = false` is absence, not refusal.** With the
  flag off, the tool is not in the registry, not in the prompt's roster, and
  the model cannot name it; a skill that asks for it gets the ordinary
  unknown-tool refusal naming the config key. The session's `/help` and the
  REQ-617 command roster say whether `agent` is available.
  *Amended 2026-10-07 (verify):* the `/help` clause is **not implemented** by
  this REQ and moves to Deferred (follow-up); the rest of BR-14 stands.

## Acceptance Criteria

- [ ] AC-1: In a session with `agent.enabled` (default), the prompt's tool
  roster lists `agent` with a schema that admits `tasks: [{task, name?,
  tier?, context?}]`; with `agent.enabled = false` the roster omits it and a
  model call to `agent` returns the unknown-tool refusal naming
  `agent.enabled`. (BR-14)
- [ ] AC-2: A call with one task runs one child whose first model request
  contains the session system prompt, the `context` string, and the `task`
  as the only user message — and contains no block from the parent's
  conversation. Asserted by inspecting the captured provider request, not
  inferred from a size (informed by LESSON-519). (BR-1)
- [ ] AC-3: A child's registry has no `agent` tool: a child that calls
  `agent` receives the unknown-tool refusal, and no grandchild event is
  published. (BR-2)
- [ ] AC-4: A call with `max_children_per_call + 1` tasks is refused whole
  with `too_many_children` naming both numbers; a second call that would
  exceed `max_children_per_turn` is refused with `child_cap_reached`; in
  both cases no `agent_child_started` is published. (BR-3)
- [ ] AC-5: A call with three tasks against a provider stub that parks each
  child until all three have made their first request completes — the three
  `agent_child_started` events precede every `agent_child_finished`, and a
  sequential implementation would deadlock on the stub. Concurrency is proven
  by the rendezvous, not by a wall-clock bound. (BR-4)
- [ ] AC-6: While a call with a held child is running, a second prompt on the
  session is refused busy; and a client subscribed to the session receives
  the child's `tool_started` event *before* the call finishes, verified with
  a parked verifier that holds the child's tool and asserts the event
  arrived while parked (informed by LESSON-518, BUG-226). (BR-4, BR-13)
- [ ] AC-7: At `guarded`, a child whose task requires `shell` publishes
  `agent_child_consent_requested` carrying the child's name and the ordinary
  consent payload; granting it lets the child continue; the grant is then
  visible to a sibling child in the same call, which does not re-ask. At an
  unattended session with no decision for that gate, the child's `shell`
  call fails typed and the child's result reflects it; no consent was
  invented. (BR-5)
- [ ] AC-8: At `plan`, a child's `edit` denies exactly as the parent's would.
  (BR-5)
- [ ] AC-9: With a Build-tier binding configured, a child requesting
  `tier: build` under a Think-tier parent runs on the Build route and its
  result's `route` says so; the parent's next model call still goes to the
  Think route. With no Build binding, the child runs on the session's default
  route for its category and the result names that route — no refusal.
  (BR-6)
- [ ] AC-10: With a `local-only` boundary configured, a child that reads a
  path under it (a) routes local for its remaining calls, (b) returns a
  result whose `provenance` includes that path's id, and (c) pins the
  parent's next model call local — verified by egress capture showing no
  remote request from either the child after the read or the parent after
  the result. (BR-6, BR-9)
- [ ] AC-11: `agent_child_started` carries the four bounds; `ChildResult`
  echoes the same four values byte-for-byte; a mid-child reroute emits
  `context_pressure` with `child_id` and does not change the echoed bounds.
  (BR-7)
- [ ] AC-12: Every remote call a child makes produces a `CostRecord` with
  both `child_id` and `parent_turn_id`; `/cost` renders the parent turn's
  total as the sum of its own calls and its children's, with one line per
  child. (BR-8)
- [ ] AC-13: With a session spend ceiling and two children in a call, each
  child's stamped share is half the remaining headroom (floor); a child
  whose next call would exceed its ceiling ends `spend_exhausted` with an
  empty report; the sibling completes; the parent's next model call then
  checks the session ceiling and, if the children consumed it, ends on the
  existing spend-exhausted path. With three children where one completes
  early having spent a third of its share, `agent_child_share_released`
  names two-thirds of that share split equally between the two running
  siblings, each sibling's `spend_ceiling_final_micro_cents` is its stamped
  share plus its part, and a sibling that would have been `spend_exhausted`
  under its stamped share completes under the raised one. Test both
  derivations end-to-end from the session ledger, not by handing the child a
  literal share (informed by LESSON-552). (BR-8)
- [ ] AC-14: Each of the eight terminal statuses is produced by a dedicated
  test: a completing child, a child refused by a project-skill gate, a
  cancelled parent turn, a child that hits `max_turns` (report is the final
  text, marked), a child whose task text alone exceeds its context budget
  (`refused` with `over_budget`), a child that exceeds its spend share, a
  child past its deadline while a tool call is in flight (the tool is
  cancelled, status `timed_out`), and a child whose provider returns a
  terminal error after its reroute path (`failed` with the code). In every
  case the parent turn continues and receives the result. (BR-10)
  *Amended 2026-10-07 (verify):* "the tool is cancelled" reads: the
  in-flight call is **abandoned** — its result never reaches a model, and
  `timed_out` comes back while it is still in flight; the tool process itself
  is not killed (follow-up — see Deferred).
- [ ] AC-15: A child whose final text is `report_max_bytes + 1` long returns
  a report of exactly `report_max_bytes` plus the typed marker naming kept
  and dropped counts, `agent_child_finished` has `truncated: true`, and the
  session transcript holds the full text. (BR-11)
- [ ] AC-16: A child may call `skill` and the expansion lands in the child's
  context, not the parent's; a child invoking a `~/.claude` user skill in a
  repo-rooted session with a boundary configured pins the child local and,
  via the result, the parent. (BR-12)
- [ ] AC-17: The transcript file for a session with one `agent` call contains
  the parent turn's records and every child's records, each child record
  carrying `child_id` and `parent_turn_id`, in one file. (BR-13)
- [ ] AC-18: **Dogfood.** In a repo with the ADLC toolkit vendored, `/analyze`
  run from a Teton session dispatches its audit dimensions through `agent`
  rather than running them inline, and the consolidated report is produced;
  `/proceed` on a REQ with an approved architecture reaches the end of Phase
  4 without stalling at "dispatch an agent" (the stall REQ-587 AC-15
  recorded). Where `/proceed` still stalls later on a companion-file read,
  record the exact step in this REQ's Validation section — that is the
  evidence for the companion-files spec, not a failure of this one.
- [ ] AC-19: A session root of kind non-project (REQ-615) refuses project
  skills inside children exactly as in the parent, and a child's `shell`
  starts in the session root with the same cwd note behaviour.

## External Dependencies

- None new. The tool uses the existing turn loop, router, egress point, cost
  ledger, transcript sink, event bus, consent surface, and config loader.

## Assumptions

- The existing turn loop can be started with an arbitrary initial context and
  a caller-supplied `max_turns` without a user prompt entering through the
  RPC surface — i.e. a child is a library call into the loop, not a
  synthetic `prompt` RPC. If the loop is coupled to the RPC-side claim in a
  way that resists this, the architecture phase must say so and propose the
  seam; this spec does not assume a particular one.
- The session turn claim is held by the parent for the whole call and
  children run *under* it rather than claiming separately. A design that has
  children take their own claims would let a second user prompt interleave
  with running children, which REQ-567's "concurrent prompts stay linear"
  rule forbids.
- The REQ-588 spend ceiling is readable and decrementable from the ledger at
  the moment a call starts, so BR-8's equal split can be derived from live
  headroom rather than a snapshot taken at session start.
- `Tier` requests map onto the REQ-558 four-tier binding table; where a tier
  has no binding, "the session's default route for the child's category"
  means whatever the router already does for a prompt turn of that category.
  No new routing policy is introduced.
- Default numbers (5 per call, 8 per turn, 12 child turns, 600 s, 32 KiB) are
  starting points chosen so `/analyze`'s four-auditor and `/review`'s
  five-reviewer fan-outs each fit in one call. The architecture phase may
  adjust them with a stated reason.
- The vendored ADLC skills phrase dispatch in Claude Code's vocabulary
  (`Agent` tool, `subagent_type`, `run_in_background`). AC-18 assumes the
  model maps that phrasing onto `agent { tasks }` from the roster line and
  the schema alone; if dogfood shows it does not, the fix is a roster
  sentence or a toolkit change, not a schema that mimics Claude Code's.
- The id was allocated with remote verification (`ADLC_ALLOC_DEGRADED` unset).

## Open Questions

_All five resolved with the author on 2026-10-05; kept for the record._

- [x] OQ-1: `max_children_per_call` default. *Resolved: 5* — `/review`'s
  five reviewers fit one call, and the skills were written assuming one
  dispatch step. Entity table updated.
- [x] OQ-2: Release an ended child's unspent spend share to running
  siblings? *Resolved: yes* — folded into BR-8 as an equal-parts release,
  ceilings only rise, `agent_child_share_released` makes it visible, AC-13
  pins the derivation end-to-end.
- [x] OQ-3: Over-bound report — truncate loudly or fail? *Resolved:
  truncate* — BR-11 stands as written; the full text is in the transcript.
- [x] OQ-4: The `context` string — whole-or-refused or digested? *Resolved:
  whole-or-refused*, folded into BR-1: `task` + `context` must fit the child
  budget or the child is `refused` with `over_budget` naming the numbers.
- [x] OQ-5: Tool name. *Resolved: `agent`* — the REQ-617 roster line says it
  is the dispatch tool the skills call "Agent".

## Out of Scope

- **Nesting** (a child dispatching children), any depth > 1, and a
  configurable depth — Deferred; BR-2 is a rule, not a default.
- **Companion-file reads** for skills (`/proceed` names three) — the other
  REQ-587 deferral; AC-18 records where it bites, it does not fix it.
- **Worktree isolation per child** (`isolation: "worktree"`), a per-child cwd,
  or a per-child scratchpad — children share the session root. LESSON-610 and
  LESSON-611 (a shared scratchpad clobbers evidence between parallel runners)
  apply to skills that fan out *write-heavy* children; those skills must
  partition their outputs by child name until a worktree option exists.
- **Resuming or messaging a finished child** (Claude Code's `SendMessage` to
  a prior agent) — a child's context is dropped when it ends.
- **Streaming partial reports** to the parent before all children finish.
- **Background children** that outlive the parent turn.
- **A child-specific permission level** ("run this child read-only under an
  `edits` parent") — a child is at the session's level, nothing else.
- **Per-child model/provider pinning** beyond a tier request — the router
  decides.
- **Agent-type definitions** (`.claude/agents/*.md` frontmatter: tools
  allowlists, model overrides, custom system prompts). A child's system
  prompt is the session's; the task text is where a skill puts the role.
- Hooks, `allowed-tools`, `context: fork` for skills.

## Deferred

_Added 2026-10-07 (verify) — found in scope, settled as follow-ups._

- **Shell cancellation reaching a child's in-flight tool.** A cancelled or
  timed-out child's blocking tool is abandoned, not killed (BR-10, AC-14 as
  amended): a `shell` command runs to its own timeout and may outlive the
  parent's turn. The parent's own cancelled `shell` behaves the same today.
  BUG to be filed at wrapup.
- **`/help` saying whether `agent` is available** (BR-14's `/help` clause) —
  not implemented by this REQ; follow-up.

## Retrieved Context

- LESSON-501 (lesson, score 13): State carried past its creator's lifetime sheds invariants silently
- REQ-567 (spec, score 13): Cross-prompt conversation carry in interactive sessions
- REQ-615 (spec, score 12): Session-root honesty for the shell tool and skill preambles
- REQ-617 (spec, score 12): The model knows the session's own commands and stops repeating itself
- REQ-589 (spec, score 12): Offer to proceed when a skill expansion exceeds the route's context budget
- REQ-591 (spec, score 12): The project-skill trust gate and its unattended allowlist
- REQ-586 (spec, score 12): A turn's context budget follows its route
- REQ-613 (spec, score 11): Teton writes TETON.md when a project has none
- REQ-618 (spec, score 11): Compaction that keeps the ask
- REQ-611 (spec, score 11): Daemon-side transcript logging
- LESSON-552 (lesson, score 11): A test that hands the minter its input never exercises the derivation
- REQ-587 (spec, score 11): Model-invoked skills — a `skill` tool lets the model expand a registered skill
- LESSON-539 (lesson, score 11): Claim first, then re-read — session state snapshotted before the turn is a stale hint
- LESSON-518 (lesson, score 11): A blocking gate's reader-loop freedom is not inherited from the await-based tests
- LESSON-519 (lesson, score 11): An 'assert by inspection, not from the error' AC needs the real artifact
