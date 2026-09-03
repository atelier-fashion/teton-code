//! The router: category routing, remote wiring, and BR-6 degradation.
//!
//! This is the *wiring* layer over a pure resolver. It does **no** routing logic
//! of its own: every decision, in **both** session modes, comes straight from
//! [`teton_core::category::resolve`] (category × table × provider health ×
//! provider usability → provider + reason). There is one dispatch key and one
//! resolver (REQ-558 BR-1, ADR-D).
//!
//! Before REQ-558 there were two: structured turns read a phase → provider
//! policy table, and freeform turns — the default experience — ignored it
//! entirely in favour of a ten-word substring list, so `teton policy set` had no
//! effect on a normal session. Both the phase evaluator and the substring list
//! are deleted rather than relocated (BR-2).
//!
//! The router's job is everything around the pure core:
//!
//! - **Legibility (BR-5)** — turn every decision, policy or heuristic, into a
//!   `route_decided` event whose `reason` names the rule/heuristic that fired.
//! - **Degradation (BR-6)** — derive the [`HarnessConfig`] each turn runs under
//!   from the *selected* provider's [`CapabilityProfile`]: a weak tool-caller gets
//!   the reduced profile (smaller tool set, shorter loop, mandatory verify), a
//!   reliable one gets the full loop.
//! - **Remote wiring (BR-1/BR-2)** — build the [`EgressContext`] (session,
//!   provider, and the phase-pinned [`CostAttribution`]) for a routed remote call,
//!   so the call goes through the single egress choke point. Privacy enforcement
//!   and cost recording therefore hold *by construction*: the router never opens
//!   a socket, it hands egress the context and egress does the rest.
//! - **Fallback on failure (AC-7)** — classify a mid-session provider failure
//!   ([`teton_providers::classify`]), emit `provider_degraded`, and re-resolve to
//!   the fallback provider (or the same provider under a reduced profile) so the
//!   session completes rather than failing.
//!
//! ## Prompt text is not here
//!
//! No routing function in this module takes prompt text, and
//! `no_routing_function_can_see_prompt_text` pins their types so adding one stops
//! compiling. The `route` classifier — the thing that *does* read a freeform
//! prompt — lives in [`crate::classify`] and hands this module a
//! [`Classification`], whose category is a [`JudgmentCategory`]: four variants,
//! none of them harness-known (AC-3). A routing function that could see the text
//! is a routing function a keyword list can be dropped back into, which is the
//! defect this REQ exists to close (BR-2).
//!
//! ## `Phase` is not here
//!
//! No routing signature in this module takes a [`teton_core::phase::Phase`]
//! (AC-9). A structured turn maps its phase to a category with
//! `teton_core::category_for_phase` and hands the router the *category*; the
//! phase is stamped onto [`Route::phase`] afterwards, where it feeds cost
//! attribution and nothing else (BR-11). [`to_protocol_phase`] survives as that
//! attribution boundary's bridge — `teton_core::Phase` in, the
//! `teton_protocol::Phase` that travels on `route_decided` / `cost_recorded`
//! out.

use std::collections::{BTreeMap, BTreeSet};

use teton_core::category::{
    resolve as resolve_category, Category as CoreCategory, CategoryResolution, CategoryTable,
    JudgmentCategory, Tier as CoreTier, TierBinding,
};
use teton_core::effort::{resolve_effort, EffortLevel, EffortOmission, ResolvedEffort};
use teton_core::entities::{ProviderCapabilities, ProviderKind};
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::{ProviderHealth, RouteOutcome};

use teton_protocol::events::{
    Event, FailureClass as ProtoFailureClass, ProviderDegraded, RouteDecided,
};
use teton_protocol::{
    Category as ProtoCategory, Phase as ProtoPhase, ProviderId, SessionId, Tier as ProtoTier,
};

use teton_providers::{
    classify, degradation_signal, CapabilityProfile, FailureAction, FailureClass,
};

use crate::broadcast::EventBus;
use crate::classify::Classification;
use crate::cost::CostAttribution;
use crate::egress::EgressContext;
use crate::harness::budget::{self, BudgetInputs, RouteBudget};
use crate::harness::turn_loop::{HarnessConfig, TurnRoute};

/// Map a `teton_core::Phase` (the routing axis) to the `teton_protocol::Phase`
/// carried on the `route_decided` / `cost_recorded` wire. The variants are
/// identical; this is the boundary bridge (see the module docs).
#[must_use]
pub fn to_protocol_phase(phase: CorePhase) -> ProtoPhase {
    match phase {
        CorePhase::Spec => ProtoPhase::Spec,
        CorePhase::Architect => ProtoPhase::Architect,
        CorePhase::Implement => ProtoPhase::Implement,
        CorePhase::Review => ProtoPhase::Review,
        CorePhase::Io => ProtoPhase::Io,
    }
}

/// Map a `teton_core::category::Category` (the dispatch key) to the
/// `teton_protocol::Category` carried on the `route_decided` / `cost_recorded`
/// wire. Total, and the same boundary bridge [`to_protocol_phase`] is.
///
/// Deliberately **one-way**. `teton_core::category::Category` carries no
/// `FromStr` and no `Deserialize` because that absence is REQ-558 AC-3's
/// guarantee that prompt text cannot name a harness-known category; the wire
/// twin is deserializable so a client can read the event, and that stays safe
/// only while no conversion runs wire → core.
#[must_use]
pub fn to_protocol_category(category: CoreCategory) -> ProtoCategory {
    match category {
        CoreCategory::Route => ProtoCategory::Route,
        CoreCategory::Redact => ProtoCategory::Redact,
        CoreCategory::Title => ProtoCategory::Title,
        CoreCategory::Digest => ProtoCategory::Digest,
        CoreCategory::Compact => ProtoCategory::Compact,
        CoreCategory::Triage => ProtoCategory::Triage,
        CoreCategory::Edit => ProtoCategory::Edit,
        CoreCategory::Shell => ProtoCategory::Shell,
        CoreCategory::Design => ProtoCategory::Design,
        CoreCategory::Debug => ProtoCategory::Debug,
        CoreCategory::Review => ProtoCategory::Review,
    }
}

/// Map a `teton_core::category::Tier` to its wire twin. Total; see
/// [`to_protocol_category`] for why the conversion runs only in this direction.
#[must_use]
pub fn to_protocol_tier(tier: CoreTier) -> ProtoTier {
    match tier {
        CoreTier::Reflex => ProtoTier::Reflex,
        CoreTier::Scan => ProtoTier::Scan,
        CoreTier::Build => ProtoTier::Build,
        CoreTier::Think => ProtoTier::Think,
    }
}

/// Where an **unbound** tier's provider comes from (REQ-557 BR-4).
///
/// The distinction exists because it differs per tier and the difference is not
/// obvious: `reflex` never inherits `default_provider`, because
/// [`CoreTier::inherits_default_provider`] says so — see
/// [`Router::inherited_binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierOrigin {
    /// A `[[tiers]]` row the user configured.
    Configured,
    /// Unbound; filled from `default_provider`.
    DefaultProvider,
    /// Unbound; filled from the local tier.
    LocalTier,
    /// Unbound, with nothing to inherit.
    Unbound,
}

/// One tier's binding as `teton policy show` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierReport {
    /// The tier described.
    pub tier: CoreTier,
    /// The provider it binds, configured or inherited; `None` when it has
    /// neither.
    pub provider_id: Option<String>,
    /// The configured fallback. An inherited fill carries none — the fill is a
    /// primary, and inventing a fallback for it would be a synthesized binding
    /// (BR-8).
    pub fallback_id: Option<String>,
    /// Whether that provider was configured or inherited, and from where.
    pub origin: TierOrigin,
}

/// A registered provider as the router sees it: the concrete model it bills, its
/// capability profile (drives BR-6 degradation), and its live health (drives
/// policy fallback selection).
#[derive(Debug, Clone)]
struct ProviderRuntime {
    /// Concrete model name billed for this provider (drives cost attribution).
    model: String,
    /// Transport/vendor family. Needed because the REQ-559 per-kind reasoning
    /// defaults (ADR-E) key on it, and `CapabilityProfile` deliberately does not
    /// carry a kind of its own — duplicating `ModelProvider::kind` there would be
    /// the two-sources-of-one-fact drift LESSON-456 is about.
    kind: ProviderKind,
    /// Capability profile (tool-call tier → harness degradation, BR-6).
    capabilities: CapabilityProfile,
    /// Current health as the router sees it (BR-5 policy fallback input).
    health: ProviderHealth,
}

/// One configured **remote** provider, as an offer that must name a provider
/// sees it (REQ-589 ADR-12).
///
/// Both fields are owned rather than borrowed from the router, because the
/// surface that reads this list holds it across a consent prompt — an `await`
/// on a human — and a borrow of the router would not survive that.
///
/// The two facts are the two an offer needs and no more: the **id** is what a
/// tier binding names and what the user typed into `[[providers]]`, and the
/// **model** is what a window proposal is looked up by
/// ([`recipe_for_model`](crate::provider_recipes::recipe_for_model), ADR-6
/// rule 1 — ids are the user's namespace, so Kimi registered as `work-model`
/// must still get Kimi's window and a provider merely *called* `anthropic`
/// must not get Anthropic's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProvider {
    /// The id the provider is configured and bound under.
    pub id: String,
    /// The concrete model it bills — the key a vendor recipe is found by.
    pub model: String,
}

/// One resolved routing decision: the selected provider, a legible reason, and
/// the harness profile the turn runs under (BR-6 degradation applied).
///
/// A `Route` is produced by [`Router::resolve`], by the taint backstop
/// ([`Router::resolve_local_pin`]), and by the fallback path
/// ([`Router::on_provider_failure`]). It is the single object the daemon threads
/// into a turn: [`Route::turn_route`] hands the harness the provider + profile,
/// and [`Router::egress_context`] builds the choke-point context for a remote
/// call.
#[derive(Debug, Clone)]
pub struct Route {
    /// Provider selected, or `None` when no provider could be selected (nothing
    /// bound for the category, or every candidate unusable).
    pub provider_id: Option<ProviderId>,
    /// Concrete model chosen, when the provider is registered.
    pub model: Option<String>,
    /// Lifecycle phase (protocol form) this turn is attributed to, or `None` for
    /// a freeform turn, which has no lifecycle position.
    ///
    /// **Not a routing input** (BR-11, AC-9). It is stamped on by the caller
    /// *after* the decision is made, and travels only as far as
    /// [`Router::egress_context`]'s [`CostAttribution`] and the `route_decided`
    /// payload. Nothing in this module reads it to choose a provider.
    pub phase: Option<ProtoPhase>,
    /// User-facing sentence explaining the decision (feeds `route_decided`, BR-5).
    pub reason: String,
    /// Structured outcome for programmatic branching — the shared vocabulary
    /// [`teton_core::category::resolve`] reports in.
    pub outcome: RouteOutcome,
    /// Harness configuration this turn runs under — the BR-6 profile of the
    /// *selected* provider. Meaningful only when a provider was selected; for the
    /// no-provider case it is the strict [`HarnessConfig::default`].
    pub harness: HarnessConfig,
    /// The context budget this route attempt runs under (REQ-586 BR-1/BR-8):
    /// the `(words, bytes)` pair, what bound it, and the window's name for the
    /// elision marker.
    ///
    /// Resolved **once**, by [`Router::budget_for`] (ADR-1), and a *copy* of
    /// [`Route::harness`]'s own [`HarnessConfig::budget`] rather than a second
    /// derivation — the two are asserted equal, because a route whose event
    /// announced one budget while its harness ran under another is exactly the
    /// two-computations-of-one-fact drift LESSON-456 is about. It lives here as
    /// well as on the config because the *route* is what `route_decided`,
    /// `/verbose` and the reroute arms hold; digging a budget out of a
    /// `HarnessConfig` to report it would make the config the wire's source.
    ///
    /// Never `None`: a route that resolved no provider carries the default
    /// (local) derivation its [`HarnessConfig::default`] harness carries, and
    /// reports nothing at all — [`Route::route_decided`] is `None` for it.
    pub budget: RouteBudget,
    /// The [`CategoryResolution`] this route was built from, when the decision
    /// came through the category chain (`teton_core::category::resolve`).
    ///
    /// [`Route::route_decided`] reads its `category` and `tier` **off this
    /// value** and recomputes neither (REQ-558 ADR-D, BR-6, AC-11): two surfaces
    /// describing one routing state must not be able to drift apart. This is the
    /// defect BUG-155 shipped four times in this subsystem one REQ ago.
    ///
    /// `None` for exactly one route: [`Router::resolve_local_pin`], the session
    /// taint backstop. That path consults **no** resolution on purpose — it is a
    /// privacy guarantee overriding every binding (BR-7), not a category
    /// decision — so it has no category to report, and minting one would be the
    /// second computation this field exists to prevent. Every other route the
    /// router produces carries one, which
    /// `every_route_but_the_taint_pin_carries_its_resolution` asserts.
    pub resolution: Option<CategoryResolution>,
    /// What this turn's request will put in its reasoning field(s) (REQ-559).
    ///
    /// Resolved **once**, here, by [`Router::effort_for`] (ADR-G).
    /// [`Route::route_decided`] and [`Route::turn_route`] both *read* it off
    /// this field and neither recomputes it — the same ADR-D discipline the
    /// `category`/`tier` projection follows, and for the same reason: the event
    /// must report the level the request actually carries (AC-4), which it
    /// cannot do if the two are computed separately.
    ///
    /// `None` only when no provider was selected.
    pub effort: Option<ResolvedEffort>,
}

impl Route {
    /// Whether a provider was actually selected.
    #[must_use]
    pub fn selected(&self) -> bool {
        self.provider_id.is_some()
    }

    /// The `route_decided` event payload for this decision, or `None` when no
    /// provider was selected (the event's `provider_id` is required).
    ///
    /// The category and the tier are **projected from [`Route::resolution`]**,
    /// never recomputed — not even the tier, which `Category::tier()` could
    /// supply. A resolution that reports a tier is the one authority on which
    /// tier this decision went through (ADR-D, AC-11).
    #[must_use]
    pub fn route_decided(&self) -> Option<RouteDecided> {
        self.provider_id.as_ref().map(|provider_id| RouteDecided {
            category: self
                .resolution
                .as_ref()
                .map(|r| to_protocol_category(r.category)),
            tier: self.resolution.as_ref().map(|r| to_protocol_tier(r.tier)),
            phase: self.phase,
            provider_id: provider_id.clone(),
            model: self.model.clone(),
            reason: self.reason.clone(),
            // Read off the route, never recomputed (ADR-D/ADR-G). This is the
            // **clamped** level — reporting the requested one would make the
            // event lie about the call (BR-5, AC-4).
            effort: self.effort,
            // Projected off `Route::budget`, never recomputed (REQ-586 ADR-1,
            // BR-8) — the same ADR-D discipline the category/tier/effort
            // projections follow. The event must report the budget the attempt
            // actually runs under, which it cannot do if the two are derived
            // separately. Always `Some(..)` on a route the router built: the
            // budget is a property of the attempt, and `None` on this wire now
            // means "a daemon that predates REQ-586".
            budget_tokens: Some(self.budget.budget_tokens as u64),
            budget_bytes: Some(self.budget.budget_bytes as u64),
            bound: Some(self.budget.bound),
            // Projected off the same value: whether the bound above is
            // actually in force, or was overruled by the floor (TASK-194 2b).
            // A surface printing the bound without it reports a ceiling the
            // route is not running under.
            bound_floored: Some(self.budget.floored),
            spend_ceiling_micro_cents: None,
        })
    }

    /// The effort this route's requests carry, with the honest floor for a
    /// route that resolved no provider (REQ-559).
    ///
    /// `Route::effort` is `None` only when no provider was selected — there was
    /// nothing to resolve against. Every caller that reaches a request has
    /// already established a provider, so this is unreachable in practice; it
    /// resolves to `Omit(ShapeNone)` rather than panicking or inventing a level,
    /// because "send no reasoning field" is the one answer that is always safe
    /// and always honest. Shared by [`Route::turn_route`] and the daemon's
    /// remote-source construction so the floor is stated once.
    #[must_use]
    pub fn effective_effort(&self) -> ResolvedEffort {
        self.effort.unwrap_or(ResolvedEffort::Omit {
            reason: EffortOmission::ShapeNone,
        })
    }

    /// The per-turn routing input for the harness ([`TurnRoute`]): provider +
    /// model + the BR-6 [`HarnessConfig`]. `None` when no provider was selected.
    #[must_use]
    pub fn turn_route(&self) -> Option<TurnRoute> {
        let provider_id = self.provider_id.clone()?;
        Some(TurnRoute {
            provider_id,
            model: self.model.clone(),
            config: self.harness.clone(),
            // The same value the event carries — one resolution, two readers.
            // `Omit(ShapeNone)` is unreachable here in practice (a selected
            // provider always resolves), but it is the honest floor: a route
            // with a provider the router does not know sends no reasoning field
            // rather than inventing a level.
            effort: self.effective_effort(),
        })
    }
}

/// The outcome of handling a mid-session provider failure (AC-7).
#[derive(Debug, Clone)]
pub struct FailureOutcome {
    /// The `provider_degraded` event to broadcast, or `None` when the failure was
    /// transient (retry) — nothing to report yet — or fatal (nothing changed).
    pub degraded: Option<ProviderDegraded>,
    /// The route to continue the session on: the fallback provider, or the same
    /// provider under a reduced harness profile. `None` when the failure is not
    /// recoverable by fallback or degradation (e.g. an auth error).
    pub route: Option<Route>,
}

