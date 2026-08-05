//! The router: phase-policy routing, remote wiring, and BR-6 degradation.
//!
//! This is the *wiring* layer over pure policy. It does **no** policy logic of
//! its own: structured-mode decisions come straight from
//! [`teton_core::policy::evaluate`] (phase × table × provider-health → provider +
//! reason), and freeform decisions from [`crate::heuristics`]. The router's job
//! is everything around that pure core:
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
//! ## Two `Phase` types
//!
//! `teton_core::Phase` is the routing axis the pure policy consumes;
//! `teton_protocol::Phase` is what travels on the `route_decided` /
//! `cost_recorded` wire. They have identical variants; [`to_protocol_phase`]
//! bridges them at the event/attribution boundary.

use std::collections::BTreeMap;

use teton_core::category::{Category as CoreCategory, CategoryResolution, Tier as CoreTier};
use teton_core::entities::RoutingPolicy;
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::{evaluate, ProviderHealth, RouteOutcome};

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
use crate::cost::CostAttribution;
use crate::egress::EgressContext;
use crate::harness::turn_loop::{HarnessConfig, TurnRoute};
use crate::heuristics::{route_freeform, FreeformConfig};

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
        CorePhase::Freeform => ProtoPhase::Freeform,
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
/// A `Route` is produced by [`Router::resolve_structured`] /
/// [`Router::resolve_freeform`] and by the fallback path
/// ([`Router::on_provider_failure`]). It is the single object the daemon threads
/// into a turn: [`Route::turn_route`] hands the harness the provider + profile,
/// and [`Router::egress_context`] builds the choke-point context for a remote
/// call.
#[derive(Debug, Clone)]
pub struct Route {
    /// Provider selected, or `None` when no provider could be selected (no policy
    /// for the phase, or every candidate unavailable).
    pub provider_id: Option<ProviderId>,
    /// Concrete model chosen, when the provider is registered.
    pub model: Option<String>,
    /// Phase (protocol form) driving the decision; `None` in freeform mode.
    pub phase: Option<ProtoPhase>,
    /// User-facing sentence explaining the decision (feeds `route_decided`, BR-5).
    pub reason: String,
    /// Structured outcome for programmatic branching (reused from the policy
    /// evaluator; freeform maps its heuristic onto the same vocabulary).
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
    /// `None` for a decision reached by the pre-category paths — phase policy
    /// and the freeform heuristic — that TASK-050 replaces. Those paths have no
    /// resolution to read, and inventing one for them would be the second
    /// computation this field exists to prevent.
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

/// The phase-policy router (architecture: Session → Router → egress).
///
/// Holds the routing table, the registered providers (model + capabilities +
/// health), the freeform default/local ids, and whether the local tier can meet
/// its BR-8 latency duty. Construction is builder-style so a caller (or a test)
/// wires exactly the providers it needs.
#[derive(Debug, Clone)]
pub struct Router {
    policies: Vec<RoutingPolicy>,
    providers: BTreeMap<String, ProviderRuntime>,
    /// Freeform default provider (coding turns, and the BR-8 bypass target).
    default_provider: Option<String>,
    /// Local tier provider id (freeform auxiliary duties).
    local_provider: Option<String>,
    /// Whether the local tier can serve its BR-8 latency duty right now.
    local_available: bool,
}

impl Router {
    /// A router with the given routing table, freeform `default_provider` (coding
    /// turns), and `local_provider` (auxiliary duties). The local tier starts
    /// available; register providers with [`Router::with_provider`].
    #[must_use]
    pub fn new(
        policies: Vec<RoutingPolicy>,
        default_provider: Option<String>,
        local_provider: Option<String>,
    ) -> Self {
        Self {
            policies,
            providers: BTreeMap::new(),
            default_provider,
            local_provider,
            local_available: true,
        }
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

    /// Resolve the provider for a **structured-mode** `phase` from the policy
    /// table (BR-5). Pure policy evaluation ([`teton_core::policy::evaluate`])
    /// decides; the router only attaches the model, phase, and BR-6 harness
    /// profile of whatever provider policy chose.
    #[must_use]
    pub fn resolve_structured(&self, phase: CorePhase) -> Route {
        let decision = evaluate(phase, &self.policies, |id| self.health_of(id));
        let provider_id = decision.provider_id;
        let harness = provider_id
            .as_deref()
            .map_or_else(HarnessConfig::default, |id| self.harness_config_for(id));
        Route {
            model: provider_id.as_deref().and_then(|id| self.model_of(id)),
            provider_id: provider_id.map(ProviderId::from),
            phase: Some(to_protocol_phase(phase)),
            reason: decision.reason,
            outcome: decision.outcome,
            harness,
            // Phase policy, not the category chain — TASK-050 repoints this at
            // `teton_core::category::resolve` and the resolution lands here.
            resolution: None,
        }
    }

    /// Resolve the provider for a **freeform** prompt via the heuristics (BR-5).
    /// Auxiliary duties go local (or bypass to the default when the local tier is
    /// unavailable, BR-8); coding turns go to the configured default.
    #[must_use]
    pub fn resolve_freeform(&self, prompt: &str) -> Route {
        // REQ-557 BR-4 / ADR-D: an unset default is a real absence. Before this
        // REQ the router picked whichever remote provider was first in the config
        // and, failing that, minted the literal id "local" — the doubled fallback
        // that produced BUG-146. A freeform turn with no default and no usable
        // local tier now says so instead of naming a provider registered nowhere.
        // BUG-155: a `default_provider` naming a provider the router refused to
        // register (remote, no declared model — ADR-E) is treated exactly like an
        // absent one, and falls into the branch below. `Config::validate` accepts
        // such a config on purpose (BR-6 checks the id is REGISTERED, and ADR-E
        // keeps usability out of validation so a pre-REQ config can still boot),
        // so this is the layer that has to notice. Reusing the no-default branch
        // rather than adding a second one keeps one classifier for one state
        // (BR-5, LESSON-456).
        let usable_default = self
            .default_provider
            .clone()
            .filter(|id| self.is_routable(id));
        let Some(default_provider_id) = usable_default else {
            // The local tier can still serve an auxiliary duty on its own; only a
            // coding turn genuinely needs the default. Rather than re-implement
            // the duty split here, fall back to local when it is available and
            // otherwise report the missing default.
            return match (&self.local_provider, self.local_available) {
                // The local tier can still serve. Route through the SAME
                // heuristic rather than short-circuiting it: REQ-544 BR-5 makes
                // every decision's reason name the signal that fired, and a fixed
                // sentence would drop the duty classification that legibility
                // depends on. The missing default is appended, not substituted.
                (Some(local), true) => {
                    let config = FreeformConfig {
                        local_provider_id: local.clone(),
                        default_provider_id: local.clone(),
                        local_available: true,
                    };
                    let decision = route_freeform(prompt, &config);
                    Route {
                        model: self.model_of(local),
                        harness: self.harness_config_for(local),
                        provider_id: Some(ProviderId::from(local.as_str())),
                        phase: None,
                        reason: format!(
                            "{} No usable default provider is configured, so a coding turn stays \
                             local too; set `default_provider` to a provider that declares a \
                             `model` to route one remotely.",
                            decision.reason
                        ),
                        outcome: RouteOutcome::Fallback,
                        resolution: None,
                    }
                }
                _ => Route {
                    model: None,
                    harness: HarnessConfig::default(),
                    provider_id: None,
                    phase: None,
                    reason: "No usable default provider is configured and the local tier \
                             cannot serve this turn. Register a provider with `teton provider add \
                             <id> --model <name>` and set `default_provider` to its id."
                        .to_owned(),
                    outcome: RouteOutcome::NoPolicy,
                    resolution: None,
                },
            };
        };
        let Some(local_provider_id) = self.local_provider.clone() else {
            // No local tier at all: every freeform turn goes to the default.
            return Route {
                model: self.model_of(&default_provider_id),
                harness: self.harness_config_for(&default_provider_id),
                provider_id: Some(ProviderId::from(default_provider_id.as_str())),
                phase: None,
                reason: format!(
                    "Freeform routing: no local tier is registered, so this turn goes to the \
                     configured default provider '{default_provider_id}'."
                ),
                outcome: RouteOutcome::Primary,
                resolution: None,
            };
        };
        let config = FreeformConfig {
            local_provider_id,
            default_provider_id,
            local_available: self.local_available,
        };
        let decision = route_freeform(prompt, &config);
        let outcome = if decision.bypassed_local {
            // The local tier was bypassed to a remote provider (BR-8) — the
            // closest policy-vocabulary fit is a fallback off the local tier.
            RouteOutcome::Fallback
        } else if self.is_degraded(&decision.provider_id) {
            RouteOutcome::PrimaryDegraded
        } else {
            RouteOutcome::Primary
        };
        Route {
            model: self.model_of(&decision.provider_id),
            harness: self.harness_config_for(&decision.provider_id),
            provider_id: Some(ProviderId::from(decision.provider_id)),
            phase: None,
            reason: decision.reason,
            outcome,
            // The freeform heuristic, not the category chain — TASK-050 deletes
            // this path and resolves the classified category instead.
            resolution: None,
        }
    }

    /// Force a route to the **local tier**, ignoring phase policy and heuristics
    /// entirely (REQ-544 C-2 / M-1).
    ///
    /// This is the taint backstop for BR-1: a session whose context has touched
    /// `local-only` content — or an unknown-provenance `shell` result — is pinned
    /// here for every subsequent turn, and a remote turn blocked at egress is
    /// re-run here rather than retried. Privacy trumps latency, so this pins local
    /// even when the local tier is latency-degraded; the caller checks whether a
    /// local engine actually exists (a remote-only machine cannot serve a tainted
    /// session and fails closed instead).
    #[must_use]
    pub fn resolve_local_pin(&self, reason: impl Into<String>) -> Route {
        let Some(provider) = self.local_provider.clone() else {
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

    /// Handle a mid-session provider failure (AC-7).
    ///
    /// Classifies `class` ([`teton_providers::classify`]) and:
    /// - **Fallback** — re-resolves to the phase's configured fallback provider
    ///   and emits `provider_degraded` naming it. The session continues there.
    /// - **Degrade** — keeps the same provider but forces the reduced BR-6 harness
    ///   profile, and emits `provider_degraded` with no fallback.
    /// - **Retry** — transient; no event, the caller retries the same route.
    /// - **Fail** — unrecoverable (e.g. auth); no route, the caller aborts.
    #[must_use]
    pub fn on_provider_failure(
        &self,
        phase: Option<CorePhase>,
        failed_provider: &str,
        class: FailureClass,
    ) -> FailureOutcome {
        let decision = classify(class);
        let signal = degradation_signal(failed_provider, decision);
        match decision.action {
            FailureAction::Fallback => {
                let fallback = self.fallback_for(phase, failed_provider);
                let route = fallback.as_deref().map(|fb| {
                    let reason = signal.as_ref().map_or_else(
                        || format!("Falling back to '{fb}' after a provider failure."),
                        |s| format!("{} Continuing on the fallback '{fb}'.", s.reason),
                    );
                    self.route_to(
                        phase,
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
                    route,
                }
            }
            FailureAction::Degrade => {
                // Keep the provider, force the reduced profile (BR-6): the failure
                // revealed weak tool-calling regardless of the declared tier.
                let reason = signal.as_ref().map_or_else(
                    || format!("'{failed_provider}' dropped to a reduced harness profile."),
                    |s| s.reason.clone(),
                );
                let route = self.route_to(
                    phase,
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
                    route: Some(route),
                }
            }
            FailureAction::Retry => FailureOutcome {
                degraded: None,
                route: Some(self.route_to(
                    phase,
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

    fn capability_of(&self, provider_id: &str) -> CapabilityProfile {
        self.providers
            .get(provider_id)
            .map_or_else(CapabilityProfile::default, |p| p.capabilities)
    }

    fn is_degraded(&self, provider_id: &str) -> bool {
        use teton_core::ToolCallTier;
        self.capability_of(provider_id).tool_call_tier == ToolCallTier::Degraded
    }

    fn model_of(&self, provider_id: &str) -> Option<String> {
        self.providers.get(provider_id).map(|p| p.model.clone())
    }

    /// Health of a provider; an unregistered id is treated as unavailable so a
    /// policy that names a provider the daemon does not know cannot select it.
    fn health_of(&self, provider_id: &str) -> ProviderHealth {
        self.providers
            .get(provider_id)
            .map_or(ProviderHealth::Unavailable, |p| p.health)
    }

    /// Whether `provider_id` is actually routable — i.e. it entered the provider
    /// map at construction.
    ///
    /// `build_router` deliberately skips a remote provider that declares no
    /// model (REQ-557 ADR-E), so map membership IS the usability check. Policy
    /// evaluation gets this for free because `health_of` reports an unmapped id
    /// as `Unavailable`; the two paths below read a provider id straight out of
    /// config and so have to ask explicitly (BUG-155).
    fn is_routable(&self, provider_id: &str) -> bool {
        self.providers.contains_key(provider_id)
    }

    /// The configured fallback provider for `phase`'s policy, when the primary
    /// (`failed`) is the one that failed. Freeform (no phase) has no policy
    /// fallback.
    ///
    /// BUG-155: the fallback id is read straight from the policy, so — unlike the
    /// primary, which `evaluate` screens through `health_of` — it has to be
    /// screened here. Without this, a mid-turn failure could fail over to a
    /// provider the router refused to register, and the turn would egress to it
    /// with the provider id as its model.
    fn fallback_for(&self, phase: Option<CorePhase>, failed: &str) -> Option<String> {
        let phase = phase?;
        let policy = self.policies.iter().find(|p| p.phase == phase)?;
        if policy.provider_id == failed {
            policy.fallback_id.clone().filter(|fb| self.is_routable(fb))
        } else {
            None
        }
    }

    fn route_to(
        &self,
        phase: Option<CorePhase>,
        provider: &str,
        outcome: RouteOutcome,
        reason: String,
        harness: HarnessConfig,
    ) -> Route {
        Route {
            provider_id: Some(ProviderId::from(provider)),
            model: self.model_of(provider),
            phase: phase.map(to_protocol_phase),
            reason,
            outcome,
            harness,
            // A failure re-route names the provider it continues on, not a fresh
            // category resolution; TASK-050 carries the original one through.
            resolution: None,
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

    /// BUG-155: a policy `fallback_id` naming a provider the router never
    /// registered is not failed over to.
    ///
    /// Pinned HERE, at the router, rather than only through an end-to-end
    /// "no bytes reached the provider" assertion. Those two guards — screening
    /// the fallback, and refusing a remote route that carries no model — mask
    /// each other: with either one in place the traffic assertion still passes,
    /// so mutating either alone looked safe. Defence in depth is fine; two
    /// guards with no independent coverage is not (LESSON-483's shape, one layer
    /// over). This test fails if the screen is removed, whatever the backstop does.
    #[test]
    fn a_fallback_to_an_unregistered_provider_is_not_taken() {
        use teton_providers::FailureClass;

        let router = Router::new(
            vec![RoutingPolicy {
                phase: CorePhase::Implement,
                provider_id: "primary".to_owned(),
                fallback_id: Some("unusable".to_owned()),
            }],
            None,
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

        let outcome = router.on_provider_failure(
            Some(CorePhase::Implement),
            "primary",
            FailureClass::MalformedResponse,
        );

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

    fn policy(phase: CorePhase, provider: &str, fallback: Option<&str>) -> RoutingPolicy {
        RoutingPolicy {
            phase,
            provider_id: provider.to_owned(),
            fallback_id: fallback.map(str::to_owned),
        }
    }

    fn router() -> Router {
        Router::new(
            vec![
                policy(CorePhase::Spec, "anthropic", Some("deepseek")),
                policy(CorePhase::Implement, "deepseek", Some("anthropic")),
            ],
            Some("deepseek".to_owned()),
            Some("local".to_owned()),
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

    #[test]
    fn structured_resolution_names_the_rule_and_pins_phase_and_model() {
        let route = router().resolve_structured(CorePhase::Spec);
        assert_eq!(route.provider_id.as_ref().unwrap().0, "anthropic");
        assert_eq!(route.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(route.phase, Some(ProtoPhase::Spec));
        assert_eq!(route.outcome, RouteOutcome::Primary);
        assert!(route.reason.contains("routing policy"), "{}", route.reason);
        // A route_decided payload is emittable and carries the reason (BR-5).
        let decided = route.route_decided().expect("provider selected");
        assert_eq!(decided.provider_id.0, "anthropic");
        assert_eq!(decided.reason, route.reason);
    }

    #[test]
    fn to_protocol_phase_is_variant_for_variant() {
        for (core, proto) in [
            (CorePhase::Spec, ProtoPhase::Spec),
            (CorePhase::Architect, ProtoPhase::Architect),
            (CorePhase::Implement, ProtoPhase::Implement),
            (CorePhase::Review, ProtoPhase::Review),
            (CorePhase::Io, ProtoPhase::Io),
            (CorePhase::Freeform, ProtoPhase::Freeform),
        ] {
            assert_eq!(to_protocol_phase(core), proto);
        }
    }

    /// The wire twins mirror the core enums variant-for-variant, spelling
    /// included: the daemon's decision and what a client reads are one fact, and
    /// the shared spelling is what lets a ledger column, a config key, and an
    /// event all say `digest`.
    ///
    /// Driven off `Category::ALL` / `Tier::ALL`, so a variant added to core is
    /// covered here the moment `to_protocol_category`'s exhaustive match forces
    /// it to be handled at all.
    #[test]
    fn the_wire_category_and_tier_mirror_the_core_ones() {
        use teton_core::Tier;

        for core in CoreCategory::ALL {
            assert_eq!(
                to_protocol_category(core).as_str(),
                core.as_str(),
                "{core} is spelled differently on the wire"
            );
        }
        for core in Tier::ALL {
            assert_eq!(to_protocol_tier(core).as_str(), core.as_str(), "{core}");
            // A category's tier survives the crossing intact.
            for category in CoreCategory::ALL.into_iter().filter(|c| c.tier() == core) {
                assert_eq!(to_protocol_tier(category.tier()), to_protocol_tier(core));
            }
        }
        // All eleven, pinned-local ones included: the event reports what
        // happened, not what a config file could have asked for.
        assert_eq!(CoreCategory::ALL.len(), 11);
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

    #[test]
    fn freeform_coding_turn_goes_to_the_default() {
        let route = router().resolve_freeform("implement the parser");
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
        assert!(route.phase.is_none());
        assert!(route.turn_route().is_some());
    }

    #[test]
    fn local_pin_forces_the_local_tier_regardless_of_policy() {
        // REQ-544 C-2 / M-1: the taint backstop pins a session to the local tier,
        // naming a legible reason — independent of any phase policy that would
        // otherwise route remote.
        let route = router().resolve_local_pin("session touched local-only content");
        assert_eq!(route.provider_id.as_ref().unwrap().0, "local");
        assert!(route.phase.is_none());
        assert_eq!(route.outcome, RouteOutcome::Fallback);
        assert!(route.reason.contains("local-only"));
        // The Spec phase policy would normally route to anthropic — the pin wins.
        assert_ne!(
            route.provider_id.as_ref().unwrap().0,
            router()
                .resolve_structured(CorePhase::Spec)
                .provider_id
                .unwrap()
                .0
        );
    }

    #[test]
    fn on_failure_fallback_returns_the_fallback_route_and_degraded_event() {
        // A Fallback-class failure on the Spec primary (anthropic) → fall back to
        // deepseek, emit provider_degraded naming it (AC-7).
        let outcome = router().on_provider_failure(
            Some(CorePhase::Spec),
            "anthropic",
            FailureClass::MalformedResponse,
        );
        let degraded = outcome
            .degraded
            .expect("fallback surfaces provider_degraded");
        assert_eq!(degraded.provider_id.0, "anthropic");
        assert_eq!(degraded.fallback_id.as_ref().unwrap().0, "deepseek");
        let route = outcome.route.expect("continues on the fallback");
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
    }

    #[test]
    fn on_failure_degrade_keeps_provider_with_a_reduced_profile() {
        let outcome = router().on_provider_failure(
            Some(CorePhase::Implement),
            "deepseek",
            FailureClass::MalformedToolCall,
        );
        let degraded = outcome
            .degraded
            .expect("degrade surfaces provider_degraded");
        assert_eq!(degraded.failure_class, ProtoFailureClass::ToolCallFailure);
        assert!(degraded.fallback_id.is_none());
        let route = outcome.route.expect("continues on the same provider");
        assert_eq!(route.provider_id.as_ref().unwrap().0, "deepseek");
        assert!(route.harness.require_verification);
        assert_eq!(route.harness.max_tools, Some(5));
    }

    #[test]
    fn on_failure_auth_error_aborts_with_no_route() {
        let outcome = router().on_provider_failure(
            Some(CorePhase::Spec),
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
