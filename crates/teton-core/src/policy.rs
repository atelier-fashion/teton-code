//! Pure routing-policy evaluation.
//!
//! Given a [`Phase`], the routing table, and a way to look up provider health,
//! [`evaluate`] returns the provider to use plus a **user-facing reason
//! sentence**. The reason string is the payload of the `route_decided` event
//! (BR-5: "control = legibility") — it is written for a human to read, and its
//! content is part of this module's contract, so it is tested, not just the
//! chosen provider.
//!
//! This module is pure: no I/O, no clock, no globals. Provider health is
//! supplied by the caller as a closure so the daemon can plug in live probe
//! results while tests plug in fixed tables.

use crate::entities::RoutingPolicy;
use crate::phase::Phase;

/// Health of a provider as seen by the router at decision time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    /// Up and reliable — eligible for the full loop.
    Healthy,
    /// Up but with weak tool-calling — used with a reduced profile (BR-6),
    /// still the primary choice.
    Degraded,
    /// Down / erroring / timing out — triggers fallback.
    Unavailable,
}

/// Which branch of the policy produced the decision. Lets callers react
/// (e.g. emit `provider_degraded`) without re-parsing the reason string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Primary provider selected, healthy.
    Primary,
    /// Primary provider selected but degraded (reduced harness profile).
    PrimaryDegraded,
    /// Primary unavailable; fell back to the configured fallback.
    Fallback,
    /// No routing policy is configured for this phase.
    NoPolicy,
    /// A policy exists but no provider in it is currently usable.
    NoHealthyProvider,
}

impl RouteOutcome {
    /// Whether a provider was actually selected (`provider_id` is `Some`).
    #[must_use]
    pub fn selected_provider(self) -> bool {
        matches!(
            self,
            RouteOutcome::Primary | RouteOutcome::PrimaryDegraded | RouteOutcome::Fallback
        )
    }
}

/// The result of evaluating the routing policy for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    /// Chosen provider id, or `None` when nothing could be selected.
    pub provider_id: Option<String>,
    /// Human-readable sentence explaining the decision (feeds `route_decided`).
    pub reason: String,
    /// Structured outcome for programmatic branching.
    pub outcome: RouteOutcome,
}

