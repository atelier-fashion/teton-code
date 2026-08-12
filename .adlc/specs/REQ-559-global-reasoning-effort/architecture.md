---
id: REQ-559
title: "Architecture — Global reasoning effort with per-provider clamping and thinking-token attribution"
status: draft
created: 2026-08-11
updated: 2026-08-11
---

## Summary

One global `EffortLevel` (`low < medium < high < xhigh < max`, default `high`)
is resolved **once per call, at route time**, in a single pure function in
`teton-core`. That function clamps the canonical level into the target
provider's declared ladder and returns a closed three-variant `ResolvedEffort`
that names exactly what goes on the wire. The same value feeds three consumers
that previously could not have agreed: the `route_decided` event, the
`TurnRequest` handed to the adapter, and the `teton effort` / `/effort`
surfaces.

The load-bearing choices are (a) making "which reasoning field(s) does this
request carry" a *return type* rather than a pair of booleans, so BR-4's mutual
exclusion is structural rather than tested; (b) making `TurnRequest.effort` a
required non-`Default` field, so BR-1's "every call states its effort" is a
compile error to violate rather than a test to remember; and (c) representing
the ladder as a bitset so the addition does not break `Copy` on
`ProviderCapabilities` / `CapabilityProfile` and ripple through ~30 call sites.

Reasoning-token attribution is a separate, independent leg: parse
`completion_tokens_details.reasoning_tokens` on the OpenAI-compatible path into
`TokenUsage`, carry it to `CostRecord`, store it in a nullable ledger column,
and render "unreported" — not `0` — where it is absent. It follows
`cached_tokens` (REQ-564) exactly, which is already an `Option<u64>` subset
column with an `ADDITIVE_COLUMNS` migration entry.

## Existing surfaces this builds on (verified, not assumed)

| Surface | Location | Relevance |
|---|---|---|
| `ProviderCapabilities { tool_call_tier, parallel_calls, max_context }` | `crates/teton-core/src/entities.rs:59` | the declared-capability home; `reasoning_shape` + `effort_ladder` join it |
| `ModelProvider.capabilities` (`#[serde(default)]`) | `crates/teton-core/src/entities.rs:107` | config already carries `[provider.capabilities]`; additive |
| `CapabilityProfile` (derives `Copy`) | `crates/teton-providers/src/capability.rs:24` | adapter-side mirror; **`Copy` is why the ladder is a bitset** |
| `TurnRequest { model, system, messages, tools, max_tokens }` | `crates/teton-providers/src/lib.rs:185` | gains `effort`; only **5** construction sites tree-wide |
| `RouteDecided` | `crates/teton-protocol/src/events.rs:308` | gains `effort` |
| `CostRecord.cached_tokens: Option<u64>` | `crates/teton-protocol/src/events.rs:511` | the exact precedent `reasoning_tokens` copies |
| `ADDITIVE_COLUMNS` | `crates/tetond/src/cost/ledger.rs:130` | the established nullable-column migration path |
| `COMMANDS` table | `crates/teton/src/slash.rs:173` | `/effort` row; `/help` is generated from it (REQ-555 BR-7) |
| `Config::unusable_providers` | `crates/teton-core/src/config.rs` | non-fatal usability pass (conventions.md) — *not* used here, see ADR-E |

Confirmed absent tree-wide before starting: `reasoning_effort`,
`reasoning_tokens`, `completion_tokens_details`. Both adapters' bodies were read
directly; neither sends any reasoning control today. REQ-557's
`ModelProvider.model: Option<String>` and `Config.default_provider:
Option<String>` are both present, so the hard dependency is satisfied.

## ADRs

### ADR-A: What goes on the wire is a return type, not a pair of flags

**Decision.** The resolution returns a closed enum whose variants *are* the
wire outcomes:

