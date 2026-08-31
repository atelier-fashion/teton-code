# REQ-600 — Architecture

## Approach

REQ-599 delivered seven seams and stopped one short of the one that mattered:
six of its steps moved *top-level* items and only `duty.rs` took methods out of
`impl DaemonRuntime`. That impl is **6,543 production lines** and
`run_prompt_turn` is **1,084** of them.

This REQ does two things in a deliberate order:

1. **Pin the ordering invariants that are not pinned** — on the code as it
   stands, before anything moves.
2. **Then** relocate the turn-path cluster and decompose `run_prompt_turn` into
   a named stage sequence, and flatten `run_session_turn_with_pressure_policy`.

The order is not stylistic. Three of the five invariants REQ-599 named as *the*
behavioural risk currently have no test that would fail if they were inverted.
Writing them after the restructure would test the new shape rather than the
invariant, and would give the restructure no net to fall into.

## Measured baseline, each figure with its rule

Verified at `b3c2a80`. Rules are stated because this REQ line has produced five
wrong answers to one question by pairing a count with the wrong rule
(LESSON-593, LESSON-597).

| quantity | rule | value |
|---|---|---:|
| `runtime/mod.rs` production | above the first **column-0** `#[cfg(test)]` | 10,306 |
| `impl DaemonRuntime` | the three `impl … DaemonRuntime` blocks, production only | 6,543 |
| — its method bodies | 89 spans, `fn` line to closing brace | 4,618 |
| — the remainder | doc comments and blanks between methods | 1,915 |
| `run_prompt_turn` | body span | 1,084 |
| — its max nesting | brace depth inside the fn | 8 |
| `run_session_turn_with_pressure_policy` | body span | 762 |
| — its max nesting | **brace depth inside the fn** (the rule AC-3 gates on) | **9** |
| — the same, other rule | indentation levels below the `fn` | 11 |
| — where the depth lives | lines at brace depth ≥ 7 | 200 |
| — the bulk | lines at brace depth 6 | 236 |

The depth-9 peak is [`turn_loop.rs:1859`](../../../crates/tetond/src/harness/turn_loop.rs).

**A measurement error worth recording**, because it nearly became a decision: a
first pass reported `run_session_turn_with_pressure_policy` at brace depth **5**
— already meeting AC-3 — by measuring a `turn_loop.rs` line range against
`mod.rs`'s lines. The correct figure is 9. The check that caught it was
re-deriving the number a second way and refusing to accept the disagreement.

## Key Decisions

### ADR-1 — OQ-1 answered: stages are `&self` methods taking `TurnContext`, with `route` explicit

The spec left open whether stages become free functions taking `TurnContext`,
methods on a `TurnStages` type, or an enum-driven sequence. The codebase already
answers it, and `crates/tetond/src/turn_context.rs` says so in its own header:
`TurnContext` exists because "every extraction from that file currently produces
another ten-argument function."

- **`TurnContext` carries the bundle.** `TurnCore` holds the four facts every
  per-turn function needs; `TurnContext` adds the gate and invoker.
- **`route` stays an explicit parameter.** Not an oversight — `turn_context.rs`
  ADR-3 excludes it deliberately, because `route` is rebound on every fallback
  reroute, and keeping it in the signature is what keeps the reroute *visible*.
  A stage sequence that hid `route` inside a context would erase the one thing
  the signature is carrying.
- **Methods on `&self`, in the `duty.rs` shape.** That module is the single
  precedent for taking methods out of this impl, and its five `*_route`
  resolvers are the house style: one small named method per concern, a shared
  helper behind them.

**Rejected — a `TurnStages` type.** `turn_context.rs` forbids exactly this:
"a context that starts answering questions becomes a second place for turn logic
to live, which is exactly what REQ-599 has to untangle." A `TurnStages` holding
state *and* behaviour is that second place.

