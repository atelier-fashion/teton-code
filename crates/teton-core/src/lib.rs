//! teton-core — router, session state, and cost ledger.
//!
//! Pure logic only: this crate holds no I/O dependencies (no async runtime,
//! no HTTP client) so the routing and privacy-boundary logic stays trivially
//! testable and cannot itself perform egress. Everything here is data types and
//! pure functions; the daemon (`tetond`) supplies the I/O — keychain
//! resolution, network calls, the filesystem — around them.
//!
//! Module map:
//! - [`phase`] — the ADLC [`Phase`] enum (decision D-4).
//! - [`entities`] — the System Model data types (providers, policies,
//!   boundaries). Session, cost-record, and task-artifact state live in the
//!   daemon (`teton_protocol` wire types + `tetond` structured artifacts), so
//!   this crate no longer duplicates them.
//! - [`config`] — the TOML config schema and its validation, including the
//!   BR-7 no-raw-credentials rule.
//! - [`policy`] — pure routing-policy evaluation ([`policy::evaluate`]).
//! - [`boundary`] — pure privacy-boundary glob matching.

pub mod boundary;
pub mod config;
pub mod entities;
pub mod phase;
pub mod policy;

pub use boundary::{match_boundary, BoundaryError, BoundaryMatcher};
pub use config::{Config, ConfigError, LoadError};
pub use entities::{
    BoundaryMode, ModelProvider, PrivacyBoundary, ProviderCapabilities, ProviderKind,
    RoutingPolicy, ToolCallTier,
};
pub use phase::Phase;
pub use policy::{evaluate, ProviderHealth, RouteDecision, RouteOutcome};

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
