//! The on-disk TOML configuration schema and its validation.
//!
//! The config file declares providers, the tier → provider table with its
//! per-category overrides (REQ-558), and privacy boundaries. The pre-REQ-558
//! phase → provider table (`[[routing]]`) is still *read* here so TASK-055's
//! migration has something to migrate; nothing dispatches on it.
//!
//! It never holds a raw credential (BR-7): providers carry
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
// REQ-558: `TierBinding`/`CategoryOverride` live in `category` beside the
// resolver that reads them, so the type that makes a `redact` binding
// unrepresentable (ADR-B) sits next to the match arm that relies on it. They are
// imported here rather than redeclared — one shape, one definition.
use crate::category::{
    categories_for_phase, CategoryOverride, ConfigurableCategory, JudgmentCategory, Tier,
    TierBinding,
};
use crate::entities::{ModelProvider, PrivacyBoundary};
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
    /// The category a freeform judgment turn is assigned when classification is
    /// bypassed or fails (REQ-558 BR-9).
    ///
    /// Defaults to `edit`, the coding-turn category — today's behavior is that a
    /// non-auxiliary freeform prompt is a coding turn, and the default is chosen
    /// to preserve it rather than to be neutral.
    ///
    /// Serialized **unconditionally** (no `skip_serializing_if`), for the same
    /// reason as [`LocalModelConfig::auto_accept`]: AC-12 requires the declared
    /// default be *configuration-visible* rather than a hidden constant, and a
    /// key that disappears from a written-out config whenever it holds its
    /// default is precisely the hidden constant that rules out.
    #[serde(default)]
    pub judgment_default: JudgmentCategory,
    /// **Retired (REQ-558):** the pre-REQ-558 `[[routing]]` phase → provider
    /// table, read exactly once by [`Config::migrate_routing_to_categories`].
    ///
    /// It is not a routing input and has no reader other than the migration —
    /// the name says `legacy_` precisely so that a grep answers "does anything
    /// dispatch on the phase table?" without anyone having to read for it.
    /// [`Config::tiers`] and [`Config::categories`] are the routing table now,
    /// and a category is the dispatch key in both session modes (BR-1).
    ///
    /// It stays *deserializable* for two reasons, both load-bearing:
    ///
    /// - a table that cannot be opened cannot be migrated, and
    /// - `phase` is a [`Phase`], which has no `Freeform` variant (ADR-G), so a
    ///   `[[routing]] phase = "freeform"` entry is still refused at load. Drop
    ///   the field entirely and serde would *ignore* the unknown key instead —
    ///   turning a rejection into silence, which AC-7 forbids.
    ///
    /// It stays *serializable-when-non-empty* for a third: the migration clears
    /// it only after it has read it, so any config write that happens before
    /// the migration ran preserves the user's table rather than deleting a
    /// routing choice it never migrated.
    #[serde(rename = "routing", default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_routing: Vec<LegacyRoutingRule>,
    /// The tier → provider table — the primary routing surface (REQ-558).
    ///
    /// Four rows cover all eleven categories, because every category inherits
    /// its tier's binding ([`crate::Category::tier`]). An **empty** table is a
    /// perfectly loadable config: unbound is incomplete, not corrupt (REQ-557
    /// ADR-E), and every config authored before this REQ is in exactly that
    /// state. What such a config cannot do is route — [`crate::resolve`] answers
    /// with a sentence naming the category and its unset tier (BR-8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierBinding>,
    /// Per-category overrides of the tier binding (REQ-558).
    ///
    /// `redact` and `route` are not nameable here: [`ConfigurableCategory`] has
    /// no variant for either, so the binding is unrepresentable rather than
    /// merely rejected (ADR-B). An entry naming one fails to deserialize with a
    /// message that says *pinned* (AC-4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<CategoryOverride>,
    /// Privacy boundaries (repo-relative globs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<PrivacyBoundary>,
    /// Registered MCP servers (ADR-003 / AC-9). Declared here — the main config
    /// document — so a server registers in one place alongside providers,
    /// routing, and boundaries, rather than in a separate side file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_server: Vec<McpServerConfig>,
}

/// One row of the retired `[[routing]]` phase → provider table, as it appears
/// on disk in a config authored before REQ-558.
///
/// This is a **file format**, not an entity: it exists so
/// [`Config::migrate_routing_to_categories`] can open a pre-REQ config, and
/// nothing dispatches on it. Its predecessor `RoutingPolicy` lived in
/// `entities.rs` and was exported from the crate root; that type is gone, and
/// the distinction is the point — an entity the system runs on versus a shape
/// we read once on the way to deleting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyRoutingRule {
    /// The lifecycle phase this rule applied to. Typed as [`Phase`], which has
    /// no `Freeform` variant (ADR-G) — that is what keeps a
    /// `phase = "freeform"` entry refused at load.
    pub phase: Phase,
    /// Primary provider id (FK → [`ModelProvider::id`]).
    pub provider_id: String,
    /// Optional fallback provider id, used when the primary errored or timed
    /// out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_id: Option<String>,
}

/// One migrated `[[routing]]` rule: the phase it bound, the provider it named,
/// and **every category that binding became** (BR-10, AC-7).
///
/// `categories` is the reporting surface: a user with one `implement` rule has
/// to be told it became `edit` *and* `shell`, because a knob that silently
/// splits into two is a knob whose second half the user does not know they can
/// now set differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedPhase {
    /// The phase the retired rule named.
    pub phase: Phase,
    /// The provider it routed to, carried verbatim onto each category it wrote.
    pub provider_id: String,
    /// The categories this rule was written out as, in expansion order.
    pub categories: Vec<ConfigurableCategory>,
    /// The rule's fallback, dropped because that provider cannot serve a turn.
    ///
    /// `reject_unusable_binding` refuses exactly this id from a user setting a
    /// binding over the wire, so the migration must not write it either — a
    /// migration is not a privileged author. Reported rather than dropped
    /// silently: the id is disappearing from the user's file.
    pub dropped_fallback: Option<String>,
    /// The categories this rule mapped to but did **not** write, because
    /// something already held them.
    ///
    /// This is the mirror of the one-to-many expansion and it loses information
    /// rather than adding it, so it is if anything the more important half to
    /// report. Five phases map onto four category groups: `spec` and
    /// `architect` both become `design`, so a user who routed design work and
    /// architecture work to different providers cannot any more, and has to be
    /// told which of the two survived rather than discovering it by watching
    /// where their turns go.
    pub dropped: Vec<DroppedBinding>,
}

/// A category a retired rule mapped to but could not claim, and what holds it
/// instead — either an earlier rule in the same migration (`spec` beating
/// `architect` to `design`) or an explicit `[[categories]]` row the user wrote,
/// which always wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedBinding {
    /// The contested category.
    pub category: ConfigurableCategory,
    /// The provider the binding that won names.
    pub kept_provider_id: String,
}

/// A retired rule the migration **refused to write**, because the provider it
/// names cannot serve a turn (REQ-557 ADR-E: a remote provider declaring no
/// `model`).
///
/// The rule is still consumed — it is inert either way — but nothing is written
/// in its place, and that is the point. A `[[categories]]` override never falls
/// through to its tier, so persisting a dead override does not degrade the
/// category, it *removes* it: `edit` is the BR-9 default, where every ordinary
/// freeform coding turn lands, so a dead `edit` row is every coding turn
/// failing. Writing nothing leaves the category on its tier, which is where a
/// config that never had the rule would have put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRule {
    /// The phase the retired rule named.
    pub phase: Phase,
    /// The provider it named, which cannot serve a turn.
    pub provider_id: String,
    /// The categories it would have bound, had the provider been usable — the
    /// bindings the user is losing, by name.
    pub categories: Vec<ConfigurableCategory>,
}

