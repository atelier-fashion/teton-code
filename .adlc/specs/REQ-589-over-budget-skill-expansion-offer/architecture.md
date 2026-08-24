# REQ-589 — Architecture

## Approach

The offer replaces a hard refusal at **Stage A** (`runtime.rs:3601`) with a question, on
the user-typed path only. Nothing about the *measurement* changes: `skill_fit` →
`would_seed_fit` and the router's stamped `RouteBudget` remain the single sources of the
figures, because a second estimator is exactly what REQ-586's own verify pass caught.

What is new is a **classifier** (`window_verdict` × `BudgetBound` → `Remedy`), a **fourth
consent question** on the existing permission wire, a **one-turn suspension** of the
pressure gate, and a **session-scoped memo** of observed provider rejections.

Three of the spec's premises turned out to be false, and the exploration phase is where
they were caught. Each is now an ADR rather than a surprise in Phase 4: the typed context
outcome does not exist on the local tier (ADR-3), there is no trust gate on the typed
path (ADR-10), and `config/set` cannot write two rows atomically (ADR-5).

## Key decisions

### ADR-1 — The offer is a widened single-select on the existing consent wire

`PermissionOutcome::Selected { option_id }` is single-choice. The spec's `OfferAnswer
{ proceed, apply_remedy }` implies two independent booleans, which the wire cannot carry.

**Decision:** express the four combinations as four named option ids —
`over_budget_proceed_once`, `over_budget_proceed_and_remedy`, `over_budget_remedy_only`,
`over_budget_decline` — in a `Vec<PermissionOption>` widened conditionally, exactly as
`options_for` (`permissions.rs:2055`) appends `enable_permanent` only when a `WebTier` is
in hand. The remedy-bearing options appear **only** when BR-7 grants that bound a remedy.

**Rationale:** no protocol change, no new consent vocabulary (keeping ASSUME-B's promise),
and it reuses a precedent that already solved the same shape. Rejected: widening
`PermissionOutcome` (protocol churn for one caller), and two sequential
`PermissionRequest`s (two prompts for one decision, and the second is unanswerable if the
first is declined).

**Binding constraint from the precedent:** the `enable_permanent` label *names the key it
writes* (`[web] permission_allow += "…"`), and its comment records that an earlier version
promised a write that was silently a no-op. Every remedy option label MUST name the
concrete write — `capabilities.max_context = 1000000` for `kimi` — never "raise the limit".

### ADR-2 — A new `PermissionSubject::SkillOverBudget` variant

`PermissionSubject` is `#[serde(tag = "kind")]` and matched exhaustively client-side, so a
new variant is a **compile-time forcing function**: `consent_gate` (`session_ui.rs:2836`),
`resolve_permission` (`:2891`), and `render_consent_subject` (`~:3042`) cannot silently
skip it. That is the property we want for BR-4 — the no-terminal and `Unrecognized` arms
must map to refusal, never to proceed.

The subject carries only measured integers, the bound, the verdict, the skill name, and a
sanitized provider id. **No provider response body** is in scope, preserving the invariant
`a_skill_refusal_carries_no_provider_response_body` pins.

Per **ASSUME-018**, the skill name in a project-sourced offer is repository-authored text.
It renders under the same distinguishing treatment project skills already get, never as
bare harness vocabulary.

### ADR-3 — The typed context outcome is extended to the local engine (D-11)

**Problem.** `HarnessError::ContextLengthExceeded` is constructed only at
`completion.rs:535` and `:1259`, both from `ProviderError::ContextLengthExceeded` on the
**remote** path. `LocalEngineSource::produce_turn` can only fail with
`HarnessError::Engine(EngineError::Backend(..))`, which `runtime.rs:4057` maps to
`INTERNAL_ERROR`. BR-3, BR-12 and BR-14.1 all name the typed outcome as their backstop —
on the local tier, which is the route the reported `/analyze` failure ran on.

**Decision.** `LocalEngineSource` produces the typed outcome when the engine refuses for
window reasons. This is the **head of the DAG**: BR-14.1's withdrawal trigger depends on
it.

**Rationale.** The alternative is matching the string `"the local engine could not serve
the turn"` — a predicate mirrored away from the precondition that made it true, which is
LESSON-528 verbatim and one of the lessons retrieval surfaced for this REQ. `WindowedEngine`
(`completion.rs:1865`) already exists as the instrument to test it.

