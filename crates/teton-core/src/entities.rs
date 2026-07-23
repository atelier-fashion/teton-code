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

/// Where a [`ModelSelection`] came from (System Model: `ModelSelection.source`).
///
/// Variant names and the `snake_case` rule mirror
/// [`teton_protocol::events::SelectionSource`] exactly, the same
/// no-drift-across-the-wire-boundary technique [`ProviderKind`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    /// The hardware probe's proposal, accepted as offered.
    Probe,
    /// The user chose a different catalog entry, or declined the local tier
    /// (REQ-547 BR-3/BR-4).
    UserOverride,
    /// A `[local_model] pinned` config key decided it with no prompt (BR-9).
    ConfigPin,
    /// The explicit opt-in auto-accept path took the decision unattended
    /// (REQ-547 BR-5) — the CI/unattended route.
    AutoAccept,
}

/// The recorded answer to a model proposal (System Model: `ModelSelection`).
///
/// This is **machine state, not project config** (REQ-547 D-4): "which model
/// this machine installed" is not a property of a repository, so the daemon
/// persists this record beside the weights while the user's TOML holds only the
/// *inputs* ([`crate::config::LocalModelConfig`]). Persisting it is what makes
/// BR-10's "a recorded decision is not re-litigated" a state read rather than a
/// re-prompt.
///
/// It deliberately carries **no install path**. BR-11 keeps absolute filesystem
/// paths out of every protocol payload, and this record is projected straight
/// onto the wire as `model_selection_decided`, so the path is not merely omitted
/// from the projection — there is no field to omit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    /// The chosen catalog model name; `None` exactly when the local tier was
    /// declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// How the decision was reached.
    pub source: SelectionSource,
    /// True when the user declined the local tier (BR-4): run remote-only and do
    /// not re-prompt on later starts.
    pub declined_local: bool,
    /// When the decision was recorded, as Unix epoch milliseconds. An integer
    /// rather than a formatted stamp, matching the cost ledger's
    /// `recorded_at_ms` — and keeping this crate free of a date-time dependency.
    pub decided_at_ms: u64,
}

impl ModelSelection {
    /// Records a decision to install `model_name`.
    #[must_use]
    pub fn accepted(
        model_name: impl Into<String>,
        source: SelectionSource,
        decided_at_ms: u64,
    ) -> Self {
        Self {
            model_name: Some(model_name.into()),
            source,
            declined_local: false,
            decided_at_ms,
        }
    }

    /// Records a decision to decline the local tier (BR-4).
    ///
    /// The source is always [`SelectionSource::UserOverride`]: only a user may
    /// answer a proposal (spec Permissions table), and neither a config pin nor
    /// the auto-accept path can produce a decline.
    #[must_use]
    pub fn declined(decided_at_ms: u64) -> Self {
        Self {
            model_name: None,
            source: SelectionSource::UserOverride,
            declined_local: true,
            decided_at_ms,
        }
    }

    /// Whether this decision names a model the daemon should install and load.
    ///
    /// False for a decline, so callers ask this rather than testing
    /// `model_name.is_some()` and missing the declined case.
    #[must_use]
    pub fn installs_local_model(&self) -> bool {
        !self.declined_local && self.model_name.is_some()
    }
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
    fn model_selection_records_an_acceptance() {
        let sel = ModelSelection::accepted("qwen2.5-coder-7b", SelectionSource::Probe, 1_700_000);
        assert_eq!(sel.model_name.as_deref(), Some("qwen2.5-coder-7b"));
        assert!(!sel.declined_local);
        assert!(sel.installs_local_model());
        assert_eq!(sel.decided_at_ms, 1_700_000);
    }

    #[test]
    fn model_selection_records_a_decline_with_no_model() {
        // BR-4: declining is persisted, runs remote-only, and never names a
        // model to install.
        let sel = ModelSelection::declined(1_700_001);
        assert_eq!(sel.model_name, None);
        assert!(sel.declined_local);
        assert!(!sel.installs_local_model());
        assert_eq!(sel.source, SelectionSource::UserOverride);
    }

    #[test]
    fn model_selection_round_trips_and_omits_an_absent_model_name() {
        for sel in [
            ModelSelection::accepted("qwen2.5-coder-3b", SelectionSource::ConfigPin, 1),
            ModelSelection::accepted("qwen2.5-coder-7b", SelectionSource::AutoAccept, 2),
            ModelSelection::accepted("qwen2.5-coder-3b", SelectionSource::UserOverride, 3),
            ModelSelection::declined(4),
        ] {
            let text = toml::to_string(&sel).unwrap();
            let back: ModelSelection = toml::from_str(&text).unwrap();
            assert_eq!(back, sel, "round-trip mismatch; serialized as:\n{text}");
        }
        assert!(!toml::to_string(&ModelSelection::declined(4))
            .unwrap()
            .contains("model_name"));
    }

    #[test]
    fn model_selection_carries_no_install_path() {
        // BR-11: this record is projected straight onto the wire, so an install
        // path must not exist as a field in the first place.
        let text =
            toml::to_string(&ModelSelection::accepted("m", SelectionSource::Probe, 1)).unwrap();
        for forbidden in ["path", "url", "/Users/", "/home/"] {
            assert!(!text.contains(forbidden), "leaked `{forbidden}`: {text}");
        }
    }

    #[test]
    fn selection_source_uses_the_spec_wire_names() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            source: SelectionSource,
        }
        for (source, expected) in [
            (SelectionSource::Probe, "probe"),
            (SelectionSource::UserOverride, "user_override"),
            (SelectionSource::ConfigPin, "config_pin"),
            (SelectionSource::AutoAccept, "auto_accept"),
        ] {
            let text = toml::to_string(&Wrap { source }).unwrap();
            assert!(text.contains(expected), "got: {text}");
            let back: Wrap = toml::from_str(&text).unwrap();
            assert_eq!(back.source, source);
        }
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
