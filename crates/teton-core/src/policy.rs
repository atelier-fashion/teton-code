//! The shared vocabulary of a routing decision: provider health in, a
//! structured outcome out.
//!
//! This module used to hold `evaluate`, the pure phase → provider policy
//! evaluator. REQ-558 made the *category* the runtime dispatch key in both
//! session modes, so [`crate::category::resolve`] took over the job and
//! `evaluate` lost its last caller. It is deleted rather than left compiling,
//! together with its table-driven test suite — a dead test module still reads
//! as coverage to anyone counting tests (ADR-J).
//!
//! What survives is the part both dispatch paths always shared and the resolver
//! still needs: [`ProviderHealth`] going in and [`RouteOutcome`] coming out.
//! They stay here rather than moving into `category` because they are consumed
//! by `router.rs` and `runtime.rs` today; relocating them would touch three
//! crates to no purpose, and two vocabularies for one concept is how surfaces
//! drift (ADR-J, LESSON-456).

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

/// Which branch of resolution produced the decision. Lets callers react
/// (e.g. emit `provider_degraded`) without re-parsing the reason string.
///
/// Produced by [`crate::category::resolve`] and read by the daemon's router and
/// runtime (REQ-558 ADR-J). The variant docs still name the retired phase-policy
/// branch alongside the category one, because the cost ledger and existing
/// `route_decided` consumers read outcomes recorded under both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Primary provider selected, healthy.
    Primary,
    /// Primary provider selected but degraded (reduced harness profile).
    PrimaryDegraded,
    /// Primary unavailable or unroutable; fell back to the configured fallback.
    Fallback,
    /// Nothing is configured for this dispatch key — no policy for the phase,
    /// or no tier binding and no override for the category.
    NoPolicy,
    /// A binding exists but no provider in it is currently usable — unavailable,
    /// or not routable at all (REQ-557 ADR-E).
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