```rust
/// What this call puts in the request body. Exhaustive by construction: no
/// variant names two fields, so "never both shapes" (BR-4, AC-2) is a
/// property of the type rather than of a test that has to keep passing.
pub enum ResolvedEffort {
    /// Send the effort field at this already-clamped level (`effort_only`).
    Effort(EffortLevel),
    /// Send the thinking flag alone (`thinking_flag_only`). Carries **no**
    /// level: this provider takes a boolean, and reporting a level the wire
    /// does not carry is the BR-6 misattribution family (BUG-146, BUG-153).
    ThinkingFlag,
    /// Send neither field, and say why.
    Omit(EffortOmission),
}

pub enum EffortOmission {
    /// The provider's declared shape is `none` — the local tier (BR-6, BR-7).
    ShapeNone,
    /// The provider declares an empty ladder: there is no rung to send.
    EmptyLadder,
    /// This provider refused the effort field earlier in this session
    /// (BR-12 / OQ-6). Session-scoped; the *declared* shape is unchanged.
    RefusedThisSession,
}
```

**Rationale.** BR-4 calls the mutual exclusion "a correctness constraint, not a
style preference" — Kimi K2.5/K2.6 answer HTTP 400 when both fields are sent.
Encoding it as `Option<EffortLevel>` + `bool thinking` leaves the illegal state
representable and defends it only by test. A `match` over three variants in each
adapter cannot emit two fields, because no arm has two fields to emit. AC-2 still
ships as a test (the spec asks for one), but it now asserts a property the type
already guarantees rather than being the only thing standing between us and a
400. This is the codebase's existing "structural containment, not a textual
request" posture (ADR-009).

`Omit` carrying a reason rather than being a bare `None` is BR-6: a setting the
provider ignores must be *reported* as ignored, and a reasonless absence gives
the surface nothing to say.

**Consequence.** Adding a fourth reasoning shape (the spec's Assumptions
anticipate one for a future local runtime toggle) is a new variant, and every
adapter `match` becomes a compile error until it decides what to do — which is
the desired failure mode for a wire-shape change.

### ADR-B: `TurnRequest.effort` is required and has no `Default`

**Decision.** `TurnRequest` gains `pub effort: ResolvedEffort` with **no**
`#[serde(default)]` and no `Default` impl on `ResolvedEffort`.

**Rationale.** BR-1 says omitting the field is never a valid outcome, and AC-1
asks for a test that "fails if any call path omits it". A test enumerating call
paths is exactly the guard LESSON-443 describes — correct only until someone adds
a sixth call path. Rust struct-literal syntax requires every field, so a new
`TurnRequest` construction site that has not thought about effort does not
compile. There are only **5** construction sites tree-wide (2 production:
`harness/completion.rs:484`, `harness/duty.rs:832`; 3 in tests/doc), so the
migration cost is trivial and the guarantee is permanent.

AC-1's mock-transport test still ships — the compiler proves a *value* is
supplied, not that the value is honest — but it is now a second line of defence
rather than the only one.

### ADR-C: The ladder is a bitset, so `Copy` survives

**Decision.** `EffortLadder` is a newtype over `u8` (five canonical levels, one
bit each), `Copy + Eq`, serialized as a `Vec<EffortLevel>` so the TOML/JSON
spelling is the obvious list.

**Rationale.** The natural spelling — `Option<Vec<EffortLevel>>` on
`ProviderCapabilities` — breaks `Copy`. `CapabilityProfile` derives `Copy`
(`capability.rs:24`), `harness_profile(self)` takes `self` by value, and
`ProviderCapabilities::default()` appears at ~30 sites in `runtime.rs` alone.
Losing `Copy` turns a self-contained capability change into a mechanical edit
across the daemon with no payoff. A closed 5-element ordered set is precisely
what a bitset is for; the clamp becomes a bit scan (`ladder.0 & below_mask`),
which is also the cheapest possible implementation of ADR-D.

**Consequence.** `EffortLadder` owns its serde. Round-trip
(`levels() → from_levels()`) and TOML-literal tests are mandatory, because a
hand-written `Serialize`/`Deserialize` pair is where a silent drift would live.

### ADR-D: The clamp is down-then-up, pure, and lives with the ladder

**Decision.** `EffortLadder::clamp(EffortLevel) -> Option<EffortLevel>`:
nearest supported at-or-below; if none exists, nearest supported above; `None`
only for an empty ladder. Table-driven tests across all five canonical levels ×
at least three ladders (AC-3).

**Rationale.** OQ-3 is closed and restated BR-5. The direction is
cost-conservative: a clamp that rounded *up* on the user's behalf would bill
them for a rung they did not ask for, and the user who wants the higher rung can
name it. Keeping the clamp on the ladder type (rather than in the router) is
what makes it a pure function with no daemon dependency, which is what makes
AC-3 a unit test and AC-12's "make the clamp an identity function" mutation
mechanically detectable.

