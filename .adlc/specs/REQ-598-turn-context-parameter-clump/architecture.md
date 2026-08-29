# REQ-598 — Architecture

## Approach

Name the recurring per-turn parameter cluster as a **borrowed, per-turn,
`Copy` context struct**, constructed once in `run_prompt_turn` at the point
where every field it carries has reached its final binding, and threaded to the
turn-path functions that currently take the fields one at a time.

Phase 1 established the empirical baseline that shapes this design:

- The workspace carries **25** `#[allow(clippy::too_many_arguments)]`
  attributes, but only **16** suppress a firing lint. Nine are vestigial.
- The cluster is **not** one cluster. `runtime.rs` holds two, and
  `turn_loop.rs` a third (see ADR-2).
- `engine.rs`, `category.rs`, `main.rs`, and `budget.rs` carry none of the five
  fields and are out of scope (OQ-3, answered in the requirement).

The measurement that produced the vestigial/earned split is worth recording
because it is not the obvious one: under the workspace's `clippy::all = deny`,
a bare `cargo clippy` **aborts at the first crate that errors** and reports a
single site. Downgrading the one lint (`-A clippy::all -W
clippy::too_many_arguments`) is what makes the whole workspace report. A future
audit that skips this will conclude all 25 are load-bearing.

## Key Decisions

### ADR-1: Three types, layered on a shared core — not one wide struct

The requirement's Assumptions section anticipated this: "If a subset of the 25
sites turns out to want a *different* bundle, the answer is two small structs,
not one wide one." The sites do want different bundles. Measured:

| Bundle | Fields | Sites |
|---|---|---|
| **core** (universal) | `events`, `session_id`, `config`, `router` | all of the below |
| **turn** | core **+ `gate`** | `offer_or_refuse_over_budget`, `build_tools`, `run_one_attempt` |
| **duty** | core **+ `local_engine`, `prompt_spend`** (never `gate`) | `resolve_duty`, `build_duty_route`, `spawn_title_session`, and the five `*_route` fns |

The decisive evidence that duty is a real second bundle, not an accident: in
`run_one_attempt`, the four calls to `digest_route`, `triage_route`,
`shell_route`, and `compact_route` pass the **identical six arguments** in the
identical order. And `spawn_title_session` — which calls `title_route` — has no
`gate` at all and could not construct a turn context if one were required.

So:

```rust
/// The four facts every per-turn function needs. All shared borrows, so `Copy`.
struct TurnCore<'a> { events, session_id, config, router }

/// The turn path: the core plus the gate that authorizes this turn's tools.
struct TurnContext<'a> { core: TurnCore<'a>, gate: &'a Arc<PermissionGate> }

/// Duty routing: the core plus the two facts that travel with every duty
/// resolution. Deliberately gate-free — a duty route authorizes nothing.
struct DutyContext<'a> { core: TurnCore<'a>, local_engine, prompt_spend }
```

`TurnCore` is shared by composition rather than duplicated in both structs. The
alternative — declaring the four fields twice — is the drift hazard LESSON-586
names ("two surfaces describing one state must not be able to drift"), and it
buys nothing.

`gate` is carried as `&'a Arc<PermissionGate>` because `permission_gate_for`
returns `Arc<PermissionGate>` and `build_tools` wants the `Arc` while the other
two consumers want `&PermissionGate` — the `Arc` form derefs to both.

### ADR-2: `turn_loop.rs` is out of scope for this REQ

`turn_loop.rs`'s four suppressed functions carry a third cluster: `tools`,
`tool_ctx`, `gate`, `events: &SessionEvents`, `ctx`, `config: &HarnessConfig`,
`hook`. Note the **types differ** from the turn cluster's — `&SessionEvents`
not `&Arc<EventBus>`, `&HarnessConfig` not `&Config` — and there is no
`session_id` and no `router`. `run_session_turn`'s own doc comment explains
why there is no `session_id`: it is read off `events`, deliberately, "so the
prefix cache and the event attribution cannot name two different sessions."

Bundling that cluster is real work with its own rationale to preserve, and it
is a *harness*-layer concept while this REQ's is a *runtime*-layer one. Folding
both into one REQ would produce exactly the wide struct the requirement warns
against. Its 4 suppressions stay; REQ-599 inherits the question.

### ADR-3: `TurnContext` does **not** own `route` — OQ-1 answered

The requirement's entity table listed `route` as a field "set once routing has
resolved; absent before", and OQ-1 asked whether that should instead be a
typestate. **Neither.** `route` stays an explicit parameter.

The evidence is at the call site. `run_one_attempt` is invoked inside a
`'turn: loop`, and `route` is **reassigned on every fallback reroute** within
that loop. A context owning `route` would therefore have to be rebuilt each
iteration — which buys nothing over passing it — or it would go stale, which is
a bug. An `Option<Route>` field has the same problem and adds an unrepresentable
state; a typestate split has it too, plus a much larger diff.

There is a second reason, and it is the stronger one: the reroute is
*ordering-dependent logic*, and BR-7 asks that such logic stay visible. A
`route` parameter in the signature is that visibility. Hiding it in a context
would make the one thing that changes per attempt look like the things that
do not.

