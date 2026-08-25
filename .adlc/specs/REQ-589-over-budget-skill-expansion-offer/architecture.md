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
new variant is a **compile-time forcing function**. Verified after TASK-241 landed, the
forced sites are exactly three: `render_event` (`session_ui.rs:522`), `consent_gate`
(`:2837`), and `render_consent_subject` (`:3041`). `tetond` compiles clean — it holds no
exhaustive match on either enum.

**Correction (TASK-241).** This ADR originally listed `resolve_permission` (`:2891`) as a
forced site. It is not: it matches on `consent_gate`'s `ConsentGate` result, not on the
subject, so it does **not** redden. This matters more than a footnote, because
`resolve_permission` is where BR-4's silence-is-never-consent behaviour actually executes.
The compiler will not point TASK-251 at it. That arm must be reasoned about deliberately
and tested directly — it is precisely the LESSON-508 shape, a guard whose absence is
silent.

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
LESSON-528 verbatim and one of the lessons retrieval surfaced for this REQ.

**Amended after TASK-239 (ratified).** This ADR presupposed a *typed* engine refusal to
match on. There was none: `over_window()` returned `EngineError::Backend(String)`, and
`EngineError` had only `Unavailable` and `Backend`. So within the task's stated file list
the only way to recognize a window refusal was to match the string — the very thing the
ADR forbids. TASK-239 therefore added `EngineError::ContextWindowExceeded` in
`crates/teton-inference/src/engine.rs`, outside its declared ownership, and flagged it
rather than forcing it. **Accepted.** The `Display` text is byte-identical to the old
`Backend` payload so existing assertions pass unedited, and no exhaustive `match` on
`EngineError` exists anywhere (verified), so the blast radius is nil. Without it the real
`LlamaEngine` would still yield `INTERNAL_ERROR` and the task would be theatre.

**Shape:** a sibling `HarnessError::LocalContextLengthExceeded`, not
`provider_id: Option<String>`. Absence-means-local is true only while exactly two
`CompletionSource` impls exist; a third would falsify it silently at every read site. The
sibling keeps the remote variant and its `Display` byte-identical, and `context_refusal()`
is the tier-agnostic projection downstream consumers read instead of matching — the
`privacy_block_detail` precedent already in that file.

**Known state:** `cargo build --workspace` is RED from TASK-241's variant until TASK-251
lands the three CLI arms. That is ADR-2's forcing function, not a regression. No task may
claim a green workspace before TASK-251.

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
end-to-end coverage. The lesson's force is concrete here: without the suspension the turn
*sheds history and usually succeeds*, so every end-to-end leg stays green while the
conversation quietly shrinks. There is no natural failing signal.

**Implemented (TASK-245), better than specified.** `PressurePolicy` is a two-variant enum,
deliberately neither `Copy` nor `Clone`, taken **by value** by
`run_session_turn_with_pressure_policy`. Its only consumer is
`enforces_this_iteration(&mut self)`, which is `mem::replace(self, Enforced)` — so there is
no reset statement to forget and the method structurally cannot answer `false` twice.
Leaking across turns or iterations is a compile error rather than a discipline.
`run_session_turn_with_source` keeps its exact signature and delegates with `Enforced`, so
~20 existing callers compile untouched.

A `HarnessConfig` field was considered and **rejected**: that struct is a route's long-lived
settings borrowed by every turn, so a flag there must be set and unset *around* a turn —
precisely the shape D-7 asks not to depend on.

**Two edges named rather than silently widened.** The `max_turns` and `EndTurn` exit gates
are NOT suspended: they bound what the *next* turn carries, after this turn's prompt was
already assembled. And a degenerate `max_turns == 0` would return through the truncating
`max_turns` exit before any model call, so an accepted turn could lose blocks without
sending anything — unreachable from `HarnessConfig::default()`, recorded here rather than
expanding the exception to cover it.

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

### ADR-13 — Wire vocabulary decisions taken during TASK-241, and kept

Four deviations from the spec's entity table were made in implementation and are ratified
here rather than reverted:

- **`overrun_words`/`overrun_bytes` are not on the wire.** Both are a `saturating_sub` of
  two fields already present. Carrying them would be two ways to say one fact — LESSON-545's
  shape. A test pins their absence.
- **No `remedy` field on the subject.** Per ADR-1 the remedy rides the option ids and their
  labels. A subject-level remedy could claim a fix while the offered options contained none.