### ADR-E: OQ-1 — a per-kind default table the user may override, and the unknown-endpoint default ladder is `{low, high}`

**Decision.** `ProviderCapabilities` gains two `Option` fields.
`None` means *not declared*, and the per-kind default table applies:

| `ProviderKind` | default `reasoning_shape` | default `effort_ladder` |
|---|---|---|
| `Local` | `none` | empty |
| `OpenaiCompatible` | `effort_only` (BR-4 / OQ-2) | **`{low, high}`** |
| `Anthropic` | `effort_only` | `{low, medium, high, xhigh, max}` |
| `Custom` | `effort_only` | `{low, high}` |

A user overrides either per provider in config:

```toml
[[provider]]
id = "deepseek"
kind = "openai-compatible"
model = "deepseek-chat"
[provider.capabilities]
effort_ladder = ["low", "high", "xhigh", "max"]
```

**Rationale for the shape default**: OQ-2 is closed — an unknown endpoint states
its effort, because defaulting to `none` reintroduces this REQ's originating
defect (Kimi billing at `max`) at exactly the BYOM endpoint REQ-544 BR-6 exists
to serve, and the opposite risk (a 400) already has a handler in BR-12.

**Rationale for `{low, high}`, which the spec does not name and which is the real
decision here**: the tempting default for an unknown OpenAI-compatible endpoint
is the full canonical ladder. That is wrong, and wrong in the REQ's own failure
direction. A Kimi K3 registered as `openai-compatible` (the expected
registration — Teton has no Kimi kind) would receive `xhigh`, which K3 does not
accept, 400, fall back to BR-12's `none` shape, and land back on Kimi's `max`
default. The originating defect, reached the long way round.

`{low, high}` is the intersection of every published target ladder — OpenAI
(`minimal/low/medium/high`), Kimi K3 (`low/high/max`), DeepSeek V4
(`low/high/xhigh/max`) all contain both rungs. With the default effort of `high`,
an undeclared endpoint receives `high`: accepted everywhere, and on Kimi a real
downgrade from `max`, which is the defect fixed. Users who want the higher rungs
declare the ladder, which is the OQ-1 override doing its job.

`minimal` is deliberately **not** a canonical level: it exists only on OpenAI, the
ladder members are canonical levels by definition (BR-3), and adding a sixth rung
for one vendor buys a per-provider spelling table this design does not otherwise
need. Canonical spellings *are* the wire spellings on Anthropic, DeepSeek and
Kimi, so no mapping table exists at all.

**Why not `Config::validate` / `unusable_providers` for a declared-empty ladder:**
`validate` is fail-closed and gates startup, and conventions.md restricts it to
structural errors; `unusable_providers` marks a provider unable to serve turns at
all, which is far too harsh for an effort misconfiguration. An explicitly empty
ladder therefore means what it says — this provider accepts no rung — and
resolves to `Omit(EmptyLadder)`, which the surface reports. Nothing bricks, and
the user is told.

### ADR-F: OQ-6 — a BR-12 refusal is remembered for the session, never written to config

**Decision.** When a provider answers 400 naming the effort field, the daemon
records `(provider_id → refused)` in **session-scoped runtime state** for the
life of the session. Subsequent calls to that provider resolve to
`Omit(RefusedThisSession)` and send no reasoning field. The refusal is **never**
persisted to config and **never** mutates the declared `reasoning_shape`. The
surface says so: `teton effort` / `/effort` renders that provider as
`effort refused this session — sending none` rather than a level.

**Rationale.** The three options were: 400 once per call forever (BR-12's literal
per-call reading), remember for the session, or downgrade the declared shape.

- Per-call-forever doubles request count and latency for every call to that
  provider for the life of the session, at exactly the BYOM endpoint ADR-E's
  default targets. Correct but wasteful, as OQ-6 itself says.
- Downgrading the *declared* shape is forbidden by BR-4 in as many words: "The
  shape is declared per provider, never sniffed from a response." Persisting a
  capability conclusion drawn from one HTTP status is precisely that sniff, and
  it would survive a provider adding effort support, or a transient 400 from a
  proxy, with no way for the user to know why their setting stopped applying.
