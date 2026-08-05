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

/// User-authored inputs for the local model tier (the `[local_model]` table).
///
/// Only *inputs* live here. Which model this machine actually installed is
/// machine state, not project config, and is persisted by the daemon as a
/// [`crate::entities::ModelSelection`] instead (REQ-547 D-4) — a repository
/// checkout should not carry another machine's install decision.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LocalModelConfig {
    /// A pinned catalog model name. When set it overrides the hardware probe's
    /// pick (REQ-544 BR-9), so the pinned model is the one the daemon *proposes*
    /// on first run.
    ///
    /// It does **not** bypass consent (REQ-547 BR-1): the user still answers the
    /// proposal before a single byte is downloaded. A pin changes *which* model is
    /// proposed, never *whether* a decision is required — so an operator who pins
    /// a large model does not get an unprompted multi-gigabyte fetch on first
    /// start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// Accept the proposed model without prompting — the unattended/CI path
    /// (REQ-547 BR-5).
    ///
    /// **Defaults to `false`**, and that default is the requirement, not an
    /// implementation detail: REQ-547 narrows REQ-544's "zero-config auto-proceed"
    /// to "one confirmation, then zero-config", so the silent download is opt-in
    /// rather than the default. Serialized unconditionally (no
    /// `skip_serializing_if`) so a written-out config states the posture rather
    /// than leaving the reader to infer it.
    #[serde(default)]
    pub auto_accept: bool,
    /// Override the catalog's download base URL — the `HF_ENDPOINT`-style key
    /// (REQ-547 BR-16) for users behind a firewall or a corporate mirror. Must be
    /// an absolute `http`/`https` URL with a host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl LocalModelConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[local_model]` table out of a config that never set one.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// Top-level configuration document.
