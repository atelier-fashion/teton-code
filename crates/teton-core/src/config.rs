//! The on-disk TOML configuration schema and its validation.
//!
//! The config file declares providers, the phase → provider routing table, and
//! privacy boundaries. It never holds a raw credential (BR-7): providers carry
//! an `auth_ref` — a reference into the OS keychain — and [`Config::validate`]
//! rejects any `auth_ref` that looks like a raw API key or token, pointing the
//! user at keychain references instead.
//!
//! Validation error messages deliberately **never echo the offending
//! credential value** — only the provider id — so a config error can be logged
//! without leaking a secret (BR-7 again).

use crate::boundary::BoundaryMatcher;
use crate::entities::{ModelProvider, PrivacyBoundary, RoutingPolicy};
use crate::phase::Phase;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Top-level configuration document.
///
/// Field order matters for TOML serialization: the scalar `pinned_local_model`
/// is declared before the array-of-table fields so the emitted TOML is valid.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Config {
    /// User-pinned local model id; when set it overrides the hardware probe
    /// (BR-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_local_model: Option<String>,
    /// Registered providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ModelProvider>,
    /// The phase → provider routing table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<RoutingPolicy>,
    /// Privacy boundaries (repo-relative globs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<PrivacyBoundary>,
}

/// A configuration validation failure. No variant carries a credential value,
/// so these are safe to log (BR-7).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Two providers share an id.
    #[error("provider '{0}' is defined more than once; provider ids must be unique")]
    DuplicateProvider(String),

    /// An `auth_ref` looks like a raw credential rather than a keychain
    /// reference. The message names only the provider, never the value.
    #[error(
        "provider '{provider_id}': auth_ref looks like a raw API key or token. \
         Config files must store only an OS-keychain reference \
         (for example auth_ref = \"keychain:{provider_id}\"), never the credential itself. \
         Put the secret in your OS keychain and reference it here (BR-7)."
    )]
    RawKeyInAuthRef {
        /// The provider whose `auth_ref` is malformed.
        provider_id: String,
    },

    /// A remote provider is missing its required `endpoint`.
    #[error("provider '{0}' is a remote provider and must set an `endpoint`")]
    MissingEndpoint(String),

    /// A routing rule references a provider id that no provider declares.
    #[error("routing policy for the {phase} phase references unknown provider '{provider_id}'")]
    UnknownProvider {
        /// The phase whose rule dangles.
        phase: Phase,
        /// The missing provider id.
        provider_id: String,
    },

    /// A routing rule's `fallback_id` references an unknown provider.
    #[error(
        "routing policy for the {phase} phase references unknown fallback provider '{fallback_id}'"
    )]
    UnknownFallback {
        /// The phase whose fallback dangles.
        phase: Phase,
        /// The missing fallback provider id.
        fallback_id: String,
    },

    /// A privacy-boundary glob failed to compile.
    #[error("privacy boundary glob '{glob}' is not a valid pattern")]
    InvalidBoundaryGlob {
        /// The offending glob (user-authored, not a secret).
        glob: String,
    },
}

impl Config {
    /// Parse a config document from a TOML string. Does not validate — call
    /// [`Config::validate`] afterwards (or use [`Config::load`]).
    ///
    /// # Errors
    /// Returns the underlying TOML deserialization error on malformed input.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// Serialize this config back to TOML.
    ///
    /// # Errors
    /// Returns the underlying TOML serialization error (unreachable for
    /// well-formed configs).
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Parse **and** validate in one step.
    ///
    /// # Errors
    /// Returns [`LoadError::Parse`] on malformed TOML or [`LoadError::Validate`]
    /// when the document violates a schema rule (BR-7 raw keys, dangling FKs,
    /// bad globs, …).
    pub fn load(input: &str) -> Result<Self, LoadError> {
        let cfg = Self::from_toml(input).map_err(LoadError::Parse)?;
        cfg.validate().map_err(LoadError::Validate)?;
        Ok(cfg)
    }

    /// Validate cross-field invariants and the BR-7 no-raw-keys rule.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut ids: HashSet<&str> = HashSet::with_capacity(self.providers.len());
        for p in &self.providers {
            if !ids.insert(p.id.as_str()) {
                return Err(ConfigError::DuplicateProvider(p.id.clone()));
            }
            if let Some(auth_ref) = &p.auth_ref {
                if looks_like_raw_key(auth_ref) {
                    return Err(ConfigError::RawKeyInAuthRef {
                        provider_id: p.id.clone(),
                    });
                }
            }
            if p.kind.is_remote() && p.endpoint.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ConfigError::MissingEndpoint(p.id.clone()));
            }
        }

        for rule in &self.routing {
            if !ids.contains(rule.provider_id.as_str()) {
                return Err(ConfigError::UnknownProvider {
                    phase: rule.phase,
                    provider_id: rule.provider_id.clone(),
                });
            }
            if let Some(fallback) = &rule.fallback_id {
                if !ids.contains(fallback.as_str()) {
                    return Err(ConfigError::UnknownFallback {
                        phase: rule.phase,
                        fallback_id: fallback.clone(),
                    });
                }
            }
        }

        // Surface bad globs at load time rather than silently at egress.
        BoundaryMatcher::new(&self.boundaries)
            .map_err(|e| ConfigError::InvalidBoundaryGlob { glob: e.glob })?;

        Ok(())
    }
}

