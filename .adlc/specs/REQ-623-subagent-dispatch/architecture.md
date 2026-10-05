# REQ-623 — Architecture: subagent dispatch

## Approach

An `agent` tool whose work is **N child turn-loops run as tokio tasks on the
daemon's existing turn machinery**, awaited from the parent loop's async path,
with every child-scoped fact (events, cost, transcript, provenance, bounds)
stamped where it is produced rather than reconstructed afterwards.

Three facts from exploration shape everything below:

1. **The loop is already a library call.** `run_session_turn_with_pressure_policy`
   (`crates/tetond/src/harness/turn_loop.rs:2363`) takes a `CompletionSource`,
   `ToolRegistry`, `ToolContext`, `PermissionGate`, `SessionEvents`, a
   caller-built `ContextManager`, `HarnessConfig`, provenance hook and duties.
   `DaemonRuntime::run_prompt_turn` (`runtime/turn.rs:419`) is an RPC wrapper
   around it in eight stages — claim, route, name, assemble, settle, prepare,
   attempt, commit. A child needs **route → assemble → attempt** and nothing
   else: no claim (it runs under the parent's), no naming duty, no skill
   settle (the task text is not a skill), no commit (its context is dropped;
   only its ledger rows, transcript records and events persist). The spec's
   first Assumption holds; no new seam into the loop is needed.
2. **`Tool::run` is synchronous and the loop already has a downcast seam for a
   tool the loop must treat specially.** `Tool::as_skill()`
   (`harness/tools/mod.rs:1108`, REQ-587 ADR-2) lets the loop reach the
   concrete `SkillTool` without widening the trait. `Tool::refine` is the
   loop's async half, but it is shaped for a duty refining an outcome, not for
   running work. A child dispatch is minutes of awaiting; the right place is
   the loop's async path, reached by the same downcast pattern.
3. **The spend ceiling is per prompt, not per session.** REQ-588's egress
   check compares an in-memory accumulator against `spend_ceiling`
   (`egress/mod.rs:534-537`), scoped to the prompt turn. "Session ceiling" in
   the spec's BR-8 means *this prompt's* ceiling; the headroom children split
   is `ceiling − parent accumulator` at the moment the call starts.

## Data model changes

**Protocol (`crates/teton-protocol`)** — additive, serde-defaulted:

- New types: `ChildId` (newtype over `String`, `"<call_id>/<name>"`),
  `ChildTask`, `ChildBounds`, `ChildStatus` (eight variants, `lowercase`),
  `ChildResult`, `AgentRefusal` (`too_many_children` | `child_cap_reached` |
  `duplicate_name` | `empty_task`).
- New `Event` variants: `AgentCallStarted`, `AgentChildStarted`,
  `AgentChildConsentRequested`, `AgentChildShareReleased`,
  `AgentChildFinished`, `AgentCallFinished`, `AgentCallRefused`.
- `ToolStarted`, `ToolFinished`, `ContextPressure`, `CostRecorded`,
  `PermissionRequest` gain `child_id: Option<ChildId>` and
  `parent_turn_id: Option<TurnId>` with `#[serde(default,
  skip_serializing_if = "Option::is_none")]`, so a pre-REQ CLI decoding a
  post-REQ daemon's stream ignores them and a post-REQ CLI decoding an old
  stream reads `None`. They go on **payloads, not `EventEnvelope`**: the
  envelope is `session_id + seq + flattened event` and the transcript tap
  reserves its keys (`transcript/record.rs:47-56`); a child id is a fact
  about the event, not the envelope.

**Cost ledger (`crates/tetond/src/cost/ledger.rs`)** — two nullable columns on
`cost_records`: `child_id TEXT`, `parent_turn_id TEXT`, added through the
existing `ALTER TABLE … ADD COLUMN` migration list (lines 150-175 pattern).
`report.rs` groups by `parent_turn_id` and nests `child_id` rows beneath.

**Config (`crates/teton-core/src/config.rs`)** — `[agent]` table,
`AgentConfig`, following `CostConfig`'s shape: `#[serde(default)]`,
`is_unset()` for `skip_serializing_if`, structural validation in
`Config::validate` (every cap ≥ 1, `child_max_turns ≥ 1`,
`report_max_bytes ≥ 1024`), defaults `{ enabled: true,
max_children_per_call: 5, max_children_per_turn: 8, child_max_turns: 12,
child_deadline_secs: 600, report_max_bytes: 32768 }`.

**Transcript (`crates/tetond/src/transcript/record.rs`)** — `child_id` and
`parent_turn_id` in the **bodies** of `ToolCallInput`, `ToolResult`, and the
new agent records; never among the reserved top-level keys. One session, one
file (REQ-611 BR-5) is unchanged.

## API changes

No new RPC methods. The model-facing surface is one tool:

```json
{ "name": "agent",
  "input_schema": { "tasks": [ { "task": "string", "name": "string?",
                                 "tier": "reflex|scan|build|think?",
                                 "context": "string?" } ] } }
```

The tool result is a JSON `ChildResult[]` under `ResultDisposition::UntrustedData`
— a child's report is model output about repository content and is framed
exactly as a `read` result would be, never as instructions.

`/cost` (existing RPC) gains per-child rows nested under their parent turn;
the CLI renders them indented. `PermissionRequest` carries the child id so
the CLI's consent prompt can label the ask.

## Service layer — the pieces

```
harness/tools/agent.rs        AgentTool: schema, validation, caps, JoinSet fan-out,
                               share pool, consent serialisation, result assembly
harness/child.rs              ChildDispatcher trait (what the tool needs from the
                               runtime), ChildSpec, PausableDeadline, report bound
runtime/child_turn.rs         impl ChildDispatcher for DaemonRuntime: route → assemble
                               → attempt, reusing the stage fns of runtime/turn.rs
cost/share.rs                 SharePool: per-call spend shares, equal split, release
harness/turn_loop.rs          SessionEvents::for_child(); the `as_agent()` async arm
teton-core/src/config.rs      AgentConfig
teton-protocol/src/events.rs  types + variants above
teton/src/activity.rs         children on the activity line
teton/src/cost_ui.rs          per-child rows
tests/e2e/harness.rs          MockProvider matching + rendezvous (ADR-6)
```

Layering rule: `harness/tools/agent.rs` depends on the `ChildDispatcher`
trait in `harness/child.rs`, never on `runtime::*`. The runtime implements the
trait. This is the same direction `SkillTool` keeps (it takes `Arc<SkillRegistry>`,
`Arc<PermissionGate>` and a `Handle`, not a runtime), so the tools module
stays testable without a daemon.

## Key decisions

### ADR-1: The loop awaits the agent tool on its async path via `Tool::as_agent()`

**Decision.** Add `fn as_agent(&self) -> Option<&AgentTool> { None }` to
`Tool`, mirroring `as_skill()`. In `run_the_allowed_tool`
(`turn_loop.rs:1744`), before the `block_in_place_if_multithread(|| tools.dispatch(..))`
arm (line 1956), the loop checks `as_agent()` and, when it hits, `.await`s
`AgentTool::dispatch(ctx, args, parent)` instead. `AgentTool::run` is still
implemented — it returns a typed `agent_requires_async_dispatch` refusal —
so a registry that calls `dispatch` by name (a unit fixture, a future caller)
gets an honest answer instead of a hang.

**Why not `Tool::refine`.** `refine` receives an *outcome* to improve and a
`ToolDuties` handle; it is the duty-ranking seam. Overloading it to run
minutes of children would make "the outcome of `run`" a placeholder every
reader has to know is fake. The downcast is one line in one place and names
what it is.

**Why not `block_in_place` + `block_on` inside `run`.** That is BUG-226's
shape with a longer duration: a worker core handed to a thread that then
re-enters the runtime. The parent loop is already async; awaiting there costs
nothing and keeps the parent's event forwarder draining (BR-4, LESSON-518's
parked-verifier test is AC-6).