- Session-scoped memory is not what BR-12 forbids. BR-12 forbids **silent
  retries** — making the failing request again and hoping. Remembering does the
  opposite: it declines to make a request already known to fail. The declared
  shape is untouched, so the next session tries again and a provider that gained
  support self-heals with no config edit.

Visibility is the condition that makes this acceptable rather than a hidden
downgrade: `Omit(RefusedThisSession)` is a *reason*, ADR-A requires the surface
to render reasons, and BR-6's discipline ("a silently ignored setting is the
misattribution family of BUG-146 and BUG-153") applies to a runtime-discovered
no-op exactly as it does to a declared one.

**Consequence.** The memo lives beside the session's other runtime degradation
state, not in `Config`. It is keyed by `provider_id`, so two providers pointing
at the same endpoint are remembered separately — the key matches the thing the
user configured, per the codebase's "a remembered grant is scoped by its key"
principle.

### ADR-G: Effort is resolved once, at route time, and travels

**Decision.** `resolve_effort` is called **once** per call, in the daemon at the
point the route is decided. Its `ResolvedEffort` is (1) written into
`RouteDecided.effort`, (2) stored on the remote source / duty struct, and (3)
read from there when the `TurnRequest` is built. Adapters do **not** clamp;
they only `match` the variant they were handed.

**Rationale.** AC-4 requires `route_decided` to report the clamped level, and
AC-8 requires the surfaces to render "through the same function the router
calls — asserted by a shared-resolver test, not by string coincidence". Two
call sites computing the same thing is the drift LESSON-456 and REQ-555 BR-4 are
about; one value flowing to three consumers cannot disagree with itself. It also
keeps `teton-providers` free of routing policy, preserving the crate's existing
no-policy posture.

The surfaces call the same `resolve_effort` per registered provider to build
their per-provider view, which is delivered inside the existing
`ConfigSnapshot` (`config/get`) — no new RPC, as the spec requires.

### ADR-H: Anthropic sends `output_config.effort` alone — no `thinking` block

**Decision.** With `reasoning_shape = effort_only`, the Anthropic adapter emits
`output_config.effort` and does **not** emit `thinking: {type: "adaptive"}`,
even though Anthropic accepts both.