///
/// Field order matters for TOML serialization: the scalar `pinned_local_model`
/// and the `[local_model]` table are declared before the array-of-table fields so
/// the emitted TOML is valid.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Config {
    /// **Deprecated (REQ-547):** REQ-544's top-level spelling of the local-model
    /// pin.
    ///
    /// It is no longer honoured — [`Config::validate`] now *rejects* a config that
    /// sets it (see [`ConfigError::DeprecatedLegacyPin`]) and points the user at
    /// `[local_model] pinned` instead. It is never promoted into the effective
    /// pin: silently honouring it post-REQ-547 would mean downloading a model the
    /// probe never proposed. The field is retained only so its presence can be
    /// *detected* and reported, not so it can take effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_local_model: Option<String>,
    /// Local-model tier inputs (`[local_model]`): the pin, the auto-accept
    /// opt-in, and the catalog base-URL override.
    #[serde(default, skip_serializing_if = "LocalModelConfig::is_unset")]
    pub local_model: LocalModelConfig,
    /// Registered providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ModelProvider>,
    /// The provider an unrouted turn goes to (REQ-557 BR-4).
    ///
    /// `None` is a **real absence**, not a placeholder: before REQ-557 the
    /// router picked whichever remote provider happened to be first in
    /// `providers` and, failing that, minted the literal id `"local"` — the
    /// doubled fallback that produced BUG-146. An unset default now surfaces as
    /// a nameable "no default provider configured" condition instead of a route
    /// to a provider registered nowhere (LESSON-456).
    ///
    /// A value naming an unregistered id is a **validity** error (unlike an
    /// absent [`ModelProvider::model`], which is a usability condition — see
    /// [`Config::unusable_providers`]): it names something that does not exist,
    /// rather than omitting something that does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
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

    /// `default_provider` names an id that is not registered (REQ-557 BR-6).
    /// Rejected at load rather than becoming a route that fails later, further
    /// from the cause, with the wrong name attached (LESSON-456).
    #[error(
        "default_provider names provider '{default_provider}', which is not registered. \
         Registered providers: {registered}. Set `default_provider` to one of them, or \
         register it with `teton provider add`."
    )]
    UnknownDefaultProvider {
        /// The dangling id.
        default_provider: String,
        /// Comma-separated registered ids, so the fix is readable from the error.
        registered: String,
    },

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

    /// `[local_model] pinned` is not shaped like a catalog model name. Caught at
    /// load time rather than at first-run selection, where the failure would
    /// surface as a confusing "no such model" long after the typo (REQ-547).
    #[error(
        "[local_model] pinned = \"{name}\" is not a valid catalog model name. A model name is a \
         catalog id such as \"qwen2.5-coder-3b\" — letters, digits, '.', '-' and '_' only, and \
         never a path or URL. Run `teton model list` to see the names this build ships."
    )]
    InvalidPinnedModel {
        /// The offending value (user-authored, never a credential).
        name: String,
    },

    /// The hard-deprecated top-level `pinned_local_model` key is set (REQ-547
    /// Decision 2). The pin moved into the `[local_model]` table; the old key is
    /// no longer honoured — a config that still sets it is rejected with a
    /// migration instruction rather than silently promoted (which, post-REQ-547,
    /// would mean an unprompted download the probe never proposed). Same posture
    /// as [`ConfigError::FreeformRoutingPolicy`]: reject the inert key loudly
    /// instead of ignoring it.
    #[error(
        "the top-level `pinned_local_model` key is no longer supported (it was REQ-544's \
         spelling). Move it into the local-model table: replace `pinned_local_model = \"{name}\"` \
         with a `[local_model]` section containing `pinned = \"{name}\"`."
    )]
    DeprecatedLegacyPin {
        /// The value found under the deprecated key (user-authored, never a
        /// credential).
        name: String,
    },

    /// `[local_model] base_url` is not a usable catalog base URL (BR-16).
    #[error(
        "[local_model] base_url = \"{base_url}\" is not a usable catalog base URL. It must be an \
         absolute http/https URL including a host, e.g. \"https://hf-mirror.example.com\" — the \
         HF_ENDPOINT-style override that points model downloads at a mirror (BR-16)."
    )]
    InvalidLocalModelBaseUrl {
        /// The offending value (user-authored, never a credential).
        base_url: String,
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
        self.validate_local_model()?;

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

        // REQ-557 BR-6: a `default_provider` naming an unregistered id is a
        // validity error. Note what is deliberately NOT checked here: a remote
        // provider with no `model`. That is a *usability* condition handled by
        // `unusable_providers` — see ADR-E and the note on `ModelProvider::model`.
        // Rejecting it here would refuse daemon startup, which both blocks
        // migration of a pre-REQ config and contradicts BR-7's "the daemon starts
        // with that provider unusable".
        if let Some(default_provider) = &self.default_provider {
            if !ids.contains(default_provider.as_str()) {
                let mut registered: Vec<&str> = ids.iter().copied().collect();
                registered.sort_unstable();
                return Err(ConfigError::UnknownDefaultProvider {
                    default_provider: default_provider.clone(),
                    registered: if registered.is_empty() {
                        "(none)".to_owned()
                    } else {
                        registered.join(", ")
                    },
                });
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

    /// Validates the `[local_model]` inputs (REQ-547).
    ///
    /// The pin's *shape* is what a config-time check can honestly assert: this
    /// crate holds no catalog (that is `teton-inference`), so it rejects values
    /// that could never name a catalog entry — a path, a URL, a blank string —
    /// and leaves "is there such a model?" to the daemon, which has the catalog
    /// and can list the alternatives.
    /// Remote providers that cannot serve a turn because they declare no
    /// `model` (REQ-557 BR-1, ADR-E). Returns their ids, sorted.
    ///
    /// This is the **non-fatal** half of the model requirement, and the split
    /// from [`Self::validate`] is the whole point of ADR-E. `Config::load`
    /// validates internally and the daemon converts a load error into a refusal
    /// to start; a validation-level model requirement would therefore
    ///
    /// - block a pre-REQ config (every provider `model: None`) from starting
    ///   long enough for [`Self::migrate_models`] to run, and
    /// - make a **single** unresolvable provider prevent startup entirely,
    ///   contradicting BR-7's "the daemon starts with that provider unusable".
    ///
    /// A config naming a provider we cannot yet price is not corrupt; it is
    /// incomplete in one entry. Callers report these ids and refuse to route to
    /// them — the daemon still starts and every other provider still works.
    ///
    /// Local providers are never unusable on this axis: their model is owned by
    /// the REQ-547 consent flow, not by this field.
    #[must_use]
    pub fn unusable_providers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .providers
            .iter()
            .filter(|p| p.kind.is_remote() && p.model.as_deref().unwrap_or("").trim().is_empty())
            .map(|p| p.id.clone())
            .collect();
        out.sort_unstable();
        out
    }

    /// One-shot migration of pre-REQ-557 configs: fill in each remote provider's
    /// `model` from `resolve`, and report the ones it could not resolve.
    ///
    /// `resolve` maps a provider id to the model that provider was *implicitly*
    /// serving before REQ-557 — the legacy price-table lookup `billing_model`
    /// used to perform. It is injected rather than imported so this crate stays
    /// I/O-free and carries no dependency on the daemon's price table.
    ///
    /// Returns the ids it could **not** resolve, sorted. Those keep
    /// `model: None` and are reported by the caller; the migration never
    /// guesses a value and never falls back to the provider id (REQ-557 BR-1,
    /// BR-7 — the fallback-identifier shape of LESSON-456).
    ///
    /// Idempotent: a provider that already declares a model is left untouched,
    /// so a second run is a no-op and reports nothing.
    ///
    /// **The `resolve` closure must not outlive this migration.** A live
    /// provider-id → model lookup is exactly the derivation ADR-A deletes; see
    /// LESSON-443 on guards that survive the condition they were written for.
    pub fn migrate_models<F>(&mut self, resolve: F) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut unresolved = Vec::new();
        for p in &mut self.providers {
            if !p.kind.is_remote() {
                continue;
            }
            if p.model.as_deref().unwrap_or("").trim().is_empty() {
                match resolve(&p.id).filter(|m| !m.trim().is_empty()) {
                    Some(model) => p.model = Some(model),
                    None => unresolved.push(p.id.clone()),
                }
            }
        }
        unresolved.sort_unstable();
        unresolved
    }

    fn validate_local_model(&self) -> Result<(), ConfigError> {
        // Decision 2 (REQ-547 review): the legacy top-level `pinned_local_model`
        // is hard-deprecated. Reject it before anything else touches a pin, so no
        // path can promote an unvalidated legacy value (M-7). An operator who
        // pinned under the old spelling is told to migrate rather than having the
        // key silently ignored — or, worse, silently honoured as a download the
        // probe would never have proposed.
        if let Some(legacy) = &self.pinned_local_model {
            return Err(ConfigError::DeprecatedLegacyPin {
                name: legacy.clone(),
            });
        }

        // Shape-check the effective pin (now only `[local_model] pinned`).
        if let Some(pinned) = &self.local_model.pinned {
            if !is_model_name_shaped(pinned) {
                return Err(ConfigError::InvalidPinnedModel {
                    name: pinned.clone(),
                });
            }
        }

        if let Some(base_url) = &self.local_model.base_url {
            if !is_absolute_http_url(base_url) {
                return Err(ConfigError::InvalidLocalModelBaseUrl {
                    base_url: base_url.clone(),
                });
            }
        }

        Ok(())
    }

    /// The model the user pinned, from the `[local_model] pinned` key.
    ///
    /// REQ-544's top-level `pinned_local_model` is hard-deprecated — a config that
    /// sets it fails validation (see [`ConfigError::DeprecatedLegacyPin`]), so it
    /// is *never* promoted into the effective pin. This is now simply the current
    /// key, kept as a named accessor because the daemon resolves the effective pin
    /// in one place and hands it to the probe, the consent gate, and `model/list`
    /// so they cannot disagree about which pin is in force.
    #[must_use]
    pub fn effective_pinned_local_model(&self) -> Option<&str> {
        self.local_model.pinned.as_deref()
    }
}