**Rejected — an enum-driven sequence.** The stages are not uniform. Each
consumes and produces different values (measured below), so an enum would need a
payload per variant and a `match` that reconstructs the sequence the function
already expresses. It buys nothing and costs a layer.

### ADR-2 — The decomposition makes an invariant visible that is currently only commented

`TurnContext::new` is called at `mod.rs:4643`, and BR-2.1 requires it happen
after the REQ-580 warming hold at `4586`. Today those two facts are 57 lines
apart inside a 1,084-line body, and what holds them together is a doc comment.

After decomposition the orchestrator body reads:

```
let router = self.hold_for(&config, &route)…;      // the warming hold
let tctx   = TurnContext::new(events, &session_id, &config, &router, &gate, invoker);
```

— two adjacent named statements. The ordering becomes a property of the shape
rather than of a comment a future edit can drift away from. **This is the
strongest argument for the change**, and it is why AC-4's tests are worth
writing even if the rest of the REQ were abandoned.

### ADR-3 — Two stage groups, split by the pivot, and that split is the invariant

The stages before `TurnContext::new` cannot take a `TurnContext` — it does not
exist yet, and BR-2.1 says it must not. So:

- **Pre-pivot stages** take explicit parameters and return small named values.
- **Post-pivot stages** take `&TurnContext` plus `route`.

This is not an inconsistency to apologise for. The type system now refuses to
let a pre-hold stage read a post-hold context, which is the invariant BR-2.1
states in prose.

Measured stage boundaries and the values that cross each one — the evidence that
these are real seams rather than convenient line numbers:

| stage | lines | values escaping to later stages |
|---|---:|---|
| claim the session | 71 | 2 (`turn_id`, `probed`) |
| assemble config, gate, skills | 27 | 3 |
| resolve the route (incl. warming hold) | 106 | 5 |
| **`TurnContext::new` — the pivot** | — | 1 (`tctx`) |
| assemble harness, tools, system prompt | 157 | 7 |
| settle expansion and budget | 227 | 8 |
| run the attempt loop | 423 | 4 |
| commit the outcome | 24 | 0 |

No boundary leaks more than 8 values, and the largest stage — the 423-line
attempt loop — leaks 4. A boundary that needed fifteen values would not be a
seam; these are.

### ADR-4 — Write the three unpinned invariant tests first, on unrestructured code

Of the five BR-3 invariants, exploration found:

| # | invariant | today |
|---|---|---|
| 1 | typed-outcome arms before the generic remote arm | **PINNED** — 3 tests |
| 2 | gates before the parses they guard (LESSON-520) | **comment only** |
| 3 | the claim before the session-state read (LESSON-539) | **comment only** |
| 4 | presence gates keep reader-loop freedom (LESSON-518) | **no test located** |
| 5 | `TurnContext` construction after the warming hold | **PINNED** — 1 test |

AC-4 requires all five to fail on inversion. Three must therefore be written,
and they must be written **before** the restructure:

- written after, they would pin the new shape, not the invariant;
- written before, each can be shown to fail on inversion against the code that
  motivated the invariant in the first place.

Every inversion is **run and its observed output recorded** — REQ-602 shipped a
mutation table containing an outcome that could not occur, which meant a bound
nobody had seen fire (LESSON-597).

### ADR-5 — AC-2's target requires the whole turn-path cluster, not `run_prompt_turn` alone

The arithmetic, under the baseline table's rule, counting each method's span
**including its doc block**:

| move | lines removed | `impl DaemonRuntime` becomes | meets ≤ 4,500 |
|---|---:|---:|---|
| `run_prompt_turn` alone | 1,085 | 5,458 | **no** |
| the full turn-path cluster | 2,815 | **3,728** | yes |

