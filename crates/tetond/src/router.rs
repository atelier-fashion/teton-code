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

use std::collections::BTreeMap;

use teton_core::category::{
    resolve as resolve_category, Category as CoreCategory, CategoryResolution, CategoryTable,
    JudgmentCategory, Tier as CoreTier, TierBinding,
};
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
    /// Capability profile (tool-call tier → harness degradation, BR-6).
    capabilities: CapabilityProfile,
    /// Current health as the router sees it (BR-5 policy fallback input).
    health: ProviderHealth,
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
        model: impl Into<String>,
        capabilities: CapabilityProfile,
        health: ProviderHealth,
    ) -> Self {
        self.providers.insert(
            id.into(),
            ProviderRuntime {
                model: model.into(),
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

    /// Set whether the local tier can meet its BR-8 latency duty (false when it is
    /// below the hardware floor, benchmark-disabled, or shed under pressure).
    #[must_use]
    pub fn with_local_available(mut self, available: bool) -> Self {
        self.local_available = available;
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
            return Route {
                model: None,
                harness: HarnessConfig::default(),
                provider_id: None,
                phase: None,
                reason: "This session is pinned to the local tier for privacy, but no local \
                         provider is registered, so the turn cannot be served."
                    .to_owned(),
                outcome: RouteOutcome::NoPolicy,
                resolution: None,
            };
        };
        Route {
            model: self.model_of(&provider),
            harness: self.harness_config_for(&provider),
            provider_id: Some(ProviderId::from(provider)),
            phase: None,
            reason: reason.into(),
            outcome: RouteOutcome::Fallback,
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
                    self.continue_on(
                        route,
                        fb,
                        RouteOutcome::Fallback,
                        reason,
                        self.harness_config_for(fb),
                    )
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
                    degraded_harness_config(),
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
        if let Some(decided) = route.route_decided() {
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
        let attribution = match route.phase {
            Some(phase) => CostAttribution::new(model).with_phase(phase),
            None => CostAttribution::new(model),
        };
        Some(
            EgressContext::new(provider_id)
                .with_session(session_id)
                .with_cost(attribution),
        )
    }

    /// The BR-6 [`HarnessConfig`] a `provider_id` should run under, derived from
    /// its capability profile. An unregistered provider defaults to the strict
    /// (Native) profile.
    #[must_use]
    pub fn harness_config_for(&self, provider_id: &str) -> HarnessConfig {
        HarnessConfig::from_harness_profile(self.capability_of(provider_id).harness_profile())
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
    /// `reflex` is defined as "sub-second, every turn, **never leaves the
    /// machine**" (REQ-558's tier table), so it inherits the local tier and
    /// nothing else. Filling it from `default_provider` like the other three
    /// would send a tier whose entire purpose is locality to whatever remote
    /// provider happened to be first in the config — and this is the ordinary
    /// upgrade path, not a contrived one: REQ-557's migration sets
    /// `default_provider` to the first remote provider, and an upgraded config
    /// has no `[[tiers]]` at all.
    ///
    /// `scan`/`build`/`think` inherit `default_provider` and fall back to the
    /// local tier, so an offline install still routes. Nothing is synthesized at
    /// any step — every candidate is config- or engine-declared (BR-8).
    ///
    /// The `reflex` exclusion is asked of [`CoreTier::inherits_default_provider`]
    /// rather than re-spelled here, because TASK-055's migration writes this
    /// same fill down as real `[[tiers]]` rows and must exclude `reflex` for the
    /// same reason. One fact, one home.
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
    fn inherited_binding(&self, tier: CoreTier) -> Option<(TierOrigin, String)> {
        if tier.inherits_default_provider() {
            if let Some(default) = self.default_provider.clone() {
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
        let harness = provider_id
            .as_deref()
            .map_or_else(HarnessConfig::default, |id| self.harness_config_for(id));
        Route {
            model: provider_id.as_deref().and_then(|id| self.model_of(id)),
            provider_id: provider_id.map(ProviderId::from),
            // Attribution only, and stamped on by the caller after the fact
            // (BR-11, AC-9). The resolver never saw a phase.
            phase: None,
            reason: resolution.reason.clone(),
            outcome: resolution.outcome,
            harness,
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
    /// It carries the failed route's phase and resolution forward, because the
    /// turn's *category* has not changed — only which provider is serving it. The
    /// resolution's own `fallback_id` is cleared: it has now been used, and a
    /// route that could fail over to itself would loop.
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
            harness,
            resolution: failed.resolution.clone().map(|mut r| {
                r.fallback_id = None;
                r
            }),
        }
    }
}

/// The forced reduced BR-6 harness profile, used when a failure reveals weak
/// tool-calling regardless of the provider's declared capability tier.
fn degraded_harness_config() -> HarnessConfig {
    use teton_core::ToolCallTier;
    HarnessConfig::from_harness_profile(
        CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            ..CapabilityProfile::default()
        }
        .harness_profile(),
    )
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
    use teton_core::category::category_for_phase;
    use teton_core::ToolCallTier;

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
            let decided = route_from(resolution.clone())
                .route_decided()
                .unwrap_or_else(|| panic!("{category} selected no provider"));

            assert_eq!(decided.category, Some(to_protocol_category(category)));
            assert_eq!(decided.tier, Some(to_protocol_tier(resolution.tier)));
            assert_eq!(decided.provider_id.0, resolution.provider_id.unwrap());
            assert!(!decided.reason.is_empty(), "{category}");
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
        }
    }

    fn degraded() -> CapabilityProfile {
        CapabilityProfile {
            tool_call_tier: ToolCallTier::Degraded,
            parallel_calls: false,
            max_context: 32_000,
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
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider(
            "deepseek",
            "deepseek-chat",
            native(),
            ProviderHealth::Healthy,
        )
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
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        );

        // A scan/build/think tier inherits the remote default...
        assert_eq!(
            router.resolve(CoreCategory::Edit).provider_id.unwrap().0,
            "frontier-remote"
        );
        // ...but reflex does not: the tier is defined as never leaving the
        // machine, so it inherits the local tier even when a remote default is
        // set. `title` is the reflex category the fill can actually reach —
        // `route` and `redact` are pinned before inheritance is consulted.
        assert_eq!(
            router.resolve(CoreCategory::Title).provider_id.unwrap().0,
            "local"
        );
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
        .with_provider("real", "a-model", native(), ProviderHealth::Healthy);

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

    /// BR-8: an unbound tier inherits the **declared** default — `default_provider`
    /// first (REQ-557 BR-4: "the provider an unrouted turn goes to"), then the
    /// local tier, which is what makes an offline machine route at all.
    ///
    /// Both are ids someone declared; neither is synthesized. With neither
    /// declared, the category names itself and its unset tier rather than
    /// borrowing a binding from somewhere else.
    #[test]
    fn an_unbound_tier_inherits_the_declared_default_then_the_local_tier() {
        // `scan` is unbound in the fixture, so `digest` inherits the default.
        let route = router().resolve(CoreCategory::Digest);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");

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

    /// The `reflex` tier is defined as "sub-second, every turn, **never leaves
    /// the machine**" (REQ-558's tier table). So an UNBOUND reflex tier must
    /// fall to the local provider, never to `default_provider`.
    ///
    /// This matters on the ordinary upgrade path, not a contrived one: REQ-557's
    /// migration sets `default_provider` to the first remote provider, and an
    /// upgraded config has no `[[tiers]]` at all. Filling every tier from that
    /// one value sends a tier whose whole purpose is locality to a frontier
    /// model — and reports it that way in `policy show`.
    #[test]
    fn an_unbound_reflex_tier_falls_to_local_never_to_the_remote_default() {
        let router = Router::new(
            CategoryTable::new().with_local_provider("on-device"),
            Some("frontier-remote".to_owned()),
        )
        .with_provider(
            "frontier-remote",
            "claude-opus-4",
            native(),
            ProviderHealth::Healthy,
        )
        .with_provider("on-device", "qwen", native(), ProviderHealth::Healthy);
        // `title` is the reflex category that is neither pinned nor classified,
        // so it is the one the fill can actually reach.
        let route = router.resolve(CoreCategory::Title);
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("on-device"),
            "reflex resolved to {:?}; the tier is defined as never leaving the \
             machine, so an unbound reflex must inherit the local tier, not the \
             remote default: {}",
            route.provider_id,
            route.reason
        );

        // The other three tiers legitimately inherit the remote default.
        for category in [
            CoreCategory::Digest,
            CoreCategory::Edit,
            CoreCategory::Design,
        ] {
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
        let router =
            router().with_provider("kimi", "kimi-k2", degraded(), ProviderHealth::Degraded);
        let cfg = router.harness_config_for("kimi");
        assert!(cfg.require_verification);
        assert_eq!(cfg.max_tools, Some(5));
        assert!(cfg.max_turns <= 5);
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
}