/// The category router (architecture: Session → Router → egress).
///
/// Holds the configured tier/category table, the registered providers (model +
/// capabilities + health), the declared default provider, the BR-9 judgment
/// default, and whether the local tier can meet its BR-8 latency duty.
/// Construction is builder-style so a caller (or a test) wires exactly the
/// providers it needs.
#[derive(Debug, Clone)]
pub struct Router {
    /// The table **as configured** — `[[tiers]]`, `[[categories]]`, and the local
    /// tier's id. [`Router::effective_table`] is what resolution reads.
    table: CategoryTable,
    providers: BTreeMap<String, ProviderRuntime>,
    /// The declared default provider (REQ-557 BR-4): the binding a tier the user
    /// has not bound inherits.
    default_provider: Option<String>,
    /// The category a freeform judgment turn takes when classification is
    /// bypassed or fails (BR-9).
    judgment_default: JudgmentCategory,
    /// Whether the local tier can serve its BR-8 latency duty right now.
    local_available: bool,
    /// The effort level in force for this turn (REQ-559 BR-2): one global
    /// setting, resolved by the caller as
    /// `session_override.or(config.effort)` and defaulted to `high` — never
    /// absent (BR-1).
    effort: EffortLevel,
    /// The per-prompt spend ceiling in force, in micro-cents (REQ-588 BR-2).
    ///
    /// Config-derived per-turn state like [`Router::effort`] beside it, and
    /// held here rather than projected through [`Route`] on purpose: a spend
    /// ceiling is **not a routing decision**. It does not choose a provider,
    /// and threading it through the routing type would invite a later reader to
    /// treat it as one. The router carries it only so the one place that emits
    /// `route_decided` can stamp it.
    spend_ceiling_micro_cents: Option<u64>,
    /// Providers that answered 400 on the effort field earlier in this session
    /// (REQ-559 BR-12 / ADR-F).
    ///
    /// **Session-scoped and never persisted.** The declared `reasoning_shape`
    /// is untouched, so the next session tries again and a provider that gains
    /// support self-heals with no config edit. Keyed by provider id — the key
    /// the user configured — so two providers pointing at one endpoint are
    /// remembered separately.
    effort_refused: BTreeSet<String>,
    /// Whether `[privacy] redact = true` on this daemon (REQ-586 BR-4, ADR-1).
    ///
    /// A routing input only in the budget sense: the redaction scan reads a
    /// whole assembled body, and its input cap is what a remote route's *byte*
    /// budget must fit inside, so a route derived without this fact would send
    /// turns the gate `redaction_gate` installs could not scan. The router
    /// never saw the flag before REQ-586 (gotcha #2) — `build_router` feeds it
    /// from the same `config.privacy.redact` the gate consults, so the bound
    /// and the gate cannot disagree.
    redact_scan: bool,
}

impl Router {
    /// A router reading `table`, with `default_provider` as the binding an
    /// unbound tier inherits (REQ-557 BR-4).
    ///
    /// The local tier starts available and the judgment default starts at
    /// [`JudgmentCategory::Edit`]; register providers with
    /// [`Router::with_provider`].
    #[must_use]
    pub fn new(table: CategoryTable, default_provider: Option<String>) -> Self {
        Self {
            table,
            providers: BTreeMap::new(),
            default_provider,
            judgment_default: JudgmentCategory::default(),
            local_available: true,
            effort: EffortLevel::default(),
            spend_ceiling_micro_cents: None,
            effort_refused: BTreeSet::new(),
            // Off unless the daemon says otherwise: `[privacy] redact` is
            // opt-in, and a router that assumed the scan were on would hold
            // every remote route to the scannable bound nothing asked for.
            redact_scan: false,
        }
    }

    /// Set the effort level this router resolves against (REQ-559 BR-2/BR-8).
    ///
    /// Read from `Config::effort` (or the session override), which is why it is
    /// configuration-visible rather than a constant compiled in here.
    #[must_use]
    pub fn with_effort(mut self, effort: EffortLevel) -> Self {
        self.effort = effort;
        self
    }

    /// Set the per-prompt spend ceiling this router reports (REQ-588 BR-2).
    ///
    /// Read from `Config::cost`, and `None` on an un-opted-in machine — which
    /// is what keeps the emitted event byte-identical to before this REQ.
    #[must_use]
    pub fn with_spend_ceiling(mut self, micro_cents: Option<u64>) -> Self {
        self.spend_ceiling_micro_cents = micro_cents;
        self
    }

    /// Seed the set of providers that refused the effort field this session
    /// (REQ-559 BR-12 / ADR-F). Session-scoped; never read from or written to
    /// config.
    #[must_use]
    pub fn with_effort_refusals(mut self, refused: BTreeSet<String>) -> Self {
        self.effort_refused = refused;
        self
    }

    /// The effort level in force (the pre-clamp request). Exposed so the
    /// `teton effort` / `/effort` surfaces report the same number the router is
    /// working from (BR-9).
    #[must_use]
    pub fn effort(&self) -> EffortLevel {
        self.effort
    }

    /// **The** per-provider effort resolution (REQ-559 ADR-G, BR-9).
    ///
    /// Every `Route` this router builds calls this, and so does the
    /// `teton effort` / `/effort` view — one function, so the event, the request
    /// and the surface cannot disagree about what a provider is being sent
    /// (LESSON-456). The clamp itself lives in `teton_core::effort`, is pure,
    /// and is table-tested there.
    ///
    /// `None` when no provider was selected: there is nothing to resolve
    /// against, and minting a value would be reporting a decision that was never
    /// made.
    #[must_use]
    pub fn effort_for(&self, provider_id: Option<&str>) -> Option<ResolvedEffort> {
        let id = provider_id?;
        let refused = self.effort_refused.contains(id);
        let Some(runtime) = self.providers.get(id) else {
            // The local tier is legitimately absent from `providers`: it comes
            // from the engine rather than from a `[[providers]]` entry
            // (REQ-557 ADR-D), so a config that declares no local provider still
            // routes here. It must still report a **declared** no-op (BR-6):
            // `None` would mean "a daemon that predates effort", which is a
            // different claim from "effort does not apply to this tier, and here
            // is why" — and a silently ignored setting is the BUG-146/BUG-153
            // misattribution family.
            if self.table.local_provider_id.as_deref() == Some(id) {
                return Some(resolve_effort(
                    self.effort,
                    ProviderKind::Local,
                    &ProviderCapabilities::default(),
                    refused,
                ));
            }
            // Any other unknown id is the no-tier-available case the caller
            // turns into an error. It carries no capability declaration and no
            // engine, so it has no honest resolution.
            return None;
        };
        Some(resolve_effort(
            self.effort,
            runtime.kind,
            &runtime.capabilities.to_core(),
            refused,
        ))
    }

    /// **The** per-route context budget (REQ-586 ADR-1, BR-1/BR-8).
    ///
    /// The `effort_for` shape, and for the same reason: every `Route` this
    /// router builds gets its budget here, so the `route_decided` event, the
    /// `HarnessConfig` the loop runs under, the elision marker and every
    /// refusal read one value instead of four derivations that can drift
    /// (LESSON-456). The derivation itself is
    /// [`crate::harness::budget::derive`] — pure, table-tested there, and
    /// called from **exactly one place in this crate's routing layer**: here.
    ///
    /// What the router contributes is the *classification*, not the
    /// arithmetic:
    ///
    /// - **local or nothing** — the local tier is classified from
    ///   [`CategoryTable::local_provider_id`], never from "its capabilities
    ///   look like the default" (BR-8, gotcha #9): the tier comes from the
    ///   engine rather than from a `[[providers]]` entry, so it is legitimately
    ///   absent from `providers` and a capability-shape test would call every
    ///   undeclared remote provider local. `None` — an unresolvable route —
    ///   takes the same inputs: there is no window to derive from, and the
    ///   default pair is what its [`HarnessConfig::default`] harness carries.
    /// - **remote** — the window and the user's cap come off
    ///   `capability_of(id)`, and the reservation is the `max_tokens` the
    ///   adapters actually send ([`HarnessConfig::default`]'s `gen_params`), so
    ///   the budget leaves room for the generation the same config asks for.
    /// - the id travels into the inputs because the window's *name* is part of
    ///   the fact: the in-prompt elision marker says whose window ran out (BR-7).
    ///
    /// That classification is assembled by [`Router::budget_inputs_for`]
    /// (REQ-589 TASK-259), which the offer path also reads for the *declared*
    /// window. This method is where it meets `derive`, and the only place it
    /// does.
    #[must_use]
    pub fn budget_for(&self, provider_id: Option<&str>) -> RouteBudget {
        budget::derive(self.budget_inputs_for(provider_id))
    }