/// Whether `value` could name a catalog entry.
///
/// Catalog ids look like `qwen2.5-coder-3b`: ASCII alphanumerics plus `.`, `-`
/// and `_`. Rejecting everything else catches the mistakes that actually happen
/// — a filesystem path, a URL, a quoted display name, an empty string — at load
/// time instead of at first-run selection.
fn is_model_name_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Whether `value` is an absolute `http`/`https` URL with a non-empty host.
///
/// Deliberately hand-rolled rather than pulling in a URL parser: this crate is
/// the pure-logic core and the check it needs is narrow — a scheme, a host, and
/// no embedded whitespace. Full URL semantics are the download client's problem
/// (`tetond`), which parses it for real before fetching anything.
fn is_absolute_http_url(value: &str) -> bool {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !host.starts_with(':')
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

    // ---- REQ-557: model identity, default provider, usability, migration ----

    /// The pre-REQ-557 config shape. Deliberately written as raw TOML rather
    /// than built from the struct: the point is that bytes authored before this
    /// REQ still parse (ADR-B).
    const PRE_REQ_557_TOML: &str = r#"
[[providers]]
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic"

[[providers]]
id = "mystery"
kind = "openai-compatible"
endpoint = "https://api.mystery.example"
auth_ref = "keychain:mystery"
"#;

    #[test]
    fn a_pre_req_557_config_still_loads() {
        // ADR-B: if `model` were a required String this fails to DESERIALIZE,
        // and a config that cannot be opened can never be migrated.
        let cfg = Config::load(PRE_REQ_557_TOML).expect("pre-REQ-557 config must still load");
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.iter().all(|p| p.model.is_none()));
    }

    #[test]
    fn a_remote_provider_without_a_model_is_unusable_not_invalid() {
        // ADR-E: the load path refuses daemon startup on a validation error, so
        // making this a validation error would (a) block migration of a pre-REQ
        // config and (b) make ONE unresolvable provider prevent startup —
        // contradicting BR-7's "the daemon starts with that provider unusable".
        let cfg = Config::load(PRE_REQ_557_TOML).expect("must load");
        assert!(
            cfg.validate().is_ok(),
            "missing model must not be a validity error"
        );
        assert_eq!(cfg.unusable_providers(), vec!["anthropic", "mystery"]);
    }

    #[test]
    fn a_local_provider_is_never_unusable_for_lacking_a_model() {
        // The local tier's model is owned by the REQ-547 consent flow.
        let cfg = Config::load(
            r#"
[[providers]]
id = "local"
kind = "local"
"#,
        )
        .expect("must load");
        assert!(cfg.unusable_providers().is_empty());
    }

    #[test]
    fn migration_fills_what_it_can_and_reports_what_it_cannot() {
        let mut cfg = Config::load(PRE_REQ_557_TOML).expect("must load");
        let unresolved = cfg.migrate_models(|id| match id {
            "anthropic" => Some("claude-opus-5".to_owned()),
            _ => None,
        });
        assert_eq!(
            unresolved,
            vec!["mystery"],
            "must report by id, never guess"
        );
        assert_eq!(
            cfg.providers[0].model.as_deref(),
            Some("claude-opus-5"),
            "resolvable provider is migrated"
        );
        assert!(
            cfg.providers[1].model.is_none(),
            "unresolvable provider keeps None rather than falling back to its id"
        );
        // BR-7: the daemon still starts; only the unresolved provider is unusable.
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.unusable_providers(), vec!["mystery"]);
    }

    #[test]
    fn migration_is_idempotent() {
        let mut cfg = Config::load(PRE_REQ_557_TOML).expect("must load");
        let resolve = |id: &str| Some(format!("model-for-{id}"));
        let first = cfg.migrate_models(resolve);
        let snapshot = cfg.providers.clone();
        let second = cfg.migrate_models(resolve);
        assert!(first.is_empty() && second.is_empty());
        assert_eq!(cfg.providers, snapshot, "a second run must be a no-op");
    }

    #[test]
    fn migration_never_overwrites_a_declared_model() {
        let mut cfg = Config::load(PRE_REQ_557_TOML).expect("must load");
        cfg.providers[0].model = Some("declared-by-the-user".to_owned());
        let _ = cfg.migrate_models(|_| Some("from-the-price-table".to_owned()));
        assert_eq!(
            cfg.providers[0].model.as_deref(),
            Some("declared-by-the-user")
        );
    }

    #[test]
    fn two_providers_may_share_a_vendor_and_differ_only_in_model() {
        // BR-3 — the case the whole REQ exists to make expressible. Viable
        // because `auth_ref` binds to the endpoint origin, not the provider id.
        let cfg = Config::load(
            r#"
[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"

[[providers]]
id = "sonnet"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-sonnet-5"
auth_ref = "keychain:anthropic"
"#,
        )
        .expect("two providers on one vendor must validate");
        assert!(cfg.unusable_providers().is_empty());
    }

    #[test]
    fn a_dangling_default_provider_is_rejected_and_names_the_alternatives() {
        // BR-6 / AC-5: unlike a missing model, this names something that does
        // not exist, so it IS a validity error.
        let err = Config::load(
            r#"
default_provider = "ghost"

[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"
"#,
        )
        .expect_err("a dangling default_provider must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "names the dangling id: {msg}");
        assert!(msg.contains("opus"), "lists the registered ids: {msg}");
    }

    #[test]
    fn a_resolvable_default_provider_validates() {
        let cfg = Config::load(
            r#"
default_provider = "opus"

[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"
"#,
        )
        .expect("must load");
        assert_eq!(cfg.default_provider.as_deref(), Some("opus"));
    }

    fn sample_config() -> Config {
        Config {
            pinned_local_model: None,
            default_provider: Some("anthropic-prod".to_owned()),
            local_model: LocalModelConfig {
                pinned: Some("qwen2.5-coder-3b".to_owned()),
                auto_accept: false,
                base_url: Some("https://hf-mirror.example.com".to_owned()),
            },
            providers: vec![
                ModelProvider {
                    id: "local".to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    // Local: model is owned by the REQ-547 consent flow, not here.
                    model: None,
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
                    model: Some("claude-opus-5".to_owned()),
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
                    model: Some("deepseek-chat".to_owned()),
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
                    trusted: true,
                },
                McpServerConfig {
                    id: "knowledge".to_owned(),
                    transport: McpTransport::Http {
                        endpoint: "https://mcp.example.com/rpc".to_owned(),
                    },
                    trusted: false,
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

    // -----------------------------------------------------------------------
    // [local_model] (REQ-547)
    // -----------------------------------------------------------------------

    #[test]
    fn auto_accept_defaults_to_false() {
        // BR-5 is opt-in: REQ-547 narrows REQ-544's silent auto-proceed to "one
        // confirmation, then zero-config", so a config that says nothing must
        // mean "prompt me" — never "download 18 GB without asking".
        assert!(!LocalModelConfig::default().auto_accept);
        assert!(!Config::default().local_model.auto_accept);

        // Including when the table exists but omits the key.
        let cfg = Config::load("[local_model]\npinned = \"qwen2.5-coder-3b\"\n").expect("loads");
        assert!(!cfg.local_model.auto_accept);

        // And when the whole document is empty.
        assert!(!Config::load("").expect("loads").local_model.auto_accept);
    }

    #[test]
    fn local_model_section_round_trips_through_toml() {
        let cfg = Config {
            local_model: LocalModelConfig {
                pinned: Some("qwen2.5-coder-7b".to_owned()),
                auto_accept: true,
                base_url: Some("https://hf-mirror.example.com/".to_owned()),
            },
            ..Config::default()
        };
        let toml_text = cfg.to_toml().expect("serialize");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        assert!(toml_text.contains("[local_model]"), "toml: {toml_text}");
    }

    #[test]
    fn an_unset_local_model_table_is_not_written_out() {
        // A config that never mentioned the local model should not grow an empty
        // `[local_model]` table the first time it is rewritten.
        let toml_text = Config::default().to_toml().expect("serialize");
        assert!(!toml_text.contains("local_model"), "toml: {toml_text}");
    }

    #[test]
    fn load_reads_a_local_model_section() {
        let toml_text = r#"
[local_model]
pinned = "qwen2.5-coder-7b"
auto_accept = true
base_url = "https://hf-mirror.example.com"
"#;
        let cfg = Config::load(toml_text).expect("should load and validate");
        assert_eq!(cfg.local_model.pinned.as_deref(), Some("qwen2.5-coder-7b"));
        assert!(cfg.local_model.auto_accept);
        assert_eq!(
            cfg.local_model.base_url.as_deref(),
            Some("https://hf-mirror.example.com")
        );
    }

    #[test]
    fn a_pinned_model_that_is_not_a_catalog_name_is_rejected() {
        for bad in [
            "",                              // blank
            "/Users/me/models/qwen.gguf",    // a path, not a name
            "https://example.com/qwen.gguf", // a URL, not a name
            "qwen 2.5 coder",                // spaces
            "qwen2.5-coder-3b\n",            // trailing newline
            "../../etc/passwd",              // traversal-shaped
            "qwen:latest",                   // tag syntax from another tool
        ] {
            let mut cfg = sample_config();
            cfg.local_model.pinned = Some(bad.to_owned());
            let err = cfg.validate().unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidPinnedModel {
                    name: bad.to_owned()
                },
                "value: {bad:?}"
            );
        }
    }

    #[test]
    fn the_invalid_pin_message_is_actionable() {
        let mut cfg = sample_config();
        cfg.local_model.pinned = Some("/Users/me/models/qwen.gguf".to_owned());
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(msg.contains("[local_model] pinned"), "message: {msg}");
        assert!(
            msg.contains("teton model list"),
            "message should say how to find a valid name: {msg}"
        );
        assert!(
            msg.contains("qwen2.5-coder-3b"),
            "message should show the expected shape: {msg}"
        );
    }

    #[test]
    fn valid_catalog_names_are_accepted() {
        for good in [
            "qwen2.5-coder-3b",
            "qwen2.5-coder-30b-a3b",
            "Llama_3.1-8B",
            "m",
        ] {
            let mut cfg = sample_config();
            cfg.local_model.pinned = Some(good.to_owned());
            cfg.validate()
                .unwrap_or_else(|e| panic!("`{good}` should be accepted, got {e}"));
        }
    }

    #[test]
    fn the_legacy_top_level_pin_key_is_hard_deprecated() {
        // Decision 2 (REQ-547 review): a config that still sets REQ-544's top-level
        // `pinned_local_model` must FAIL validation with a migration instruction,
        // rather than being silently promoted into the effective pin — which, post
        // REQ-547, would mean an unprompted download the probe never proposed.
        let mut cfg = Config {
            pinned_local_model: Some("qwen2.5-coder-7b".to_owned()),
            ..Config::default()
        };
        let err = cfg.validate().unwrap_err();
        assert_eq!(
            err,
            ConfigError::DeprecatedLegacyPin {
                name: "qwen2.5-coder-7b".to_owned(),
            }
        );
        // The message names the migration: the old key, the new table, and the new
        // key spelled out with the same value.
        let msg = err.to_string();
        assert!(msg.contains("pinned_local_model"), "message: {msg}");
        assert!(
            msg.contains("[local_model]"),
            "message must name the new home: {msg}"
        );
        assert!(
            msg.contains("pinned = \"qwen2.5-coder-7b\""),
            "message must show the migrated key: {msg}"
        );

        // It is rejected even when it agrees with the new key — the old spelling is
        // gone, not merely superseded by a disagreeing one.
        cfg.local_model.pinned = Some("qwen2.5-coder-7b".to_owned());
        assert!(matches!(
            cfg.validate().unwrap_err(),
            ConfigError::DeprecatedLegacyPin { .. }
        ));

        // The new key alone validates cleanly.
        cfg.pinned_local_model = None;
        cfg.validate()
            .expect("the [local_model] pinned key alone is valid");
    }

    #[test]
    fn the_effective_pin_reads_only_the_current_key() {
        let mut cfg = Config::default();
        assert_eq!(cfg.effective_pinned_local_model(), None);

        // The deprecated legacy key is never promoted into the effective pin
        // (validation rejects it outright; the accessor does not resurrect it).
        cfg.pinned_local_model = Some("legacy-model".to_owned());
        assert_eq!(cfg.effective_pinned_local_model(), None);

        cfg.local_model.pinned = Some("current-model".to_owned());
        assert_eq!(cfg.effective_pinned_local_model(), Some("current-model"));
    }

    #[test]
    fn a_malformed_base_url_is_rejected() {
        for bad in [
            "hf-mirror.example.com",      // no scheme
            "ftp://mirror.example.com",   // wrong scheme
            "https://",                   // no host
            "https:///models",            // empty host
            "https://:8080/models",       // port with no host
            "file:///Users/me/models",    // not http(s)
            "https://mirror example.com", // embedded space
            "",                           // blank
        ] {
            let mut cfg = sample_config();
            cfg.local_model.base_url = Some(bad.to_owned());
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::InvalidLocalModelBaseUrl {
                    base_url: bad.to_owned()
                },
                "value: {bad:?}"
            );
        }
    }

    #[test]
    fn the_malformed_base_url_message_is_actionable() {
        let mut cfg = sample_config();
        cfg.local_model.base_url = Some("hf-mirror.example.com".to_owned());
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(msg.contains("[local_model] base_url"), "message: {msg}");
        assert!(msg.contains("BR-16"), "message should cite BR-16: {msg}");
        assert!(
            msg.contains("https://"),
            "message should show the expected form: {msg}"
        );
    }

    #[test]
    fn usable_base_urls_are_accepted() {
        for good in [
            "https://huggingface.co",
            "https://hf-mirror.example.com/",
            "http://localhost:8080",
            "https://mirror.corp.example.com/models/gguf",
            "https://10.0.0.5:8443",
        ] {
            let mut cfg = sample_config();
            cfg.local_model.base_url = Some(good.to_owned());
            cfg.validate()
                .unwrap_or_else(|e| panic!("`{good}` should be accepted, got {e}"));
        }
    }

    #[test]
    fn load_accepts_a_keychain_ref_config() {
        let toml_text = r#"
[local_model]
pinned = "qwen2.5-coder-3b"

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
        assert_eq!(cfg.local_model.pinned.as_deref(), Some("qwen2.5-coder-3b"));
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.routing[0].phase, Phase::Architect);
    }
}