- **`source: SkillSource` was added.** ASSUME-018 requires a project-sourced skill name to
  render under the distinguishing treatment project skills already get, and the client
  cannot infer that from a name alone.
- **`measured_tokens`/`budget_tokens`, not `_words`** — matching what `RouteDecided` and
  `ContextPressure` already call these figures on the wire.

**Two hazards this created, for downstream tasks:**

1. **Name collision.** The protocol's new `SkillStage` shares its name with
   `tetond::harness::budget::SkillStage`; the daemon type carries the refusal clause it
   words, so it could not be re-exported. TASK-247 must map between them explicitly.
2. **`SkillStage`/`WindowVerdict`/`RemedyKind` each carry `#[serde(other)] Unknown`,** and
   the reasoning is load-bearing: the `PermissionSubject` `kind` tag stays closed and
   fail-closed, but an unknown *value inside a known kind* would fail the entire
   `permission_request` frame — rendering nothing and parking the daemon's waiter, which
   has no timeout. BR-4's refusal would never fire. So tolerance there is deliberate.
   `WindowVerdict::Unknown` MUST render as a hedge and must never be silently relabelled
   `WindowUnknown`, which is a different, specific claim about the route.

### ADR-14 — Gate decisions taken in TASK-244, ratified

- **The offer does not consult grants.** It is asked under the same `skill:` key
  `authorize_skill` remembers a dynamic-context answer under, so a remembered
  "allow for this session" on a skill's command slots would otherwise settle every later
  oversized expansion of that skill — a grant answering a question nobody asked. BR-10's
  non-persistence is therefore two guards, not one: nothing is written, AND nothing
  already written is read. `interpret_over_budget` takes no `&self` so the grant map is
  not in scope.
- **`LevelAllow::DoesNotSettle` — a `full` session still asks.** `full` means "do not ask
  me about tool calls"; an over-budget expansion is a turn the daemon has *measured* and
  expects the provider to reject. Letting an allow row settle it would convert today's
  refusal into an oversized send nobody approved, in precisely the posture where nobody is
  watching. `deny` still denies, so `plan` still refuses.
- **BR-3's "leads with the remedy" is option ORDER**, not a separate field.
- **`remedy_only` narrows to `SkillConsent::Declined`** — a human decided this turn does
  not run — with `apply_remedy` riding beside the consent rather than widening it. ASSUME-B
  holds: no `SkillConsent` arm was added.

### ADR-15 — The window comparison excludes the reservation, and the sentence says why

TASK-242 surfaced that the reachability table's `Window` + `FitsWindow` row survives only
under one reading. Measured against `window − reservation` — the figure `derive` budgets
from — over-budget would imply over-window by construction on a `Window` route, and that
row would be unreachable (the floor does not rescue it: the derived pair is componentwise
≥ the window-derived pair on every route).

**Decision: compare against the RAW declared window.** BR-3's own framing is that the
reservation is *this daemon's* policy while the window is *the provider's* bound. The
`FitsWindow` band on a `Window` route is therefore exactly the reservation band, and the
row is reachable. `a_window_bound_route_can_still_fit_the_window_it_declared` pins the
boundary (85,333 vs 85,334 words against a 128,000 window).

**Consequence for TASK-243 — BR-3's wording must be weakened here.** "The send is expected
to serve" is too strong for this band: the prompt fits the declared window but leaves
little or no room for the *reply*, because the reservation it is eating is precisely what
the budget set aside for generation. On `Window`/`UserCap` + `FitsWindow`, the sentence
must say the prompt fits the declared window but may leave little room for the response —
never an unqualified promise. `LocalEngine`/`DefaultUnknown` are unaffected (no window
fact), and `ExceedsWindow` is unaffected (already the strong warning).

### ADR-16 — The daemon words the offer; the client only presents it

TASK-243 found that of the three sentences it composed, only the four option **labels**
reach a reader. `PermissionSubject::SkillOverBudget` is a structure the client renders and
`authorize_skill_over_budget` takes no description — so the verdict clause, BR-7b's
no-durable-fix sentence and BR-14.2's observed-rejection lead have **no surface**. They
would ship dead: a producer with no consumer, invisible to a green suite (LESSON-544).

The tension is real: BR-5 requires ONE composer, while the structured subject exists so the
client can render. Resolving both:

**The composed sentence rides on the subject as a field. The client renders it verbatim and
re-words nothing.** The structure stays — the client uses it for presentation (layout,
emphasis, which option rows to draw) and for the `Unknown` hedge — but the *words* have one
home, in `skill_refusal`. A client that re-worded from the structure would be the second
composer BR-5 forbids, and the two would drift the first time either changed.