    /// The inputs [`Router::budget_for`] derives from — for the surfaces that
    /// need a route's **declared window** rather than its budget (REQ-589 BR-3).
    ///
    /// The two are different facts and neither substitutes for the other.
    /// `RouteBudget::budget_tokens` is *this daemon's* policy: the window less
    /// the generation reservation, floored, and byte-clamped when the redact
    /// scan binds. [`BudgetInputs::window`] is the *provider's* declared
    /// `capabilities.max_context`, which is what
    /// [`budget::window_verdict`](crate::harness::budget::window_verdict)
    /// measures against (ADR-15: the reservation is deliberately not subtracted
    /// there) and what
    /// [`budget::proposed_window`](crate::harness::budget::proposed_window)
    /// substitutes to test a vendor recipe. A `Route` carries only the budget
    /// and `capability_of` is private, so until now there was no way to ask.
    ///
    /// # This hands out the inputs. It is not a licence to derive.
    ///
    /// `derive` is called from **exactly one place in this crate's routing
    /// layer** — `budget_for`, immediately above — and that is REQ-586 AC-12,
    /// not a style preference. A caller that takes these inputs and derives its
    /// own budget has minted a *second* figure: correct at the instant it is
    /// computed, and wrong as soon as the route is re-decided mid-turn, with
    /// nothing to say which of the two the turn actually ran under. REQ-586's
    /// own verify pass caught precisely that — `/verbose` naming a budget the
    /// turn was not running under — which is why the budget has one home at all.
    ///
    /// So **read a field; never re-derive**. Anything that wants the budget
    /// already has one: `Route::budget`, `HarnessConfig::budget`, or
    /// `budget_for` above, each a copy of the pair the route was decided with.
    /// `the_routing_layer_derives_a_budget_in_exactly_one_place` fails if that
    /// is broken, because a rule only a comment enforces is a convention rather
    /// than a guard.
    ///
    /// `pub(crate)` is part of the same answer: the offer path lives in this
    /// crate, and no client outside it may repeat the derivation at all (BR-8,
    /// AC-12) — so the reach of these inputs stops at the crate boundary.
    #[must_use]
    pub(crate) fn budget_inputs_for<'a>(&self, provider_id: Option<&'a str>) -> BudgetInputs<'a> {
        match provider_id {
            Some(id) if !self.is_local_tier(id) => {
                let capabilities = self.capability_of(id);
                BudgetInputs {
                    window: capabilities.max_context,
                    cap: capabilities.context_budget_cap,
                    reservation: budget::generation_reservation(),
                    is_local: false,
                    redact_scan: self.redact_scan,
                    provider_id: Some(id),
                }
            }
            // The local tier and the unresolvable route: no declared window, no
            // cap, and the engine's own `n_ctx` — not a provider declaration —
            // is what the default pair is sized against (OQ-3).
            _ => BudgetInputs::local(),
        }
    }

    /// Set the category a freeform judgment turn falls back to (BR-9). Read from
    /// `Config::judgment_default`, which is why it is configuration-visible
    /// rather than a constant compiled in here (AC-12).
    #[must_use]
    pub fn with_judgment_default(mut self, category: JudgmentCategory) -> Self {
        self.judgment_default = category;
        self
    }

    /// Register a provider's model, capability profile, and current health.
    #[must_use]
    pub fn with_provider(
        mut self,
        id: impl Into<String>,
        kind: ProviderKind,
        model: impl Into<String>,
        capabilities: CapabilityProfile,
        health: ProviderHealth,
    ) -> Self {
        self.providers.insert(
            id.into(),
            ProviderRuntime {
                model: model.into(),
                kind,
                capabilities,
                health,
            },
        );
        self
    }

    /// The configured freeform default provider, or `None` when none is set.
    ///
    /// `None` is a real state (REQ-557 BR-4), not a placeholder to be filled in
    /// later, and this accessor exists so that claim is assertable at the type
    /// level rather than only through the behaviour it produces. Both halves of
    /// the pre-REQ fallback chain — the positional `.find(is_remote)` and its
    /// tail through `local_provider` to the literal `"local"` — would show up
    /// here as a `Some` nobody configured.
    #[must_use]
    pub fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    /// Every configured remote provider, in id order (REQ-589 ADR-12).
    ///
    /// # What it is for
    ///
    /// BR-9's `BindTierRemote` remedy has to name a provider to bind a tier to,
    /// and D-9 authorized *performing* that remedy — not choosing where a whole
    /// category's spend goes. ADR-12 splits the difference on the count this
    /// returns: **exactly one** configured remote may be proposed by name, and
    /// **two or more** are presented as a choice rather than picked silently.
    /// Zero is a real answer too, and the honest one — there is no remote to
    /// bind to, and the offer says so instead of inventing a candidate.
    ///
    /// The count is `.len()` on the result. There is deliberately no separate
    /// counting accessor beside this: two ways to say one fact is LESSON-545's
    /// shape, and a count that could disagree with the list it summarizes is
    /// exactly the disagreement the single representation rules out.
    ///
    /// # What "remote" means here, and what it does not
    ///
    /// * **Not local.** Local-ness is classified from
    ///   [`CategoryTable::local_provider_id`], never from "its capabilities look
    ///   like the default" and never from [`ProviderKind`] (BR-8, gotcha #9):
    ///   the local tier's engine belongs to the daemon rather than to a
    ///   `[[providers]]` entry, so it is normally absent from the map — but it
    ///   is not *guaranteed* absent, which is why the filter is explicit rather
    ///   than implied by iteration.
    /// * **Configured, which already means routable.** `build_router` skips a
    ///   remote provider that declares no `model` (REQ-557 ADR-E), so map
    ///   membership *is* the usability check `Router::is_routable` reads, and
    ///   every entry here has a model for a recipe lookup to key on.
    /// * **Not screened on health.** A provider that is unavailable right now is
    ///   still configured, and this list feeds a **config write**, not a routing
    ///   decision. Filtering on health would make the same offer name different
    ///   providers on two consecutive prompts, and would hide the user's own
    ///   provider from them because it happened to be down while they were
    ///   being asked about it.
    ///
    /// Order is the `BTreeMap`'s — lexicographic by id, and stable across
    /// prompts for that reason. An offer that renumbered its choices between
    /// two renders of the same question would be a consent surface whose
    /// options moved under the answer.
    #[must_use]
    pub fn remote_providers(&self) -> Vec<RemoteProvider> {
        self.providers
            .iter()
            .filter(|(id, _)| !self.is_local_tier(id))
            .map(|(id, runtime)| RemoteProvider {
                id: id.clone(),
                model: runtime.model.clone(),
            })
            .collect()
    }

    /// Set whether the local tier can meet its BR-8 latency duty (false when it is
    /// below the hardware floor, benchmark-disabled, or shed under pressure).
    #[must_use]
    pub fn with_local_available(mut self, available: bool) -> Self {
        self.local_available = available;
        self
    }

    /// Set whether the egress redaction scan is enabled (REQ-586 BR-4).
    ///
    /// Read from `[privacy] redact` by `build_router` — the same field
    /// `redaction_gate` consults before installing the gate — so a remote
    /// route's byte budget is bounded by what the scan can actually read
    /// whole. Without it a large-window route would assemble a body the gate
    /// then refused as unscannable, which is a size failure wearing a privacy
    /// error's clothes.
    #[must_use]
    pub fn with_redact_scan(mut self, redact_scan: bool) -> Self {
        self.redact_scan = redact_scan;
        self
    }

    /// Update a registered provider's health at runtime (e.g. after a probe or a
    /// failure). No-op for an unregistered id.
    pub fn set_health(&mut self, id: &str, health: ProviderHealth) {
        if let Some(p) = self.providers.get_mut(id) {
            p.health = health;
        }
    }

    /// Resolve `category` to a provider — **the** dispatch entry point, used
    /// identically in both session modes (BR-1).
    ///
    /// The decision itself is [`teton_core::category::resolve`]'s: override →
    /// tier → declared error, with provider health and provider *usability*
    /// injected as closures (ADR-D, ADR-E). The router only attaches the model
    /// and the BR-6 harness profile of whatever the resolver chose, and carries
    /// the resolution itself on the route, so every surface downstream reads one
    /// answer rather than computing a second (AC-11).
    ///
    /// It takes no phase and no prompt. No phase, because lifecycle position
    /// stopped being a routing input (AC-9); no prompt, because a routing
    /// function that can see the text is a function a keyword list can be
    /// dropped into — the defect this REQ exists to close (BR-2).
    #[must_use]
    pub fn resolve(&self, category: CoreCategory) -> Route {
        self.route_from(self.resolution_for(category))
    }

    /// The pure [`CategoryResolution`] behind [`Router::resolve`], for a caller
    /// that needs the decision itself rather than a turn-ready [`Route`].
    ///
    /// It exists for exactly one caller: [`crate::classify::plan`], which asks
    /// the resolver whether the `route` category can be served *before* deciding
    /// to classify. Handing it the resolution rather than a `Route` keeps the
    /// bypass question answered by `teton_core::category::resolve` (ADR-D) and
    /// keeps the classifier free of any locality check of its own (LESSON-484).
    ///
    /// [`Router::resolve`] is defined in terms of this, so there is one
    /// construction site and the two cannot answer differently.
    #[must_use]
    pub fn resolution_for(&self, category: CoreCategory) -> CategoryResolution {
        let table = self.effective_table();
        resolve_category(
            category,
            &table,
            |id| self.health_of(id),
            // ADR-E / BUG-155: the resolver never emits a provider the daemon
            // would refuse to route to. BUG-155's Critical finding was three
            // config-reading paths that each bypassed this screen, and a new
            // dispatch axis is a fourth unless it is screened where the decision
            // is made (LESSON-484).
            |id| self.is_usable(id),
        )
    }

    /// Every category's resolution, for `teton policy show` (ADR-A's table).
    ///
    /// The **reporting** surface: it resolves all eleven categories and
    /// dispatches none of them. That distinction is why it exists as its own
    /// method rather than as a loop over [`Router::resolution_for`] in the
    /// snapshot builder — `call_sites`' source scan reads routing calls to
    /// derive which categories the harness actually reaches, and a loop over
    /// every category at the reporting surface would either be miscounted as
    /// eleven call sites or force the scan to special-case a caller. Resolving
    /// the whole table is the router's own job, stated once, here.
    ///
    /// It is defined in terms of [`Router::resolution_for`], so the row a user
    /// reads for a category and the decision a turn makes for it are the same
    /// value (BR-6, AC-11).
    #[must_use]
    pub fn table_report(&self) -> Vec<CategoryResolution> {
        CoreCategory::ALL
            .into_iter()
            .map(|category| self.resolution_for(category))
            .collect()
    }

    /// The category a freeform judgment turn takes when classification is
    /// **bypassed or fails** — the BR-9 declared default, read from
    /// `Config::judgment_default` (AC-12).
    #[must_use]
    pub fn judgment_default(&self) -> JudgmentCategory {
        self.judgment_default
    }

    /// [`Router::judgment_default`] as a [`CoreCategory`] — what a freeform turn
    /// dispatches on when the classifier did not choose (ADR-C, BR-3, BR-9).
    ///
    /// It deliberately takes no prompt. That is not an accident of the current
    /// implementation, it is the guarantee: the `route` classifier lives *outside*
    /// this module ([`crate::classify`]) and hands the router a
    /// [`JudgmentCategory`], so no routing function ever receives text. Giving
    /// this one the prompt would be precisely where a substring list reappears —
    /// which BR-2 deletes rather than relocates.
    #[must_use]
    pub fn freeform_category(&self) -> CoreCategory {
        CoreCategory::from(self.judgment_default)
    }

    /// Resolve the category a **freeform** turn was classified into, with the
    /// classification's own signal folded into the reason (ADR-C, BR-3).
    ///
    /// One `route_decided` answers both halves of the question a user asks when a
    /// turn goes somewhere surprising: *why this category* (the classifier ran,
    /// or was bypassed, or failed) and *why this provider* (the resolver's
    /// sentence, verbatim). Composed here, in one place, rather than at each call
    /// site — a second surface assembling its own version of this sentence is the
    /// drift BR-6 exists to prevent.
    ///
    /// [`Route::resolution`] is untouched: it still carries the resolver's own
    /// answer, which is what `route_decided` projects the category and tier from
    /// (AC-11).
    #[must_use]
    pub fn resolve_judgment(&self, classification: &Classification) -> Route {
        let resolved = self.resolve(CoreCategory::from(classification.category));
        Route {
            reason: format!("{} {}", classification.sentence(), resolved.reason),
            ..resolved
        }
    }

    /// Force a route to the **local tier**, ignoring the category table
    /// entirely (REQ-544 C-2 / M-1, REQ-558 BR-7).
    ///
    /// This is the taint backstop for BR-1: a session whose context has touched
    /// `local-only` content — or an unknown-provenance `shell` result — is pinned
    /// here for every subsequent turn, and a remote turn blocked at egress is
    /// re-run here rather than retried. Privacy trumps latency, so this pins local
    /// even when the local tier is latency-degraded; the caller checks whether a
    /// local engine actually exists (a remote-only machine cannot serve a tainted
    /// session and fails closed instead).
    ///
    /// Category routing is a cost decision and this is a privacy guarantee, so
    /// the two do not compose: the caller checks taint *before* resolving a
    /// category, and this function reads no binding at all (BR-7, LESSON-432).
    #[must_use]
    pub fn resolve_local_pin(&self, reason: impl Into<String>) -> Route {
        let Some(provider) = self.table.local_provider_id.clone() else {
            // No local tier to derive anything from, and no turn will run: the
            // strict default config, whose budget is the default (local) pair
            // under `bound: local_engine` — the same fact
            // `HarnessConfig::default()` has always carried, now named. Nothing
            // reports it: `route_decided()` is `None` without a provider.
            let harness = HarnessConfig::default();
            return Route {
                model: None,
                budget: harness.budget.clone(),
                harness,
                provider_id: None,
                phase: None,
                reason: "This session is pinned to the local tier for privacy, but no local \
                         provider is registered, so the turn cannot be served."
                    .to_owned(),
                outcome: RouteOutcome::NoPolicy,
                resolution: None,
                effort: None,
            };
        };
        // REQ-559 BR-7: the taint pin lands on the local tier, whose declared
        // shape is `none`, so a global bump to `max` cannot put a reasoning
        // field on a locally-served turn. The cap comes from the clamp table,
        // not from per-category configuration (which BR-2 forbids).
        let effort = self.effort_for(Some(&provider));
        // Through `harness_config_for` like every other route, so the pin gets
        // the local tier's derived budget (`bound: local_engine`) from the one
        // classifier rather than by construction (REQ-586 BR-8).
        let harness = self.harness_config_for(&provider);
        Route {
            model: self.model_of(&provider),
            budget: harness.budget.clone(),
            harness,
            provider_id: Some(ProviderId::from(provider)),
            phase: None,
            reason: reason.into(),
            outcome: RouteOutcome::Fallback,
            effort,
            // BR-7: the taint pin overrides every category binding, so this route
            // is deliberately *not* a category resolution — it is the privacy
            // backstop refusing to consult one.
            resolution: None,
        }
    }

    /// Handle a mid-session provider failure on `route` (AC-7).
    ///
    /// Classifies `class` ([`teton_providers::classify`]) and:
    /// - **Fallback** — continues on the fallback **the failed route's own
    ///   resolution already chose**, and emits `provider_degraded` naming it.
    /// - **Degrade** — keeps the same provider but forces the reduced BR-6 harness
    ///   profile, and emits `provider_degraded` with no fallback.
    /// - **Retry** — transient; no event, the caller retries the same route.
    /// - **Fail** — unrecoverable (e.g. auth); no route, the caller aborts.
    ///
    /// Only the **Fallback** arm spends the fallback. `Degrade` and `Retry` hand
    /// the turn back to the *same* provider, so the alternative the resolution
    /// chose is still unspent and is still there for the next failure — a
    /// timeout followed by a malformed response falls over, rather than
    /// reporting that no fallback is configured.
    ///
    /// It takes the failed [`Route`] rather than a phase (AC-9). That is not only
    /// a signature change: the fallback now comes from the resolution the turn
    /// was routed by, already screened for usability and health when it was
    /// resolved (ADR-E), instead of being re-read out of the configured table on
    /// the failure path — the second read that is exactly BUG-155's shape.
    #[must_use]
    pub fn on_provider_failure(
        &self,
        route: &Route,
        failed_provider: &str,
        class: FailureClass,
    ) -> FailureOutcome {
        let decision = classify(class);
        let signal = degradation_signal(failed_provider, decision);
        match decision.action {
            FailureAction::Fallback => {
                let fallback = self.fallback_for(route);
                let next = fallback.as_deref().map(|fb| {
                    let reason = signal.as_ref().map_or_else(
                        || format!("Falling back to '{fb}' after a provider failure."),
                        |s| format!("{} Continuing on the fallback '{fb}'.", s.reason),
                    );
                    // The one arm that actually switches providers, and so the
                    // one arm that spends the fallback.
                    Self::consume_fallback(self.continue_on(
                        route,
                        fb,
                        RouteOutcome::Fallback,
                        reason,
                        self.harness_config_for(fb),
                    ))
                });
                FailureOutcome {
                    degraded: Some(ProviderDegraded {
                        provider_id: ProviderId::from(failed_provider),
                        failure_class: to_protocol_failure_class(class),
                        fallback_id: fallback.map(ProviderId::from),
                    }),
                    route: next,
                }
            }
            FailureAction::Degrade => {
                // Keep the provider, force the reduced profile (BR-6): the failure
                // revealed weak tool-calling regardless of the declared tier.
                let reason = signal.as_ref().map_or_else(
                    || format!("'{failed_provider}' dropped to a reduced harness profile."),
                    |s| s.reason.clone(),
                );
                let next = self.continue_on(
                    route,
                    failed_provider,
                    RouteOutcome::PrimaryDegraded,
                    reason,
                    // The failed provider's own budget under a reduced profile
                    // (ADR-2): the tool-call tier changed, the window did not.
                    self.degraded_harness_config(failed_provider),
                );
                FailureOutcome {
                    degraded: Some(ProviderDegraded {
                        provider_id: ProviderId::from(failed_provider),
                        failure_class: to_protocol_failure_class(class),
                        fallback_id: None,
                    }),
                    route: Some(next),
                }
            }
            FailureAction::Retry => FailureOutcome {
                degraded: None,
                route: Some(self.continue_on(
                    route,
                    failed_provider,
                    RouteOutcome::Primary,
                    format!("Retrying '{failed_provider}' after a transient failure."),
                    self.harness_config_for(failed_provider),
                )),
            },
            FailureAction::Fail => FailureOutcome {
                degraded: None,
                route: None,
            },
        }
    }

    /// Broadcast the `route_decided` event for `route` (BR-5), when a provider was
    /// selected. Scoped to `session_id`.
    pub fn emit_route_decided(&self, bus: &EventBus, session_id: Option<SessionId>, route: &Route) {
        if let Some(mut decided) = route.route_decided() {
            // BR-2: stamped here, at the one place the event is published, so
            // the ceiling the surface names is the ceiling this turn ran under.
            decided.spend_ceiling_micro_cents = self.spend_ceiling_micro_cents;
            bus.publish(session_id, Event::RouteDecided(decided));
        }
    }

    /// Broadcast a `provider_degraded` event (AC-7), scoped to `session_id`.
    pub fn emit_provider_degraded(
        &self,
        bus: &EventBus,
        session_id: Option<SessionId>,
        degraded: ProviderDegraded,
    ) {
        bus.publish(session_id, Event::ProviderDegraded(degraded));
    }

    /// Build the [`EgressContext`] for a routed **remote** call: the selected
    /// provider, the owning `session_id`, and the phase-pinned [`CostAttribution`]
    /// (BR-2). Threading this into [`crate::egress::Egress::send`] is what makes
    /// privacy enforcement (BR-1) and cost recording (BR-2) hold by construction.
    ///
    /// Returns `None` when the route selected no provider, or the provider is
    /// unregistered (no model to bill).
    #[must_use]
    pub fn egress_context(
        &self,
        route: &Route,
        session_id: impl Into<SessionId>,
    ) -> Option<EgressContext> {
        let provider_id = route.provider_id.clone()?;
        let model = route.model.clone()?;
        // Both attribution dimensions, and neither derived from the other: the
        // phase is what the spend belongs to (stamped on by the caller after
        // the decision), the category is what it was *for* (read off the
        // resolution the turn was routed by — ADR-D, never recomputed from the
        // phase). A freeform turn has the second without the first.
        let mut attribution = CostAttribution::new(model);
        if let Some(phase) = route.phase {
            attribution = attribution.with_phase(phase);
        }
        if let Some(category) = route.resolution.as_ref().map(|r| r.category) {
            attribution = attribution.with_category(to_protocol_category(category));
        }
        Some(
            EgressContext::new(provider_id)
                .with_session(session_id)
                .with_cost(attribution),
        )
    }

    /// The BR-6 [`HarnessConfig`] a `provider_id` should run under, derived from
    /// its capability profile. An unregistered provider defaults to the strict
    /// (Native) profile.
    ///
    /// Two facts about the provider, stamped in one place (REQ-586 ADR-1): the
    /// *tool-call* profile (how long a loop, how many tools, verification) and
    /// the *context budget* the turn runs under. The budget goes on through
    /// [`HarnessConfig::with_route_budget`] rather than being set field by
    /// field, so a config's budget-bearing fields cannot disagree with the
    /// [`RouteBudget`] the route reports.
    #[must_use]
    pub fn harness_config_for(&self, provider_id: &str) -> HarnessConfig {
        HarnessConfig::from_harness_profile(self.capability_of(provider_id).harness_profile())
            .with_route_budget(self.budget_for(Some(provider_id)))
    }

    // ---- internal helpers ----

    /// The table resolution actually reads: the configured rows, plus a
    /// **declared default** for every tier the config leaves unbound.
    ///
    /// An unbound tier inherits, in order:
    ///
    /// 1. `default_provider` — REQ-557 BR-4's key, whose documented meaning is
    ///    literally "the provider an unrouted turn goes to";
    /// 2. the local tier, which exists on this machine whether or not the config
    ///    mentions it, and is the whole of REQ-544's local-first promise: a
    ///    machine with no remote provider at all still routes.
    ///
    /// Every id here is **declared** — one by a config key, one by the engine —
    /// so nothing is synthesized (BR-8), and this is the only place a default is
    /// applied. `resolve`'s precedence is untouched: it sees a table and walks
    /// override → tier → declared error, and an id that got here by inheritance
    /// is screened by `is_usable` exactly like one the user typed.
    ///
    /// TASK-055's migration now writes this fill down as real `[[tiers]]` rows
    /// on the first start after upgrade, which is what makes it visible and
    /// editable rather than an invisible runtime default. The fill stays anyway,
    /// and is not merely belt-and-braces: it covers the states the migration by
    /// construction cannot reach — a daemon with no config path at all, a config
    /// whose migration write failed, a tier the user deleted by hand, and a
    /// `default_provider` set after the migration already ran. What changed is
    /// that the fill is no longer the *only* record of the answer.
    fn effective_table(&self) -> CategoryTable {
        CoreTier::ALL
            .into_iter()
            .filter(|tier| self.table.tier_binding(*tier).is_none())
            .filter_map(|tier| self.inherited_provider(tier).map(|p| (tier, p)))
            .fold(self.table.clone(), |table, (tier, provider_id)| {
                table.with_tier(TierBinding {
                    tier,
                    provider_id,
                    fallback_id: None,
                })
            })
    }

    /// What an **unbound** tier inherits, or `None` when it inherits nothing.
    ///
    /// `reflex` and `scan` inherit the local tier and nothing else — a tier
    /// whose work was already local before this REQ stays local until the user
    /// says otherwise. `reflex` by definition ("sub-second, every turn, **never
    /// leaves the machine**"); `scan` because its only reached category,
    /// `digest`, ran on the local engine unconditionally and sent nothing
    /// anywhere. Filling either from `default_provider` would be the ordinary
    /// upgrade path, not a contrived one: REQ-557's migration sets
    /// `default_provider` to the first remote provider, and an upgraded config
    /// has no `[[tiers]]` at all — so a config that had never let a file body
    /// off the machine would start shipping them because of a key the user set
    /// for their turns.
    ///
    /// `build`/`think` — the turn tiers — inherit `default_provider` and fall
    /// back to the local tier, so an offline install still routes. Nothing is
    /// synthesized at any step; every candidate is config- or engine-declared
    /// (BR-8).
    ///
    /// The exclusion is asked of [`CoreTier::inherits_default_provider`] rather
    /// than re-spelled here, because TASK-055's migration writes this same fill
    /// down as real `[[tiers]]` rows and must make the same exclusions for the
    /// same reasons. One fact, one home.
    fn inherited_provider(&self, tier: CoreTier) -> Option<String> {
        self.inherited_binding(tier).map(|(_, provider)| provider)
    }

    /// [`Router::inherited_provider`] with *where it came from* attached.
    ///
    /// The two exist as one function because `teton policy show` has to report
    /// the fill, not merely apply it: a user looking at `reflex → local` on a
    /// machine whose `default_provider` is `anthropic` is owed the reason, and
    /// the reason is [`CoreTier::inherits_default_provider`]. A second function
    /// computing the origin by comparing ids against `default_provider` would be
    /// a restatement of that rule, and would go wrong the first time someone
    /// sets `default_provider` to the local tier's own id.
    /// An **unusable** `default_provider` inherits nothing, and falls through to
    /// the local tier exactly as an absent one does.
    ///
    /// This is [`Router::is_usable`] — BUG-155's screen — applied to the one
    /// candidate that was reaching a tier binding without it. `default_provider`
    /// is a plain config key: it can name a provider that was deleted, or a
    /// remote provider that declares no `model` and therefore never entered the
    /// map (REQ-557 ADR-E). Returning it unscreened bound every unbound tier to
    /// an id that cannot serve, and because the arm returned early it never
    /// reached the local one — so a machine with a healthy local tier failed
    /// every turn rather than serving them locally, which is the state
    /// [`Router::inherited_provider`]'s own contract says must route.
    ///
    /// Health is deliberately *not* screened here. A binding is what the table
    /// says; whether the provider is up is `category::resolve`'s question, and
    /// it has a fallback to try and a sentence to write. Usability is different:
    /// an id that can never serve is not a binding at all.
    fn inherited_binding(&self, tier: CoreTier) -> Option<(TierOrigin, String)> {
        if tier.inherits_default_provider() {
            if let Some(default) = self
                .default_provider
                .clone()
                .filter(|id| self.is_usable(id))
            {
                return Some((TierOrigin::DefaultProvider, default));
            }
        }
        self.table
            .local_provider_id
            .clone()
            .map(|local| (TierOrigin::LocalTier, local))
    }

    /// What `tier` is bound to and where that binding came from — the tier half
    /// of `teton policy show` (ADR-H).
    ///
    /// The configured row wins; an unbound tier reports the fill
    /// [`Router::inherited_binding`] would apply, so the table a user reads is
    /// the table resolution reads (BR-6). It reports the binding, not a routing
    /// decision: whether the named provider is healthy and usable is
    /// `category::resolve`'s answer, and it is given per category in the rows
    /// below it.
    #[must_use]
    pub fn tier_report(&self, tier: CoreTier) -> TierReport {
        if let Some(binding) = self.table.tier_binding(tier) {
            return TierReport {
                tier,
                provider_id: Some(binding.provider_id.clone()),
                fallback_id: binding.fallback_id.clone(),
                origin: TierOrigin::Configured,
            };
        }
        match self.inherited_binding(tier) {
            Some((origin, provider_id)) => TierReport {
                tier,
                provider_id: Some(provider_id),
                fallback_id: None,
                origin,
            },
            None => TierReport {
                tier,
                provider_id: None,
                fallback_id: None,
                origin: TierOrigin::Unbound,
            },
        }
    }

    /// Turn a [`CategoryResolution`] into the [`Route`] the daemon threads into a
    /// turn: the resolver's provider, reason and outcome verbatim, plus the model
    /// and BR-6 harness profile of whatever it chose.
    ///
    /// The resolution rides along on [`Route::resolution`] rather than being
    /// consumed here, which is what lets `route_decided` report the category and
    /// tier without recomputing either (ADR-D, AC-11).
    fn route_from(&self, resolution: CategoryResolution) -> Route {
        let provider_id = resolution.provider_id.clone();
        // The no-provider arm keeps the strict default, whose budget is the
        // default (local) pair under `bound: local_engine` (REQ-586): the
        // category resolved to nobody, so there is no window to derive from,
        // and no attempt will be made — `route_decided()` reports nothing here.
        let harness = provider_id
            .as_deref()
            .map_or_else(HarnessConfig::default, |id| self.harness_config_for(id));
        Route {
            model: provider_id.as_deref().and_then(|id| self.model_of(id)),
            budget: harness.budget.clone(),
            provider_id: provider_id.map(ProviderId::from),
            // Attribution only, and stamped on by the caller after the fact
            // (BR-11, AC-9). The resolver never saw a phase.
            phase: None,
            reason: resolution.reason.clone(),
            outcome: resolution.outcome,
            harness,
            effort: self.effort_for(resolution.provider_id.as_deref()),
            resolution: Some(resolution),
        }
    }

    fn capability_of(&self, provider_id: &str) -> CapabilityProfile {
        self.providers
            .get(provider_id)
            .map_or_else(CapabilityProfile::default, |p| p.capabilities)
    }

    fn model_of(&self, provider_id: &str) -> Option<String> {
        self.providers.get(provider_id).map(|p| p.model.clone())
    }

    /// Whether `provider_id` names the local tier.
    fn is_local_tier(&self, provider_id: &str) -> bool {
        self.table.local_provider_id.as_deref() == Some(provider_id)
    }

    /// Health of a provider as resolution sees it.
    ///
    /// An unregistered id is unavailable, so a binding naming a provider the
    /// daemon does not know cannot select it.
    ///
    /// The **local tier** is the one id whose absence from the map is normal —
    /// its engine belongs to the daemon rather than to a `[[providers]]` entry —
    /// and its BR-8 latency duty is reported here, on the *health* axis, rather
    /// than through `is_usable`. That placement is a legibility decision, not a
    /// tidiness one: "not routable" is REQ-557 ADR-E's remote-provider-declares-
    /// no-`model` condition and the resolver's sentence says exactly that, which
    /// would be a wrong explanation for a local tier shed under memory pressure.
    /// A tier that cannot serve is *unavailable*, and reads as unavailable.
    fn health_of(&self, provider_id: &str) -> ProviderHealth {
        if self.is_local_tier(provider_id) && !self.local_available {
            return ProviderHealth::Unavailable;
        }
        match self.providers.get(provider_id) {
            Some(p) => p.health,
            None if self.is_local_tier(provider_id) => ProviderHealth::Healthy,
            None => ProviderHealth::Unavailable,
        }
    }

    /// Whether `provider_id` is actually routable — i.e. it entered the provider
    /// map at construction.
    ///
    /// `build_router` deliberately skips a remote provider that declares no
    /// model (REQ-557 ADR-E), so map membership IS the usability check.
    fn is_routable(&self, provider_id: &str) -> bool {
        self.providers.contains_key(provider_id)
    }

    /// Whether the daemon would actually route a turn to `provider_id` — the
    /// `usable` screen injected into [`teton_core::category::resolve`] (ADR-E).
    ///
    /// Two ways to be usable, because there are two kinds of provider:
    ///
    /// - a **registered** provider — [`Router::is_routable`], which is REQ-557
    ///   ADR-E's rule that a remote provider declaring no `model` never enters
    ///   the map at all;
    /// - the **local tier**, whose engine comes from the daemon rather than from
    ///   `[[providers]]`, so map membership is not its test. Whether it can serve
    ///   is a health fact, reported by [`Router::health_of`].
    ///
    /// Neither branch is consulted by [`Router::resolve_local_pin`]: the taint
    /// backstop pins local even when the tier is below its BR-8 duty, because
    /// privacy trumps latency (BR-7).
    ///
    /// # What this screen is actually for
    ///
    /// Worth being exact, because overstating it is how a guard ends up with no
    /// coverage. It does **not** currently keep an unusable provider out of a
    /// route on its own: [`Router::health_of`] reports an unmapped id as
    /// `Unavailable`, so an unusable remote provider is rejected either way. What
    /// the screen decides is *which rejection* — and therefore what the user
    /// reads. `not routable` carries ADR-E's sentence ("a remote provider that
    /// declares no `model` cannot serve a turn"), which names the cause and the
    /// remedy; `unavailable` says the provider is down, which for a provider
    /// missing its `model` is simply untrue and leaves the user nothing to do.
    ///
    /// The second, load-bearing direction is the local tier, where the screen
    /// *widens*: without its arm, a local tier absent from `[[providers]]` — the
    /// normal case, and every offline install — would be rejected as unroutable
    /// and nothing would route at all.
    ///
    /// Both directions are pinned by tests that fail if this function is replaced
    /// by `|_| true` or by `is_routable` alone.
    fn is_usable(&self, provider_id: &str) -> bool {
        self.is_routable(provider_id) || self.is_local_tier(provider_id)
    }

    /// The fallback to continue `route` on, read **off the resolution the turn
    /// was routed by** rather than re-read from the configured table.
    ///
    /// `category::resolve` already screened this id for usability and health when
    /// it built the resolution, and set it to `None` once the fallback has itself
    /// been used — so a second failure has nowhere further to go and says so.
    /// That is deliberate: re-reading config on the failure path is the shape
    /// BUG-155 found three times, where one path screened providers and another
    /// did not.
    ///
    /// A route with no resolution — [`Router::resolve_local_pin`], the taint
    /// backstop — therefore has no fallback at all. A tainted session must not
    /// fail over to a remote provider (BR-7), and here it cannot.
    fn fallback_for(&self, route: &Route) -> Option<String> {
        route.resolution.as_ref()?.fallback_id.clone()
    }

    /// The route to continue a failed turn on: a new provider, a new reason, the
    /// same turn.
    ///
    /// It carries the failed route's phase and resolution forward **verbatim**,
    /// because the turn's *category* has not changed — only which provider is
    /// serving it.
    ///
    /// The fallback is deliberately **not** cleared here. Two of the three arms
    /// that call this keep serving on the *same* provider — `Retry` re-attempts
    /// it, `Degrade` re-attempts it under a reduced harness profile — so the
    /// fallback has not been used and must still be there when the retry itself
    /// fails. Clearing it unconditionally meant one transient timeout removed the
    /// configured fallback for the rest of the turn, and the daemon then told the
    /// user "provider failed and no fallback is configured" about a config that
    /// configures one. Reachable inside the 2-attempt budget on the plain
    /// `timeout → malformed` sequence. Consumption belongs to the arm that
    /// consumes ([`Router::consume_fallback`]), not to the arm that retries.
    fn continue_on(
        &self,
        failed: &Route,
        provider: &str,
        outcome: RouteOutcome,
        reason: String,
        harness: HarnessConfig,
    ) -> Route {
        Route {
            provider_id: Some(ProviderId::from(provider)),
            model: self.model_of(provider),
            phase: failed.phase,
            reason,
            outcome,
            // The continuing route's budget is the continuing config's — the
            // caller already derived it (`harness_config_for` on a fallback or
            // a retry, `degraded_harness_config` on a degrade), so this arm
            // copies rather than re-deriving (REQ-586 AC-12).
            budget: harness.budget.clone(),
            harness,
            effort: self.effort_for(Some(provider)),
            resolution: failed.resolution.clone(),
        }
    }

    /// The forced reduced BR-6 harness profile for `failed`, used when a
    /// failure reveals weak tool-calling regardless of the declared tier.
    ///
    /// It derives from the **failed provider's own** capability profile with
    /// only [`ToolCallTier::Degraded`] forced on, rather than from
    /// `CapabilityProfile::default()`: what the failure revealed is that this
    /// provider calls tools badly, not that it forgot how big its window is.
    /// Only the tier is overridden — every other capability, the ladder, the
    /// reasoning shape, the cap, is still the provider's own — because the tier
    /// is the one fact the failure is evidence about.
    ///
    /// **What actually keeps the window is the line below it.** Pre-REQ-586 the
    /// budget rode the profile, so a default-profile degrade re-budgeted a 128k
    /// route to the unknown-window default mid-turn and reported
    /// `bound: default_unknown` for a provider that declares a window (gotcha
    /// #1). ADR-2's fix was to stop deriving the budget from the profile at all:
    /// it is stamped from [`Router::budget_for`], the crate's one derivation,
    /// against `capability_of(failed)`. So the window survives the degrade, the
    /// bound stays `window`, and no refit is needed because the budget did not
    /// move (BR-1, AC-15) — pinned by
    /// `a_degrade_keeps_the_failed_providers_budget`, which is red if that
    /// stamp is dropped or re-sourced.
    ///
    /// The spread above is therefore *not* what protects the budget today, and
    /// TASK-192's mutation (b) — writing `..CapabilityProfile::default()` here —
    /// is an **equivalent mutant**: [`CapabilityProfile::harness_profile`]'s
    /// `Degraded` arm is a constant that reads no other field, so no test can
    /// distinguish the two spellings. The spread stays because it states the
    /// right rule for the day that arm starts reading one.
    fn degraded_harness_config(&self, failed: &str) -> HarnessConfig {
        use teton_core::ToolCallTier;
        let degraded = CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            ..self.capability_of(failed)
        };
        HarnessConfig::from_harness_profile(degraded.harness_profile())
            .with_route_budget(self.budget_for(Some(failed)))
    }

    /// Spend the resolution's fallback: the route is now *running on* it, so a
    /// further failure has nowhere left to go and must say so rather than fail
    /// over to itself and loop.
    ///
    /// Called from exactly one place — the [`FailureAction::Fallback`] arm — and
    /// that is the whole point. "The fallback has been used" is a fact about
    /// having switched providers, so only the switch may assert it.
    fn consume_fallback(mut route: Route) -> Route {
        if let Some(resolution) = route.resolution.as_mut() {
            resolution.fallback_id = None;
        }
        route
    }
}