/// Error from [`Config::load`] — either the TOML failed to parse or the parsed
/// document failed validation.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// Malformed TOML.
    #[error("failed to parse config TOML: {0}")]
    Parse(#[source] toml::de::Error),
    /// The parsed config violates a schema rule.
    #[error(transparent)]
    Validate(#[from] ConfigError),
}

/// Heuristic: does `value` look like a raw credential rather than a keychain
/// reference?
///
/// It is intentionally conservative in favor of *rejecting* — a false positive
/// costs the user a rewording toward a keychain ref, while a false negative
/// could leak a secret into a config file (BR-7). It matches on two signals:
/// known secret prefixes, and long unbroken high-entropy tokens that carry no
/// scheme (`keychain:`) or path separator.
fn looks_like_raw_key(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }

    // 1. Well-known secret prefixes used by common providers/vendors.
    const KEY_PREFIXES: &[&str] = &[
        "sk-",         // OpenAI / DeepSeek / Kimi / many OpenAI-compatible
        "sk_",         // Stripe-style secret keys
        "sk-ant-",     // Anthropic
        "sk-proj-",    // OpenAI project keys
        "rk_",         // restricted keys
        "pk_live_",    // publishable-live
        "ghp_",        // GitHub personal access token (classic)
        "gho_",        // GitHub OAuth
        "ghs_",        // GitHub server-to-server
        "github_pat_", // GitHub fine-grained PAT
        "xoxb-",       // Slack bot token
        "xoxp-",       // Slack user token
        "xapp-",       // Slack app token
        "akia",        // AWS access key id
        "asia",        // AWS temporary access key id
        "aiza",        // Google API key
        "ya29.",       // Google OAuth access token
        "hf_",         // Hugging Face
        "gsk_",        // Groq
        "bearer ",     // an inlined Authorization header value
    ];
    let lower = v.to_ascii_lowercase();
    if KEY_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }

    // 2. A valid keychain reference is either scheme-qualified (contains ':')
    //    or a short bare identifier. Treat a long, unbroken, mixed
    //    letters-and-digits token with no scheme/path separator as a raw key.
    let has_separator = v.contains(':') || v.contains('/');
    if !has_separator && v.len() >= 40 {
        let token_charset = v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '='));
        let has_alpha = v.chars().any(|c| c.is_ascii_alphabetic());
        let has_digit = v.chars().any(|c| c.is_ascii_digit());
        if token_charset && has_alpha && has_digit {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{
        BoundaryMode, ModelProvider, ProviderCapabilities, ProviderKind, ToolCallTier,
    };

    fn sample_config() -> Config {
        Config {
            pinned_local_model: Some("qwen2.5-coder-3b".to_owned()),
            providers: vec![
                ModelProvider {
                    id: "local".to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    auth_ref: None,
                    capabilities: ProviderCapabilities {
                        tool_call_tier: ToolCallTier::Degraded,
                        parallel_calls: false,
                        max_context: 8192,
                    },
                },
                ModelProvider {
                    id: "anthropic-prod".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com".to_owned()),
                    auth_ref: Some("keychain:anthropic-prod".to_owned()),
                    capabilities: ProviderCapabilities {
                        tool_call_tier: ToolCallTier::Native,
                        parallel_calls: true,
                        max_context: 200_000,
                    },
                },
                ModelProvider {
                    id: "deepseek".to_owned(),
                    kind: ProviderKind::OpenAiCompatible,
                    endpoint: Some("https://api.deepseek.com".to_owned()),
                    auth_ref: Some("keychain:deepseek".to_owned()),
                    capabilities: ProviderCapabilities::default(),
                },
            ],
            routing: vec![
                RoutingPolicy {
                    phase: Phase::Architect,
                    provider_id: "anthropic-prod".to_owned(),
                    fallback_id: Some("deepseek".to_owned()),
                },
                RoutingPolicy {
                    phase: Phase::Implement,
                    provider_id: "deepseek".to_owned(),
                    fallback_id: Some("anthropic-prod".to_owned()),
                },
                RoutingPolicy {
                    phase: Phase::Io,
                    provider_id: "local".to_owned(),
                    fallback_id: None,
                },
            ],
            boundaries: vec![
                PrivacyBoundary {
                    path_glob: "secrets/**".to_owned(),
                    mode: BoundaryMode::LocalOnly,
                },
                PrivacyBoundary {
                    path_glob: "docs/**".to_owned(),
                    mode: BoundaryMode::RedactThenRemote,
                },
            ],
        }
    }

    #[test]
    fn config_round_trips_through_toml() {
        let cfg = sample_config();
        let toml_text = cfg.to_toml().expect("serialize");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
    }

    #[test]
    fn empty_config_round_trips() {
        let cfg = Config::default();
        let toml_text = cfg.to_toml().expect("serialize");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back);
    }

    #[test]
    fn valid_config_passes_validation() {
        sample_config()
            .validate()
            .expect("sample config should be valid");
    }

    #[test]
    fn raw_anthropic_key_in_auth_ref_is_rejected() {
        let mut cfg = sample_config();
        cfg.providers[1].auth_ref = Some("sk-ant-api03-abc123DEF456ghi789".to_owned());
        let err = cfg.validate().unwrap_err();
        assert_eq!(
            err,
            ConfigError::RawKeyInAuthRef {
                provider_id: "anthropic-prod".to_owned()
            }
        );
    }

    #[test]
    fn rejection_message_points_at_keychain_and_never_echoes_the_secret() {
        let secret = "sk-ant-api03-TOPSECRETshouldNeverLeak0000";
        let mut cfg = sample_config();
        cfg.providers[1].auth_ref = Some(secret.to_owned());
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(
            msg.contains("keychain"),
            "message should mention keychain: {msg}"
        );
        assert!(msg.contains("BR-7"), "message should cite BR-7: {msg}");
        assert!(
            !msg.contains(secret),
            "error message must never echo the raw credential: {msg}"
        );
        // Provider id is safe to include and helps the user find the problem.
        assert!(msg.contains("anthropic-prod"), "message: {msg}");
    }

    #[test]
    fn various_raw_key_shapes_are_rejected() {
        for raw in [
            "sk-1234567890abcdefghijklmnop",
            "sk-ant-api03-xyz",
            "ghp_16CharsOrMoreOfTokenMaterial123456",
            "slack-token-shaped-placeholder",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyD-EXAMPLEkeymaterial1234567890abcd",
            // Long unbroken high-entropy token, no scheme separator:
            "a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6q7r8s9t0",
        ] {
            assert!(
                looks_like_raw_key(raw),
                "should be treated as a raw key: {raw}"
            );
        }
    }

    #[test]
    fn keychain_references_are_accepted() {
        for good in [
            "keychain:anthropic-prod",
            "keychain:my-openai-key",
            "anthropic-prod", // short bare identifier
            "prod_key",       // short, has underscore
            "env:OPENAI_KEY", // scheme-qualified
            "1password://vault/item",
        ] {
            assert!(
                !looks_like_raw_key(good),
                "keychain reference should be accepted: {good}"
            );
        }
    }

    #[test]
    fn remote_provider_without_endpoint_is_rejected() {
        let mut cfg = sample_config();
        cfg.providers[1].endpoint = None;
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::MissingEndpoint("anthropic-prod".to_owned())
        );
    }

    #[test]
    fn local_provider_without_endpoint_is_fine() {
        let cfg = sample_config();
        // provider[0] is Local with endpoint None — must validate cleanly.
        assert_eq!(cfg.providers[0].kind, ProviderKind::Local);
        cfg.validate().expect("local provider needs no endpoint");
    }

    #[test]
    fn duplicate_provider_id_is_rejected() {
        let mut cfg = sample_config();
        cfg.providers[2].id = "local".to_owned();
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::DuplicateProvider("local".to_owned())
        );
    }

    #[test]
    fn routing_to_unknown_provider_is_rejected() {
        let mut cfg = sample_config();
        cfg.routing[0].provider_id = "ghost".to_owned();
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownProvider {
                phase: Phase::Architect,
                provider_id: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn routing_to_unknown_fallback_is_rejected() {
        let mut cfg = sample_config();
        cfg.routing[0].fallback_id = Some("ghost".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownFallback {
                phase: Phase::Architect,
                fallback_id: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn invalid_boundary_glob_is_rejected() {
        let mut cfg = sample_config();
        cfg.boundaries[0].path_glob = "secrets/[unterminated".to_owned();
        match cfg.validate().unwrap_err() {
            ConfigError::InvalidBoundaryGlob { glob } => {
                assert!(glob.contains("unterminated"), "glob: {glob}");
            }
            other => panic!("expected InvalidBoundaryGlob, got {other:?}"),
        }
    }

    #[test]
    fn load_parses_and_validates_a_raw_key_config() {
        // A hand-written config that inlines a raw key must fail `load`.
        let toml_text = r#"
[[providers]]
id = "anthropic-prod"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "sk-ant-api03-not-a-keychain-ref-000000"
"#;
        match Config::load(toml_text) {
            Err(LoadError::Validate(ConfigError::RawKeyInAuthRef { provider_id })) => {
                assert_eq!(provider_id, "anthropic-prod");
            }
            other => panic!("expected RawKeyInAuthRef, got {other:?}"),
        }
    }

    #[test]
    fn load_accepts_a_keychain_ref_config() {
        let toml_text = r#"
pinned_local_model = "qwen2.5-coder-3b"

[[providers]]
id = "anthropic-prod"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic-prod"

[[routing]]
phase = "architect"
provider_id = "anthropic-prod"

[[boundaries]]
path_glob = "secrets/**"
mode = "local-only"
"#;
        let cfg = Config::load(toml_text).expect("should load and validate");
        assert_eq!(cfg.pinned_local_model.as_deref(), Some("qwen2.5-coder-3b"));
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.routing[0].phase, Phase::Architect);
    }
}
