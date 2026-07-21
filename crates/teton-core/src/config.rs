//! The on-disk TOML configuration schema and its validation.
//!
//! The config file declares providers, the phase → provider routing table, and
//! privacy boundaries. It never holds a raw credential (BR-7): providers carry
//! an `auth_ref` — a reference into the OS keychain (or an `env:`/`op://`
//! reference) — and [`Config::validate`] accepts an `auth_ref` only if it matches
//! a recognized reference form (a positive scheme allowlist), rejecting anything
//! else — a raw key or a fake-scheme value — so a credential can never be
//! persisted to a plaintext config.
//!
//! Validation error messages deliberately **never echo the offending
//! credential value** — only the provider id — so a config error can be logged
//! without leaking a secret (BR-7 again).

use crate::boundary::BoundaryMatcher;
use crate::entities::{ModelProvider, PrivacyBoundary, RoutingPolicy};
use crate::mcp::{McpServerConfig, McpTransport};
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
    /// Registered MCP servers (ADR-003 / AC-9). Declared here — the main config
    /// document — so a server registers in one place alongside providers,
    /// routing, and boundaries, rather than in a separate side file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server: Vec<McpServerConfig>,
}

/// A configuration validation failure. No variant carries a credential value,
/// so these are safe to log (BR-7).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Two providers share an id.
    #[error("provider '{0}' is defined more than once; provider ids must be unique")]
    DuplicateProvider(String),

    /// An `auth_ref` is not a recognized credential *reference*. The message
    /// names only the provider and the accepted forms, never the value.
    #[error(
        "provider '{provider_id}': auth_ref is not a recognized credential reference. \
         Config files must store only a reference to the secret, never the credential itself: \
         use a keychain reference (\"keychain://<service>/<account>\" or \"keychain:{provider_id}\"), \
         an environment reference (\"env:<VAR>\"), or a 1Password reference (\"op://<vault>/<item>\"). \
         Put the secret in your OS keychain with `teton provider add` (BR-7)."
    )]
    UnrecognizedAuthRef {
        /// The provider whose `auth_ref` is not a recognized reference.
        provider_id: String,
    },

    /// A remote provider is missing its required `endpoint`.
    #[error("provider '{0}' is a remote provider and must set an `endpoint`")]
    MissingEndpoint(String),

    /// A routing rule targets the `freeform` phase. Freeform prompts are routed
    /// by heuristics (BR-5), not the phase→provider table, so such a rule can
    /// never take effect — it is rejected rather than silently ignored (M-6).
    #[error(
        "routing policy for the freeform phase can never take effect: freeform prompts are \
         routed by heuristics, not the phase→provider routing table. Remove the \
         `phase = \"freeform\"` routing entry, or run the session in structured mode to route \
         it by policy (BR-5)."
    )]
    FreeformRoutingPolicy,

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

    /// Two MCP servers share an id (AC-9). The id is the `<server>` namespace in
    /// `mcp__<server>__<tool>`, so it must be unique.
    #[error("mcp server '{0}' is defined more than once; mcp server ids must be unique")]
    DuplicateMcpServer(String),

    /// A `stdio` MCP server declares no `command` to spawn (AC-9).
    #[error("mcp server '{0}' uses the stdio transport and must set a non-empty `command`")]
    McpMissingCommand(String),

    /// An `http` MCP server declares no `endpoint` to reach (AC-9).
    #[error("mcp server '{0}' uses the http transport and must set a non-empty `endpoint`")]
    McpMissingEndpoint(String),
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
                if !is_recognized_auth_ref(auth_ref) {
                    return Err(ConfigError::UnrecognizedAuthRef {
                        provider_id: p.id.clone(),
                    });
                }
            }
            if p.kind.is_remote() && p.endpoint.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ConfigError::MissingEndpoint(p.id.clone()));
            }
        }

        for rule in &self.routing {
            // M-6: a freeform routing entry is inert — freeform prompts route by
            // heuristics, never the policy table — so reject it with an actionable
            // message rather than accepting a rule that can never fire.
            if rule.phase == Phase::Freeform {
                return Err(ConfigError::FreeformRoutingPolicy);
            }
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

        // MCP servers (AC-9): ids are the `mcp__<server>__<tool>` namespace, so
        // they must be unique; each transport must carry the field it needs to be
        // reachable (a stdio `command`, an http `endpoint`) or registration would
        // silently fail at connect time instead of at load.
        let mut mcp_ids: HashSet<&str> = HashSet::with_capacity(self.mcp_server.len());
        for server in &self.mcp_server {
            if !mcp_ids.insert(server.id.as_str()) {
                return Err(ConfigError::DuplicateMcpServer(server.id.clone()));
            }
            match &server.transport {
                McpTransport::Stdio { command, .. } => {
                    if command.trim().is_empty() {
                        return Err(ConfigError::McpMissingCommand(server.id.clone()));
                    }
                }
                McpTransport::Http { endpoint } => {
                    if endpoint.trim().is_empty() {
                        return Err(ConfigError::McpMissingEndpoint(server.id.clone()));
                    }
                }
            }
        }

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

/// Whether `value` is a recognized credential *reference* (BR-7).
///
/// A **positive scheme allowlist**: an `auth_ref` is valid only if it names one
/// of the reference forms the daemon can resolve —
///
/// - a keychain reference: `keychain://<service>/<account>` (what the CLI emits)
///   or the `keychain:<account>` shorthand,
/// - an environment reference: `env:<VAR>`, or
/// - a 1Password reference: `op://<vault>/<item>`.
///
/// Everything else is rejected: a raw `sk-...` key, a bare high-entropy token, or
/// any `scheme:value` whose scheme is not on the list (e.g. `foo:AKIA...`). This
/// replaces the old negative heuristic, which any value shorter than 40 chars or
/// containing a `:`/`/` slipped past — letting a raw key be persisted to a
/// plaintext config (REQ-544 MED-3). The reference body after the scheme must be
/// non-empty (a bare `keychain:` or `env:` is not a valid reference).
fn is_recognized_auth_ref(value: &str) -> bool {
    // `keychain:` also matches the `keychain://` form, so listing it once covers
    // both. Order does not matter — a value has at most one of these schemes.
    const RECOGNIZED_SCHEMES: &[&str] = &["keychain:", "env:", "op://"];
    let v = value.trim();
    RECOGNIZED_SCHEMES
        .iter()
        .any(|scheme| v.strip_prefix(scheme).is_some_and(|rest| !rest.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{
        BoundaryMode, ModelProvider, ProviderCapabilities, ProviderKind, ToolCallTier,
    };
    use std::collections::BTreeMap;

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
                    kind: ProviderKind::OpenaiCompatible,
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
            mcp_server: vec![
                McpServerConfig {
                    id: "fs".to_owned(),
                    transport: McpTransport::Stdio {
                        command: "mcp-server-filesystem".to_owned(),
                        args: vec!["--root".to_owned(), ".".to_owned()],
                        env: BTreeMap::from([("MCP_LOG".to_owned(), "info".to_owned())]),
                    },
                },
                McpServerConfig {
                    id: "knowledge".to_owned(),
                    transport: McpTransport::Http {
                        endpoint: "https://mcp.example.com/rpc".to_owned(),
                    },
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
            ConfigError::UnrecognizedAuthRef {
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
            // REQ-544 MED-3: the shapes the old heuristic let through —
            // a short key (<40 chars), and a `scheme:value` whose scheme is not
            // recognized (a raw key wearing a fake scheme).
            "AKIAIOSFODNN7EX",
            "foo:AKIAIOSFODNN7EXAMPLE",
            "keychain", // a scheme name with no `:` is not a reference
            "env",
            "keychain:", // a bare scheme with no body is not a reference
            "env:",
        ] {
            assert!(
                !is_recognized_auth_ref(raw),
                "should be rejected as a raw key / unrecognized reference: {raw}"
            );
        }
    }

    #[test]
    fn recognized_references_are_accepted() {
        for good in [
            "keychain://teton/anthropic", // the shape the CLI emits
            "keychain:anthropic-prod",    // shorthand
            "keychain:my-openai-key",
            "env:OPENAI_KEY",
            "op://vault/item", // 1Password
        ] {
            assert!(
                is_recognized_auth_ref(good),
                "recognized reference should be accepted: {good}"
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
    fn routing_rule_for_the_freeform_phase_is_rejected() {
        // REQ-544 M-6: a freeform routing entry can never fire (heuristics, not the
        // policy table, route freeform prompts), so validation must reject it with a
        // clear, actionable message instead of silently accepting an inert rule.
        let mut cfg = sample_config();
        cfg.routing.push(RoutingPolicy {
            phase: Phase::Freeform,
            provider_id: "deepseek".to_owned(),
            fallback_id: None,
        });
        let err = cfg.validate().unwrap_err();
        assert_eq!(err, ConfigError::FreeformRoutingPolicy);
        let msg = err.to_string();
        assert!(msg.contains("freeform"), "message: {msg}");
        assert!(
            msg.contains("heuristics") && msg.contains("structured"),
            "message should explain why and how to fix: {msg}"
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
    fn duplicate_mcp_server_id_is_rejected() {
        // AC-9: the server id is the `mcp__<server>__<tool>` namespace, so two
        // servers may not share one.
        let mut cfg = sample_config();
        cfg.mcp_server[1].id = "fs".to_owned();
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::DuplicateMcpServer("fs".to_owned())
        );
    }

    #[test]
    fn stdio_mcp_server_without_a_command_is_rejected() {
        let mut cfg = sample_config();
        cfg.mcp_server[0].transport = McpTransport::Stdio {
            command: "   ".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::McpMissingCommand("fs".to_owned())
        );
    }

    #[test]
    fn http_mcp_server_without_an_endpoint_is_rejected() {
        let mut cfg = sample_config();
        cfg.mcp_server[1].transport = McpTransport::Http {
            endpoint: String::new(),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::McpMissingEndpoint("knowledge".to_owned())
        );
    }

    #[test]
    fn load_accepts_an_mcp_server_config_from_the_main_toml() {
        // AC-9: an MCP server declared in the main config document — the
        // `[[mcp_server]]` table with a nested `[mcp_server.transport]` — parses,
        // validates, and lands in `Config::mcp_server`. This is the single-source
        // registration the daemon reads (no separate side file).
        let toml_text = r#"
[[mcp_server]]
id = "demo"

[mcp_server.transport]
kind = "stdio"
command = "sh"
args = ["mcp_server.sh"]
"#;
        let cfg = Config::load(toml_text).expect("should load and validate");
        assert_eq!(cfg.mcp_server.len(), 1);
        assert_eq!(cfg.mcp_server[0].id, "demo");
        match &cfg.mcp_server[0].transport {
            McpTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "sh");
                assert_eq!(args, &["mcp_server.sh".to_owned()]);
            }
            other => panic!("expected a stdio transport, got {other:?}"),
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
            Err(LoadError::Validate(ConfigError::UnrecognizedAuthRef { provider_id })) => {
                assert_eq!(provider_id, "anthropic-prod");
            }
            other => panic!("expected UnrecognizedAuthRef, got {other:?}"),
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