### ADR-2: Children are tokio tasks under the parent's claim, joined by a `JoinSet`

**Decision.** `AgentTool::dispatch` builds one `ChildSpec` per task, spawns
each via `runtime.spawn(dispatcher.run_child(spec))` into a `JoinSet`, and
awaits the set. The parent's `ClaimedTurn` is never touched; a second prompt
is refused busy by the existing claim stage (LESSON-539). Cancellation: the
parent's `ClientPresence`/cancellation reaching `run_the_allowed_tool` aborts
the `JoinSet` (`JoinSet::abort_all`), and each child's `run_child` is
structured so an abort landing in a tool call cuts it the way the parent's
cancellation trim does today (`turn_loop.rs:1616-1632`). A child that was
aborted reports `cancelled`.

**Deadline.** `PausableDeadline` — a clock the child runner stops while the
gate is awaiting a human (`PermissionGate` exposes the await) and resumes
after. The child's `run_child` is `tokio::select!`ed against the deadline's
`expired()` future; expiry aborts the in-flight call and reports `timed_out`
with the bound. The consent exclusion is what BR-5's "an unattended minute
cannot turn a child `timed_out`" needs; a plain `tokio::time::timeout` cannot
express it.

### ADR-3: Child events are the parent's events with two more facts, stamped by the emitter