/// What one run of [`Config::migrate_routing_to_categories`] changed.
///
/// Returned rather than logged so the caller owns the wording and the tests can
/// assert on the content: "reported by name" (AC-7) is an acceptance criterion,
/// and a criterion whose only witness is an `eprintln!` cannot be tested.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoutingMigration {
    /// Every retired rule the run consumed **and wrote**, in table order.
    pub phases: Vec<MigratedPhase>,
    /// Every retired rule the run consumed and deliberately did not write,
    /// because its provider cannot serve a turn.
    pub skipped: Vec<SkippedRule>,
    /// The tiers materialized from [`Config::default_provider`], in tier order.
    /// Never contains [`Tier::Reflex`].
    pub default_tiers: Vec<Tier>,
    /// The provider [`Self::default_tiers`] were bound to, when any were.
    pub default_provider: Option<String>,
    /// [`Config::default_provider`], when it was screened out and therefore
    /// wrote no tiers at all.
    ///
    /// Not counted by [`Self::is_empty`]: nothing was consumed and nothing
    /// written, so there is no reason to rewrite the file — and the migration
    /// self-heals, writing the tiers on the first start after the provider
    /// declares a model.
    pub skipped_default: Option<String>,
}

impl RoutingMigration {
    /// Whether the run changed nothing — the second-start case, and the guard
    /// on whether the config file is rewritten at all.
    ///
    /// A **skipped** rule counts as a change. It wrote no binding, but it was
    /// consumed out of `legacy_routing`, so the file must be rewritten to drop
    /// the retired table — otherwise the next start finds the same rule, skips
    /// it again, and reports it again forever.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.phases.is_empty() && self.skipped.is_empty() && self.default_tiers.is_empty()
    }
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

    /// A `[[tiers]]` row binds a tier to an unregistered provider id.
    ///
    /// Same posture, and the same message shape, as
    /// [`ConfigError::UnknownDefaultProvider`] (REQ-557 BR-6): it names
    /// something that does not exist, so it is a **validity** error rather than
    /// a route that fails later, further from the cause, with the wrong name
    /// attached (LESSON-456). A tier with *no* row is not this error — that is
    /// an incomplete config, which loads.
    #[error(
        "the '{tier}' tier is bound to provider '{provider_id}', which is not registered. \
         Registered providers: {registered}. Bind the tier to one of them with \
         `teton policy set-tier {tier} <provider>`, or register it with `teton provider add`."
    )]
    UnknownTierProvider {
        /// The tier whose binding dangles.
        tier: Tier,
        /// The missing provider id.
        provider_id: String,
        /// Comma-separated registered ids, so the fix is readable from the error.
        registered: String,
    },

    /// A `[[tiers]]` row names an unregistered fallback provider id.
    #[error(
        "the '{tier}' tier names fallback provider '{fallback_id}', which is not registered. \
         Registered providers: {registered}. Name one of them as the fallback, or register it \
         with `teton provider add`."
    )]
    UnknownTierFallback {
        /// The tier whose fallback dangles.
        tier: Tier,
        /// The missing fallback provider id.
        fallback_id: String,
        /// Comma-separated registered ids.
        registered: String,
    },

    /// A `[[categories]]` override binds a category to an unregistered provider.
    #[error(
        "the '{category}' category override names provider '{provider_id}', which is not \
         registered. Registered providers: {registered}. Bind the category to one of them with \
         `teton policy set-category {category} <provider>`, or register it with \
         `teton provider add`."
    )]
    UnknownCategoryProvider {
        /// The category whose override dangles.
        category: ConfigurableCategory,
        /// The missing provider id.
        provider_id: String,
        /// Comma-separated registered ids.
        registered: String,
    },

    /// A `[[categories]]` override names an unregistered fallback provider.
    #[error(
        "the '{category}' category override names fallback provider '{fallback_id}', which is \
         not registered. Registered providers: {registered}. Name one of them as the fallback, \
         or register it with `teton provider add`."
    )]
    UnknownCategoryFallback {
        /// The category whose fallback dangles.
        category: ConfigurableCategory,
        /// The missing fallback provider id.
        fallback_id: String,
        /// Comma-separated registered ids.
        registered: String,
    },

    /// Two `[[tiers]]` rows bind the same tier.
    ///
    /// Rejected rather than resolved first-row-wins: the second row is a user's
    /// explicit instruction, and silently honouring the first is the "the knob
    /// did nothing" defect this REQ exists to remove (BR-1). Same posture as
    /// [`ConfigError::DuplicateProvider`].
    #[error(
        "the '{0}' tier is bound more than once; a tier names exactly one provider. \
         Remove the duplicate `[[tiers]]` entry — the extra one would be silently ignored."
    )]
    DuplicateTierBinding(Tier),

    /// Two `[[categories]]` rows override the same category.
    #[error(
        "the '{0}' category is overridden more than once; a category names exactly one provider. \
         Remove the duplicate `[[categories]]` entry — the extra one would be silently ignored."
    )]
    DuplicateCategoryOverride(ConfigurableCategory),

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
    /// would mean an unprompted download the probe never proposed): reject the
    /// inert key loudly instead of ignoring it.
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
                return Err(ConfigError::UnknownDefaultProvider {
                    default_provider: default_provider.clone(),
                    registered: registered_ids(&ids),
                });
            }
        }

        self.validate_category_table(&ids)?;

        // REQ-544 M-6 rejected a `phase = "freeform"` routing rule here, loudly.
        // ADR-G retires the variant, which moves that rejection outward to
        // deserialization: serde's unknown-variant error fires before `validate`
        // ever runs, so the check could not be reached and its error variant
        // could not be constructed. The *behaviour* is unchanged and pinned by
        // `a_freeform_routing_entry_is_still_rejected_after_the_schema_change`.
        //
        // The FK checks below survive the retirement of the table they guard,
        // and deliberately: `migrate_routing_to_categories` copies each rule's
        // provider and fallback verbatim onto a `[[categories]]` row, which
        // `validate_category_table` then checks on the NEXT load. Drop these and
        // a dangling legacy rule stops being a refusal to start and becomes a
        // migration that writes a config the daemon refuses to start on —
        // the same error, one restart later, with the migration in between it
        // and the cause.
        for rule in &self.legacy_routing {
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

    /// Validates the REQ-558 tier/category table against the registered
    /// provider ids.
    ///
    /// Two things it deliberately does **not** do:
    ///
    /// - **It does not require any binding.** A config with no `[[tiers]]` rows
    ///   loads. Unbound is incomplete, not corrupt (REQ-557 ADR-E) — and since
    ///   `Config::load` failing is the daemon refusing to start, requiring a
    ///   binding here would make every pre-REQ-558 config unopenable, including
    ///   by the migration written to fix it.
    /// - **It does not screen a tier's provider for being remote.** A user may
    ///   bind `reflex` to a remote provider and get a remote `title`; what they
    ///   cannot get is a remote `redact` or `route`, and that is enforced where
    ///   the decision is made — in [`crate::resolve`], by a type with no variant
    ///   for either (BR-4, BR-5, ADR-B) — not by a check here that a second
    ///   config path could bypass.
    fn validate_category_table(&self, ids: &HashSet<&str>) -> Result<(), ConfigError> {
        let mut bound: HashSet<Tier> = HashSet::with_capacity(self.tiers.len());
        for binding in &self.tiers {
            if !bound.insert(binding.tier) {
                return Err(ConfigError::DuplicateTierBinding(binding.tier));
            }
            if !ids.contains(binding.provider_id.as_str()) {
                return Err(ConfigError::UnknownTierProvider {
                    tier: binding.tier,
                    provider_id: binding.provider_id.clone(),
                    registered: registered_ids(ids),
                });
            }
            if let Some(fallback) = &binding.fallback_id {
                if !ids.contains(fallback.as_str()) {
                    return Err(ConfigError::UnknownTierFallback {
                        tier: binding.tier,
                        fallback_id: fallback.clone(),
                        registered: registered_ids(ids),
                    });
                }
            }
        }

        let mut overridden: HashSet<ConfigurableCategory> =
            HashSet::with_capacity(self.categories.len());
        for over in &self.categories {
            if !overridden.insert(over.name) {
                return Err(ConfigError::DuplicateCategoryOverride(over.name));
            }
            if !ids.contains(over.provider_id.as_str()) {
                return Err(ConfigError::UnknownCategoryProvider {
                    category: over.name,
                    provider_id: over.provider_id.clone(),
                    registered: registered_ids(ids),
                });
            }
            if let Some(fallback) = &over.fallback_id {
                if !ids.contains(fallback.as_str()) {
                    return Err(ConfigError::UnknownCategoryFallback {
                        category: over.name,
                        fallback_id: fallback.clone(),
                        registered: registered_ids(ids),
                    });
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
            .filter(|p| p.is_unusable_for_lacking_a_model())
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
            if p.declared_model().is_none() {
                match resolve(&p.id).filter(|m| !m.trim().is_empty()) {
                    Some(model) => p.model = Some(model),
                    None => unresolved.push(p.id.clone()),
                }
            }
        }
        unresolved.sort_unstable();
        unresolved
    }

    /// One-shot migration of a pre-REQ-558 config: turn the retired
    /// `[[routing]]` phase table into `[[categories]]` overrides, and write the
    /// `default_provider` fill down as real `[[tiers]]` rows (BR-10, AC-7).
    ///
    /// Returns what changed, so the caller can report each one-to-many
    /// expansion **by name** and persist only when there is something to
    /// persist.
    ///
    /// # The phase table becomes category overrides, not tier bindings
    ///
    /// A retired rule said "every turn in phase *p* goes to provider *x*". The
    /// faithful translation is one `[[categories]]` row per category the phase
    /// maps to, because that is the only form that preserves what the rule
    /// said. Tier bindings cannot: `spec`/`architect` and `review` both land on
    /// `think`, so a config that routed design to one vendor and critique to
    /// another would lose one of them to whichever rule was written last —
    /// silently, and in the user's favour exactly half the time.
    ///
    /// The expansion comes from [`categories_for_phase`], which is the *same*
    /// function structured dispatch uses (ADR-F). BR-10's mapping table and
    /// ADR-C's dispatch mapping are one piece of knowledge; written twice they
    /// drift, and the drift is invisible because one runs at config load and the
    /// other on every structured turn.
    ///
    /// # It never overwrites an explicit new-world binding
    ///
    /// A category that already has a `[[categories]]` row keeps it. The rule is
    /// consumed either way — it is inert, and leaving it on disk would mean
    /// re-reading it forever — but a legacy row never wins over something the
    /// user wrote in the current vocabulary.
    ///
    /// # `default_provider` materializes into `build` and `think`
    ///
    /// `Router::effective_table` already fills an unbound turn tier from
    /// `default_provider`, so an upgraded config routes; that fill is invisible
    /// and uneditable. Writing it down makes it both.
    ///
    /// **`reflex` and `scan` are deliberately excluded** —
    /// [`Tier::inherits_default_provider`] is asked rather than the list being
    /// re-spelled here. Both were local before this REQ and stay local until
    /// the user binds them: `reflex` by definition, `scan` because its only
    /// reached category is `digest`, which summarizes tool output and ran on
    /// the local engine unconditionally. Persisting either as
    /// `<remote default>` would write the change that predicate exists to
    /// prevent into the user's own file, where a later reader would take it for
    /// their choice — and for `scan` it would begin sending file contents and
    /// build logs to a vendor API on the first start after upgrade, off a key
    /// the user set for their turns.
    ///
    /// An explicit `[[routing]] phase = "io"` rule is a different matter and
    /// still migrates to its provider: that is an intent the user expressed,
    /// and the migration honours it (loudly — see `digest_egress_notice`). Only
    /// the *unbound* case stays local.
    ///
    /// This leg is gated on the tier table being **entirely empty**, which is
    /// what a config that predates the table looks like. A user who has bound
    /// even one tier has engaged with the new schema, and the migration does not
    /// argue with them.
    ///
    /// # Idempotence
    ///
    /// Keyed on the absence of the old state and the presence of the new: the
    /// consumed rules are gone from `legacy_routing` (and therefore from the
    /// file the caller writes), and the three tiers are bound. A second start
    /// finds nothing, changes nothing, and reports nothing.
    pub fn migrate_routing_to_categories(&mut self) -> RoutingMigration {
        let mut report = RoutingMigration::default();

        // `take` is the consumption: a rule that has been read is gone, whether
        // or not it wrote anything, so the caller's write drops the retired
        // table from disk and the next start has nothing to find.
        for rule in std::mem::take(&mut self.legacy_routing) {
            // The rule's categories, computed once: they are what gets written
            // when the provider can serve, and what gets *reported as lost* when
            // it cannot.
            let names: Vec<ConfigurableCategory> = categories_for_phase(rule.phase)
                .iter()
                // Total by construction: no phase maps to `route` or `redact`,
                // the two categories config cannot name (ADR-B). Filtering
                // rather than asserting keeps the migration total if that ever
                // changes — a config it cannot express is not one it should
                // refuse to open.
                .filter_map(|c| c.configurable())
                .collect();

            // A binding naming a provider that cannot serve is not a weaker
            // binding, it is a *hole*: an override never falls through to its
            // tier, so writing one removes the category from routing entirely.
            // `edit` is the BR-9 freeform default, so a dead `edit` row is every
            // ordinary coding turn failing — on a config that worked yesterday.
            // The rule is consumed either way; nothing is written in its place,
            // which leaves each category on its tier exactly as if the rule had
            // never existed.
            if !self.provider_can_serve(&rule.provider_id) {
                report.skipped.push(SkippedRule {
                    phase: rule.phase,
                    provider_id: rule.provider_id,
                    categories: names,
                });
                continue;
            }

            // `reject_unusable_binding` refuses a fallback the same way it
            // refuses a primary, and a migration is not a privileged author. An
            // unusable fallback is inert at resolution anyway (`resolve`
            // screens it), so dropping it costs nothing and keeps a dead id out
            // of the user's file.
            let (fallback_id, dropped_fallback) = match rule.fallback_id {
                Some(id) if !self.provider_can_serve(&id) => (None, Some(id)),
                other => (other, None),
            };

            let mut written = Vec::new();
            let mut dropped = Vec::new();
            for name in names {
                // First claim wins, and "first" is the user's own table order.
                // The claimant may be an explicit `[[categories]]` row (which
                // must never lose to a retired one) or an earlier rule in this
                // same run. Either way the loser is reported, not swallowed.
                if let Some(held) = self.categories.iter().find(|over| over.name == name) {
                    dropped.push(DroppedBinding {
                        category: name,
                        kept_provider_id: held.provider_id.clone(),
                    });
                    continue;
                }
                self.categories.push(CategoryOverride {
                    name,
                    provider_id: rule.provider_id.clone(),
                    fallback_id: fallback_id.clone(),
                });
                written.push(name);
            }
            report.phases.push(MigratedPhase {
                phase: rule.phase,
                provider_id: rule.provider_id,
                categories: written,
                dropped_fallback,
                dropped,
            });
        }

        if self.tiers.is_empty() {
            if let Some(default_provider) = self.default_provider.clone() {
                // The same screen, for the same reason. A tier bound to a
                // provider that cannot serve is worse than an unbound one:
                // `Router::effective_table` fills an *unbound* tier from the
                // local model and keeps the machine routing, and writing the
                // dead id down replaces that fill with a hole. Leaving the
                // tiers unwritten costs nothing — the fill still applies — and
                // the migration self-heals, writing them on the first start
                // after the provider declares a model.
                if self.provider_can_serve(&default_provider) {
                    for tier in Tier::ALL
                        .into_iter()
                        .filter(|t| t.inherits_default_provider())
                    {
                        self.tiers.push(TierBinding {
                            tier,
                            provider_id: default_provider.clone(),
                            fallback_id: None,
                        });
                        report.default_tiers.push(tier);
                    }
                    if !report.default_tiers.is_empty() {
                        report.default_provider = Some(default_provider);
                    }
                } else {
                    report.skipped_default = Some(default_provider);
                }
            }
        }

        report
    }

    /// Whether a binding naming `provider_id` could actually serve a turn — the
    /// migration's copy of the screen [`crate::ModelProvider`] owns and
    /// `reject_unusable_binding` applies to a user's own `config/set`.
    ///
    /// An **unregistered** id answers `true` here, deliberately. That is not the
    /// migration's condition to report: [`Self::validate`] already rejects a
    /// retired rule or a `default_provider` naming an unknown provider, names
    /// it, and lists what is registered — and it runs before this does. Two
    /// sentences for one condition is how they drift.
    fn provider_can_serve(&self, provider_id: &str) -> bool {
        self.providers
            .iter()
            .find(|p| p.id == provider_id)
            .is_none_or(|p| !p.is_unusable_for_lacking_a_model())
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

/// The registered provider ids, sorted and comma-separated — the "and here is
/// what you could have named instead" half of a dangling-reference message
/// (REQ-557 BR-6).
///
/// `(none)` rather than an empty string when nothing is registered: a message
/// that trails off after "Registered providers:" reads like a formatting bug,
/// and "nothing is registered" is the most useful thing the error could say.
fn registered_ids(ids: &HashSet<&str>) -> String {
    let mut registered: Vec<&str> = ids.iter().copied().collect();
    registered.sort_unstable();
    if registered.is_empty() {
        "(none)".to_owned()
    } else {
        registered.join(", ")
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

    /// BUG-155 (mutation check): deleting `migrate_models`' `!p.kind.is_remote()`
    /// guard left the entire workspace suite green, because no migration fixture
    /// contained a local provider.
    ///
    /// What the guard prevents: `teton provider add on-device --kind local`
    /// produces a `[[providers]] kind = "local"` entry with no model, which is
    /// its normal state — REQ-547's consent flow owns the local model selection.
    /// Without the guard the migration resolves that entry against the price
    /// table by id and writes the answer into the config **permanently**,
    /// creating a second source of truth for a fact the consent flow owns. That
    /// is the drift OQ-4 deferred precisely to avoid.
    #[test]
    fn migration_leaves_a_local_provider_alone() {
        let mut cfg = Config {
            providers: vec![
                ModelProvider {
                    id: "local".to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    model: None,
                    auth_ref: None,
                    capabilities: ProviderCapabilities::default(),
                },
                ModelProvider {
                    id: "anthropic".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com/v1/messages".to_owned()),
                    model: None,
                    auth_ref: Some("keychain:anthropic".to_owned()),
                    capabilities: ProviderCapabilities::default(),
                },
            ],
            ..Config::default()
        };

        // A resolver that would happily answer for ANY id, including "local" —
        // the guard, not the resolver, is what protects the local entry.
        let unresolved = cfg.migrate_models(|id| Some(format!("model-for-{id}")));

        assert!(unresolved.is_empty());
        assert_eq!(
            cfg.providers[0].model, None,
            "a local provider's model is owned by the REQ-547 consent flow; the \
             migration must never write one (OQ-4)"
        );
        assert_eq!(
            cfg.providers[1].model.as_deref(),
            Some("model-for-anthropic"),
            "the remote provider alongside it still migrates"
        );
    }

    /// BUG-155: `declared_model()` is the single definition of "declares a
    /// model", and blank counts as absent everywhere. Before it, this predicate
    /// was written out three times and `build_router`'s copy disagreed.
    #[test]
    fn a_blank_model_counts_as_no_model_everywhere() {
        for blank in ["", "   ", "\t"] {
            let cfg = Config {
                providers: vec![ModelProvider {
                    id: "p".to_owned(),
                    kind: ProviderKind::OpenaiCompatible,
                    endpoint: Some("https://api.example.com/v1".to_owned()),
                    model: Some(blank.to_owned()),
                    auth_ref: None,
                    capabilities: ProviderCapabilities::default(),
                }],
                ..Config::default()
            };
            assert_eq!(cfg.providers[0].declared_model(), None, "{blank:?}");
            assert!(
                cfg.providers[0].is_unusable_for_lacking_a_model(),
                "{blank:?}"
            );
            assert_eq!(cfg.unusable_providers(), vec!["p"], "{blank:?}");
        }
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
            judgment_default: JudgmentCategory::Edit,
            tiers: vec![
                TierBinding {
                    tier: Tier::Reflex,
                    provider_id: "local".to_owned(),
                    fallback_id: None,
                },
                TierBinding {
                    tier: Tier::Scan,
                    provider_id: "local".to_owned(),
                    fallback_id: Some("deepseek".to_owned()),
                },
                TierBinding {
                    tier: Tier::Build,
                    provider_id: "deepseek".to_owned(),
                    fallback_id: Some("anthropic-prod".to_owned()),
                },
                TierBinding {
                    tier: Tier::Think,
                    provider_id: "anthropic-prod".to_owned(),
                    fallback_id: Some("deepseek".to_owned()),
                },
            ],
            // The case the per-category override exists for: a different vendor
            // reviews the code than the one that wrote it.
            categories: vec![CategoryOverride {
                name: ConfigurableCategory::Review,
                provider_id: "deepseek".to_owned(),
                fallback_id: None,
            }],
            legacy_routing: vec![
                LegacyRoutingRule {
                    phase: Phase::Architect,
                    provider_id: "anthropic-prod".to_owned(),
                    fallback_id: Some("deepseek".to_owned()),
                },
                LegacyRoutingRule {
                    phase: Phase::Implement,
                    provider_id: "deepseek".to_owned(),
                    fallback_id: Some("anthropic-prod".to_owned()),
                },
                LegacyRoutingRule {
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
        cfg.legacy_routing[0].provider_id = "ghost".to_owned();
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
        cfg.legacy_routing[0].fallback_id = Some("ghost".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownFallback {
                phase: Phase::Architect,
                fallback_id: "ghost".to_owned(),
            }
        );
    }

    // NOTE: REQ-544 M-6's `routing_rule_for_the_freeform_phase_is_rejected` is
    // gone with `ConfigError::FreeformRoutingPolicy` (ADR-G) — it built its
    // input by constructing `RoutingPolicy { phase: Phase::Freeform }`, which no
    // longer exists. The behaviour it pinned did not go with it: see
    // `a_freeform_routing_entry_is_still_rejected_after_the_schema_change`,
    // which drives the same config through `Config::load` as text.

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

    // -----------------------------------------------------------------------
    // [[tiers]] / [[categories]] / judgment_default (REQ-558)
    // -----------------------------------------------------------------------

    /// The shape a user authors: four tiers, one override, one declared default.
    const TIER_TABLE_TOML: &str = r#"
judgment_default = "design"

[[providers]]
id = "on-device"
kind = "local"

[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"

[[tiers]]
tier = "reflex"
provider_id = "on-device"

[[tiers]]
tier = "think"
provider_id = "opus"
fallback_id = "on-device"

[[categories]]
name = "review"
provider_id = "opus"
"#;

    #[test]
    fn load_reads_a_tier_and_category_table() {
        let cfg = Config::load(TIER_TABLE_TOML).expect("should load and validate");
        assert_eq!(cfg.tiers.len(), 2);
        assert_eq!(cfg.tiers[0].tier, Tier::Reflex);
        assert_eq!(cfg.tiers[0].provider_id, "on-device");
        assert_eq!(cfg.tiers[0].fallback_id, None);
        assert_eq!(cfg.tiers[1].tier, Tier::Think);
        assert_eq!(cfg.tiers[1].fallback_id.as_deref(), Some("on-device"));
        assert_eq!(cfg.categories.len(), 1);
        assert_eq!(cfg.categories[0].name, ConfigurableCategory::Review);
        assert_eq!(cfg.categories[0].provider_id, "opus");
        assert_eq!(cfg.judgment_default, JudgmentCategory::Design);
    }

    #[test]
    fn the_tier_and_category_tables_round_trip_through_toml() {
        // The REQ-557 round-trip precedent, extended to the new tables: what the
        // daemon writes back must be what a user could have authored.
        let cfg = Config::load(TIER_TABLE_TOML).expect("must load");
        let toml_text = cfg.to_toml().expect("serialize");
        assert!(toml_text.contains("[[tiers]]"), "toml: {toml_text}");
        assert!(toml_text.contains("[[categories]]"), "toml: {toml_text}");
        assert!(toml_text.contains("tier = \"think\""), "toml: {toml_text}");
        assert!(toml_text.contains("name = \"review\""), "toml: {toml_text}");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        Config::load(&toml_text).expect("a re-serialized config must still validate");
    }

    #[test]
    fn a_categories_entry_naming_a_pinned_category_says_pinned_not_misspelled() {
        // AC-4. The *rejection* is free — `ConfigurableCategory` has no variant
        // for either, so the binding is unrepresentable (ADR-B). What this test
        // guards is the **message**: serde's bare "unknown variant `redact`"
        // names the key but reads like a typo, and a user who deletes and
        // retypes it learns nothing.
        for pinned in ["redact", "route"] {
            let toml_text = format!(
                r#"
[[providers]]
id = "on-device"
kind = "local"

[[categories]]
name = "{pinned}"
provider_id = "on-device"
"#
            );
            let err = Config::load(&toml_text)
                .expect_err("a binding for a pinned category must be rejected at load");
            assert!(
                matches!(err, LoadError::Parse(_)),
                "{pinned}: expected a parse rejection, got {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains(pinned), "{pinned}: {msg}");
            assert!(
                msg.contains("pinned"),
                "{pinned}: the message must say the key is forbidden, not \
                 misspelled: {msg}"
            );
            assert!(
                !msg.contains("unknown variant"),
                "{pinned}: serde's typo-shaped message leaked through: {msg}"
            );
        }
    }

    #[test]
    fn a_categories_entry_naming_nothing_at_all_still_reads_as_a_typo() {
        // The other half of the same message: a real misspelling must NOT be
        // reported as a pin, and the accepted list must not advertise a
        // category that cannot be bound.
        let err = Config::load(
            r#"
[[categories]]
name = "redct"
provider_id = "on-device"
"#,
        )
        .expect_err("an unknown category must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("redct"), "{msg}");
        assert!(
            msg.contains("edit"),
            "the message lists what is accepted: {msg}"
        );
        assert!(!msg.contains("redact"), "{msg}");
        assert!(!msg.contains("'route'"), "{msg}");
    }

    #[test]
    fn a_binding_naming_an_unregistered_provider_is_rejected_with_the_alternatives() {
        // REQ-557 BR-6's shape, applied to all four dangling-reference slots the
        // new table has. Asserted per slot rather than once: three of the four
        // are copies of the same six lines, which is exactly where a missing
        // check hides.
        fn assert_rejected(slot: &str, cfg: &Config) {
            let err = cfg
                .validate()
                .expect_err(&format!("{slot}: a dangling id must be rejected"));
            let msg = err.to_string();
            assert!(msg.contains("ghost"), "{slot} must name the id: {msg}");
            assert!(
                msg.contains("anthropic-prod") && msg.contains("deepseek"),
                "{slot} must list the registered ids: {msg}"
            );
        }

        let mut cfg = sample_config();
        cfg.tiers[3].provider_id = "ghost".to_owned();
        assert_rejected("tier provider", &cfg);

        let mut cfg = sample_config();
        cfg.tiers[3].fallback_id = Some("ghost".to_owned());
        assert_rejected("tier fallback", &cfg);

        let mut cfg = sample_config();
        cfg.categories[0].provider_id = "ghost".to_owned();
        assert_rejected("category provider", &cfg);

        let mut cfg = sample_config();
        cfg.categories[0].fallback_id = Some("ghost".to_owned());
        assert_rejected("category fallback", &cfg);
    }

    #[test]
    fn the_dangling_binding_error_names_the_tier_or_category_it_came_from() {
        let mut cfg = sample_config();
        cfg.tiers[3].provider_id = "ghost".to_owned();
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownTierProvider {
                tier: Tier::Think,
                provider_id: "ghost".to_owned(),
                registered: "anthropic-prod, deepseek, local".to_owned(),
            }
        );

        let mut cfg = sample_config();
        cfg.categories[0].fallback_id = Some("ghost".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownCategoryFallback {
                category: ConfigurableCategory::Review,
                fallback_id: "ghost".to_owned(),
                registered: "anthropic-prod, deepseek, local".to_owned(),
            }
        );
    }

    #[test]
    fn binding_the_same_tier_or_category_twice_is_rejected() {
        // My call, not the task's: `CategoryTable::tier_binding` resolves
        // first-row-wins, so a second row would be silently ignored — a knob
        // that does nothing, which is the defect this REQ exists to remove
        // (BR-1). Same posture as `DuplicateProvider`.
        let mut cfg = sample_config();
        cfg.tiers.push(TierBinding {
            tier: Tier::Think,
            provider_id: "deepseek".to_owned(),
            fallback_id: None,
        });
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::DuplicateTierBinding(Tier::Think)
        );

        let mut cfg = sample_config();
        cfg.categories.push(CategoryOverride {
            name: ConfigurableCategory::Review,
            provider_id: "local".to_owned(),
            fallback_id: None,
        });
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::DuplicateCategoryOverride(ConfigurableCategory::Review)
        );
    }

    #[test]
    fn a_config_with_no_tier_bindings_still_loads() {
        // REQ-557 ADR-E's posture: an empty table is incomplete, not corrupt.
        // `Config::load` failing is the daemon refusing to start, so requiring a
        // binding here would make every config authored before this REQ
        // unopenable — including by the migration written to fix it.
        let cfg = Config::load(
            r#"
[[providers]]
id = "on-device"
kind = "local"
"#,
        )
        .expect("a config that binds no tier must still load");
        assert!(cfg.tiers.is_empty() && cfg.categories.is_empty());
        assert!(Config::load("")
            .expect("an empty document loads")
            .tiers
            .is_empty());
    }

    #[test]
    fn a_pre_req_558_config_carrying_the_phase_routing_table_still_loads() {
        // The `[[routing]]` table stays *readable* so the migration can open it:
        // a table that cannot be opened cannot be migrated.
        let cfg = Config::load(
            r#"
[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"

[[routing]]
phase = "architect"
provider_id = "opus"
"#,
        )
        .expect("a pre-REQ-558 config must still load");
        assert_eq!(cfg.legacy_routing.len(), 1);
        assert_eq!(cfg.legacy_routing[0].phase, Phase::Architect);
        assert!(cfg.tiers.is_empty(), "and it binds no tier yet");
    }

    #[test]
    fn a_freeform_routing_entry_is_still_rejected_after_the_schema_change() {
        // The architecture's "Corrections to the Requirement": BR-10's "drop the
        // freeform entry" describes a config that has never loaded, so the
        // migration has nothing to drop.
        //
        // ADR-G moved the *mechanism* of that rejection without changing the
        // *behaviour*: it used to be `ConfigError::FreeformRoutingPolicy` raised
        // by `Config::validate`; now `Phase` has no `Freeform` variant, so serde
        // refuses the value one layer earlier and validation never runs. This
        // test exists to keep that promise pinned to the observable outcome —
        // this config does not load — rather than to whichever layer says no.
        let err = Config::load(
            r#"
[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
auth_ref = "keychain:anthropic"

[[routing]]
phase = "freeform"
provider_id = "opus"

[[tiers]]
tier = "think"
provider_id = "opus"
"#,
        )
        .expect_err("a freeform routing entry must still be rejected");
        assert!(
            matches!(err, LoadError::Parse(_)),
            "the rejection is now a parse error, not a validation error: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("freeform"),
            "the message must still name the offending value: {msg}"
        );
    }

    // ---- REQ-558 BR-10 / AC-7: the phase table becomes categories ----------

    /// AC-7's fixture: a config with a rule for every phase that can appear.
    ///
    /// **Five, not six.** The architecture's "Corrections to the Requirement":
    /// a `[[routing]]` rule targeting `freeform` has never loaded, so the
    /// migration has nothing to drop — see
    /// `a_freeform_routing_entry_is_still_rejected_after_the_schema_change`,
    /// which drives the sixth entry through `Config::load` and asserts it is
    /// still refused.
    const PRE_REQ_558_TOML: &str = r#"
default_provider = "opus"

[[providers]]
id = "on-device"
kind = "local"

[[providers]]
id = "opus"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"

[[providers]]
id = "cheap"
kind = "openai-compatible"
endpoint = "https://api.deepseek.com"
model = "deepseek-chat"
auth_ref = "keychain:cheap"

[[routing]]
phase = "spec"
provider_id = "opus"

[[routing]]
phase = "architect"
provider_id = "opus"
fallback_id = "cheap"

[[routing]]
phase = "implement"
provider_id = "cheap"
fallback_id = "opus"

[[routing]]
phase = "review"
provider_id = "opus"

[[routing]]
phase = "io"
provider_id = "on-device"
"#;

    /// The provider a category resolves to after migration, or `None` when the
    /// migration wrote no override for it.
    fn override_for(cfg: &Config, name: ConfigurableCategory) -> Option<&str> {
        cfg.categories
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.provider_id.as_str())
    }

    #[test]
    fn the_migration_applies_the_documented_mapping_for_all_five_phases() {
        // BR-10's table, end to end: spec + architect → design, implement →
        // {edit, shell}, review → review, io → {digest, triage, title, compact}.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        let report = cfg.migrate_routing_to_categories();

        assert_eq!(report.phases.len(), 5, "one report entry per retired rule");
        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Design),
            Some("opus"),
            "spec and architect both bind `design`"
        );
        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Edit),
            Some("cheap")
        );
        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Shell),
            Some("cheap"),
            "`implement` is one knob that became two — `shell` must carry the \
             same provider, or half the phase silently stops being routed"
        );
        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Review),
            Some("opus")
        );
        for io in [
            ConfigurableCategory::Digest,
            ConfigurableCategory::Triage,
            ConfigurableCategory::Title,
            ConfigurableCategory::Compact,
        ] {
            assert_eq!(
                override_for(&cfg, io),
                Some("on-device"),
                "the `io` rule must reach `{io}`"
            );
        }
        // `debug` was reachable from no phase, so nothing claims to know where
        // the user wanted it. It falls to its tier rather than being invented.
        assert_eq!(override_for(&cfg, ConfigurableCategory::Debug), None);

        // And the result is a config the daemon can start on next time — a
        // migration that writes something `validate` rejects turns a refusal to
        // start into a refusal to start one restart later.
        cfg.validate()
            .expect("a migrated config must still validate");
    }

    #[test]
    fn the_migration_expands_through_the_same_map_structured_dispatch_uses() {
        // ADR-F: BR-10's migration table and ADR-C's dispatch map are one piece
        // of knowledge. This is the test that keeps them one — re-encode the
        // expansion here (or in the migration) and it goes red, which is the
        // only way the drift becomes visible: one is exercised at config load,
        // the other on every structured turn.
        for phase in Phase::ALL {
            let mut cfg = Config {
                providers: vec![ModelProvider {
                    id: "opus".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com".to_owned()),
                    model: Some("claude-opus-5".to_owned()),
                    auth_ref: Some("keychain:anthropic".to_owned()),
                    capabilities: ProviderCapabilities::default(),
                }],
                legacy_routing: vec![LegacyRoutingRule {
                    phase,
                    provider_id: "opus".to_owned(),
                    fallback_id: None,
                }],
                ..Config::default()
            };
            let report = cfg.migrate_routing_to_categories();

            let expected: Vec<ConfigurableCategory> = categories_for_phase(phase)
                .iter()
                .filter_map(|c| c.configurable())
                .collect();
            assert_eq!(
                report.phases[0].categories, expected,
                "the `{phase}` migration must expand through `categories_for_phase`"
            );
            assert_eq!(
                cfg.categories.iter().map(|o| o.name).collect::<Vec<_>>(),
                expected,
                "and must write exactly those rows"
            );

            // The dispatch half of the same fact: whatever a structured turn in
            // this phase dispatches on has to be one of the categories the
            // migration bound, or an upgraded config stops routing the phase it
            // was configured for.
            let dispatched = crate::category::category_for_phase(phase)
                .configurable()
                .expect("no phase dispatches on a pinned category");
            assert!(
                expected.contains(&dispatched),
                "a structured `{phase}` turn dispatches on `{dispatched}`, which the \
                 migration did not bind: {expected:?}"
            );
        }
    }

    #[test]
    fn each_rules_fallback_travels_with_it_onto_every_category() {
        // The old rule's fallback was half of what it said. Dropping it on the
        // expansion would leave the user's second choice behind without a word.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        cfg.migrate_routing_to_categories();

        for (name, fallback) in [
            (ConfigurableCategory::Edit, Some("opus")),
            (ConfigurableCategory::Shell, Some("opus")),
            // `design` came from `spec`, the first of the two rules that map to
            // it — see `two_phases_that_collapse_onto_one_category_say_so`.
            (ConfigurableCategory::Design, None),
            (ConfigurableCategory::Review, None),
        ] {
            let row = cfg
                .categories
                .iter()
                .find(|o| o.name == name)
                .unwrap_or_else(|| panic!("no `{name}` row"));
            assert_eq!(row.fallback_id.as_deref(), fallback, "`{name}` fallback");
        }
    }

    /// A pre-REQ config where the routed provider declares no `model` — the
    /// state [`Config::validate`] permits **on purpose**, so that a pre-REQ
    /// config boots at all and [`Config::migrate_models`] gets a chance to run.
    ///
    /// `implement → my-llama` is the ordinary shape of it: a provider added
    /// before REQ-557 existed, whose model the price table could not resolve.
    const UNUSABLE_ROUTED_PROVIDER_TOML: &str = r#"
[[providers]]
id = "on-device"
kind = "local"

[[providers]]
id = "my-llama"
kind = "openai-compatible"
endpoint = "http://127.0.0.1:8080"

[[routing]]
phase = "implement"
provider_id = "my-llama"
"#;

    /// **The migration must not write a binding that cannot serve.**
    ///
    /// An override never falls through to its tier, so a dead `[[categories]]`
    /// row does not degrade the category — it removes it. `edit` is the BR-9
    /// freeform default, where every ordinary coding turn lands, so persisting
    /// `edit → my-llama` turns "one provider is unusable" into "every freeform
    /// turn hard-fails", on the first start after upgrade, on a config that
    /// worked the day before.
    ///
    /// `reject_unusable_binding` refuses this exact binding from a user over
    /// `config/set`. The migration is not a privileged author.
    #[test]
    fn a_rule_naming_an_unusable_provider_is_skipped_and_named() {
        let mut cfg = Config::load(UNUSABLE_ROUTED_PROVIDER_TOML).expect("the fixture must load");
        // The premise: this config is valid, and the provider is unusable.
        assert_eq!(cfg.unusable_providers(), vec!["my-llama".to_owned()]);

        let report = cfg.migrate_routing_to_categories();

        assert!(
            cfg.categories.is_empty(),
            "a dead binding must not be persisted: {:?}",
            cfg.categories
        );
        assert!(
            report.phases.is_empty(),
            "and it must not be reported as migrated"
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].provider_id, "my-llama");
        assert_eq!(report.skipped[0].phase, Phase::Implement);
        assert_eq!(
            report.skipped[0].categories,
            vec![ConfigurableCategory::Edit, ConfigurableCategory::Shell],
            "the bindings the user is losing are reported by name"
        );

        // Consumed, so the retired table leaves the file and this is reported
        // once rather than on every start.
        assert!(cfg.legacy_routing.is_empty());
        assert!(!report.is_empty(), "the file must still be rewritten");

        // And it is genuinely idempotent: a second run finds nothing.
        let mut again = cfg.clone();
        assert!(again.migrate_routing_to_categories().is_empty());

        // Non-vacuity: give the same provider a model and the rule migrates.
        let mut usable = Config::load(UNUSABLE_ROUTED_PROVIDER_TOML).expect("loads");
        usable.providers[1].model = Some("llama-3".to_owned());
        let report = usable.migrate_routing_to_categories();
        assert!(report.skipped.is_empty());
        assert_eq!(
            usable.categories.iter().map(|o| o.name).collect::<Vec<_>>(),
            vec![ConfigurableCategory::Edit, ConfigurableCategory::Shell]
        );
    }

    /// The same screen on the fallback, because `reject_unusable_binding`
    /// applies it to both ids. The rule still migrates — its primary is fine —
    /// but the dead id is not copied onto the new rows, and its disappearance
    /// from the user's file is reported rather than silent.
    #[test]
    fn an_unusable_fallback_is_dropped_from_the_rows_it_would_have_been_copied_onto() {
        let mut cfg = Config::load(UNUSABLE_ROUTED_PROVIDER_TOML).expect("loads");
        cfg.providers.push(ModelProvider {
            id: "good".to_owned(),
            kind: ProviderKind::Anthropic,
            endpoint: Some("https://api.anthropic.com".to_owned()),
            model: Some("claude-opus-5".to_owned()),
            auth_ref: Some("keychain:anthropic".to_owned()),
            capabilities: ProviderCapabilities::default(),
        });
        cfg.legacy_routing = vec![LegacyRoutingRule {
            phase: Phase::Implement,
            provider_id: "good".to_owned(),
            fallback_id: Some("my-llama".to_owned()),
        }];

        let report = cfg.migrate_routing_to_categories();

        assert_eq!(report.phases.len(), 1);
        assert_eq!(
            report.phases[0].dropped_fallback.as_deref(),
            Some("my-llama")
        );
        for row in &cfg.categories {
            assert_eq!(row.provider_id, "good", "the usable primary still migrates");
            assert_eq!(
                row.fallback_id, None,
                "a fallback that cannot serve is not written down: {row:?}"
            );
        }
    }

    /// **The `default_provider` → tiers leg gets the same screen.** A tier bound
    /// to a provider that cannot serve is worse than an unbound one: an unbound
    /// tier inherits the local model and the machine keeps routing, and writing
    /// the dead id down replaces that with a hole.
    #[test]
    fn an_unusable_default_provider_writes_no_tiers() {
        let mut cfg = Config::load(UNUSABLE_ROUTED_PROVIDER_TOML).expect("loads");
        cfg.legacy_routing.clear();
        cfg.default_provider = Some("my-llama".to_owned());

        let report = cfg.migrate_routing_to_categories();

        assert!(cfg.tiers.is_empty(), "no dead tier rows: {:?}", cfg.tiers);
        assert!(report.default_tiers.is_empty());
        assert_eq!(report.skipped_default.as_deref(), Some("my-llama"));
        assert!(
            report.is_empty(),
            "nothing was consumed and nothing written, so there is no reason \
             to rewrite the file"
        );

        // Non-vacuity, and the self-healing property: once it declares a model,
        // the next start writes the tiers.
        cfg.providers[1].model = Some("llama-3".to_owned());
        let report = cfg.migrate_routing_to_categories();
        assert_eq!(report.default_tiers, vec![Tier::Build, Tier::Think]);
        assert!(report.skipped_default.is_none());
    }

    #[test]
    fn two_phases_that_collapse_onto_one_category_say_so() {
        // The mirror of the one-to-many expansion, and the half that LOSES
        // information: `spec` and `architect` both become `design`, so a user
        // who routed them to different providers cannot any more. First claim
        // wins — "first" being their own table order — and the loser is named
        // rather than vanishing.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        let report = cfg.migrate_routing_to_categories();

        let spec = report
            .phases
            .iter()
            .find(|p| p.phase == Phase::Spec)
            .expect("the `spec` rule is reported");
        assert_eq!(spec.categories, vec![ConfigurableCategory::Design]);
        assert!(spec.dropped.is_empty(), "`spec` came first, so it won");

        let architect = report
            .phases
            .iter()
            .find(|p| p.phase == Phase::Architect)
            .expect("the `architect` rule is reported");
        assert!(
            architect.categories.is_empty(),
            "`architect` had nothing left to claim"
        );
        assert_eq!(architect.dropped.len(), 1);
        assert_eq!(architect.dropped[0].category, ConfigurableCategory::Design);
        assert_eq!(
            architect.dropped[0].kept_provider_id, "opus",
            "the report names the binding that survived, so the user can see \
             what they now have instead of what they wrote"
        );
    }

    /// `default_provider` materializes into the **turn** tiers only.
    ///
    /// `Router::effective_table` already sends an unbound `build`/`think` to
    /// `default_provider`; writing it down makes that visible and editable.
    ///
    /// `reflex` and `scan` are both excluded, and for one reason stated once in
    /// [`Tier::inherits_default_provider`]: a tier whose work was already local
    /// before this REQ stays local until the user says otherwise. `reflex` by
    /// definition; `scan` because its only reached category, `digest`, was
    /// hardcoded to the local engine and sent nothing anywhere — so inheriting
    /// a key the user set for their *turns* would start shipping file contents
    /// and build logs to a vendor API on the first start after upgrade.
    #[test]
    fn the_default_provider_becomes_build_and_think_but_never_reflex_or_scan() {
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        let report = cfg.migrate_routing_to_categories();

        assert_eq!(report.default_tiers, vec![Tier::Build, Tier::Think]);
        assert_eq!(report.default_provider.as_deref(), Some("opus"));
        assert_eq!(
            cfg.tiers.iter().map(|b| b.tier).collect::<Vec<_>>(),
            vec![Tier::Build, Tier::Think]
        );
        for excluded in [Tier::Reflex, Tier::Scan] {
            assert!(
                cfg.tiers.iter().all(|b| b.tier != excluded),
                "a migrated config must carry NO written `{excluded}` binding: \
                 persisting `{excluded} = <remote default>` is the change \
                 `Tier::inherits_default_provider` exists to prevent, written \
                 into the user's own file where a later reader would take it \
                 for their choice"
            );
        }
    }

    #[test]
    fn the_default_provider_leg_leaves_a_partly_bound_table_alone() {
        // A user who has bound even one tier has engaged with the new schema.
        // The migration materializes the *absence* of a table, not the absence
        // of a row — it does not argue with a table someone is already editing.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        cfg.tiers.push(TierBinding {
            tier: Tier::Think,
            provider_id: "cheap".to_owned(),
            fallback_id: None,
        });
        let report = cfg.migrate_routing_to_categories();

        assert!(report.default_tiers.is_empty());
        assert_eq!(cfg.tiers.len(), 1, "the user's own row, untouched");
        assert_eq!(cfg.tiers[0].provider_id, "cheap");
    }

    #[test]
    fn the_migration_runs_once() {
        // Keyed on the absence of the old state and the presence of the new: the
        // retired rules are consumed, the tiers are bound. A second start finds
        // nothing, changes nothing, and reports nothing.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        let first = cfg.migrate_routing_to_categories();
        assert!(!first.is_empty());

        let after_first = cfg.clone();
        let second = cfg.migrate_routing_to_categories();

        assert!(
            second.is_empty(),
            "a second run must report nothing new: {second:?}"
        );
        assert_eq!(cfg, after_first, "and must change nothing");
        assert!(
            cfg.legacy_routing.is_empty(),
            "the retired table is consumed, so the written-out config no longer \
             carries it — which is what makes the next start find nothing"
        );
        assert!(
            !cfg.to_toml().expect("serializes").contains("[[routing]]"),
            "and the retired table must not be written back"
        );
    }

    #[test]
    fn the_migration_never_overwrites_an_explicit_category_override() {
        // A legacy row never beats something the user wrote in the current
        // vocabulary. The rule is still consumed — it is inert, and leaving it
        // on disk means re-reading it forever — but it is reported as dropped
        // rather than vanishing without a word.
        let mut cfg = Config::load(PRE_REQ_558_TOML).expect("the fixture must load");
        cfg.categories.push(CategoryOverride {
            name: ConfigurableCategory::Edit,
            provider_id: "opus".to_owned(),
            fallback_id: None,
        });
        let report = cfg.migrate_routing_to_categories();

        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Edit),
            Some("opus"),
            "the explicit override wins"
        );
        assert_eq!(
            override_for(&cfg, ConfigurableCategory::Shell),
            Some("cheap"),
            "and the half of the expansion that was NOT already bound still lands"
        );
        let implement = report
            .phases
            .iter()
            .find(|p| p.phase == Phase::Implement)
            .expect("the rule is still reported");
        assert_eq!(implement.categories, vec![ConfigurableCategory::Shell]);
        assert_eq!(implement.dropped.len(), 1);
        assert_eq!(implement.dropped[0].category, ConfigurableCategory::Edit);
        assert_eq!(implement.dropped[0].kept_provider_id, "opus");
    }

    #[test]
    fn a_config_with_nothing_to_migrate_reports_nothing() {
        // The post-REQ config: no retired table, a tier table already bound.
        let mut cfg = sample_config();
        cfg.legacy_routing.clear();
        assert!(cfg.migrate_routing_to_categories().is_empty());
    }

    #[test]
    fn judgment_default_is_a_real_key_that_defaults_to_edit_and_is_written_out() {
        // BR-9 / AC-12: the declared default is configuration-visible, not a
        // hidden constant. A key that vanishes from a written-out config
        // whenever it holds its default IS a hidden constant, so it serializes
        // unconditionally.
        assert_eq!(Config::default().judgment_default, JudgmentCategory::Edit);
        assert_eq!(
            Config::load("").expect("loads").judgment_default,
            JudgmentCategory::Edit,
            "a config that says nothing means today's behavior: a non-auxiliary \
             freeform prompt is a coding turn"
        );

        let toml_text = Config::default().to_toml().expect("serialize");
        assert!(
            toml_text.contains("judgment_default = \"edit\""),
            "the declared default must be readable from the config file: {toml_text}"
        );

        // And changing it is a config edit, not a recompile.
        let cfg = Config::load("judgment_default = \"debug\"\n").expect("loads");
        assert_eq!(cfg.judgment_default, JudgmentCategory::Debug);
        let back = Config::from_toml(&cfg.to_toml().expect("serialize")).expect("deserialize");
        assert_eq!(back.judgment_default, JudgmentCategory::Debug);
    }

    #[test]
    fn judgment_default_admits_only_the_four_judgment_categories() {
        // BR-2/AC-3's guarantee reaching config: the key's type is
        // `JudgmentCategory`, so `judgment_default = "digest"` is a load error
        // rather than a harness-known category assigned from a config file.
        for not_a_judgment in ["digest", "redact", "route", "title", "shell"] {
            let err = Config::load(&format!("judgment_default = \"{not_a_judgment}\"\n"))
                .expect_err("only the judgment four may be the declared default");
            assert!(
                err.to_string().contains(not_a_judgment),
                "{not_a_judgment}: {err}"
            );
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
        assert_eq!(cfg.legacy_routing[0].phase, Phase::Architect);
    }
}
