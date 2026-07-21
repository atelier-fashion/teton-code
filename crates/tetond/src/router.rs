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

use teton_core::entities::RoutingPolicy;
use teton_core::phase::Phase as CorePhase;
use teton_core::policy::{evaluate, ProviderHealth, RouteOutcome};

use teton_protocol::events::{
    Event, FailureClass as ProtoFailureClass, ProviderDegraded, RouteDecided,
};
use teton_protocol::{Phase as ProtoPhase, ProviderId, SessionId};

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
}

impl Route {
    /// Whether a provider was actually selected.
    #[must_use]
    pub fn selected(&self) -> bool {
        self.provider_id.is_some()
    }

    /// The `route_decided` event payload for this decision, or `None` when no
    /// provider was selected (the event's `provider_id` is required).
    #[must_use]
    pub fn route_decided(&self) -> Option<RouteDecided> {
        self.provider_id.as_ref().map(|provider_id| RouteDecided {
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
    default_provider: String,
    /// Local tier provider id (freeform auxiliary duties).
    local_provider: String,
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
        default_provider: impl Into<String>,
        local_provider: impl Into<String>,
    ) -> Self {
        Self {
            policies,
            providers: BTreeMap::new(),
            default_provider: default_provider.into(),
            local_provider: local_provider.into(),
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
        }
    }

    /// Resolve the provider for a **freeform** prompt via the heuristics (BR-5).
    /// Auxiliary duties go local (or bypass to the default when the local tier is
    /// unavailable, BR-8); coding turns go to the configured default.
    #[must_use]
    pub fn resolve_freeform(&self, prompt: &str) -> Route {
        let config = FreeformConfig {
            local_provider_id: self.local_provider.clone(),
            default_provider_id: self.default_provider.clone(),
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
        let provider = self.local_provider.clone();
        Route {
            model: self.model_of(&provider),
            harness: self.harness_config_for(&provider),
            provider_id: Some(ProviderId::from(provider)),
            phase: None,
            reason: reason.into(),
            outcome: RouteOutcome::Fallback,
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

    /// The configured fallback provider for `phase`'s policy, when the primary
    /// (`failed`) is the one that failed. Freeform (no phase) has no policy
    /// fallback.
    fn fallback_for(&self, phase: Option<CorePhase>, failed: &str) -> Option<String> {
        let phase = phase?;
        let policy = self.policies.iter().find(|p| p.phase == phase)?;
        if policy.provider_id == failed {
            policy.fallback_id.clone()
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
            "deepseek",
            "local",
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