**Decision.** `SessionEvents::for_child(&self, child_id, parent_turn_id) ->
SessionEvents` returns an emitter that stamps `child_id`/`parent_turn_id`
into every payload that has the fields, on the same bus, same `session_id`,
same transcript sink. The child's loop takes this emitter and knows nothing
about children. New `agent_*` events are published by `AgentTool` through the
*parent's* emitter.

**Why on the emitter.** LESSON-501: a fact recorded where it is known cannot
be lost at a later seam. Every alternative — a thread-local, a bus-level
"current child", a post-hoc re-tag in the tap — re-derives the id somewhere
the child is gone.

**Golden sequences.** Child events interleave nondeterministically
(LESSON-591). The e2e suite asserts child events by *set and per-child
order* (filtered by `child_id`), and the parent's sequence excludes
child-scoped events before comparison. No fixture pins a cross-child order.

### ADR-4: Spend shares live in a per-call `SharePool`; the parent's accumulator is shared

**Decision.** `SharePool` (`cost/share.rs`) is created per call with
`headroom = parent.ceiling − parent.accumulator` and `n` children; it hands
each child `floor(headroom / n)` and records it on `ChildBounds`. A child's
egress is built with `spend_ceiling = Some(pool.ceiling_of(child))` read
through an `Arc<SharePool>` **at check time** (so a raised ceiling is seen),
and its own per-child accumulator. Every child's `CostRecord` **also** adds
into the parent's accumulator (the `Arc<teton_core::cost_ceiling::PromptSpend>`
`run_prompt_turn` creates once per prompt and every `Egress` of that prompt shares,
`egress/mod.rs:549`), so after the call the parent's next model call checks the real
headroom on the existing `SpendCeilingReached` path (BR-8's last sentence,
LESSON-557: both halves exist already — the typed outcome and its arm).
`SharePool::release(child)` on a child's terminal status splits its unspent
share equally among still-running children (floor; remainder stays unused),
raises their ceilings, and returns the recipients for the
`agent_child_share_released` event.

**Why a pool and not N routers.** `Router::with_spend_ceiling` stamps a
number at construction. A ceiling that rises needs a reader, not a stamp.

### ADR-5: One `PermissionGate`, asks serialized per call, no fourth door

**Decision.** Children authorize through the session's gate at the session's
level — `authorize` by tool name before `run`, exactly the parent's path.
`AgentTool` holds a per-call `tokio::sync::Mutex<()>` that a child takes
around the gate's ask await, so concurrent asks present one at a time; the
child publishes `agent_child_consent_requested` (with its name) beside the
gate's ordinary `permission_request` (which now carries `child_id`). A grant
lands in the gate's session-scoped `grants` map and is visible to siblings
without re-asking. Unattended: the gate's existing decision (REQ-591
`trusted_project_roots` and the per-key unattended rule) applies; a deny is
a typed tool failure inside the child.

