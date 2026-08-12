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
//! - [`category`] — the purpose-oriented routing [`Category`], its [`Tier`],
//!   and the pure [`resolve`] function that is the runtime dispatch key
//!   (REQ-558).
//! - [`entities`] — the System Model data types (providers, policies,
//!   boundaries). Session, cost-record, and task-artifact state live in the
//!   daemon (`teton_protocol` wire types + `tetond` structured artifacts), so
//!   this crate no longer duplicates them.
//! - [`config`] — the TOML config schema and its validation, including the
//!   BR-7 no-raw-credentials rule.
//! - [`mcp`] — user-declared MCP servers (the `[[mcp_server]]` config table,
//!   ADR-003 / AC-9).
//! - [`policy`] — the shared decision vocabulary: [`ProviderHealth`] in,
//!   [`RouteOutcome`] out. Its phase-policy evaluator was deleted with its last
//!   caller when [`category::resolve`] became the one resolver (REQ-558 ADR-J).
//! - [`boundary`] — pure privacy-boundary glob matching.
//! - [`lifetime`] — the daemon's arm/disarm/defer/commit decision as a pure
//!   state machine, so the exit-on-last-client behaviour is testable without a
//!   socket, launchd, or a TTY (REQ-565 BR-9).

pub mod boundary;
pub mod category;
pub mod config;
pub mod effort;
pub mod entities;
pub mod lifetime;
pub mod mcp;
pub mod phase;
pub mod policy;

pub use boundary::{match_boundary, BoundaryError, BoundaryMatcher};
// REQ-558: `TierBinding`/`CategoryOverride` live beside the resolver that reads
// them rather than in `entities`, so the type that makes a `redact` binding
// unrepresentable (ADR-B) sits next to the match arm that relies on it. They are
// re-exported here, so `teton_core::TierBinding` is the stable path either way.
pub use category::{
    categories_for_phase, category_for_phase, resolve, BindingSource, Category, CategoryOrigin,
    CategoryOverride, CategoryResolution, CategoryTable, ConfigurableCategory, JudgmentCategory,
    ParseCategoryError, ParseJudgmentCategoryError, ParseTierError, Tier, TierBinding,
};
pub use config::{
    Config, ConfigError, LegacyRoutingRule, LifetimeConfig, LoadError, LocalModelConfig,
    MigratedPhase, PermissionsConfig, PrivacyConfig, RoutingMigration, ShutdownPolicyKind,
    SkippedRule, WebConfig, WebTier,
};
// REQ-559: the effort vocabulary is re-exported at the crate root so
// `teton_core::EffortLevel` is the stable path for the daemon, the adapters and
// the CLI alike — one ladder, one clamp, one resolver (BR-3, BR-9).
pub use effort::{
    default_ladder_for, default_shape_for, level_list, resolve_effort, EffortLadder, EffortLevel,
    EffortOmission, ParseEffortLevelError, ReasoningShape, ResolvedEffort, ALL_LEVELS,
};
pub use entities::{
    BoundaryMode, ModelProvider, ModelSelection, PrivacyBoundary, ProviderCapabilities,
    ProviderKind, SelectionSource, ToolCallTier,
};
pub use mcp::{McpServerConfig, McpTransport};
pub use phase::Phase;
pub use policy::{ProviderHealth, RouteOutcome};

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