### ADR-4 — Every remedy writes through `config/set`; no second write path

`PermissionGate::persist_web_tier` (`permissions.rs:1888`) is a tempting precedent — a
durable config write performed from *inside* a consent answer, explicitly not a BR-10(b)
commitment because it is raise-only. Structurally it is very close to `apply_remedy`.

**Decision: rejected.** All four remedies route through `config/set`, inheriting its
posture verbatim including the known one (REQ-570 BR-10(b), `presence` non-default,
`refuse_unattested_commitment` degrading to allow with a stderr line on a shipped build).

**Rationale.** A second durable-write path for the same class of fact is the shape
LESSON-456 exists to prevent, and `architecture.md:169-172` already warns against
proliferating config-write flows. One path, one posture, one place to audit. The cost is
that raise-only remedies carry a heavier gate than they strictly need; that is the correct
trade against a divergent second path.

### ADR-5 — Ordering replaces atomicity for the two-write remedy (D-12)

AC-8 demands the `BindTierRemote` pair be "applied together or not at all". `config/set`
persists one `ConfigUpdate` per call and `architecture.md:169-172` forbids generalizing it.

**Decision.** Do not pursue atomicity. **Order the writes so the forbidden state is
unreachable:** `RegisterProvider` carrying `max_context` **first**, `SetTierBinding`
**second**.

**Rationale.** The failure AC-8 names is specifically a half-applied remedy that leaves
`max_context = 0` on a newly-bound remote tier — the original circle. That state is
reachable only from the *reverse* order. In the chosen order a partial failure leaves a
declared window on a tier still bound locally: no circle, no routing change, nothing the
user must undo. The invariant is bought with sequencing rather than with a new multi-op
envelope or a fourth preview/commit trio.

### ADR-6 — The recipe lookup matches `example_model`, and must actually clear the refusal

BR-7c proposes a window value from `provider_recipes.rs`. Two constraints:

1. **Match on the registered provider's `model` against a recipe's `example_model`**, not
   on `id_suggestion` — a recipe id is only a suggestion and the user may register under
   any id. This imitates the existing production caller at `runtime.rs:6746`.
2. **Never propose a value that would not clear the measured expansion.** Ollama's recipe
   window is **4,096** — smaller than Teton's own local pair. Writing it faithfully would
   propose a window that cannot resolve the refusal and would derive a `floored` budget.
   Where the recipe value would not clear the measurement, the remedy is not offered with
   that number; the offer asks for a value instead.

Where the provider matches no recipe, **ask; never invent** (BR-7c). ASSUME-016 is the
standing warning here: every recalled vendor window in REQ-586 was wrong.

### ADR-7 — `ProviderRecipe` gains `verified_on` (OQ-2)

The verification date exists only in `//` comments beside each catalog entry; there is no
field to read at runtime. Promote it to data and record it alongside the written value, so
a later `/doctor` can distinguish a window the user measured from one inherited from a
recipe. The write is **not** blocked on freshness.

### ADR-8 — BR-12's suspension gates exactly two calls, for exactly one iteration

The top-of-loop drop is not a named function. It is two calls inside
`run_session_turn_with_source` (`turn_loop.rs`, from 860):

```
941   let compaction = ctx.compact_if_pressured(compact).await;
...
954       &ctx.truncate_to_budget(),
```

**Decision.** A per-turn flag, threaded from `runtime.rs` (where the accept answer is
known) into the loop, skips **exactly** those two calls on the **first iteration** of the
accepted turn, and clears before the second. Scope is per-turn, never per-session.

Per **LESSON-508**, this suspension is a redundant guard whose deletion would be silent —
it gets its own seam-level unit test asserting the flag's removal reddens, not only
end-to-end coverage.

### ADR-9 — The observed rejection follows `EffortRefusals`, and is not a grant

BR-14.2 needs "this skill on this route was rejected at the window" remembered for the
session. `EffortRefusals` (`runtime.rs:551`) is the near-exact precedent: session-scoped,
**never persisted**, keyed by `(SessionId, provider_id)`, with `mark()` returning the
first-time transition so the caller can announce once, and a doc comment that already says
*"Remembering is not retrying."*