**Why no `authorize_child_tool`.** The mapper proposed a fourth entry point
keyed by child id. A grant keyed by child would be *narrower* than the
session — the opposite of BR-5 ("visible to the parent and to sibling
children") — and a second door is a second place for the LESSON-552 class of
bug (a key minted from the wrong derivation).

### ADR-6: `MockProvider` gains request matching and a rendezvous hold

**Decision.** `tests/e2e/harness.rs`'s `MockProvider` serves scripted replies
in order; with concurrent children the arrival order is a scheduler
accident, so ordered scripting cannot address a child. Add
`MockProvider::start_matching(Vec<(Matcher, MockResponse)>, default)` where
`Matcher` is a request-body substring (the child's `task` text), and
`MockResponse::rendezvous(n)` — a reply held until `n` requests are parked
on it, then released to all. AC-5's rendezvous *is* this primitive; AC-6's
parked verifier reuses it. Egress capture (`global_capture`,
`assert_no_boundary_bytes`) is unchanged — it is request-order-agnostic.

**Why in the fixture and not the product.** Nothing in the daemon needs to
change for tests to address children; only the stub's addressing model was
built for one request at a time.

### ADR-7: `agent` is cap-exempt, write-capable, registered per turn behind `agent.enabled`

**Decision.** Registered in `build_tools` (`runtime/turn.rs:3643`) after
`register_skill_tool`, only when `config.agent.enabled` and the turn is a
prompt turn (a child's `build_tools` call passes `ToolSet::Child`, which
skips it — BR-2). Added to `CAP_EXEMPT_TOOLS` with the reason "a fan-out the
user's skills depend on must not vanish on a degraded profile — a skill that
dispatches on one route and runs inline on another is a skill that lies".
Classified `Allowance::Twice` (write-capable) in `harness/repeat.rs` — the
REQ-617 ledger's existing rule for tools whose effect can write. Default
permission row: `Allow` at every level — the tool itself writes nothing;
what a child does is gated per tool.

### ADR-8: Child provenance is the union of the child's `system_sources` and touched ids, carried on the result block

**Decision.** `run_child` returns, with the report, the `BTreeSet<ProvenanceId>`
its `ContextManager` accumulated (the same set the parent's egress check
reads). `AgentTool` writes the result block into the parent's context with
that set as the block's provenance, so the parent's existing egress check
pins on it without a new rule. `unknown` in the set (a `~/.claude` user skill
in a child, BR-12) pins the parent exactly as the parent's own `skill` would.

## Proposed additions to `.adlc/context/architecture.md`

- A "Child turns" subsection under the turn-loop section: the three stages a
  child runs, the four it skips, `as_agent()` beside `as_skill()`, and the
  rule that child ids are stamped by `SessionEvents::for_child` and nowhere
  else.
- Under cost: "the prompt ceiling is split into shares by `SharePool`;
  shares rise, never fall; the parent accumulator is the sum".
- Under testing: `MockProvider` matching and rendezvous, and the LESSON-591
  rule for child events in golden sequences.

## Lessons applied

- **BUG-226 / LESSON-518** — ADR-1 keeps the parent loop awaiting, never
  blocking; AC-6 is the parked verifier.
- **LESSON-501** — ADR-3 and ADR-8 record child facts at the emitter and on
  the result block, not at a later seam.
- **LESSON-539** — ADR-2: no second claim; children run under the parent's.
- **LESSON-552** — ADR-4/AC-13 test the share derivation from the ledger, not
  a literal; ADR-5 refuses a second grant-key derivation.
- **LESSON-591** — ADR-3: no cross-child order in any golden sequence.
- **LESSON-557** — ADR-4 reuses the existing `SpendCeilingReached` arm rather
  than adding a half-typed outcome.
- **LESSON-570** — the REQ-617 roster sentence for `agent` is written for the
  product after this REQ lands (TASK on docs/prompt).
- **ASSUME-010** — `agent.rs` keeps its `#[cfg(test)]` module last, after
  `impl Tool`.
- **LESSON-610/611** — out of scope for the product, but the dogfood AC-18
  run must partition skill outputs by child name; noted in the e2e task.