**Rationale.** BR-4 is unambiguous ("`effort_only` sends the effort field
alone") and AC-2 pins it as a test asserting no request ever carries both.
Anthropic's `thinking` is already adaptive by default when effort is set, so
omitting it changes nothing observable; and a single-shape invariant that holds
for *every* provider is worth more than a per-provider exception that makes AC-2
conditional. Recorded explicitly because a future reader will notice Anthropic
permits both and wonder whether the omission was an oversight. It was not.

### ADR-I: OQ-5 — effort is snapshotted at turn start

**Decision.** The effective effort for a turn is read once when the turn begins.
A `/effort` issued mid-turn applies to the **next** turn.

**Rationale.** OQ-5 names the hazard itself: applying mid-turn splits one turn
across two effort levels and makes its `CostRecord` ambiguous. A turn-start
snapshot costs nothing and keeps one `CostRecord` describing one setting. This
also gives the `SessionState.effort_override` / `Config.effort` pair from the
System Model a coherent reading: `/effort <level>` writes the persisted config
(BR-8) *and* sets the session's override, and resolution is
`session.override.or(config.effort).unwrap_or(HIGH)` — never an absent field
(BR-1).

## Data model

```rust
// teton-core::effort  (new module — pure, no I/O, no daemon deps)

pub enum EffortLevel { Low, Medium, High, Xhigh, Max }   // BR-3, ordered, closed
impl Default for EffortLevel { fn default() -> Self { Self::High } }  // BR-1

pub struct EffortLadder(u8);                              // ADR-C, Copy
pub enum ReasoningShape { EffortOnly, ThinkingFlagOnly, None }
pub enum ResolvedEffort { Effort(EffortLevel), ThinkingFlag, Omit(EffortOmission) }
pub enum EffortOmission { ShapeNone, EmptyLadder, RefusedThisSession }

pub fn default_shape_for(kind: ProviderKind) -> ReasoningShape;   // ADR-E
pub fn default_ladder_for(kind: ProviderKind) -> EffortLadder;    // ADR-E
pub fn resolve_effort(                                            // ADR-G, BR-9
    requested: EffortLevel,
    kind: ProviderKind,
    caps: &ProviderCapabilities,
    refused_this_session: bool,
) -> ResolvedEffort;
```

Changed shapes, all additive with defaults (no `PROTOCOL_VERSION` bump — same
posture as `CostRecord::cached_tokens` and `PrivacyBlock::cause`):

| Type | Field | Notes |
|---|---|---|
| `ProviderCapabilities` | `reasoning_shape: Option<ReasoningShape>` | `None` → per-kind default (ADR-E) |
| `ProviderCapabilities` | `effort_ladder: Option<EffortLadder>` | `None` → per-kind default; `Copy` preserved |
| `Config` | `effort: EffortLevel` | serialized unconditionally — a declared default must be configuration-visible (precedent: `judgment_default`) |
| `TurnRequest` | `effort: ResolvedEffort` | **required**, no serde default (ADR-B) |
| `RouteDecided` | `effort: Option<ResolvedEffort>` | `Option` for wire-additivity only; the daemon always sends it |
| `TokenUsage` | `reasoning_tokens: Option<u64>` | `None` = unreported, never `0` (BR-10) |
| `CostRecord` | `reasoning_tokens: Option<u64>` | subset of `output_tokens`, never added (BR-10) |
| `ConfigSnapshot` | `effort: Option<EffortView>` | the shared per-provider view (BR-9, AC-8) |
| `ConfigUpdate` | `SetEffort(EffortLevel)` | new variant, not a new RPC |
| ledger `cost_records` | `reasoning_tokens INTEGER` | via `ADDITIVE_COLUMNS`, nullable, never backfilled |

## Test strategy

- **Clamp** (AC-3): table-driven, 5 levels × ≥3 ladders, in `teton-core`.
- **Always-sent** (AC-1): mock-transport capture across all four tiers and both
  adapters; plus ADR-B's compile-time guarantee.
- **Never-both** (AC-2, AC-2b): assert over the capture that no body carries two
  reasoning fields; plus ADR-A's type-level guarantee. AC-2b adds an undeclared
  `openai-compatible` provider whose mock answers 400 on the field, asserting
  the typed error, the single fallback, and no both-shapes body.
- **Event fidelity** (AC-4): session at `xhigh` + 3-rung ladder → event says `high`.
- **Local no-op** (AC-5, AC-6): local-routed and `route`-category calls carry no
  reasoning field with the session at `max`; surface says "not applicable".
- **Persistence** (AC-7): `/effort low`, daemon restart, fresh session → `low`.
- **Shared resolver** (AC-8): the surface's per-provider rows are asserted to be
  produced by `resolve_effort`, not by matching rendered strings.
- **Attribution** (AC-9): a response with `completion_tokens_details.reasoning_tokens`
  yields that value on the `CostRecord` with `output_tokens` **byte-identical to
  today's parse**; without the field → `None` → "unreported", not `0`.
- **Egress** (AC-11): `max` effort on a `local-only` boundary session → zero
  remote calls carrying boundary content.
- **Mutation** (AC-12): removing the always-send rule, and making the clamp an
  identity function, each turn at least one test red.

**BUG-159 hazard**: `call_sites.rs` and `harness/duty.rs` read production source
with `.expect("readable source file")`. The AC-12 mutation check must not run
concurrently with edits to `src/`; a panic there is BUG-159, not a regression
from this REQ.

## Open questions carried forward

- **OQ-4** (per-category reasoning-token breakout in `teton cost`) stays open. No
  AC depends on it, and it widens coupling to REQ-558's landing. The nullable
  `category` column already on `cost_records` means it is a query change later,
  not a schema change.
- **OQ-1's override ergonomics** — whether a ladder should also be settable via
  `teton provider add` flags rather than only a config-file edit — is deferred.
  The config-file path satisfies OQ-1; a flag is additive.

## Boundary with REQ-560

This REQ owns the `/effort` `COMMANDS` row, its bare-argument read path, and its
`/help` entry (BR-9). REQ-560 renders the effort **value** in its status line and
owns `/permissions`; it must not add or alias `/effort`. Nothing here touches the
status line or permission levels.
