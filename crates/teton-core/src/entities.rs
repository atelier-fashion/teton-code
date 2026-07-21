//! Core domain entities from REQ-544's System Model.
//!
//! These are pure data types with serde derives; they hold no behavior that
//! performs I/O. Credential *resolution* (turning [`ModelProvider::auth_ref`]
//! into a live secret) is `tetond`'s job — this crate only ever sees the
//! reference, never the secret itself (BR-7).

use crate::phase::Phase;
use serde::{Deserialize, Serialize};

/// The transport/vendor family of a provider. Drives which adapter is used and
/// whether an `endpoint` is required.
///
/// The variant name and `kebab-case` serde rule match
/// [`teton_protocol::ProviderKind`] exactly, so the two crates share one casing
/// and one technique — no per-variant `#[serde(rename)]` and no `OpenAi`/`Openai`
/// drift across the wire boundary (REQ-544 minor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// The on-device model tier (llama.cpp / MLX). No network endpoint.
    Local,
    /// Any OpenAI-compatible chat/completions endpoint (DeepSeek, Kimi, Ollama,
    /// vLLM, …). Registerable with no code change (BR-6). Wire form:
    /// `openai-compatible`.
    OpenaiCompatible,
    /// The Anthropic Messages API.
    Anthropic,
    /// An operator-supplied custom remote adapter.
    Custom,
}

impl ProviderKind {
    /// Whether this kind reaches off the machine and therefore requires an
    /// `endpoint` and flows through the egress choke point.
    #[must_use]
    pub fn is_remote(self) -> bool {
        !matches!(self, ProviderKind::Local)
    }
}

/// How reliably a provider follows tool-call protocol. Drives adapter
/// degradation (BR-6): weak tool-callers get a reduced harness profile rather
/// than the full agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallTier {
    /// Reliable native tool-calling — eligible for the full agent loop.
    #[default]
    Native,
    /// Weak tool-calling — routed with a reduced tool set and mandatory
    /// verification (BR-6).
    Degraded,
    /// No tool-calling support at all.
    None,
}

/// Capability profile of a provider; consulted by the router and adapter layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Tool-call reliability tier (adapter-degradation input).
    #[serde(default)]
    pub tool_call_tier: ToolCallTier,
    /// Whether the provider supports parallel tool calls in one turn.
    #[serde(default)]
    pub parallel_calls: bool,
    /// Maximum context window in tokens (`0` means "unknown / unset").
    #[serde(default)]
    pub max_context: u32,
}

/// A registered model provider (System Model: `ModelProvider`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProvider {
    /// Stable unique id, referenced by routing policies.
    pub id: String,
    /// Transport/vendor family.
    pub kind: ProviderKind,
    /// Endpoint URL; required for remote kinds, absent for `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Reference to an OS-keychain entry holding the credential. Never the raw
    /// credential itself (BR-7); config validation rejects raw-key-shaped
    /// values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
    /// Capability profile (tool-call tier, parallel support, context window).
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
}

/// One row of the phase → provider routing table (System Model:
/// `RoutingPolicy`). In structured mode this table, not per-prompt heuristics,
/// determines routing (BR-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    /// The lifecycle phase this rule applies to.
    pub phase: Phase,
    /// Primary provider id (FK → [`ModelProvider::id`]).
    pub provider_id: String,
    /// Optional fallback provider id, used when the primary errors/times out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_id: Option<String>,
}

/// Whether boundary content may leave the machine, and how (System Model:
/// `PrivacyBoundary.mode`). Default is the strict [`BoundaryMode::LocalOnly`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoundaryMode {
    /// Content never leaves the machine — the hard guarantee of BR-1.
    #[default]
    LocalOnly,
    /// Content may be sent remotely only after redaction (post-MVP; see OQ-7).
    RedactThenRemote,
}

/// A repo-relative glob marking files under a privacy rule (System Model:
/// `PrivacyBoundary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyBoundary {
    /// Repo-relative glob (e.g. `secrets/**`).
    pub path_glob: String,
    /// The privacy mode for matching files. Defaults to `local-only`.
    #[serde(default)]
    pub mode: BoundaryMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_remoteness() {
        assert!(!ProviderKind::Local.is_remote());
        assert!(ProviderKind::OpenaiCompatible.is_remote());
        assert!(ProviderKind::Anthropic.is_remote());
        assert!(ProviderKind::Custom.is_remote());
    }

    #[test]
    fn defaults_are_the_strict_and_ungated_choices() {
        assert_eq!(BoundaryMode::default(), BoundaryMode::LocalOnly);
        assert_eq!(ToolCallTier::default(), ToolCallTier::Native);
    }

    #[test]
    fn provider_kind_serializes_kebab_for_openai_compatible() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            kind: ProviderKind,
        }
        let s = toml::to_string(&Wrap {
            kind: ProviderKind::OpenaiCompatible,
        })
        .unwrap();
        assert!(s.contains("openai-compatible"), "got: {s}");
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back.kind, ProviderKind::OpenaiCompatible);
    }

    #[test]
    fn boundary_mode_serializes_kebab_case() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            mode: BoundaryMode,
        }
        let s = toml::to_string(&Wrap {
            mode: BoundaryMode::RedactThenRemote,
        })
        .unwrap();
        assert!(s.contains("redact-then-remote"), "got: {s}");
    }
}
