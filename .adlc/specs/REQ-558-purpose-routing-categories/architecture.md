# REQ-558 — Architecture

## Approach

The REQ frames this as "replace `Phase` with a category as the dispatch key". The
exploration changed that framing in one material way, and the whole design follows
from it.

**There are exactly three model call sites in the daemon.** Turn completion
(local, `harness/completion.rs:185`), turn completion (remote,
`harness/completion.rs:312`), and `summarize_if_large` (`harness/context.rs:660`,
hardcoded to the local engine with a mechanical-truncation fallback). That is all
the model traffic that exists.

So of the eleven categories:

| | Categories | Call site |
|---|---|---|
| **Real traffic** | `edit`, `design`, `debug`, `review` | all four are *the same* turn-completion call — they differ only in what the classifier says |
| **Real traffic** | `digest` | `summarize_if_large`, currently local-only and unrouted |
| **Must be built** | `route` | the classifier does not exist; today it is the `AUXILIARY_SIGNALS` substring match |
| **Declared, unreached** | `redact`, `title`, `compact`, `triage`, `shell` | no model call exists at any of these points |

Six of the seven "harness-known" categories have no call site. The spec
anticipated some of this ("a category with no call site is **declared but
unreached** in v1, which is honest") but the fraction is much larger than that
sentence implies, and it is the single most important thing to be clear about:
**this REQ ships five categories with traffic and six without.**

That is still the right REQ. What it delivers is not "eleven configurable
models" — it is:

1. `AUXILIARY_SIGNALS` deleted, so `"explain the tradeoffs…"` stops going to a 3B
   model (AC-1, the direct defect).
2. The configured table read on **every** turn including freeform, so
   `teton policy set` stops being inert (BR-1, the headline defect).
3. A real classifier replacing a ten-word substring list.
4. `digest` routed instead of hardcoded.
5. A config schema and a resolver that are **complete for all eleven**, so the
   remaining six call sites can be tagged later without another config migration.

Point 5 is the reason to declare all eleven now rather than only the five that
work: the schema stabilizes once.

## Key Decisions

### ADR-A: All eleven categories are declared; the six without call sites are marked unreached, and a test keeps the marker honest

**Decision**: `Category` is the full eleven-variant enum. The resolver handles all
eleven (AC-2 iterates all eleven). `teton policy show` renders a category with no
call site as `declared, no call site yet`. A test enumerates the reached set and
asserts the marker matches reality.

**Rationale**: this closes OQ-4 with its middle option. Declaring only the five
that work would contradict the System Model's closed set and AC-2, and would force
a second config migration when each call site lands. But shipping six silent knobs
with no signal invites a user to tune something that does nothing — LESSON-481's
shape exactly ("a gate that hides a feature from users also hides it from the test
suite").

The load-bearing part is the **test**, not the marker. A hand-maintained "unreached"
list rots the moment someone adds a call site; the test makes the list a derived
fact.

**Consequences**: `policy show` output carries a status column. When a later REQ
adds the `triage` call site, the test fails until the marker is updated — which is
the intended prompt.

### ADR-B: `redact` is absent from the *configurable* enum, so the pin needs no guard

**Decision**: two enums.

- `Category` — all eleven. Used at call sites, by the resolver, and on the wire.
- `ConfigurableCategory` — ten. `redact` is **not a variant**.

Config deserializes into `ConfigurableCategory`. `resolve(Category::Redact, …)`
returns the local tier by an unconditional match arm that never consults config,
because config cannot express a redact binding.

**Rationale**: BR-4 demands the pin be "an unconditional property of the category,
not a guard predicated on the absence of a binding" — and cites LESSON-443, whose
rule is *"never express a guard's condition in terms of the absence of a feature
you intend to build."* A `if config.get("redact").is_none() { local }` guard
evaporates the day someone adds the key. Making `redact` unrepresentable in config
means there is no condition to get wrong.

**This also satisfies AC-4's "rejected at load naming the pin" for free.** A TOML
`[[categories]] name = "redact"` fails to deserialize with serde's unknown-variant
error, which names `redact` and lists the accepted values. Validation W1 worried
this needed new `deny_unknown_fields` machinery; it does not — the type split does
both jobs. A targeted check upgrades the message to name the pin explicitly rather
than reading as a typo.

**Consequences**: two enums to keep in sync. A `From<ConfigurableCategory> for
Category` conversion is total and the reverse is fallible, which is the correct
asymmetry.

**And one consequence worth stating out loud**: because the rejection is a
deserialization failure, a config carrying `categories.redact` makes the daemon
**refuse to start** (serde error → `Config::load` error → `load_config`'s
"Refusing to start"). That is the intended severity and it is consistent with
REQ-557 treating a dangling `default_provider` as a validity error — both name
something that does not exist, and both are fixed by deleting one line. It is
called out because REQ-557's ADR-E exists precisely to remind us that
refuse-to-start is a big hammer: the test for whether it is the right one is
whether the user can act on it without the daemon running, and here they can.

### ADR-C: Structured mode maps phase→category deterministically; only freeform calls the classifier

**Decision**: the classifier (`route`) runs **only** for a freeform turn's judgment
categories. A structured turn derives its category from the phase it is already in,
by a total function with no model call.

**Rationale**: this is what the spec means by *"the difference between modes is
where the four judgment categories get their signal (ADLC artifacts vs. the `route`
classifier)."* It also matters for latency: the classifier is a new local model call
that did not exist before, and running it on structured turns — which already know
what they are doing — would add cost for no information.

**Consequences**: the phase→category map is used in two places (this dispatch and
BR-10's migration) and must be one function, not two. See ADR-F.

### ADR-D: One resolver, one return value, three surfaces derived from it

**Decision**: `teton_core::category::resolve()` is the only function that answers
"where does this category go". It returns

```rust
CategoryResolution {
    category: Category,
    tier: Tier,
    provider_id: Option<String>,
    reason: String,
    outcome: RouteOutcome,   // reused from policy.rs, not a new enum
}
```

`route_decided`'s payload, `teton policy show`, and the turn-failure sentence are
all constructed **from this struct**. None of them computes its own answer.

**Rationale**: BR-6, and AC-11 which I added at the Phase-1 gate. This is the rule
BUG-155 violated four times in this exact subsystem one REQ ago — a rule enforced
where it was convenient rather than where the decision is made (LESSON-484). The
resolver mirrors `policy::evaluate`'s existing shape (pure, health via closure,
returns a reason naming the signal that fired) so it inherits a proven pattern
rather than inventing one.

**Consequences**: `RouteOutcome` is shared between phase-policy evaluation and
category resolution. That is deliberate — two vocabularies for one concept is how
surfaces drift.

### ADR-E: The category resolver screens providers through `is_routable`, exactly as REQ-557's router does

**Decision**: resolution consults provider usability (REQ-557 ADR-E: a remote
provider with no declared `model` is unusable) and never emits a provider id that
is not routable. An unresolvable category names **itself** and its unset binding.

**Rationale**: BR-8 already says "no synthesized provider id at any step", and
BUG-155 is the proof that this needs saying at every new dispatch path — its
Critical finding was three config-reading paths that each bypassed the router's
usability screen. A new dispatch axis is a fourth such path unless it is screened
by construction.

**Consequences**: the mutation check for AC-10 gains a leg: un-screening the
category resolver must turn a test red.

### ADR-F: The phase→category map is one function, used by both dispatch and migration

**Decision**: a single `category_for_phase(Phase) -> Category` (plus its
one-to-many sibling for migration) in `teton-core`. ADR-C's structured dispatch and
BR-10's migration both call it.

**Rationale**: BR-10's mapping table and ADR-C's dispatch mapping are the same
knowledge. Written twice, they drift — and the drift is invisible, because one is
exercised at config-load and the other on every structured turn.

### ADR-G: `Phase::Freeform` retires; the ledger's `"freeform"` string maps to `None` explicitly

**Decision**: remove the variant. In `phase_from_wire`, keep an explicit
`"freeform" => None` arm **above** the catch-all, with a comment naming it as the
retired variant, and a test asserting it.

**Rationale**: validation W2 flagged that retiring the variant silently reattributes
historical cost rows. Exploration narrowed the exposure: `resolve_freeform` already
sets `phase: None`, so **freeform turns have always recorded a NULL phase**. The
only rows carrying the literal `"freeform"` come from a *structured* session
explicitly created at `Phase::Freeform` — a rare and arguably incoherent state.

So option (c) from BR-11 — accept the reattribution — is right, but the reattribution
must not be an accident of a catch-all arm. An explicit arm plus a test records
that a human decided this, which is the difference between a decision and a bug.

**Consequences**: `Phase::ALL` becomes `[Phase; 5]` and its `len() == 6` test
updates. `CliPhase::Freeform` goes. The structured machine's initial state must
move off `Phase::Freeform` — see the risk note below.

### ADR-J: `policy::evaluate` and its tests are deleted with their last caller; `RouteOutcome` stays put

**Decision**: when TASK-050 repoints `resolve_structured` at `category::resolve`,
the same change deletes `policy::evaluate` **and its table-driven test module**
(`policy.rs:129–279`). `policy.rs` survives as the home of `RouteOutcome` alone —
the shared outcome vocabulary — and nothing else.

**Rationale**: `evaluate` has exactly one production caller (`router.rs:246`), and
TASK-049 retires `RoutingPolicy`, its input type. After both, it is dead code —
but *implied* dead code, which is how REQ-557 shipped `billing_model`'s orphaned
doc comment: a deletion that every task assumed another task would do. The
~150-line test module is the more dangerous half, because a dead test suite still
counts as coverage to anyone reading a test-count.

`RouteOutcome` does **not** move. It is consumed by `router.rs` and `runtime.rs`
today and by `category::resolve` after this REQ; relocating it would touch three
crates to no purpose, and a module that exists to hold one shared enum is a
legitimate module. Two vocabularies for one concept is the drift LESSON-456 is
about — one enum, one home, both dispatch paths reading it.

**Consequences**: `policy.rs` shrinks to a type definition. If a future REQ finds
it has nothing else to say, merging it into `category.rs` is a one-line change —
but not in this REQ, where it would inflate the diff for tidiness.

### ADR-H: `teton policy` keeps its noun; `set` gains tier and category forms

**Decision**: `teton policy set-tier <tier> <provider>` and
`teton policy set-category <category> <provider>`, with `teton policy show`
unchanged in name. No `teton route` noun.

**Rationale**: closes OQ-2. "Policy" still describes what the table is; the rename
buys no clarity and costs a breaking CLI change plus doc churn. REQ-555 deliberately
kept `/provider` shell-only rather than renaming for tidiness — same instinct.

### ADR-I: `shell` ships declared-and-unreached, which makes OQ-1 moot for v1

**Decision**: `shell` is one category bound to `build`, with no call site.

**Rationale**: closes OQ-1. Its *construction* half happens inside turn completion —
and you cannot know a turn will emit a shell call until after the model has already
responded, so it is not available as a routing input. Its *interpretation* half has
no call site at all (`shell.rs:90` returns raw output). Both halves are unreached,
so "does `shell` split further" cannot be answered by evidence this release and
costs nothing to defer.

`debug` keeps its split from `design` (OQ-5): both bind to `think`, so an
imperfect classifier produces an identical routing outcome unless a user has
overridden one — the split is cheap now and awkward to add later.

## Corrections to the Requirement

Recorded here rather than silently worked around:

- **BR-10 / AC-7's `freeform` leg is unconstructible.** `Config::validate` already
  rejects a `[[routing]]` rule targeting `freeform`
  (`ConfigError::FreeformRoutingPolicy`, `config.rs:337`) — the doc comment says
  "reject the inert key loudly". A config carrying that entry has never loaded, so
  the migration has nothing to drop. AC-7 is implemented as: the migration handles
  the **five** valid phases, and a freeform routing entry remains rejected at load.
- **AC-7's "all six phase entries" fixture** follows from the same fact and becomes
  five.

## Data Model Changes

| Type | Location | Change |
|---|---|---|
| `Category` | `teton-core/src/category.rs` (new) | 11-variant enum + `origin` (`harness_known` / `intent_classified`) as a const fn |
| `ConfigurableCategory` | same | 10 variants; `redact` unrepresentable (ADR-B) |
| `Tier` | same | 4-variant enum, `reflex`/`scan`/`build`/`think` |
| `JudgmentCategory` | same | 4 variants; the classifier's **return type** (AC-3's type-level guarantee) |
| `TierBinding`, `CategoryOverride` | `teton-core/src/entities.rs` | replace `RoutingPolicy` as the configured table |
| `Config.tiers`, `Config.categories` | `teton-core/src/config.rs` | replace `Config.routing` |
| `Phase` | `teton-core/src/phase.rs` | `Freeform` removed; `ALL` becomes `[Phase; 5]` |
| `RouteDecided` | `teton-protocol/src/events.rs` | `+ category`, `+ tier`; `phase` stays `Option` |
| `CostRecord` / `LedgerRow` | unchanged | `phase` retained — BR-11 |

## Risks

- **The structured machine initializes at `Phase::Freeform`** (`structured/machine.rs:113`
  per the blast-radius map). Retiring the variant forces a new initial state. This
  is the one place where ADR-G touches behavior rather than types, and it needs a
  deliberate answer in TASK-051 rather than the nearest compiling substitute.
- **The classifier adds a local model call to every freeform judgment turn.** BR-5's
  bypass covers the unavailable case, but the latency cost on the *available* case is
  new and unmeasured. The spec's own assumption is "the risk is latency, not
  accuracy"; dogfooding is the measurement.
- **`digest` becomes routable, and its call site guards an invariant.** LESSON-447:
  when a best-effort step guards an invariant, its failure fallback must still
  enforce the invariant by degraded means. `summarize_if_large` already truncates
  mechanically on engine failure — that fallback must survive routing failure too, or
  a routing error silently becomes an over-window prompt.

## Proposed addition to `.adlc/context/architecture.md`

Under **Key Patterns**:

> - **Dispatch on purpose, not on lifecycle position** — what a call *is for*
>   (classify, summarize, edit, critique) decides which model serves it. Lifecycle
>   phase remains an attribution and gating fact, not a routing input. A call site
>   that knows its own purpose states it; only genuine ambiguity is classified, and
>   only into a type that cannot name a purpose the call site already knew.

To be applied at `/wrapup`.

## Task Graph

```
        TASK-048  (teton-core: Category/Tier/Judgment types + pure resolver)
             │
   ┌─────────┼──────────┬──────────────┐
   ▼         ▼          ▼              ▼
TASK-049  TASK-050  TASK-051      TASK-052
(config   (router    (Phase        (protocol +
 schema+   dispatch,  retirement)   route_decided)
 redact)   delete
           AUXILIARY_SIGNALS)
   └─────────┼──────────┴──────────────┘
             ▼
   ┌─────────┼─────────┐
   ▼         ▼         ▼
TASK-053  TASK-054  TASK-055
(route    (digest   (migration
 classifier routed)  phase→category)
 + bypass)
   └─────────┼─────────┘
             ▼
        TASK-056  (CLI: policy set-tier/set-category, show + unreached marker)
             │
             ▼
        TASK-057  (e2e, egress-capture, mutation checks)
```

Four tiers. The middle two are fully parallel. No task exceeds three dependencies.