**Decision.** Same shape, keyed by `(SessionId, skill, route)`.

**Two boundaries, both load-bearing:**
- It must not suppress or pre-answer the next offer (BR-10). A stored *observation* can
  only make the next question better informed; a stored *consent* could send something
  nobody approved.
- Per **ASSUME-017**, it lives in **one store, daemon-side only**. The CLI must not
  memoize it, or a stale record replays into a later session.

Per **LESSON-543**, the system prompt needs a resident fact stating that consents are not
persisted and observations are — otherwise the model will tell the user it "remembers"
a consent it does not have.

### ADR-10 — The project-skill trust gate is built on the typed path (D-10)

`authorize_project_skill_trust` has exactly one production caller,
`harness/tools/skill.rs:1572`, on the **model-invoked** path. `accept_invocation`
(`runtime.rs:2904`) is synchronous and gates nothing. So a user who types `/name` today
runs a project-authored skill body with no acknowledgment.

**Decision.** Introduce the acknowledgment on the typed path: `accept_invocation` becomes
`async`, and the gate is called **before** Stage A, so nobody authorizes an over-budget
send from a repository they have not said they trust. A declined trust yields the trust
refusal and no budget offer.

**This is new functionality and an accepted scope increase**, not the reordering BR-6
originally described. The signature change reaches every caller — a compile-time forcing
function, which is why it is safe to make.

### ADR-11 — `/doctor` reports against the stamped route, or says there is none

A session that has sent no turn has no decided route, so "the skills that will not fit on
the current route" is not always a well-formed question.

**Decision.** Report against the session's stamped `RouteBudget` when one exists;
otherwise say *"no route decided yet"*. A diagnostic must not force a router resolution as
a side effect. Figures come from the same measurement and the same stamped budget as the
live path (LESSON-456), and the surface labels its answer a **floor** — `Body` stage only,
since a dynamic-context skill cannot be pre-measured.

### ADR-12 — `BindTierRemote` asks which provider when more than one is configured (OQ-1)

D-9 authorized *performing* the remedy, not choosing where a category's spend goes.
Exactly one configured remote provider may be proposed by name; two or more are presented
as a choice. A provider-enumeration helper on `Router` is new code.

## Reachability — normative, and it bounds the test matrix

Verdict and bound are **not independent axes**. Only these pairs occur; a test written for
any other cell passes vacuously (LESSON-520):

| Bound | Reachable verdicts | Remedy (BR-7) |
|---|---|---|
| `LocalEngine` | `WindowUnknown` only | `BindTierRemote` |
| `DefaultUnknown` | `WindowUnknown` only | `DeclareWindow` |
| `Window` | `FitsWindow`, `ExceedsWindow` | `RaiseWindow` |
| `UserCap` | `FitsWindow`, `ExceedsWindow` | `RaiseCap` |
| `RedactScan` | `FitsWindow`, `ExceedsWindow` | none (BR-7b) |

## Testing posture

Not decoration — REQ-585 and REQ-587 each shipped Critical defects past a green
~3,500-test suite:

- **LESSON-544 / LESSON-552** — drive every new wire fact end-to-end from a real turn.
  A struct-literal test leaves the producer unguarded; mutating a producer line must redden.
- **LESSON-519 / LESSON-520** — verify the config write by reading the file *and*
  re-parsing it (`config_preservation.rs:885` is the double-check exemplar), and pair every
  refusal test with an accepted counterpart on the same fixture.
- **LESSON-508** — ADR-8's suspension and BR-10's non-persistence are redundant guards;
  each needs its own seam test.
- **LESSON-546** — the recipe window value has one home, enforced by a resident test.
- **BUG-191** — PTY leg pinning the offer's rendered bytes (`pty_e2e.rs:1521` is the pattern).
- **Fixture gap:** `skill_turn.rs`'s `Harness` cannot build `LocalEngine`, `UserCap`, or
  `RedactScan` routes. Those legs go against `budget.rs`'s `remote(window, cap, redact_scan)`
  helper, `BudgetInputs::local()`, and `redact_egress.rs:1110`'s `router_for` pattern.
- **Suite hygiene:** run with `--no-fail-fast` (a failure count here is otherwise a floor),
  and build the workspace before targeted `-p tetond --test …` runs or a mutation can look
  survived against a stale daemon.
