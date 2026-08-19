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
//! - [`config_doc`] — the format-preserving delta engine: what a config change
//!   does to the *document* it lands in, so a write touches its own keys and
//!   leaves the user's comments, ordering and unknown keys byte-for-byte alone
//!   (REQ-574 BR-1). Pure text-in/text-out; the atomic write stays in `tetond`.
//! - [`endpoint_composition`] — what `provider add` persists when a user pastes
//!   a vendor's *base* URL instead of the absolute request URL Teton POSTs
//!   verbatim. The per-kind canonical request paths live here and nowhere else
//!   (REQ-578 BR-2); composition happens at the registration seam only, so
//!   every downstream consumer keeps seeing the literal request URL.
//! - [`capability`] — the one derivation of the web capability's state
//!   ([`WebCapabilityState`]) from the `[web]` table plus local-model presence.
//!   Shared by the refusal clause, the status surface, the setup flow, and the
//!   web tool's registration predicate, so those four cannot disagree about
//!   what this machine can do (REQ-572 BR-3).
//! - [`mcp`] — user-declared MCP servers (the `[[mcp_server]]` config table,
//!   ADR-003 / AC-9).
//! - [`policy`] — the shared decision vocabulary: [`ProviderHealth`] in,
//!   [`RouteOutcome`] out. Its phase-policy evaluator was deleted with its last
//!   caller when [`category::resolve`] became the one resolver (REQ-558 ADR-J).
//! - [`boundary`] — pure privacy-boundary glob matching.
//! - [`provenance_id`] — [`ProvenanceId`], the minted identity a privacy
//!   verdict keys on. Constructible only through its named constructors, so a
//!   raw `String` cannot enter the provenance channel (REQ-571 ADR-A).
//! - [`lifetime`] — the daemon's arm/disarm/defer/commit decision as a pure
//!   state machine, so the exit-on-last-client behaviour is testable without a
//!   socket, launchd, or a TTY (REQ-565 BR-9).
//! - [`session_root`] — the session root as a pure value (REQ-583 ADR-1): what
//!   kind of place a directory is, how it is spelled to a person, how a value
//!   from it is bounded before it lands in a prompt, the one project-marker
//!   table, and the `--cwd`/`/cd` argument grammar. The daemon's probe supplies
//!   the I/O; the CLI's banner links this so the two spellings cannot drift.

pub mod boundary;
pub mod capability;
pub mod category;
pub mod config;
pub mod config_doc;
pub mod effort;
pub mod endpoint_composition;
pub mod entities;
pub mod lifetime;
pub mod mcp;
pub mod phase;
pub mod policy;
pub mod provenance_id;
pub mod session_root;

pub use boundary::{match_boundary, BoundaryError, BoundaryMatcher};
// REQ-572 BR-3: re-exported at the crate root because the classifier's whole
// point is that one answer serves the prompt, the status surface, the setup
// flow and the tool registry — `teton_core::web_capability_state` is the one
// path all of them name it by.
pub use capability::{web_capability_state, SearchGap, WebCapabilityState};
// REQ-558: `TierBinding`/`CategoryOverride` live beside the resolver that reads
// them rather than in `entities`, so the type that makes a `redact` binding
// unrepresentable (ADR-B) sits next to the match arm that relies on it. They are
// re-exported here, so `teton_core::TierBinding` is the stable path either way.
pub use category::{
    categories_for_phase, category_for_phase, resolve, BindingSource, Category, CategoryOrigin,
    CategoryOverride, CategoryResolution, CategoryTable, ConfigurableCategory, JudgmentCategory,
    ParseCategoryError, ParseJudgmentCategoryError, ParseTierError, Tier, TierBinding,
};
// REQ-578: the three URL predicates are re-exported alongside the schema they
// were written for. `teton provider add` gates its registration seam on the same
// shape rule the `[web]` search endpoint is held to, and warns about the same
// cleartext condition — one spelling each, so the CLI and the validator cannot
// come to different conclusions about the same string.
pub use config::{
    is_absolute_http_url, is_cleartext_to_a_remote_host, is_recognized_auth_ref, url_host, Config,
    ConfigError, LegacyRoutingRule, LifetimeConfig, LoadError, LocalModelConfig, MigratedPhase,
    PermissionsConfig, PrivacyConfig, RoutingMigration, ShutdownPolicyKind, SkippedRule, WebConfig,
    WebTier,
};
// REQ-574: re-exported at the crate root for the same reason the config schema
// is — the daemon's one config-write body and the `/web setup` preview both
// name these, and `teton_core::apply_config_delta` is the single path to the
// only code that may edit a user's config document.
pub use config_doc::{apply_config_delta, array_element_section, table_section, DeltaError};
// REQ-559: the effort vocabulary is re-exported at the crate root so
// `teton_core::EffortLevel` is the stable path for the daemon, the adapters and
// the CLI alike — one ladder, one clamp, one resolver (BR-3, BR-9).
pub use effort::{
    default_ladder_for, default_shape_for, level_list, resolve_effort, EffortLadder, EffortLevel,
    EffortOmission, ParseEffortLevelError, ReasoningShape, ResolvedEffort, ALL_LEVELS,
};
// REQ-578: re-exported at the crate root for the same reason the config schema
// is — the CLI's registration flow, its doctor advisory and the tetond-side
// bridge test that pins these against the recipe catalog all name them, and
// `teton_core::compose_endpoint` is the one path to the only code allowed to
// decide what a provider's stored endpoint is.
pub use endpoint_composition::{
    canonical_request_path, compose_endpoint, ComposedEndpoint, ANTHROPIC_DEFAULT_ENDPOINT,
    ANTHROPIC_REQUEST_PATH, OPENAI_COMPATIBLE_REQUEST_PATH,
};
pub use entities::{
    BoundaryMode, ModelProvider, ModelSelection, PrivacyBoundary, ProviderCapabilities,
    ProviderKind, SelectionSource, ToolCallTier,
};
pub use mcp::{McpServerConfig, McpTransport};
pub use phase::Phase;
pub use policy::{ProviderHealth, RouteOutcome};
// REQ-571 ADR-A: re-exported at the crate root because the provenance channel
// spans the daemon's tools, its egress inspector, and this crate's boundary
// matcher — `teton_core::ProvenanceId` is the one path all three name it by.
pub use provenance_id::{ProvenanceError, ProvenanceId};
// REQ-583 ADR-1: re-exported at the crate root because the root's spelling is
// printed by the daemon (environment block, jail refusals) and by the CLI
// (banner, launch notice, `/cd`), and `teton_core::display_for` is the one path
// both name it by — one derivation, so the two surfaces cannot drift.
pub use session_root::{
    bounded_field, classify, display_for, middle_elide, resolve_cwd_argument, CwdArgError,
    CwdGrammarRow, CWD_ARGUMENT_GRAMMAR, CWD_GRAMMAR_HOME, CWD_GRAMMAR_SHELL_CWD,
    DISPLAY_MAX_CHARS, NAME_MAX_CHARS, PROJECT_MARKERS,
};

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