/// Evaluate the routing policy for `phase`.
///
/// `health` maps a provider id to its current [`ProviderHealth`]. The function
/// consults the primary provider first and only falls back on
/// [`ProviderHealth::Unavailable`] — a merely [`ProviderHealth::Degraded`]
/// primary is kept (it just gets a reduced harness profile, BR-6).
pub fn evaluate<H>(phase: Phase, policies: &[RoutingPolicy], health: H) -> RouteDecision
where
    H: Fn(&str) -> ProviderHealth,
{
    let Some(policy) = policies.iter().find(|p| p.phase == phase) else {
        return RouteDecision {
            provider_id: None,
            reason: format!(
                "No routing policy is configured for the {phase} phase, so the harness cannot select a provider by policy."
            ),
            outcome: RouteOutcome::NoPolicy,
        };
    };

    let primary = policy.provider_id.as_str();
    match health(primary) {
        ProviderHealth::Healthy => RouteDecision {
            provider_id: Some(primary.to_owned()),
            reason: format!("Routing the {phase} phase to '{primary}' per your routing policy."),
            outcome: RouteOutcome::Primary,
        },
        ProviderHealth::Degraded => RouteDecision {
            provider_id: Some(primary.to_owned()),
            reason: format!(
                "Routing the {phase} phase to '{primary}' per your routing policy; its tool-calling is degraded, so the harness will use a reduced profile."
            ),
            outcome: RouteOutcome::PrimaryDegraded,
        },
        ProviderHealth::Unavailable => match policy.fallback_id.as_deref() {
            Some(fallback) => match health(fallback) {
                ProviderHealth::Unavailable => RouteDecision {
                    provider_id: None,
                    reason: format!(
                        "'{primary}' is unavailable and its fallback '{fallback}' is also unavailable, so no provider can be selected for the {phase} phase."
                    ),
                    outcome: RouteOutcome::NoHealthyProvider,
                },
                _ => RouteDecision {
                    provider_id: Some(fallback.to_owned()),
                    reason: format!(
                        "'{primary}' is unavailable, so the {phase} phase is falling back to '{fallback}'."
                    ),
                    outcome: RouteOutcome::Fallback,
                },
            },
            None => RouteDecision {
                provider_id: None,
                reason: format!(
                    "'{primary}' is unavailable for the {phase} phase and no fallback provider is configured."
                ),
                outcome: RouteOutcome::NoHealthyProvider,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(phase: Phase, provider: &str, fallback: Option<&str>) -> RoutingPolicy {
        RoutingPolicy {
            phase,
            provider_id: provider.to_owned(),
            fallback_id: fallback.map(str::to_owned),
        }
    }

    /// A full routing table with a fallback on every phase.
    fn full_table() -> Vec<RoutingPolicy> {
        Phase::ALL
            .iter()
            .map(|&p| policy(p, "primary", Some("fallback")))
            .collect()
    }

    #[test]
    fn every_phase_with_policy_present_and_healthy_selects_primary() {
        let table = full_table();
        for phase in Phase::ALL {
            let d = evaluate(phase, &table, |_| ProviderHealth::Healthy);
            assert_eq!(d.outcome, RouteOutcome::Primary, "phase {phase}");
            assert_eq!(d.provider_id.as_deref(), Some("primary"), "phase {phase}");
            // Reason is a user-facing sentence naming the phase and provider.
            assert!(d.reason.contains(phase.as_str()), "reason: {}", d.reason);
            assert!(d.reason.contains("primary"), "reason: {}", d.reason);
            assert!(
                d.reason.ends_with('.'),
                "reason not a sentence: {}",
                d.reason
            );
        }
    }

    #[test]
    fn every_phase_with_policy_absent_yields_no_policy() {
        let empty: Vec<RoutingPolicy> = Vec::new();
        for phase in Phase::ALL {
            let d = evaluate(phase, &empty, |_| ProviderHealth::Healthy);
            assert_eq!(d.outcome, RouteOutcome::NoPolicy, "phase {phase}");
            assert!(d.provider_id.is_none());
            assert!(!d.outcome.selected_provider());
            assert!(
                d.reason.contains(phase.as_str()) && d.reason.contains("No routing policy"),
                "reason: {}",
                d.reason
            );
        }
    }

    #[test]
    fn degraded_primary_is_kept_not_failed_over() {
        let table = full_table();
        for phase in Phase::ALL {
            let d = evaluate(phase, &table, |id| {
                if id == "primary" {
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Healthy
                }
            });
            assert_eq!(d.outcome, RouteOutcome::PrimaryDegraded, "phase {phase}");
            assert_eq!(d.provider_id.as_deref(), Some("primary"));
            assert!(d.reason.contains("degraded"), "reason: {}", d.reason);
            assert!(d.reason.contains("reduced profile"), "reason: {}", d.reason);
        }
    }

    #[test]
    fn unavailable_primary_with_healthy_fallback_uses_fallback() {
        let table = full_table();
        for phase in Phase::ALL {
            let d = evaluate(phase, &table, |id| {
                if id == "primary" {
                    ProviderHealth::Unavailable
                } else {
                    ProviderHealth::Healthy
                }
            });
            assert_eq!(d.outcome, RouteOutcome::Fallback, "phase {phase}");
            assert_eq!(d.provider_id.as_deref(), Some("fallback"));
            assert!(d.reason.contains("falling back"), "reason: {}", d.reason);
            assert!(
                d.reason.contains("primary") && d.reason.contains("fallback"),
                "reason: {}",
                d.reason
            );
        }
    }

    #[test]
    fn unavailable_primary_with_degraded_fallback_still_uses_fallback() {
        let table = full_table();
        let d = evaluate(Phase::Implement, &table, |id| {
            if id == "primary" {
                ProviderHealth::Unavailable
            } else {
                ProviderHealth::Degraded
            }
        });
        assert_eq!(d.outcome, RouteOutcome::Fallback);
        assert_eq!(d.provider_id.as_deref(), Some("fallback"));
    }

    #[test]
    fn unavailable_primary_without_fallback_yields_no_healthy_provider() {
        let table: Vec<RoutingPolicy> = Phase::ALL
            .iter()
            .map(|&p| policy(p, "primary", None))
            .collect();
        for phase in Phase::ALL {
            let d = evaluate(phase, &table, |_| ProviderHealth::Unavailable);
            assert_eq!(d.outcome, RouteOutcome::NoHealthyProvider, "phase {phase}");
            assert!(d.provider_id.is_none());
            assert!(
                d.reason.contains("no fallback provider is configured"),
                "reason: {}",
                d.reason
            );
        }
    }

    #[test]
    fn unavailable_primary_and_unavailable_fallback_yields_no_healthy_provider() {
        let table = full_table();
        for phase in Phase::ALL {
            let d = evaluate(phase, &table, |_| ProviderHealth::Unavailable);
            assert_eq!(d.outcome, RouteOutcome::NoHealthyProvider, "phase {phase}");
            assert!(d.provider_id.is_none());
            assert!(
                d.reason.contains("fallback") && d.reason.contains("unavailable"),
                "reason: {}",
                d.reason
            );
        }
    }

    #[test]
    fn only_the_matching_phase_policy_is_used() {
        // Table has a spec-only rule; asking for implement must miss it.
        let table = vec![policy(Phase::Spec, "frontier", None)];
        let hit = evaluate(Phase::Spec, &table, |_| ProviderHealth::Healthy);
        assert_eq!(hit.provider_id.as_deref(), Some("frontier"));
        let miss = evaluate(Phase::Implement, &table, |_| ProviderHealth::Healthy);
        assert_eq!(miss.outcome, RouteOutcome::NoPolicy);
    }
}