### ADR-4: The construction point is after the **last rebinding**, not after the claim — BR-2 generalized

This is the highest-risk decision in the REQ and the one most likely to ship a
silent behavior change.

BR-2 requires `TurnContext` construction after the turn is claimed. That is
necessary but **not sufficient**, and taking it as sufficient would introduce a
real defect. Tracing `run_prompt_turn`:

| line | event |
|---|---|
| ~4937 | `_claim` taken |
| ~4953 | `session_cwd` re-read from the registry (BR-2 / LESSON-539) |
| ~4987 | `config` bound |
| ~4999 | `gate` bound |
| ~5035 | `router` bound |
| **~5063–5099** | **`router` SHADOW-REBOUND by the REQ-580 warming hold** |
| ~5192 | `build_tools` |
| ~5305, ~5420 | `offer_or_refuse_over_budget` |
| ~5578 | `run_one_attempt` (inside `'turn: loop`) |

When the local tier is warming, `hold_for` parks the turn, and on wake
`let router = self.turn_router(&config, &session_id)` builds a **fresh** router
from the settled tier state and re-dispatches the route. A `TurnContext`
constructed at 5035 — which satisfies BR-2 — would carry the **pre-hold**
router past the rebind, and every downstream consumer would route against a
tier state that no longer exists. That silently breaks REQ-580's stated
guarantee: "a turn served after the wait must be built from the route it is
served *by*."

**Decision**: `TurnContext` is constructed **after the hold's `match`
resolves**, where all five fields have reached their final binding for the
turn. `DutyContext` is constructed inside `run_one_attempt`, after the one
`local_engine` slot read.

**Generalization (LESSON-586's rule).** BR-2 names one instance — the claim —
of a class: *a context must not be constructed before any point that rebinds a
field it captures*. The claim is one such point; the warming hold is another,
and it was not named. The requirement gains **BR-2.1** stating the class, and
the guard is mechanical rather than a comment: a test asserts the constructed
context's `router` is the post-hold one on a warming-tier turn (TASK-297).

### ADR-5: The traceability sweep keys on the hazard, and carries a vacuity floor

AC-8 (as amended in Phase 1) needs three arms. LESSON-585 supplies the shape:
key the sweep on the **hazard**, not on the remedy, and put a floor under it,
because a sweep's failure mode is seeing *less*.

- **Disappearance** — workspace-scoped, so a genuine file-to-file move is not a
  false positive.
- **Re-association** — id → owning-item mapping preserved.
- **Orphaning** — the hazard arm. A doc-comment run no longer adjacent to an
  item. This is the arm that catches the REQ-596/597 defect, where a method was
  inserted between `config_snapshot`'s doc comment and its attribute: no id left
  the file, so the first two arms are blind to it.
- **Vacuity floor** — the sweep asserts it saw at least the number of ids and
  annotated items known to exist. Without it, a selector bug makes the sweep
  pass by matching nothing.

### ADR-6: The count ratchet greps source, not clippy output

AC-1's regression test must see the two `engine.rs` sites, which live behind
`#[cfg(feature = "llama")]` and are compiled by neither CI nor AC-3's command.
LESSON-515 is precisely this failure ("a feature-gated target is invisible to
every refactor" — a `SessionId` parameter added in REQ-564 shipped broken in
0.1.14 because the gated call site was never type-checked).

A ratchet driven by clippy output would silently stop counting them, and the
number would drift down for the wrong reason. The test walks the source tree.

## Out of scope, and explicitly so

- `turn_loop.rs` (ADR-2), `engine.rs`, `category.rs`, `main.rs`, `budget.rs`,
  `carry.rs`, `skill.rs` — 11 suppressions total stay.
- Splitting `runtime.rs` (REQ-599).
- Any behavior change noticed in passing. File it; do not fold it in.

## Expected suppression arithmetic

Reported in the PR body split by population, per AC-1:

| population | count | mechanism |
|---|---|---|
| baseline | 25 | |
| **(a) vestigial** removed | 7 | 2 stacked AC-2 duplicates + 5 `*_route` fns already at the 7-arg threshold |
| **(b) earned** removed | 5–7 | `build_tools`, `spawn_title_session`, `resolve_duty`, `build_duty_route` collapse below threshold; `offer_or_refuse_over_budget` collapses to 8 → clean only if `invoker` also moves into the context |
| expected remainder | 11–13 | includes 2 `engine.rs` (feature-gated), 4 `turn_loop.rs`, 1 each `main.rs`/`budget.rs`/`carry.rs`/`skill.rs`/`category.rs`, and `run_prompt_turn` + `run_one_attempt` |

`run_prompt_turn` keeps its suppression: it is the constructor site and
receives its parameters off the wire. `run_one_attempt` likely keeps its own —
it drops 5 but still carries `route`, `phase`, `tools`, `tool_ctx`,
`stream_events`, `ctx`, `prompt_spend`, `pressure`. **The final number is
measured, not predicted**; the range above is a bound, and the PR body records
what actually happened.