The cluster: `run_prompt_turn` (1,085), `offer_or_refuse_over_budget` (305),
`accept_invocation` (256), `run_one_attempt` (209), `settle_dynamic_context`
(185), `apply_over_budget_remedy` (176), `unserved_turn_error` (170),
`build_tools` (154), `permission_gate_for` (87), `dispatch_route` (47),
`unserved_turn_error_announcing` (42), `turn_router` (29), `hold_for` (21),
`classify_freeform` (21), `record_user_prompt_urls` (18), `user_urls_for` (10).

Every one is on the turn path, so none is excluded by Out of Scope — which
excludes `derive_provider_setup` (324) and `provider_test_within` (310), the
second and third largest methods in the impl, precisely because they are not.

Had the target been left for this document to choose, 5,458 would have been a
tempting number to write down. It was fixed in the spec first, which is the
point of having fixed it there.

### ADR-6 — The derived checks are part of the change, and were tested rather than predicted

REQ-599 broke seventeen source-scanning checks by moving code (LESSON-594). The
blast radius here was **measured by planting a probe module** under `runtime/`
and running the guards, not by reading them:

| guard | new module under `runtime/` | why |
|---|---|---|
| `runtime_module_map.rs` | **fails** | demands every module appear in REQ-599's architecture map table |
| `runtime_visibility.rs` | passes | corpus enumerated from disk; `MUST_BE_PRESENT` asserts listed files *exist*, not that the set matches |
| `runtime_doc_paths.rs` | passes | same, corpus enumerated from disk |
| `traceability_sweep.rs` | passes | recursive since REQ-602 |

That REQ-602 made three of these four absorb a new module is the concrete return
on landing it first.

Four further checks assert on *content* and will need repair as code moves —
each verified by reading the assertion:

- `mod.rs:25940` — `.offer_or_refuse_over_budget(` appears **exactly twice**,
  "both `run_prompt_turn`'s own budget stages". Decomposition moves both.
- `taint.rs:1064` — exactly one setter call site, in the `web/override` handler.
- `projects/scan.rs:518` — `pub(crate) fn store_session_skills(` exists under
  `runtime/`.
- `suppression_ratchet.rs:42` — `REACHED = 13`, bounded on **both** sides.
  `turn_loop.rs` carries 4 `too_many_arguments` suppressions; flattening it may
  remove some, which trips the *lower* bound. That is good news the ratchet is
  designed to make you state out loud.

### ADR-7 — Commit order by blast radius; AC-7 needs CI to be allowed to finish

Order: invariant tests → cluster relocation → stage decomposition → `turn_loop`
flattening → derived-check repair → verification. Tests first (they are the
net), pure relocation before control-flow change (so a bisect separates "moved"
from "restructured"), and the doc-only repairs last.

**AC-7's obstacle is mechanical and known.** `.github/workflows/ci.yml:10-12`
sets `concurrency: group: ci-${{ github.ref }}` with `cancel-in-progress: true`.
Pushing step *n+1* cancels step *n*'s still-running `macos-latest` job — which is
exactly how REQ-599's identical criterion ended up NOT MET, on the one runner
that has caught a real ordering defect in this line (LESSON-591).

This REQ satisfies AC-7 by **pushing each step and letting its CI finish before
pushing the next**. Changing the concurrency group to include the SHA would fix
it for every PR, but that is a repo-wide CI change outside this REQ's scope and
is flagged for the user rather than taken unilaterally.

## Task Graph

```
TASK-308  pin the three unpinned BR-3 invariants        (AC-4)   ──┐
                                                                  │
TASK-309  relocate the turn-path cluster -> runtime/turn.rs (AC-2) ┼─> TASK-310  decompose run_prompt_turn (AC-1)
                                                                  │        │
TASK-311  flatten run_session_turn_with_pressure_policy (AC-3) ────┘        │
                                                                           │
TASK-312  repair the derived checks + document the map  (AC-6) <───────────┘
                                                                           │
TASK-313  final verification, every figure with its rule (AC-5, AC-7) <─────┘
```

TASK-311 depends only on TASK-308: it is a different file with a different
parameter cluster, so it can run alongside the relocation.