Rejected: composing client-side from the structure (two composers), and dropping the
sentences (three true things about a route the user never sees, on the one surface whose
job is saying truthfully what happened).

### ADR-17 — `RedactScan` + `FitsWindow` gets its own clause (ratified)

ADR-15 named only `Window`/`UserCap` and excused the window-less bounds, leaving
`RedactScan` + `FitsWindow` — which the reachability table says occurs — unaddressed.
Pasting the reservation sentence there would be **false**: that band is a byte clamp on the
egress scanner, not the generation reservation. TASK-243 split the arm on the bound and gave
`RedactScan` a clause claiming neither of AC-6's two facts. **Ratified.**

### ADR-18 — Three architecture claims TASK-250 could not satisfy, and what shipped

1. **ADR-12's "presented as a choice" is not implementable on ADR-1's wire.**
   `OverBudgetOptionLabels` carries ONE optional remedy pair and `interpret_over_budget`
   recognizes exactly four ids. An N-way provider choice needs a fifth id family or a second
   prompt. **Shipped: the half ADR-12 exists to guarantee** — at 2+ providers the option is
   *withheld*, with a record line naming every candidate and the `teton policy set-tier`
   command. Nothing is ever picked silently. The choice UI is a follow-up, and ADR-12 is
   partially discharged, not fully.

2. **BR-9's "names the tier, the provider, and the cost consequence" is unsatisfiable
   today.** `Remedy::BindTierRemote { tier }` carries no provider — `Remedy::for_bound`
   drops it deliberately so the remedy cannot be addressed to the provider the route is
   *leaving*. So the sentence and label say "a remote provider" where BR-9 and ADR-1's
   name-the-concrete-write rule require the actual name. At one configured provider we DO
   know it (we bind by name). **TASK-260 fixes this.**

3. **ADR-4 inherits `apply_config_update`'s posture, not `handle_config_set`'s.** The remedy
   gets validation, `reject_unusable_binding`, atomic persist and identical refusals — but
   NOT `refuse_daemon_wide` (REQ-570 BR-10(a)) or `refuse_unattested_commitment`
   (BR-10(b)), which wrap that body in `server.rs` and would need `&Daemon`/`&ConnState`
   threaded into a turn. **The durable config write therefore does not pass the daemon-wide
   commitment gates.** The gap is narrow — presence degrades to allow on a shipped build
   anyway, and the write is authorized by an addressed consent from the submitting
   connection — but it IS a deviation from ADR-4 as written, and it is exactly what a
   security reviewer should examine.

   **Flagged for Phase 5 — and my stated mitigation is REFUTED (TASK-254).**

   ADR-18 originally excused this on the grounds that the write is authorized by an addressed
   consent from the submitting connection. TASK-254's counter-argument stands:
   `refuse_daemon_wide` exists specifically to stop the daemon's OWN CHILDREN from making
   daemon-wide commitments, and **a consent answer arriving over a connection is not evidence
   about that connection's ancestry.** They are different controls; one does not substitute for
   the other.

   So the exposure is not AC-20's — nothing claims an attestation, and that is now pinned under
   both presence postures. It is **BR-10(a)'s**: BR-8 states the remedy "mints no new
   authority", and on the ancestry leg it arguably does. A daemon child that can reach a session
   turn can cause a durable `config.toml` write that `handle_config_set` would have refused it.

   `the_remedys_durable_write_does_not_pass_the_daemon_wide_commitment_gates` pins the contrast:
   the same payload the wire refuses under `TETON_PRESENCE_ACCEPT=fail` is applied by the seam
   the remedy uses. **This is a security decision for the owner, not a documentation note.**
   Options: route the remedy through `handle_config_set` proper, thread `&Daemon`/`&ConnState`
   into the turn, or accept it explicitly with the reasoning recorded.

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

> **Mid-phase test runs see sibling churn.** While Phase-4 agents run concurrently in one
> worktree, `cargo test` picks up other agents' UNCOMMITTED edits. TASK-258 observed 24
> failures that were entirely TASK-248's and TASK-259's in-flight `runtime.rs`/`router.rs`.
> The correct response is what it did: revert your own files on disk (no index writes),
> re-run the failing targets, and compare the failing-name sets. A failure count taken mid-
> phase is evidence about the tree, not about your change. The authoritative run is Phase 5,
> after every task has committed.


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