/// Map a `teton_providers::FailureClass` to the `teton_protocol` event vocabulary
/// carried on `provider_degraded`. Content-free by construction (class + status
/// only).
#[must_use]
fn to_protocol_failure_class(class: FailureClass) -> ProtoFailureClass {
    match class {
        FailureClass::Timeout => ProtoFailureClass::Timeout,
        FailureClass::Transport => ProtoFailureClass::ConnectionError,
        FailureClass::ClientError { status: 429 } => ProtoFailureClass::RateLimited,
        FailureClass::ClientError { status: 408 } => ProtoFailureClass::Timeout,
        FailureClass::ClientError { .. } => ProtoFailureClass::InvalidResponse,
        FailureClass::ServerError { .. } => ProtoFailureClass::ConnectionError,
        FailureClass::MalformedResponse => ProtoFailureClass::InvalidResponse,
        FailureClass::MalformedToolCall => ProtoFailureClass::ToolCallFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::ClassificationSignal;
    use crate::egress::redact::REDACT_SCANNABLE_CONTEXT_BYTES;
    use teton_core::category::category_for_phase;
    use teton_core::effort::EffortLadder;
    use teton_core::ToolCallTier;
    use teton_protocol::events::BudgetBound;

    /// A `Route` carrying `resolution`, with everything else fixed. The point of
    /// each test below is the resolution, not the wiring around it.
    fn route_from(resolution: CategoryResolution) -> Route {
        Route {
            provider_id: resolution.provider_id.clone().map(ProviderId::from),
            model: Some("some-model".to_owned()),
            phase: None,
            reason: resolution.reason.clone(),
            outcome: resolution.outcome,
            harness: HarnessConfig::default(),
            // The wiring the point of these tests is not: the default harness
            // and its own budget, so the pair still agrees (AC-12).
            budget: HarnessConfig::default().budget,
            effort: None,
            resolution: Some(resolution),
        }
    }

    /// REQ-558 AC-8: a decision that resolved a category names it, names the
    /// tier, names the provider, and carries a non-empty reason. Swept across
    /// **every** category, resolved for real, so a category that stops appearing
    /// on the wire fails here rather than in a client.
    #[test]
    fn route_decided_names_the_category_tier_provider_and_reason() {
        use teton_core::category::{resolve, CategoryTable, TierBinding};
        use teton_core::Tier;

        let table = Tier::ALL.into_iter().fold(
            CategoryTable::new().with_local_provider("on-device"),
            |t, tier| {
                t.with_tier(TierBinding {
                    tier,
                    provider_id: "remote".to_owned(),
                    fallback_id: None,
                })
            },
        );

        for category in CoreCategory::ALL {
            let resolution = resolve(category, &table, |_| ProviderHealth::Healthy, |_| true);
            let route = route_from(resolution.clone());
            let decided = route
                .route_decided()
                .unwrap_or_else(|| panic!("{category} selected no provider"));

            assert_eq!(decided.category, Some(to_protocol_category(category)));
            assert_eq!(decided.tier, Some(to_protocol_tier(resolution.tier)));
            assert_eq!(decided.provider_id.0, resolution.provider_id.unwrap());
            assert!(!decided.reason.is_empty(), "{category}");
            // REQ-586 BR-8: and the budget the attempt runs under, projected
            // off the route rather than recomputed — never absent, on any
            // category. `None` on this wire means "a daemon that predates
            // REQ-586", which is a different claim from "no budget applied".
            assert_eq!(
                decided.budget_tokens,
                Some(route.budget.budget_tokens as u64),
                "{category}"
            );
            assert_eq!(
                decided.budget_bytes,
                Some(route.budget.budget_bytes as u64),
                "{category}"
            );
            assert_eq!(decided.bound, Some(route.budget.bound), "{category}");
        }
    }

    /// AC-11 / ADR-D, as a provenance check rather than a value check: the event
    /// reports the tier **the resolution reported**, and does not recompute one
    /// from the category.
    ///
    /// The resolution here is deliberately self-inconsistent — `edit` with the
    /// `think` tier — which `resolve` would never produce. That is the only way
    /// to tell "read off the resolution" apart from "recomputed via
    /// `Category::tier()`", because the two agree on every real input. Replacing
    /// the projection with `r.category.tier()` turns this red.
    #[test]
    fn the_tier_on_the_wire_comes_from_the_resolution_not_from_the_category() {
        let resolution = CategoryResolution {
            category: CoreCategory::Edit,
            tier: CoreTier::Think,
            provider_id: Some("remote".to_owned()),
            fallback_id: None,
            source: teton_core::category::BindingSource::TierInheritance,
            reason: "a resolution nobody would compute".to_owned(),
            outcome: RouteOutcome::Primary,
        };
        assert_ne!(
            CoreCategory::Edit.tier(),
            CoreTier::Think,
            "this test is only meaningful while the two disagree"
        );

        let decided = route_from(resolution).route_decided().expect("decided");
        assert_eq!(decided.tier, Some(ProtoTier::Think));
        assert_eq!(decided.category, Some(ProtoCategory::Edit));
    }

    /// The reason travels verbatim. BR-6/AC-11's "two surfaces must not drift"
    /// is not satisfied by a paraphrase, so the event carries the resolution's
    /// own sentence, byte for byte.
    #[test]
    fn an_unresolvable_category_puts_its_own_sentence_on_the_route() {
        use teton_core::category::{resolve, CategoryTable};

        let empty = CategoryTable::new();
        let resolution = resolve(
            CoreCategory::Design,
            &empty,
            |_| ProviderHealth::Healthy,
            |_| true,
        );
        assert!(resolution.provider_id.is_none());
        assert!(
            resolution.reason.contains("'design'"),
            "{}",
            resolution.reason
        );

        // No provider was selected, so there is no `route_decided` to emit — the
        // event's `provider_id` is required. The sentence is still the
        // resolution's, and the route that carries it is the same object.
        let route = route_from(resolution.clone());
        assert!(route.route_decided().is_none());
        assert_eq!(route.reason, resolution.reason);
    }

    /// BUG-155: a `fallback_id` naming a provider the router never registered is
    /// not failed over to.
    ///
    /// Pinned HERE, at the router, rather than only through an end-to-end
    /// "no bytes reached the provider" assertion. Those two guards — screening
    /// the fallback, and refusing a remote route that carries no model — mask
    /// each other: with either one in place the traffic assertion still passes,
    /// so mutating either alone looked safe. Defence in depth is fine; two
    /// guards with no independent coverage is not (LESSON-483's shape, one layer
    /// over). This test fails if the screen is removed, whatever the backstop does.
    ///
    /// REQ-558 moves *where* the screen happens — `category::resolve` now screens
    /// the fallback when it builds the resolution, and the failure path reads the
    /// answer off it rather than re-reading config — so this asserts the same
    /// property one layer down.
    #[test]
    fn a_fallback_to_an_unregistered_provider_is_not_taken() {
        use teton_providers::FailureClass;

        let router = Router::new(
            CategoryTable::new().with_tier(TierBinding {
                tier: CoreTier::Build,
                provider_id: "primary".to_owned(),
                fallback_id: Some("unusable".to_owned()),
            }),
            None,
        )
        // Only the primary is registered. `build_router` skips a remote provider
        // that declares no model (ADR-E), so "unusable" being absent from the
        // map is exactly what that looks like from here.
        .with_provider(
            "primary",
            ProviderKind::OpenaiCompatible,
            "deepseek-chat",
            native(),
            ProviderHealth::Healthy,
        );

        let route = router.resolve(CoreCategory::Edit);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "primary");
        assert!(
            route.resolution.as_ref().unwrap().fallback_id.is_none(),
            "an unroutable fallback must not survive onto the resolution — the \
             failure path reads it from there: {route:?}"
        );

        let outcome =
            router.on_provider_failure(&route, "primary", FailureClass::MalformedResponse);

        assert!(
            outcome.route.is_none(),
            "a turn must not fail over to a provider the router considers \
             unusable — its context would egress to an endpoint the daemon told \
             the user could not serve turns: {:?}",
            outcome.route
        );
    }

    fn native() -> CapabilityProfile {
        CapabilityProfile {
            tool_call_tier: ToolCallTier::Native,
            parallel_calls: true,
            max_context: 200_000,
            ..CapabilityProfile::default()
        }
    }

    fn degraded() -> CapabilityProfile {
        CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: false,
            max_context: 32_000,
            ..CapabilityProfile::default()
        }
    }

    fn tier(tier: CoreTier, provider: &str, fallback: Option<&str>) -> TierBinding {
        TierBinding {
            tier,
            provider_id: provider.to_owned(),
            fallback_id: fallback.map(str::to_owned),
        }
    }

    /// The fixture table: `think` on a frontier provider, `build` on the cheaper
    /// one, each falling back to the other, `reflex`/`scan` deliberately left
    /// unbound so the declared-default inheritance is exercised too.
    fn router() -> Router {
        Router::new(
            CategoryTable::new()
                .with_local_provider("local")
                .with_tier(tier(CoreTier::Think, "anthropic", Some("deepseek")))
                .with_tier(tier(CoreTier::Build, "deepseek", Some("anthropic"))),
            Some("deepseek".to_owned()),
        )
        .with_provider(
            "anthropic",
            ProviderKind::Anthropic,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider(
            "deepseek",
            ProviderKind::OpenaiCompatible,
            "deepseek-chat",
            native(),
            ProviderHealth::Healthy,
        )
    }

    // ---- REQ-559: effort resolution at route time ------------------------

    /// AC-4 / BR-5. `route_decided` reports the **clamped** level, not the
    /// requested one. With the session at `xhigh` and a provider whose ladder is
    /// `{low, high, max}`, the event says `high`.
    ///
    /// Reporting the request would make the event lie about the call — the
    /// number a user reads in `route_decided` is the number that went on the
    /// wire, or it is worthless.
    #[test]
    fn route_decided_reports_the_clamped_level_not_the_requested_one() {
        let three_rungs = CapabilityProfile {
            effort_ladder: Some(EffortLadder::from_levels(&[
                EffortLevel::Low,
                EffortLevel::High,
                EffortLevel::Max,
            ])),
            ..native()
        };
        let router = Router::new(
            CategoryTable::new()
                .with_local_provider("local")
                .with_tier(tier(CoreTier::Think, "kimi", None)),
            Some("kimi".to_owned()),
        )
        .with_effort(EffortLevel::Xhigh)
        .with_provider(
            "kimi",
            ProviderKind::OpenaiCompatible,
            "kimi-k3",
            three_rungs,
            ProviderHealth::Healthy,
        );

        let route = router.resolve(CoreCategory::Design);
        let decided = route.route_decided().expect("a provider was selected");
        assert_eq!(
            decided.effort,
            Some(ResolvedEffort::clamped(
                EffortLevel::Xhigh,
                EffortLevel::High
            )),
            "the event must name the clamped level (AC-4)",
        );
        assert_eq!(
            decided.effort.unwrap().level(),
            Some(EffortLevel::High),
            "what goes on the wire is `high`, not the requested `xhigh`",
        );
        assert!(
            decided.effort.unwrap().was_clamped(),
            "and the event carries the fact that it WAS clamped, so a reader \
             does not have to infer it by comparing against the setting",
        );
        // REQ-586 AC-1/AC-4, through the real router rather than a fixture
        // literal: `three_rungs` declares the 200,000-token window `native()`
        // carries, so the event announces the pair derived from it — 199k
        // usable words after the 1,024-token generation reservation — under
        // `bound: window`.
        assert_eq!(decided.budget_tokens, Some(132_650));
        assert_eq!(decided.budget_bytes, Some(397_952));
        assert_eq!(decided.bound, Some(BudgetBound::Window));
    }

    /// ADR-G: one resolution, two readers. The value the event announces and the
    /// value the request carries are the **same** value, not two computations of
    /// one fact — which is the drift LESSON-456 is about.
    #[test]
    fn the_event_and_the_request_carry_the_same_resolution() {
        let router = router().with_effort(EffortLevel::Max);
        for category in [
            CoreCategory::Design,
            CoreCategory::Edit,
            CoreCategory::Digest,
        ] {
            let route = router.resolve(category);
            let Some(decided) = route.route_decided() else {
                continue;
            };
            let turn = route.turn_route().expect("a provider was selected");
            assert_eq!(
                decided.effort,
                Some(turn.effort),
                "{category}: the event and the request must not be able to disagree",
            );
        }
    }

    /// AC-5 / BR-6. A call routed to the local tier resolves to a **declared**
    /// no-op, not to an absent value: `Omit(ShapeNone)` is "effort does not apply
    /// here, and here is why", while `None` would mean "a daemon that predates
    /// effort". The surface renders the first as "not applicable"; it has
    /// nothing to say about the second.
    ///
    /// The local tier is deliberately absent from `providers` here — it comes
    /// from the engine, not from a `[[providers]]` entry (REQ-557 ADR-D) — which
    /// is exactly the path that would otherwise report `None`.
    #[test]
    fn a_local_route_reports_a_declared_no_op_never_an_absent_one() {
        let router = router().with_effort(EffortLevel::Max);
        let route = router.resolve_local_pin("pinned for privacy");
        assert_eq!(
            route.effort,
            Some(ResolvedEffort::omit(EffortOmission::ShapeNone)),
            "BR-6: the local tier's no-op is declared, not merely true by omission",
        );
        assert_eq!(route.turn_route().unwrap().effort.level(), None);
    }

    /// AC-6 / BR-7. With the session at `max`, a local-pinned category still
    /// carries no effort field. The cap comes from the clamp table — the local
    /// kind's empty ladder and `none` shape — and **not** from any per-category
    /// effort configuration, which BR-2 forbids outright.
    #[test]
    fn a_global_bump_to_max_cannot_inflate_a_local_pinned_category() {
        for requested in teton_core::ALL_LEVELS {
            let router = router().with_effort(requested);
            let route = router.resolve_local_pin("reflex work stays local");
            assert_eq!(
                route.effective_effort(),
                ResolvedEffort::omit(EffortOmission::ShapeNone),
                "at {requested}, a local-pinned route must still send nothing",
            );
        }
    }

    /// BR-1: the absence of a user setting resolves to the declared default
    /// (`high`), never to an absent field. A router built without
    /// `with_effort` still states an effort.
    #[test]
    fn an_unconfigured_router_still_states_an_effort() {
        let route = router().resolve(CoreCategory::Design);
        assert_eq!(
            route.effective_effort(),
            ResolvedEffort::effort(EffortLevel::High),
            "omission inherits the provider's default, and one of them is `max`",
        );
    }

    /// ADR-F: a session refusal is honoured by the same resolver, so the event,
    /// the request and the surface all report it together — a runtime no-op is
    /// as visible as a declared one.
    #[test]
    fn a_session_refusal_reaches_the_event_and_the_request() {
        let mut refused = BTreeSet::new();
        refused.insert("anthropic".to_owned());
        let router = router().with_effort_refusals(refused);
        let route = router.resolve(CoreCategory::Design);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "anthropic");
        assert_eq!(
            route.effective_effort(),
            ResolvedEffort::omit(EffortOmission::RefusedThisSession),
        );
        // And a provider that did NOT refuse is unaffected — the memo is keyed
        // by provider id, not applied session-wide.
        let other = router.resolve(CoreCategory::Edit);
        assert_eq!(other.provider_id.as_ref().unwrap().0, "deepseek");
        assert!(matches!(
            other.effective_effort(),
            ResolvedEffort::Effort { .. }
        ));
    }

    /// AC-8's premise, at the router: the surface's per-provider view and the
    /// router's per-call decision come from **one** function, so they cannot
    /// disagree for any provider at any level. Asserted on `ResolvedEffort`
    /// values, not on rendered strings — a golden-string test would pass while
    /// the two diverged, because the surface would be self-consistently wrong.
    #[test]
    fn the_surface_view_and_the_route_agree_for_every_provider_and_level() {
        for requested in teton_core::ALL_LEVELS {
            let router = router().with_effort(requested);
            for (category, provider) in [
                (CoreCategory::Design, "anthropic"),
                (CoreCategory::Edit, "deepseek"),
            ] {
                let from_route = router.resolve(category).effective_effort();
                let from_view = router
                    .effort_for(Some(provider))
                    .expect("a registered provider resolves");
                assert_eq!(
                    from_route, from_view,
                    "{provider} at {requested}: the surface and the router must \
                     read one resolution, not two",
                );
            }
        }
    }

    /// **AC-1, the headline regression.** `"explain the tradeoffs between these
    /// two architectures"` is a `design` turn, and `design` inherits `think`. It
    /// routes to the frontier provider bound there — not to the 3B local model,
    /// which is where the deleted `AUXILIARY_SIGNALS` list sent it for containing
    /// the word "explain".
    ///
    /// The prompt appears nowhere in this test because it can appear nowhere in
    /// the router: `resolve` takes a category, and `freeform_category` takes no
    /// text. That absence is the fix; the sentence above is what it is for.
    #[test]
    fn a_think_category_routes_to_its_tier_binding_not_to_the_local_tier() {
        let router = router();
        let route = router.resolve(CoreCategory::Design);
        assert_eq!(
            route.provider_id.as_ref().unwrap().0,
            "anthropic",
            "a `design` turn must reach the provider bound to `think`: {}",
            route.reason
        );
        assert_ne!(route.provider_id.as_ref().unwrap().0, "local");
        assert_eq!(route.model.as_deref(), Some("claude-opus-4"));
        let resolution = route.resolution.as_ref().expect("resolved by category");
        assert_eq!(resolution.category, CoreCategory::Design);
        assert_eq!(resolution.tier, CoreTier::Think);
    }

    /// BR-1: the configured table is read on **every** turn, in both modes. The
    /// two modes differ only in where the category comes from — the same table,
    /// the same resolver, the same answer for the same category.
    #[test]
    fn both_session_modes_resolve_through_the_same_table() {
        let router = router();
        // Structured: the caller maps its phase to a category (ADR-C) and hands
        // the router the category.
        let structured = router.resolve(category_for_phase(CorePhase::Implement));
        // Freeform: the category comes from the BR-9 declared default until
        // TASK-053's classifier lands.
        let freeform = router.resolve(router.freeform_category());

        assert_eq!(router.freeform_category(), CoreCategory::Edit);
        assert_eq!(structured.provider_id, freeform.provider_id);
        assert_eq!(structured.reason, freeform.reason);
        assert_eq!(
            structured.resolution.as_ref().map(|r| r.category),
            freeform.resolution.as_ref().map(|r| r.category),
        );
        assert_eq!(freeform.provider_id.as_ref().unwrap().0, "deepseek");
        assert!(freeform.turn_route().is_some());
    }

    /// BR-9 / AC-12: the freeform default is the *configured* one, not a constant
    /// compiled into the router. Change the config key, change the category.
    #[test]
    fn the_freeform_default_category_comes_from_configuration() {
        assert_eq!(router().freeform_category(), CoreCategory::Edit);
        let review = router().with_judgment_default(JudgmentCategory::Review);
        assert_eq!(review.freeform_category(), CoreCategory::Review);
        // And it dispatches there: `review` inherits `think`, not `build`.
        assert_eq!(
            review
                .resolve(review.freeform_category())
                .provider_id
                .unwrap()
                .0,
            "anthropic"
        );
    }

    /// A structured turn's decision names the category and pins the model. The
    /// phase is **not** on the route — the router never saw one (AC-9); the
    /// caller stamps it on afterwards for cost attribution.
    #[test]
    fn structured_resolution_names_the_category_and_pins_the_model() {
        let route = router().resolve(category_for_phase(CorePhase::Spec));
        assert_eq!(route.provider_id.as_ref().unwrap().0, "anthropic");
        assert_eq!(route.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(route.phase, None, "the router does not stamp phases");
        assert_eq!(route.outcome, RouteOutcome::Primary);
        assert!(route.reason.contains("'design'"), "{}", route.reason);
        // A route_decided payload is emittable and carries the reason (BR-5).
        let decided = route.route_decided().expect("provider selected");
        assert_eq!(decided.provider_id.0, "anthropic");
        assert_eq!(decided.reason, route.reason);
        assert_eq!(decided.category, Some(ProtoCategory::Design));
        assert_eq!(decided.tier, Some(ProtoTier::Think));
    }

    /// **AC-9**, as a compile-level assertion: no routing signature takes a
    /// `Phase`, in either its core or its protocol form.
    ///
    /// Coercing each entry point to an explicitly written `fn` type is what makes
    /// this a compile-time check rather than a comment — adding a phase parameter
    /// back to any of them stops compiling here, and it does so whether the
    /// parameter is `Phase`, `Option<Phase>`, or anything else.
    ///
    /// `Phase` has not left the crate: it still reaches [`CostAttribution`]
    /// through [`Route::phase`], which the caller stamps on *after* the decision,
    /// and [`to_protocol_phase`] still bridges it at that boundary. What it no
    /// longer does is decide anything.
    #[test]
    fn no_routing_signature_takes_a_phase() {
        let _dispatch: fn(&Router, CoreCategory) -> Route = Router::resolve;
        let _pin: fn(&Router, String) -> Route = Router::resolve_local_pin;
        let _failure: fn(&Router, &Route, &str, FailureClass) -> FailureOutcome =
            Router::on_provider_failure;

        // And the phase still travels for attribution, which is the half of AC-9
        // that must NOT hold trivially: a route with no phase and a route with
        // one differ only in what they attribute, never in where they went.
        let attributed = Route {
            phase: Some(ProtoPhase::Implement),
            ..router().resolve(CoreCategory::Edit)
        };
        assert_eq!(
            attributed.provider_id,
            router().resolve(CoreCategory::Edit).provider_id
        );
        assert_eq!(
            attributed.route_decided().unwrap().phase,
            Some(ProtoPhase::Implement)
        );
    }

    /// **AC-10's structural leg (BR-2).** Reintroducing a keyword match for a
    /// harness-known category needs a routing function that can *see* prompt text
    /// and return a [`CoreCategory`]. This enumerates every candidate and pins its
    /// type: none of them takes text.
    ///
    /// TASK-053's classifier landed against this test rather than around it. It
    /// runs *outside* this module and its answer arrives as a [`Classification`] —
    /// a [`JudgmentCategory`] plus the signal that chose it — so
    /// [`Router::resolve_judgment`] is pinned here too: the day it grows a
    /// `&str` this stops compiling.
    ///
    /// That is the whole of the guarantee at this layer, and it is worth being
    /// exact about the limit. `JudgmentCategory` already makes `"digest"` from
    /// prompt text a compile error (AC-3), so a keyword matcher can only reach a
    /// harness-known category by returning `Category` directly — which requires
    /// one of these signatures to change, which fails here. What no test at this
    /// layer can catch is a mutation that changes the signature *and* every call
    /// site in one go: nothing in the router ever receives a prompt, so there is
    /// no behaviour left to differ. The end-to-end assertion that closes AC-10 — a
    /// freeform turn whose prompt contains a harness-known keyword, asserted to
    /// route by the declared default and not by the word — is TASK-057's, per this
    /// task's own notes.
    #[test]
    fn no_routing_function_can_see_prompt_text() {
        let _freeform: fn(&Router) -> CoreCategory = Router::freeform_category;
        let _dispatch: fn(&Router, CoreCategory) -> Route = Router::resolve;
        let _resolution: fn(&Router, CoreCategory) -> CategoryResolution = Router::resolution_for;
        let _judgment: fn(&Router, &Classification) -> Route = Router::resolve_judgment;
        let _default: fn(&Router) -> JudgmentCategory = Router::judgment_default;
    }

    /// The classification signal reaches `route_decided`, and the resolution the
    /// event projects its category and tier from is **untouched** by it (AC-11):
    /// the reason gains a sentence, the decision does not gain a second author.
    #[test]
    fn a_classified_route_carries_both_the_signal_and_the_resolver_s_sentence() {
        let router = router();
        let classified = Classification {
            category: JudgmentCategory::Design,
            signal: ClassificationSignal::Classified,
        };
        let route = router.resolve_judgment(&classified);
        let plain = router.resolve(CoreCategory::Design);

        // Same decision, by every measure except the sentence.
        assert_eq!(route.provider_id, plain.provider_id);
        assert_eq!(route.resolution, plain.resolution);
        assert_eq!(
            route.resolution.as_ref().map(|r| r.reason.clone()),
            plain.resolution.as_ref().map(|r| r.reason.clone()),
            "the resolver's own answer is not rewritten"
        );

        let decided = route.route_decided().expect("a provider was selected");
        assert_eq!(decided.category, Some(ProtoCategory::Design));
        assert_eq!(decided.tier, Some(ProtoTier::Think));
        assert!(decided.reason.contains("classifier"), "{}", decided.reason);
        assert!(
            decided.reason.ends_with(&plain.reason),
            "the resolver's sentence survives verbatim: {}",
            decided.reason
        );
    }

    /// A bypassed classification still resolves through the configured table —
    /// the degraded means is a real category, not a shortcut past the resolver
    /// (LESSON-447) — and `route_decided` names the bypass (AC-5).
    #[test]
    fn a_bypassed_classification_resolves_the_declared_default_and_names_the_bypass() {
        let router = router().with_judgment_default(JudgmentCategory::Edit);
        let bypassed = Classification {
            category: router.judgment_default(),
            signal: ClassificationSignal::Bypassed {
                reason: "the local tier is unavailable.".to_owned(),
            },
        };
        let route = router.resolve_judgment(&bypassed);

        // `edit` inherits `build`, which this fixture binds to deepseek — the
        // same answer `resolve` gives, reached through the same chain.
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
        assert_eq!(
            route.resolution.as_ref().map(|r| r.category),
            Some(CoreCategory::Edit)
        );
        let decided = route.route_decided().expect("a provider was selected");
        assert!(decided.reason.contains("bypassed"), "{}", decided.reason);
        assert!(
            decided.reason.contains("'edit'"),
            "and still names the category that fired: {}",
            decided.reason
        );
    }

    /// AC-4's router leg. `redact` and `route` are pinned to the local tier by
    /// construction — and the pin has to survive [`Router::effective_table`],
    /// which is new machinery that writes a binding into every tier the config
    /// leaves unbound. Here that binding is remote for all four tiers, and both
    /// pinned categories still resolve local, because `resolve` reaches them
    /// through a branch that consults no binding at all.
    #[test]
    fn the_pinned_categories_ignore_an_inherited_remote_binding() {
        let router = Router::new(
            CategoryTable::new().with_local_provider("local"),
            Some("frontier-remote".to_owned()),
        )
        .with_provider(
            "frontier-remote",
            ProviderKind::OpenaiCompatible,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        );

        // A turn tier (build/think) inherits the remote default...
        assert_eq!(
            router.resolve(CoreCategory::Edit).provider_id.unwrap().0,
            "frontier-remote"
        );
        // ...but reflex and scan do not: their work was already happening on
        // this machine, so they inherit the local tier even when a remote
        // default is set. `title` is the reflex category the fill can actually
        // reach — `route` and `redact` are pinned before inheritance is
        // consulted — and `digest` is `scan`'s.
        for category in [CoreCategory::Title, CoreCategory::Digest] {
            assert_eq!(
                router.resolve(category).provider_id.unwrap().0,
                "local",
                "{category} must inherit the local tier"
            );
        }
        // ...and the two pinned categories are unmoved by it.
        for pinned in [CoreCategory::Redact, CoreCategory::Route] {
            let route = router.resolve(pinned);
            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                Some("local"),
                "{pinned} escaped its pin: {}",
                route.reason
            );
        }
    }

    /// ADR-E: the category chain screens every provider it would select through
    /// the router's usability rule, so a provider that never entered the provider
    /// map is never selected — not as a primary, not as a fallback.
    ///
    /// This is the leg BUG-155's Critical finding demands of every new dispatch
    /// path: three config-reading paths each bypassed the screen, and a fourth
    /// axis is a fourth bypass unless it is screened where the decision is made.
    ///
    /// The last assertion is the one that makes this test worth having, and it
    /// took a mutation run to find out. Dropping the screen (`|_| true`) leaves
    /// the *outcome* identical — `health_of` reports an unmapped id as
    /// `Unavailable`, so nothing is selected either way — and every assertion
    /// above it stays green. What changes is the sentence: the user is told the
    /// provider is down, rather than that it declares no `model` and how to fix
    /// it. Two guards with no independent coverage is exactly LESSON-483's shape,
    /// so the screen is pinned by the thing only it produces.
    #[test]
    fn an_unregistered_provider_is_never_selected_by_the_category_chain() {
        let router = Router::new(
            CategoryTable::new().with_tier(tier(CoreTier::Think, "ghost", None)),
            None,
        )
        .with_provider(
            "real",
            ProviderKind::OpenaiCompatible,
            "a-model",
            native(),
            ProviderHealth::Healthy,
        );

        let route = router.resolve(CoreCategory::Design);
        assert_eq!(route.provider_id, None, "{route:?}");
        assert_eq!(route.model, None, "and nothing downstream can bill it");
        assert_eq!(route.outcome, RouteOutcome::NoHealthyProvider);
        assert!(route.reason.contains("'ghost'"), "{}", route.reason);
        assert!(route.reason.contains("'design'"), "{}", route.reason);
        assert!(
            route.reason.contains("is not routable")
                && route.reason.contains("declares no `model`"),
            "the refusal must name ADR-E's cause and its remedy, not report the \
             provider as merely down: {}",
            route.reason
        );
    }

    /// BR-8: an unbound **turn** tier inherits the declared default —
    /// `default_provider` first (REQ-557 BR-4: "the provider an unrouted turn
    /// goes to"), then the local tier, which is what makes an offline machine
    /// route at all.
    ///
    /// Both are ids someone declared; neither is synthesized. With neither
    /// declared, the category names itself and its unset tier rather than
    /// borrowing a binding from somewhere else.
    ///
    /// `reflex` and `scan` are not turn tiers and inherit only the local tier;
    /// that is `an_unbound_reflex_or_scan_tier_falls_to_local_never_to_the_
    /// remote_default`'s.
    #[test]
    fn an_unbound_tier_inherits_the_declared_default_then_the_local_tier() {
        // `think` is bound in the fixture; `build` is not, so `edit` inherits
        // the default — which here happens to be the same provider `build`
        // would have named, so assert the *origin* too rather than the id
        // alone.
        let router = Router::new(
            CategoryTable::new()
                .with_local_provider("local")
                .with_tier(tier(CoreTier::Think, "anthropic", None)),
            Some("deepseek".to_owned()),
        )
        .with_provider(
            "anthropic",
            ProviderKind::Anthropic,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider(
            "deepseek",
            ProviderKind::OpenaiCompatible,
            "deepseek-chat",
            native(),
            ProviderHealth::Healthy,
        );
        let route = router.resolve(CoreCategory::Edit);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
        assert_eq!(
            router.tier_report(CoreTier::Build).origin,
            TierOrigin::DefaultProvider
        );

        // No default: the local tier serves instead — REQ-544's local-first
        // promise, and the reason a machine with no remote provider still routes.
        let offline = Router::new(CategoryTable::new().with_local_provider("local"), None);
        let route = offline.resolve(CoreCategory::Edit);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "local");

        // Neither: the category names itself and its unset tier (BR-8), and no
        // id is invented to fill the hole.
        let nothing = Router::new(CategoryTable::new(), None);
        let route = nothing.resolve(CoreCategory::Edit);
        assert_eq!(route.provider_id, None);
        assert_eq!(route.outcome, RouteOutcome::NoPolicy);
        assert!(route.reason.contains("'edit'"), "{}", route.reason);
        assert!(route.reason.contains("'build'"), "{}", route.reason);
    }

    /// **An unusable `default_provider` inherits nothing.** It falls through to
    /// the local tier exactly as an absent one does, which is what
    /// [`Router::inherited_provider`]'s own contract promises ("fall back to the
    /// local tier, so an offline install still routes").
    ///
    /// Not contrived: `default_provider` is a plain config key naming a provider
    /// by id, and REQ-557 ADR-E keeps a remote provider that declares no `model`
    /// out of the router entirely. So the ordinary "I deleted that provider" and
    /// "that provider never got migrated" configs both land here. Returning the
    /// id unscreened bound every unbound tier to something that cannot serve —
    /// and the arm returned early, so it never reached the local tier: a machine
    /// with a perfectly healthy local model failed every turn.
    #[test]
    fn an_unusable_default_provider_falls_through_to_the_local_tier() {
        let router = Router::new(
            CategoryTable::new().with_local_provider("on-device"),
            // Named by config, registered nowhere — REQ-557 ADR-E's shape.
            Some("deleted-vendor".to_owned()),
        )
        .with_provider(
            "on-device",
            ProviderKind::Local,
            "qwen",
            native(),
            ProviderHealth::Healthy,
        );

        // Non-vacuity: a *usable* default is still inherited, so this test is
        // measuring the screen rather than the absence of inheritance.
        let usable = Router::new(
            CategoryTable::new().with_local_provider("on-device"),
            Some("frontier".to_owned()),
        )
        .with_provider(
            "on-device",
            ProviderKind::Local,
            "qwen",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider(
            "frontier",
            ProviderKind::OpenaiCompatible,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        );
        assert_eq!(
            usable.resolve(CoreCategory::Edit).provider_id.unwrap().0,
            "frontier"
        );

        let route = router.resolve(CoreCategory::Edit);
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("on-device"),
            "an unusable default must not cost a healthy machine every turn: {}",
            route.reason
        );

        // And `policy show` reports the tier the way it actually resolves —
        // otherwise the table the user reads is not the table resolution reads.
        let report = router.tier_report(CoreTier::Build);
        assert_eq!(report.provider_id.as_deref(), Some("on-device"));
        assert_eq!(report.origin, TierOrigin::LocalTier);
        assert_eq!(
            usable.tier_report(CoreTier::Build).origin,
            TierOrigin::DefaultProvider
        );
    }

    /// **An unbound `reflex` or `scan` tier falls to the local provider, never
    /// to `default_provider`.**
    ///
    /// `reflex` is defined as "sub-second, every turn, **never leaves the
    /// machine**" (REQ-558's tier table). `scan` joins it because of what its
    /// only reached category does: `digest` summarizes tool output — file
    /// contents, build logs — and before this REQ it ran on the local engine
    /// unconditionally and sent nothing anywhere.
    ///
    /// This matters on the ordinary upgrade path, not a contrived one: REQ-557's
    /// migration sets `default_provider` to the first remote provider, and an
    /// upgraded config has no `[[tiers]]` at all. Filling every tier from that
    /// one value sends a tier whose whole purpose is locality to a frontier
    /// model — and, for `scan`, starts shipping file bodies to a vendor API
    /// because of a key the user set for their *turns*.
    #[test]
    fn an_unbound_reflex_or_scan_tier_falls_to_local_never_to_the_remote_default() {
        let router = Router::new(
            CategoryTable::new().with_local_provider("on-device"),
            Some("frontier-remote".to_owned()),
        )
        .with_provider(
            "frontier-remote",
            ProviderKind::OpenaiCompatible,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider(
            "on-device",
            ProviderKind::Local,
            "qwen",
            native(),
            ProviderHealth::Healthy,
        );
        // `title` is the reflex category that is neither pinned nor classified,
        // so it is the one the fill can actually reach. `digest` is `scan`'s.
        for (category, why) in [
            (
                CoreCategory::Title,
                "the tier is defined as never leaving the machine",
            ),
            (
                CoreCategory::Digest,
                "this duty summarizes tool output and ran locally before this REQ, \
                 so it stays local until the user binds `scan` deliberately",
            ),
        ] {
            let route = router.resolve(category);
            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                Some("on-device"),
                "{category} resolved to {:?}; {why}, so an unbound tier must \
                 inherit the local tier, not the remote default: {}",
                route.provider_id,
                route.reason
            );
        }

        // The two TURN tiers legitimately inherit the remote default: their
        // work was already going wherever the retired table pointed, so
        // inheriting is continuity rather than a change.
        for category in [CoreCategory::Edit, CoreCategory::Design] {
            let route = router.resolve(category);
            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                Some("frontier-remote"),
                "{category} should inherit the default provider"
            );
        }
    }

    /// BR-8: the local tier is usable only while it can meet its latency duty.
    /// Below the hardware floor, gated on consent, or shed under memory pressure,
    /// a binding that names it resolves to nothing rather than to a turn that
    /// will fail with `NoTierAvailable` after the decision was announced.
    #[test]
    fn a_local_tier_that_cannot_serve_is_not_selected() {
        let offline = Router::new(CategoryTable::new().with_local_provider("local"), None)
            .with_local_available(false);
        let route = offline.resolve(CoreCategory::Edit);
        assert_eq!(route.provider_id, None, "{route:?}");
        assert!(!route.outcome.selected_provider());
    }

    #[test]
    fn degraded_provider_yields_the_reduced_harness_profile() {
        let router = router().with_provider(
            "kimi",
            ProviderKind::OpenaiCompatible,
            "kimi-k2",
            degraded(),
            ProviderHealth::Degraded,
        );
        let cfg = router.harness_config_for("kimi");
        assert!(cfg.require_verification);
        assert_eq!(cfg.max_tools, Some(5));
        assert!(cfg.max_turns <= 5);
        // REQ-586: a weak tool-caller is not a small-windowed one. `degraded()`
        // declares 32,000 tokens, and the config the turn runs under is
        // budgeted from that window — the reduced profile and the budget are
        // two facts about the provider, stamped by one call.
        assert_eq!(cfg.budget.bound, BudgetBound::Window);
        assert_eq!(cfg.budget, router.budget_for(Some("kimi")));
        assert_eq!(cfg.context_budget_tokens, cfg.budget.budget_tokens);
        assert_eq!(cfg.context_budget_bytes, cfg.budget.budget_bytes);
    }

    // ---- REQ-586: the budget is a property of the route attempt ----------

    /// A router that reaches **all five** [`BudgetBound`]s: `wide` declares a
    /// window, `silent` declares none, `capped` carries a user cap below its
    /// window, `local` is the routing table's local tier, and the fifth —
    /// `RedactScan` — comes from the same router under
    /// [`Router::with_redact_scan`].
    ///
    /// Shared by the classification table below and by
    /// `budget_for_is_byte_identical_on_every_bound`, because a golden pin and
    /// the classification it guards must be reading the same five routes; two
    /// fixtures would let one drift out from under the other.
    fn five_bound_router() -> Router {
        Router::new(CategoryTable::new().with_local_provider("local"), None)
            .with_provider(
                "wide",
                ProviderKind::OpenaiCompatible,
                "wide-model",
                CapabilityProfile {
                    max_context: 128_000,
                    ..native()
                },
                ProviderHealth::Healthy,
            )
            .with_provider(
                "silent",
                ProviderKind::OpenaiCompatible,
                "silent-model",
                CapabilityProfile {
                    max_context: 0,
                    ..native()
                },
                ProviderHealth::Healthy,
            )
            .with_provider(
                "capped",
                ProviderKind::OpenaiCompatible,
                "capped-model",
                CapabilityProfile {
                    max_context: 200_000,
                    context_budget_cap: 40_000,
                    ..native()
                },
                ProviderHealth::Healthy,
            )
    }

    /// **AC-1 / BR-1, BR-2, BR-5, BR-8**, as a table: what each shape of route
    /// is budgeted at, and which constraint gets the credit.
    ///
    /// The arithmetic belongs to `harness::budget` and is table-tested there;
    /// what this pins is the router's half — the *classification* that reaches
    /// it. Every wrong classification still produces a plausible pair, so the
    /// bound is what gives it away: reading the window off the wrong provider,
    /// calling a defaulted provider "local", or missing `[privacy] redact`
    /// each change the bound while leaving a number that looks fine.
    #[test]
    fn the_route_budget_is_derived_from_the_routes_own_window() {
        let router = five_bound_router();

        // A declared window, less the 1,024-token generation reservation the
        // adapters actually send: (128,000 − 1,024) words ÷ the 3/2 safety
        // ratio, and the same figure × the 2 B/token floor.
        let wide = router.budget_for(Some("wide"));
        assert_eq!((wide.budget_tokens, wide.budget_bytes), (84_650, 253_952));
        assert_eq!(wide.bound, BudgetBound::Window);

        // No declared window: today's pair, and the event says *why* — a
        // provider stuck at 4,096 for want of one line of config must be
        // legible as that rather than as a mysterious clamp (BR-3).
        let silent = router.budget_for(Some("silent"));
        assert_eq!((silent.budget_tokens, silent.budget_bytes), (4_096, 32_768));
        assert_eq!(silent.bound, BudgetBound::DefaultUnknown);

        // The local tier: **its own** pair since REQ-590, derived from the
        // engine's `n_ctx` (32,768) less the generation reservation (1,024) by
        // the same formula every declared window runs — not the no-better-fact
        // pair above. Classified from the routing table's local provider id
        // (gotcha #9): it has no `[[providers]]` entry at all, so a
        // "capabilities look defaulted" test would have called `silent` local
        // too.
        let local = router.budget_for(Some("local"));
        assert_eq!((local.budget_tokens, local.budget_bytes), (21_162, 63_488));
        assert_eq!(local.bound, BudgetBound::LocalEngine);
        // Two facts, and REQ-590 moved one of them. The bound was the *only*
        // discriminator while the two arms returned one pair; the pair is now a
        // discriminator too, and both are asserted because a build that lost
        // the classification would still produce a plausible number, and one
        // that lost the derivation would still produce the right bound.
        assert_ne!(
            silent.bound, local.bound,
            "the bound is what says which fact the pair came from"
        );
        assert_ne!(
            (silent.budget_tokens, silent.budget_bytes),
            (local.budget_tokens, local.budget_bytes),
            "REQ-590: a route with a window fact and a route with none no longer \
             run under one pair — the local tier derives from the engine"
        );

        // The user's cap is a window ceiling, not a post-hoc clamp: both
        // currencies are recomputed from it, and it takes the credit (BR-5).
        let capped = router.budget_for(Some("capped"));
        assert_eq!(
            (capped.budget_tokens, capped.budget_bytes),
            (25_984, 77_952)
        );
        assert_eq!(capped.bound, BudgetBound::UserCap);
        assert!(
            capped.budget_tokens < router.budget_for(Some("wide")).budget_tokens,
            "a 40k cap on a 200k window must bind below an uncapped 128k route"
        );

        // `[privacy] redact = true`: the scan reads a whole assembled body, so
        // the bytes are held to what it can take — applied last, so it names
        // the bound whenever it is the thing that actually bit, and the word
        // half stays window-derived (BR-4).
        let scanning = router.clone().with_redact_scan(true);
        let redacted = scanning.budget_for(Some("wide"));
        assert_eq!(redacted.budget_bytes, REDACT_SCANNABLE_CONTEXT_BYTES);
        assert_eq!(redacted.budget_tokens, wide.budget_tokens);
        assert_eq!(redacted.bound, BudgetBound::RedactScan);
        assert_eq!(
            scanning.budget_for(Some("local")).bound,
            BudgetBound::LocalEngine,
            "the scan bounds what leaves the machine; a local turn does not leave"
        );

        // And the unresolvable route — no provider to derive from — carries
        // whatever `HarnessConfig::default()` carries, which is the same local
        // derivation: `budget_inputs_for`'s `_ =>` arm answers for both, and
        // since REQ-590 that is the engine's pair rather than the constants'.
        // Asserted as an equality against the config rather than as a literal,
        // so the two cannot drift apart whichever of them moves.
        let nowhere = router.budget_for(None);
        assert_eq!(nowhere, HarnessConfig::default().budget);
        assert_eq!(nowhere, local, "the unresolvable route is the local arm");
    }

    /// **REQ-589 TASK-259's before/after guard.** Every field of every bound's
    /// budget, pinned verbatim.
    ///
    /// The table above pins the pair and the bound, which is the
    /// *classification* it is about. This pins the whole `RouteBudget` — window
    /// label, both digest thresholds, the floor fact and the carried provider
    /// id included — as its own `Debug` rendering, because TASK-259 lifts the
    /// input construction out of `budget_for` into
    /// [`Router::budget_inputs_for`] and claims the results are unchanged. A
    /// refactor that is *supposed* to be invisible needs a test that can see
    /// everything: a lifted arm that dropped `redact_scan`, or handed
    /// `labelled_provider` the wrong id, changes none of the numbers the table
    /// above reads.
    ///
    /// The bounds are asserted separately from the snapshot so the five rows
    /// cannot quietly become five readings of one route — a snapshot updated
    /// wholesale would otherwise still look like five rows.
    #[test]
    fn budget_for_is_byte_identical_on_every_bound() {
        let router = five_bound_router();
        let scanning = router.clone().with_redact_scan(true);
        let rows = [
            ("local_engine", router.budget_for(Some("local"))),
            ("default_unknown", router.budget_for(Some("silent"))),
            ("window", router.budget_for(Some("wide"))),
            ("user_cap", router.budget_for(Some("capped"))),
            ("redact_scan", scanning.budget_for(Some("wide"))),
        ];

        assert_eq!(
            rows.iter().map(|(_, b)| b.bound).collect::<Vec<_>>(),
            vec![
                BudgetBound::LocalEngine,
                BudgetBound::DefaultUnknown,
                BudgetBound::Window,
                BudgetBound::UserCap,
                BudgetBound::RedactScan,
            ],
            "the five rows must be the five bounds, or the snapshot below pins \
             one route five times"
        );

        let snapshot = rows
            .iter()
            .map(|(name, budget)| format!("{name}: {budget:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            snapshot,
            BUDGET_FOR_GOLDEN.join("\n"),
            "a `budget_for` result changed. TASK-259's refactor is required to be \
             additive, so this is either a real behaviour change that needs its own \
             decision, or an input the extracted `budget_inputs_for` no longer passes \
             through."
        );
    }

    /// The five budgets exactly as `budget_for` produced them **before**
    /// TASK-259 extracted [`Router::budget_inputs_for`] — captured from the
    /// running code at `5a2ee33`, not hand-computed, so it is a record of
    /// behaviour rather than a restatement of the arithmetic.
    ///
    /// **One row has moved since, deliberately, in two steps.** REQ-590
    /// (TASK-270, D-3 as amended by ADR-9) stopped `local_engine` returning the
    /// no-better-fact pair's *word* half: it derived from the engine's own
    /// 16,384-token window like every other window-derived route, so `10240`
    /// replaced `4096` and the word digest threshold followed, `1500 → 3750`,
    /// while the byte half stayed the `32768` constant (D-4 had derived it —
    /// `30720` — and was reversed on measurement; see ADR-9). Then the window
    /// went to 32,768 and both halves derive: `21162 / 63488`, digest
    /// thresholds `7749 / 23250`. The reasoning is at
    /// [`derive`](crate::harness::budget::derive)'s local arm rather than left
    /// to be inferred from this table.
    ///
    /// The `redact_scan` row moved with the window too — its byte half is the
    /// scannable bound, which derives from the engine window through the scan's
    /// chunk cap: `88196 → 141224`, byte digest threshold `32298 → 51717`.
    ///
    /// **And it moved again for REQ-612**, for the other reason a derived bound
    /// moves: `REDACT_BODY_OVERHEAD_BYTES` went 14 → 23 KiB to pay for the
    /// resident repository-notes block, which took the scan's chunk count 3 → 4
    /// and the total cap with it, so the bound *widened*: `141224 → 184265`,
    /// byte digest threshold `51717 → 67479` (it is a fixed fraction of the
    /// byte half, `× 12,000 / 32,768`, and nowhere near the 163,840 ceiling).
    /// The word half is untouched at 84,650, which is the check that the clamp
    /// is byte-denominated and stayed that way. The other four rows are
    /// byte-identical again, which says the raise reached the scanned route and
    /// nothing else.
    ///
    /// The other three rows are byte-identical to the capture, which is what
    /// says the window touched the two window-derived arms and nothing else.
    ///
    /// **REQ-612 appended a field to every row.** `repo_context_cap` is a
    /// quarter of each row's byte half capped at `REPO_CONTEXT_MAX_BYTES`
    /// (ADR-5), and every one of the five reaches the cap — the narrowest byte
    /// half here is 32,768, whose quarter is exactly 8,192. So the addition
    /// moved no derived figure; it added a column, and this table is what says
    /// the column is the same on all five bounds. A row that stops reaching the
    /// cap is a route below 32 KB of bytes, which is news.
    const BUDGET_FOR_GOLDEN: [&str; 5] = [
        "local_engine: RouteBudget { budget_tokens: 21162, budget_bytes: 63488, bound: LocalEngine, window_label: \"the local context window\", digest_threshold_tokens: 7749, digest_threshold_bytes: 23250, floored: false, provider_id: None, repo_context_cap: 8192 }",
        "default_unknown: RouteBudget { budget_tokens: 4096, budget_bytes: 32768, bound: DefaultUnknown, window_label: \"silent's context window\", digest_threshold_tokens: 1500, digest_threshold_bytes: 12000, floored: false, provider_id: Some(\"silent\"), repo_context_cap: 8192 }",
        "window: RouteBudget { budget_tokens: 84650, budget_bytes: 253952, bound: Window, window_label: \"wide's context window\", digest_threshold_tokens: 20000, digest_threshold_bytes: 93000, floored: false, provider_id: Some(\"wide\"), repo_context_cap: 8192 }",
        "user_cap: RouteBudget { budget_tokens: 25984, budget_bytes: 77952, bound: UserCap, window_label: \"capped's context window\", digest_threshold_tokens: 9515, digest_threshold_bytes: 28546, floored: false, provider_id: Some(\"capped\"), repo_context_cap: 8192 }",
        "redact_scan: RouteBudget { budget_tokens: 84650, budget_bytes: 184265, bound: RedactScan, window_label: \"the redact-scannable window\", digest_threshold_tokens: 20000, digest_threshold_bytes: 67479, floored: false, provider_id: Some(\"wide\"), repo_context_cap: 8192 }",
    ];

    /// **REQ-589 TASK-259.** What the accessor is *for*: the provider's declared
    /// window, unreduced, beside the flags the recipe probe re-derives under.
    ///
    /// Deliberately not "and it derives the same budget" — after the extraction
    /// that is true by construction and would assert nothing.
    /// `budget_for_is_byte_identical_on_every_bound` is what pins the results.
    #[test]
    fn the_inputs_carry_the_declared_window_the_budget_does_not() {
        let router = five_bound_router();
        let scanning = router.clone().with_redact_scan(true);

        // The provider's own `capabilities.max_context`, verbatim: not reduced
        // by the generation reservation, and not by the user's cap. ADR-15
        // measures against exactly this figure, and the reason it can is that
        // this is the one place the raw declaration survives — every other
        // surface sees the pair `derive` made of it.
        assert_eq!(router.budget_inputs_for(Some("wide")).window, 128_000);
        let capped = router.budget_inputs_for(Some("capped"));
        assert_eq!((capped.window, capped.cap), (200_000, 40_000));
        assert_eq!(
            router.budget_for(Some("capped")).bound,
            BudgetBound::UserCap,
            "the cap binds the budget while leaving the declared window alone — \
             the daemon's policy against the provider's bound, which is the whole \
             distinction the window verdict rests on (ADR-15)"
        );

        // An undeclared window is `0`, which the derivation reads as the
        // *absence* of a window fact rather than a window of size zero.
        assert_eq!(router.budget_inputs_for(Some("silent")).window, 0);

        // The local tier and the unresolvable route say so as `is_local`, and
        // carry no provider id for the window's name to be built from.
        let local = router.budget_inputs_for(Some("local"));
        assert!(local.is_local && local.provider_id.is_none());
        assert!(router.budget_inputs_for(None).is_local);
        assert!(
            !router.budget_inputs_for(Some("silent")).is_local,
            "a provider that declared no window is not the local tier (gotcha #9)"
        );

        // `[privacy] redact` and the reservation ride the inputs, so a vendor
        // recipe tried through `budget::proposed_window` is tried under the same
        // clamp and the same reserved room the route actually runs under.
        assert!(!router.budget_inputs_for(Some("wide")).redact_scan);
        assert!(scanning.budget_inputs_for(Some("wide")).redact_scan);
        assert_eq!(
            router.budget_inputs_for(Some("wide")).reservation,
            budget::generation_reservation()
        );
    }

    /// Every call the scan below recognizes. `crate::harness::budget::derive(`
    /// and `super::budget::derive(` both end in it, so one needle covers the
    /// crate's three spellings.
    const DERIVE_CALL: &str = "budget::derive(";

    /// Where `derive` is **defined**, and the one file excluded from the sweep
    /// below: `proposed_window` derives a *candidate* budget there to answer
    /// whether a recipe's window would clear a refusal, which is the module
    /// deciding its own question. Its calls are unqualified for the same reason,
    /// so [`DERIVE_CALL`] would not see them anyway — the exclusion is checked
    /// against the definition rather than assumed.
    const BUDGET_MODULE: &str = "harness/budget.rs";

    /// Every `use` statement in `source`, flattened to one line each.
    ///
    /// Flattened because imports wrap: `turn_loop.rs` opens
    /// `use super::budget::{` and lists its items over the next three lines, and
    /// a `derive` on one of those continuation lines is exactly the import a
    /// line-at-a-time scan would miss.
    fn use_statements(source: &str) -> Vec<String> {
        let mut statements = Vec::new();
        let mut current: Option<String> = None;
        for line in source.lines() {
            let trimmed = line.trim();
            match current.as_mut() {
                Some(open) => open.push_str(trimmed),
                None if trimmed.starts_with("use ") || trimmed.starts_with("pub use ") => {
                    current = Some(trimmed.to_owned());
                }
                None => continue,
            }
            if trimmed.contains(';') {
                statements.extend(current.take());
            }
        }
        statements
    }

    /// Whether `source` could call `derive` in a spelling [`DERIVE_CALL`] cannot
    /// see — an unqualified import of it, or a glob of the module.
    ///
    /// The scan reads a qualified path, so an import is the one way to put a
    /// call beyond its reach; a scan that cannot see the thing it forbids passes
    /// vacuously forever after. `BudgetInputs` and `skill_fit` are imported
    /// unqualified all over the daemon and are not this — the token has to be
    /// `derive` itself.
    fn can_derive_unqualified(source: &str) -> Option<String> {
        use_statements(source).into_iter().find(|statement| {
            statement.contains("budget::")
                && (statement.contains("budget::*")
                    || statement
                        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .any(|token| token == "derive"))
        })
    }

    /// **REQ-586 AC-12, derived rather than trusted.** The routing layer mints a
    /// budget in exactly one place, and TASK-259's accessor did not quietly
    /// become a second one.
    ///
    /// The rule is stated in `budget_inputs_for`'s doc comment, and a rule only
    /// a doc comment enforces is a convention. This reads `router.rs` as text
    /// and checks it: one `derive` call, inside `budget_for`, and no import that
    /// would let a second one be spelled in a way this scan cannot see.
    #[test]
    fn the_routing_layer_derives_a_budget_in_exactly_one_place() {
        use crate::call_sites::scan::{code_only, count, daemon_src, production_source};

        let source = code_only(&production_source(&daemon_src().join("router.rs")));

        assert_eq!(
            count(&source, DERIVE_CALL),
            1,
            "`router.rs` derives a budget {} time(s). REQ-586 AC-12 gives the budget one \
             home so no surface can name a figure the turn is not running under — read \
             `Route::budget` or call `budget_for`, never `derive`.",
            count(&source, DERIVE_CALL)
        );

        let at = source.find(DERIVE_CALL).expect("the one derivation");
        assert_eq!(
            enclosing_fn(&source, at),
            "budget_for",
            "the routing layer's one `derive` call moved out of `budget_for`. Wherever it \
             is now is the new single source of the route's budget, which is a decision \
             REQ-586 ADR-1 made deliberately and not one to make by moving a line."
        );

        assert_eq!(
            can_derive_unqualified(&source),
            None,
            "an import would let `router.rs` call `derive` unqualified, which this test \
             cannot see. Keep the `budget::derive(...)` spelling."
        );
    }

    /// The other half of TASK-259's risk: handing out `BudgetInputs` must not
    /// grow a second derivation *somewhere else* in the daemon.
    ///
    /// An exact set rather than a direction, so both rots are loud — a new
    /// caller appearing, and the documented one being deleted while its
    /// justification stays behind. `harness/turn_loop.rs` is the one non-routing
    /// caller and is not a route's budget at all: `HarnessConfig::default()`
    /// derives the *local default* pair, the value a config carries before any
    /// route has been decided.
    ///
    /// A file that merely *could* call `derive` unqualified counts as a caller
    /// here rather than being waved through, because from this scan's side the
    /// two are indistinguishable and the permissive reading is the one that goes
    /// quiet.
    #[test]
    fn no_second_derivation_grew_elsewhere_in_the_daemon() {
        use crate::call_sites::scan::{code_only, count, production_sources};

        let sources = production_sources();

        // The exclusion below names a file, so it is only sound while the
        // definition is in it: a moved `derive` would take a whole module out of
        // the sweep silently.
        let (_, home) = sources
            .iter()
            .find(|(rel, _)| rel == BUDGET_MODULE)
            .expect("the budget module is a production source");
        assert!(
            code_only(home).contains("pub fn derive("),
            "`derive` no longer lives in {BUDGET_MODULE}, which this sweep excludes by \
             name. The exclusion has to move with it."
        );

        let callers: Vec<String> = sources
            .iter()
            .filter(|(rel, source)| {
                let code = code_only(source);
                rel != BUDGET_MODULE
                    && (count(&code, DERIVE_CALL) > 0 || can_derive_unqualified(&code).is_some())
            })
            .map(|(rel, _)| rel.clone())
            .collect();

        assert_eq!(
            callers,
            vec!["harness/turn_loop.rs".to_owned(), "router.rs".to_owned()],
            "the set of `derive` callers changed. A new one means a second budget that can \
             disagree with the route's — REQ-586's verify pass caught exactly that, \
             `/verbose` naming a budget the turn was not running under. Take \
             `Route::budget` / `HarnessConfig::budget`, or call `Router::budget_for`; \
             `Router::budget_inputs_for` hands out the inputs so a window can be *read*, \
             not so a second pair can be minted from them."
        );
    }

    /// AC-4's crate boundary, which no compiler check can state: the inputs stop
    /// at `tetond`. A thin client may not repeat the derivation at all (BR-8),
    /// and `pub` here would be the first step toward its being able to.
    #[test]
    fn the_inputs_accessor_stops_at_the_crate_boundary() {
        use crate::call_sites::scan::{code_only, count, daemon_src, production_source};

        let source = code_only(&production_source(&daemon_src().join("router.rs")));
        assert_eq!(
            count(&source, "pub(crate) fn budget_inputs_for"),
            1,
            "`budget_inputs_for` is `pub(crate)` on purpose: `budget_for` is the public \
             way to ask this router about a budget, and the raw inputs reach no further \
             than the offer path that needs the declared window."
        );
    }

    /// The name of the function whose body contains byte `at` — the last `fn `
    /// declared above it. Whole-line comments are already stripped by
    /// `code_only`, so this reads code.
    fn enclosing_fn(source: &str, at: usize) -> String {
        let start = source[..at]
            .rfind("fn ")
            .expect("a call site inside some function");
        source[start + "fn ".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect()
    }

    /// **TASK-194 2b, the producer's half.** A route the floor overruled says so
    /// on its own `route_decided`, derived end to end from a config rather than
    /// asserted against a hand-built payload.
    ///
    /// The client's rendering of this field is guarded against a
    /// `RouteDecided { bound_floored: Some(true) }` built by hand
    /// (`session_ui.rs`), and the wire's additive default against a literal
    /// frame (`events.rs`) — neither of which touches the line that *puts* the
    /// value there. `Some(self.budget.floored)` → `Some(false)` therefore left
    /// the whole workspace green, and with it the entire route-line half of
    /// TASK-194 2b: every floored route would announce `bound: user cap` beside
    /// a budget larger than that cap, which is exactly the untruth the field
    /// exists to close.
    ///
    /// Both directions, because only one of them was ever caught. A 500-token
    /// cap on a declared 200k window derives 500 − 1,024 → 0 usable and lands on
    /// the floor; the same window uncapped does not go near it. The two routes
    /// differ in nothing but the cap, so the assertion is about the floor rather
    /// than about the fixture.
    #[test]
    fn a_floored_route_says_so_on_its_route_decided() {
        fn router_capped_at(cap: u32) -> Router {
            Router::new(
                CategoryTable::new()
                    .with_local_provider("local")
                    .with_tier(tier(CoreTier::Think, "wide", None)),
                None,
            )
            .with_provider(
                "wide",
                ProviderKind::OpenaiCompatible,
                "wide-model",
                CapabilityProfile {
                    max_context: 200_000,
                    context_budget_cap: cap,
                    ..native()
                },
                ProviderHealth::Healthy,
            )
        }

        // Sub-floor: the cap is recorded, the floor is what runs, and the event
        // says the ceiling it names is not the one in force.
        let route = router_capped_at(500).resolve(CoreCategory::Design);
        assert!(
            route.budget.floored,
            "a 500-token cap must derive below the floor, or this test is \
             asserting nothing: {:?}",
            route.budget
        );
        let decided = route.route_decided().expect("a provider was selected");
        assert_eq!(
            decided.bound,
            Some(BudgetBound::UserCap),
            "the bound still names what the user set — that is what they would \
             go and change"
        );
        assert_eq!(
            decided.bound_floored,
            Some(true),
            "…and the route line must say that cap is not in force: {decided:?}"
        );
        assert!(
            decided.budget_tokens > Some(500),
            "the turn really did get more than the cap asked for, which is the \
             whole reason the flag exists: {decided:?}"
        );

        // The twin, differing only in the cap: an honoured ceiling reports
        // `Some(false)`, never `None` and never the floored answer.
        let route = router_capped_at(0).resolve(CoreCategory::Design);
        assert!(!route.budget.floored, "{:?}", route.budget);
        let decided = route.route_decided().expect("a provider was selected");
        assert_eq!(decided.bound, Some(BudgetBound::Window));
        assert_eq!(
            decided.bound_floored,
            Some(false),
            "a daemon that derived a route always answers the question — `None` \
             on this wire means a daemon predating REQ-586: {decided:?}"
        );
    }

    /// **ADR-2 / AC-15**: a degrade changes the profile, not the window.
    ///
    /// `Degrade` is the one mid-turn re-config that must *not* move the budget:
    /// the failure said this provider calls tools badly, not that it forgot how
    /// big its window is. Deriving the reduced config from
    /// `CapabilityProfile::default()` — which is what it did before REQ-586
    /// (gotcha #1) — re-budgets a 128k route to 4,096 words mid-turn and
    /// reports `default_unknown` for a provider that declares a window. Both
    /// halves are asserted, because the pair alone would also match a
    /// coincidentally-defaulted route.
    #[test]
    fn a_degrade_keeps_the_failed_providers_budget() {
        let router = Router::new(
            CategoryTable::new()
                .with_local_provider("local")
                .with_tier(tier(CoreTier::Think, "wide", None)),
            None,
        )
        .with_provider(
            "wide",
            ProviderKind::OpenaiCompatible,
            "wide-model",
            CapabilityProfile {
                max_context: 128_000,
                ..native()
            },
            ProviderHealth::Healthy,
        );

        let route = router.resolve(CoreCategory::Design);
        assert_eq!(route.budget.bound, BudgetBound::Window);
        assert_eq!(route.budget.budget_tokens, 84_650);

        let outcome = router.on_provider_failure(&route, "wide", FailureClass::MalformedToolCall);
        let next = outcome.route.expect("continues on the same provider");
        assert_eq!(
            next.harness.max_tools,
            Some(5),
            "the profile did degrade — this test is about what did not"
        );
        assert_eq!(
            next.budget, route.budget,
            "the window survives the degrade, so no refit is needed (AC-15)"
        );
        assert_eq!(next.budget.bound, BudgetBound::Window);
        assert_eq!(next.harness.budget, next.budget);
        let before = route
            .route_decided()
            .expect("the failed route reported one");
        let after = next
            .route_decided()
            .expect("the degrade keeps the provider");
        assert_eq!(
            (after.budget_tokens, after.budget_bytes, after.bound),
            (before.budget_tokens, before.budget_bytes, before.bound),
            "the event after the degrade announces the budget it announced before"
        );
        assert_eq!(after.budget_tokens, Some(84_650));

        // The user's cap is part of "the failed provider's budget" too, and it
        // is the half a re-derivation from an empty profile would lose *without*
        // changing the pair to the obvious default (TASK-192: the pair alone is
        // not enough to catch a wrong source). A capped provider that degrades
        // still runs under its cap, and still says so.
        let capped = router.clone().with_provider(
            "wide",
            ProviderKind::OpenaiCompatible,
            "wide-model",
            CapabilityProfile {
                max_context: 128_000,
                context_budget_cap: 40_000,
                ..native()
            },
            ProviderHealth::Healthy,
        );
        let route = capped.resolve(CoreCategory::Design);
        assert_eq!(route.budget.bound, BudgetBound::UserCap);
        let outcome = capped.on_provider_failure(&route, "wide", FailureClass::MalformedToolCall);
        let next = outcome.route.expect("continues on the same provider");
        assert_eq!(next.budget, route.budget);
        assert_eq!(next.budget.bound, BudgetBound::UserCap);
    }

    /// **AC-12 / BR-8**: one budget, one source.
    ///
    /// The value the event announces, the value stamped on the config the loop
    /// runs under, and the value `budget_for` computes are the *same* value —
    /// not three computations of one fact. A mutation to the derivation moves
    /// all of them together or fails here, which is what makes "no surface
    /// re-derives it" assertable rather than a comment.
    #[test]
    fn the_budget_the_event_reports_is_the_budget_the_turn_runs_under() {
        let router = router();
        for category in CoreCategory::ALL {
            let route = router.resolve(category);
            let Some(decided) = route.route_decided() else {
                continue;
            };
            let id = route.provider_id.as_ref().expect("a provider was selected");
            assert_eq!(
                route.budget, route.harness.budget,
                "{category}: the route and the config it hands the loop"
            );
            assert_eq!(
                route.budget,
                router.budget_for(Some(&id.0)),
                "{category}: and the classifier they both came from"
            );
            assert_eq!(
                decided.budget_tokens,
                Some(route.budget.budget_tokens as u64)
            );
            assert_eq!(decided.budget_bytes, Some(route.budget.budget_bytes as u64));
            assert_eq!(decided.bound, Some(route.budget.bound));

            // What the harness is actually held to, in both currencies: the
            // event would still be honest about a `RouteBudget` nothing read.
            let turn = route.turn_route().expect("a provider was selected");
            assert_eq!(
                turn.config.context_budget_tokens,
                route.budget.budget_tokens
            );
            assert_eq!(turn.config.context_budget_bytes, route.budget.budget_bytes);
            assert_eq!(turn.config.budget, route.budget);
        }

        // The two routes that report nothing still agree with themselves: the
        // taint pin (local, by privacy) and an unresolvable category.
        let pin = router.resolve_local_pin("session touched local-only content");
        assert_eq!(pin.budget, pin.harness.budget);
        assert_eq!(pin.budget.bound, BudgetBound::LocalEngine);
        let nowhere = Router::new(CategoryTable::new(), None).resolve(CoreCategory::Design);
        assert_eq!(nowhere.budget, nowhere.harness.budget);
        assert!(
            nowhere.route_decided().is_none(),
            "no provider, nothing to report"
        );
    }

    /// **BR-7**: session taint overrides every category binding.
    ///
    /// The fixture binds `think` to a remote frontier provider, which is where a
    /// `design` turn goes on any untainted session — and the pin still lands on
    /// the local tier. Category routing is a cost decision; this is a privacy
    /// guarantee, and the pin consults no binding to make it.
    ///
    /// The *ordering* — taint checked before a category is even chosen — lives in
    /// `runtime.rs`; what is assertable here is that the pin cannot be talked out
    /// of the local tier by any table.
    #[test]
    fn the_taint_pin_overrides_every_category_binding() {
        let router = router();
        let think_goes_remote = router.resolve(CoreCategory::Design);
        assert_eq!(
            think_goes_remote.provider_id.as_ref().unwrap().0,
            "anthropic",
            "this test is only meaningful while `think` is bound remotely"
        );

        let route = router.resolve_local_pin("session touched local-only content");
        assert_eq!(route.provider_id.as_ref().unwrap().0, "local");
        assert!(route.phase.is_none());
        assert_eq!(route.outcome, RouteOutcome::Fallback);
        assert!(route.reason.contains("local-only"));
        assert!(
            route.resolution.is_none(),
            "the pin must not carry a category resolution — it consulted none \
             (BR-7), and minting one would be a second answer to drift from"
        );

        // Privacy trumps latency: the pin holds even when the local tier is
        // below its BR-8 duty, where the category chain would refuse it.
        let shed = router.clone().with_local_available(false);
        assert_eq!(
            shed.resolve_local_pin("tainted").provider_id.unwrap().0,
            "local"
        );
    }

    /// A tainted session has **no fallback**, because a fallback is a binding and
    /// the pin reads none. Before REQ-558 the failure path re-read the phase
    /// policy, so a tainted *structured* session could fail over to that phase's
    /// remote fallback — the privacy pin surviving the decision but not the first
    /// provider error.
    ///
    /// The table is built here rather than taken from [`router`] because the
    /// shared fixture binds no tier to `local`, and the defect only fires when
    /// **the failed provider is some row's primary**: against that fixture,
    /// restoring the old "look the failed provider up in the table" logic
    /// changes nothing and the test stays green. BUG-156's own reproduction is
    /// this shape — `provider_id = "local"`, `fallback_id = <remote>` — which is
    /// what the local-first pitch invites a user to write, so it is the shape
    /// this test has to hold.
    #[test]
    fn a_tainted_session_cannot_fail_over_to_a_remote_provider() {
        let router = Router::new(
            CategoryTable::new()
                .with_local_provider("local")
                // The local tier as PRIMARY, with a remote fallback.
                .with_tier(tier(CoreTier::Think, "local", Some("anthropic"))),
            None,
        )
        .with_provider(
            "anthropic",
            ProviderKind::Anthropic,
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        );

        // Non-vacuity: on this very table an ordinary category failure DOES
        // reach the remote fallback, so the pinned assertion below is the pin
        // holding rather than an unreachable fallback.
        let bound = router.resolve(CoreCategory::Design);
        assert_eq!(bound.provider_id.as_ref().unwrap().0, "local");
        let reachable =
            router.on_provider_failure(&bound, "local", FailureClass::MalformedResponse);
        assert_eq!(
            reachable
                .route
                .as_ref()
                .and_then(|r| r.provider_id.as_ref())
                .map(|p| p.0.as_str()),
            Some("anthropic"),
            "an untainted turn on this binding fails over to the remote fallback"
        );

        let pinned = router.resolve_local_pin("session touched local-only content");
        assert_eq!(pinned.provider_id.as_ref().unwrap().0, "local");
        let outcome = router.on_provider_failure(&pinned, "local", FailureClass::MalformedResponse);
        assert!(
            outcome.route.is_none(),
            "a pinned session must not continue on a provider a binding named: {:?}",
            outcome.route
        );
        assert_eq!(
            outcome.degraded.and_then(|d| d.fallback_id),
            None,
            "and the degradation event must not announce a fallback it will not take"
        );
    }

    /// AC-8, as far as it can be closed by construction: every route the router
    /// produces carries the resolution it was built from — except the taint pin,
    /// which is the one path that deliberately resolves no category.
    #[test]
    fn every_route_but_the_taint_pin_carries_its_resolution() {
        let router = router();
        for category in CoreCategory::ALL {
            let route = router.resolve(category);
            let resolution = route
                .resolution
                .as_ref()
                .unwrap_or_else(|| panic!("{category} resolved without a resolution"));
            assert_eq!(resolution.category, category);
            assert_eq!(resolution.tier, category.tier());
            assert_eq!(route.reason, resolution.reason);
            assert_eq!(route.outcome, resolution.outcome);
            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                resolution.provider_id.as_deref()
            );
        }
        // A failure re-route keeps it: the provider changed, the category did not.
        let route = router.resolve(CoreCategory::Design);
        let outcome =
            router.on_provider_failure(&route, "anthropic", FailureClass::MalformedResponse);
        let next = outcome.route.expect("continues on the fallback");
        assert_eq!(
            next.resolution.as_ref().map(|r| r.category),
            Some(CoreCategory::Design)
        );

        // And the one exception, named rather than assumed.
        assert!(router.resolve_local_pin("tainted").resolution.is_none());
    }

    #[test]
    fn on_failure_fallback_returns_the_fallback_route_and_degraded_event() {
        // A Fallback-class failure on the `think` primary (anthropic) → fall back
        // to deepseek, emit provider_degraded naming it (AC-7).
        let router = router();
        let route = router.resolve(CoreCategory::Design);
        let outcome =
            router.on_provider_failure(&route, "anthropic", FailureClass::MalformedResponse);
        let degraded = outcome
            .degraded
            .expect("fallback surfaces provider_degraded");
        assert_eq!(degraded.provider_id.0, "anthropic");
        assert_eq!(degraded.fallback_id.as_ref().unwrap().0, "deepseek");
        let next = outcome.route.expect("continues on the fallback");
        assert_eq!(next.provider_id.as_ref().unwrap().0, "deepseek");

        // The fallback has now been used, so a second failure has nowhere left to
        // go rather than failing over to itself.
        let again = router.on_provider_failure(&next, "deepseek", FailureClass::MalformedResponse);
        assert!(again.route.is_none(), "{:?}", again.route);
    }

    /// **The fallback survives a retry.** A transient failure re-attempts the
    /// *same* provider, so it has not spent the alternative the resolution
    /// chose — and when the retry itself fails hard, the turn must still fall
    /// over to it.
    ///
    /// `timeout → malformed` is not a contrived pair: it is the ordinary shape
    /// of a provider going bad, and it fits inside the daemon's 2-attempt
    /// budget. Clearing the fallback on the retry arm meant the second failure
    /// reported "provider failed and no fallback is configured" — about a config
    /// that configures one, and after never having tried it.
    ///
    /// Non-vacuous by construction: the same second failure applied to the
    /// *original* route is asserted to fall over too, so this cannot pass by the
    /// retry route having lost its identity rather than its fallback.
    #[test]
    fn a_retry_does_not_spend_the_fallback() {
        let router = router();
        let route = router.resolve(CoreCategory::Design);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "anthropic");

        // 1. A transient timeout: same provider, no event, nothing spent.
        let retried = router.on_provider_failure(&route, "anthropic", FailureClass::Timeout);
        assert!(retried.degraded.is_none(), "a retry reports nothing yet");
        let next = retried.route.expect("a retry continues");
        assert_eq!(
            next.provider_id.as_ref().unwrap().0,
            "anthropic",
            "a retry re-attempts the same provider"
        );
        assert_eq!(
            next.resolution.as_ref().unwrap().fallback_id.as_deref(),
            Some("deepseek"),
            "the retry never reached the fallback, so it must not have spent it"
        );

        // 2. The retry fails hard. This is the assertion the bug broke.
        let after = router.on_provider_failure(&next, "anthropic", FailureClass::MalformedResponse);
        let continued = after
            .route
            .expect("a timeout must not cost the turn its configured fallback");
        assert_eq!(continued.provider_id.as_ref().unwrap().0, "deepseek");
        assert_eq!(
            after.degraded.and_then(|d| d.fallback_id).map(|f| f.0),
            Some("deepseek".to_owned()),
            "and the degradation event names the fallback it actually took"
        );

        // 3. Which is spent now — a third failure has nowhere left to go.
        assert!(router
            .on_provider_failure(&continued, "deepseek", FailureClass::MalformedResponse)
            .route
            .is_none());
    }

    /// The `Degrade` arm keeps the same provider under a reduced harness
    /// profile, so it has not spent the fallback either. The sibling of
    /// [`a_retry_does_not_spend_the_fallback`], for the other non-switching arm.
    #[test]
    fn a_degrade_does_not_spend_the_fallback() {
        let router = router();
        let route = router.resolve(CoreCategory::Design);

        let degraded =
            router.on_provider_failure(&route, "anthropic", FailureClass::MalformedToolCall);
        let next = degraded.route.expect("a degrade continues");
        assert_eq!(next.provider_id.as_ref().unwrap().0, "anthropic");
        assert!(next.harness.require_verification);
        assert_eq!(
            next.resolution.as_ref().unwrap().fallback_id.as_deref(),
            Some("deepseek"),
            "degrading the harness is not failing over; the fallback is unspent"
        );

        let after = router.on_provider_failure(&next, "anthropic", FailureClass::MalformedResponse);
        assert_eq!(
            after.route.expect("falls over").provider_id.unwrap().0,
            "deepseek"
        );
    }

    #[test]
    fn on_failure_degrade_keeps_provider_with_a_reduced_profile() {
        let router = router();
        let route = router.resolve(CoreCategory::Edit);
        let outcome =
            router.on_provider_failure(&route, "deepseek", FailureClass::MalformedToolCall);
        let degraded = outcome
            .degraded
            .expect("degrade surfaces provider_degraded");
        assert_eq!(degraded.failure_class, ProtoFailureClass::ToolCallFailure);
        assert!(degraded.fallback_id.is_none());
        let next = outcome.route.expect("continues on the same provider");
        assert_eq!(next.provider_id.as_ref().unwrap().0, "deepseek");
        assert!(next.harness.require_verification);
        assert_eq!(next.harness.max_tools, Some(5));
    }

    #[test]
    fn on_failure_auth_error_aborts_with_no_route() {
        let router = router();
        let route = router.resolve(CoreCategory::Design);
        let outcome = router.on_provider_failure(
            &route,
            "anthropic",
            FailureClass::ClientError { status: 401 },
        );
        assert!(outcome.degraded.is_none());
        assert!(outcome.route.is_none());
    }

    #[test]
    fn failure_class_mapping_is_content_free_and_total() {
        assert_eq!(
            to_protocol_failure_class(FailureClass::ClientError { status: 429 }),
            ProtoFailureClass::RateLimited
        );
        assert_eq!(
            to_protocol_failure_class(FailureClass::MalformedToolCall),
            ProtoFailureClass::ToolCallFailure
        );
        assert_eq!(
            to_protocol_failure_class(FailureClass::Timeout),
            ProtoFailureClass::Timeout
        );
    }

    /// **The provider enumeration BR-9's remedy is chosen from** (REQ-589
    /// ADR-12).
    ///
    /// Three claims, and each is a way the remedy goes wrong without it:
    ///
    /// * **The local tier is never a candidate.** Binding a tier to the engine
    ///   the route is already on is the circle the reported `/analyze` failure
    ///   was sitting in. The filter reads
    ///   [`CategoryTable::local_provider_id`] rather than the map's shape or a
    ///   [`ProviderKind`] (gotcha #9), so the local provider is registered here
    ///   *with* a `[[providers]]`-style entry — the case a membership test would
    ///   let through — and it must still not appear.
    /// * **The count is what ADR-12 keys on.** One configured remote may be
    ///   proposed by name; two or more are a choice; zero is a real answer that
    ///   the offer states rather than papers over.
    /// * **The model travels with the id.** The window a `BindTierRemote`
    ///   remedy declares is looked up by *model* (ADR-6 rule 1), so a list of
    ///   bare ids would send the caller back to the router — or, worse, to the
    ///   id — for the key.
    ///
    /// Health is deliberately not screened: a provider that is down right now is
    /// still configured, this list feeds a config write rather than a routing
    /// decision, and an offer whose options moved between two renders of the
    /// same question would be a consent surface that changed under the answer.
    #[test]
    fn remote_providers_lists_every_configured_remote_and_never_the_local_tier() {
        let empty = Router::new(CategoryTable::new().with_local_provider("on-device"), None);
        assert!(
            empty.remote_providers().is_empty(),
            "no remote is configured, so BR-9's remedy has nothing to bind to and the offer has \
             to say so"
        );

        let one = empty.clone().with_provider(
            "kimi",
            ProviderKind::OpenaiCompatible,
            "kimi-k3",
            native(),
            ProviderHealth::Healthy,
        );
        assert_eq!(
            one.remote_providers(),
            vec![RemoteProvider {
                id: "kimi".to_owned(),
                model: "kimi-k3".to_owned(),
            }],
            "exactly one configured remote — ADR-12's proposed-by-name case"
        );

        let many = one
            .with_provider(
                "anthropic",
                ProviderKind::Anthropic,
                "claude-opus-5",
                native(),
                // Down right now, and still a provider the user configured and
                // may want their `think` tier bound to.
                ProviderHealth::Unavailable,
            )
            .with_provider(
                // The local tier, registered in the map as well, under an
                // openai-compatible kind and a declared window — which is how
                // a served local endpoint is actually configured. Every
                // *other* way of asking "is this local?" answers wrong here:
                // map membership says remote, `ProviderKind` says remote, and
                // "its capabilities look defaulted" says remote. Only
                // `local_provider_id` says local, which is the point (gotcha
                // #9).
                "on-device",
                ProviderKind::OpenaiCompatible,
                "qwen3-8b",
                native(),
                ProviderHealth::Healthy,
            );
        assert_eq!(
            many.remote_providers(),
            vec![
                RemoteProvider {
                    id: "anthropic".to_owned(),
                    model: "claude-opus-5".to_owned(),
                },
                RemoteProvider {
                    id: "kimi".to_owned(),
                    model: "kimi-k3".to_owned(),
                },
            ],
            "two or more configured remotes are a choice, in stable id order, and the local tier \
             is not among them however it got into the map"
        );
        assert!(
            !many
                .remote_providers()
                .iter()
                .any(|provider| provider.id == "on-device"),
            "the tier a local-engine route would be leaving is never the tier's new home"
        );
    }
}
