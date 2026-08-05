//! Purpose-oriented routing categories, their tiers, and the one pure function
//! that answers "where does this category go" (REQ-558 BR-12, ADR-D).
//!
//! A category is *what a model call is for* — classify, summarize, edit,
//! critique — as opposed to *where in the lifecycle it happens*. Lifecycle
//! [`Phase`] remains a cost-attribution and ADLC-gating fact; it is no longer
//! the dispatch key.
//!
//! # The shape of this module
//!
//! - [`Tier`] — the four bindings most users configure.
//! - [`Category`] — all eleven categories. The runtime dispatch key.
//! - [`ConfigurableCategory`] — the ten a config file may bind. `redact` is
//!   **not** a variant (ADR-B).
//! - [`JudgmentCategory`] — the four the classifier may return. This is the
//!   type-level guarantee behind AC-3: there is no path from text into
//!   [`Category`], so a keyword matcher cannot assign `digest`.
//! - [`resolve`] — the single resolution function. Pure, with provider health
//!   **and** provider usability injected as closures, mirroring
//!   [`crate::policy::evaluate`]'s proven shape.
//!
//! # Two things this module makes impossible rather than merely forbidden
//!
//! **`redact` cannot be bound to a remote provider** (BR-4, ADR-B). Not because
//! [`resolve`] checks for a binding and refuses it, but because
//! [`ConfigurableCategory`] has no `Redact` variant, so a binding for it cannot
//! be represented in the first place. LESSON-443's rule is that a guard whose
//! condition is "the feature does not exist yet" is a time bomb with your own
//! roadmap as the fuse — so there is no guard here, only a type.
//!
//! **A harness-known category cannot be assigned from prompt text** (BR-2,
//! AC-3). [`Category`] has no `FromStr`, no `from_name`, and no `Deserialize`.
//! The only parse into the category space lands in [`JudgmentCategory`], whose
//! four variants are exactly the categories that are *allowed* to be inferred.
//! Adding a `&str -> Category` conversion "for convenience" reopens the hole
//! this closes.

use crate::phase::Phase;
use crate::policy::{ProviderHealth, RouteOutcome};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Tier
// ---------------------------------------------------------------------------

/// The four routing tiers — the primary configuration surface (REQ-558).
///
/// Every category inherits one of these at compile time
/// ([`Category::tier`]) and may be individually overridden, so the common
/// configuration is four settings rather than eleven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Sub-second, every turn, never leaves the machine — latency and privacy
    /// dominate.
    Reflex,
    /// Read a lot, emit a little — context window and $/input-token dominate.
    Scan,
    /// The agentic loop (read → edit → run → verify) — tool-call fidelity
    /// dominates.
    Build,
    /// Design, debug, critique — reasoning depth dominates.
    Think,
}

impl Tier {
    /// Every tier, in cost order. For exhaustive, table-driven tests and for
    /// rendering the binding table to the user.
    pub const ALL: [Tier; 4] = [Tier::Reflex, Tier::Scan, Tier::Build, Tier::Think];

    /// The lowercase wire/display name of this tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Reflex => "reflex",
            Tier::Scan => "scan",
            Tier::Build => "build",
            Tier::Think => "think",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that does not name a [`Tier`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tier '{0}'; expected one of: reflex, scan, build, think")]
pub struct ParseTierError(pub String);

impl FromStr for Tier {
    type Err = ParseTierError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Tier::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| ParseTierError(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// Category
// ---------------------------------------------------------------------------

/// Where a category's assignment comes from (System Model: `Category.origin`).
///
/// A **compile-time property of the category**, never configuration: it records
/// whether the harness knows what it is doing at the call site or has to read
/// user intent to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryOrigin {
    /// Tagged at the call site. The harness does not guess that it is
    /// compacting — it *is* compacting. **MUST NOT** be derived from prompt
    /// text (BR-2).
    HarnessKnown,
    /// Requires reading user intent; assigned by the `route` classifier, whose
    /// return type is [`JudgmentCategory`].
    IntentClassified,
}

impl CategoryOrigin {
    /// The wire/display name (`harness_known` / `intent_classified`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            CategoryOrigin::HarnessKnown => "harness_known",
            CategoryOrigin::IntentClassified => "intent_classified",
        }
    }
}

impl fmt::Display for CategoryOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The eleven routing categories — the runtime dispatch key in **both** session
/// modes (BR-1).
///
/// Deliberately carries **no** `FromStr` and **no** `Deserialize`. Config binds
/// [`ConfigurableCategory`]; the classifier returns [`JudgmentCategory`]. Those
/// are the only two ways into this type, and neither can name a harness-known
/// category from prompt text (AC-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// Classifies the four judgment categories. Must stay local — a router that
    /// calls a remote model to decide has spent what it was saving (BR-5).
    Route,
    /// Secret/PII scan before egress. **Pinned to the local tier by
    /// construction** and absent from [`ConfigurableCategory`] (BR-4, ADR-B).
    Redact,
    /// Session and branch naming.
    Title,
    /// File and diff summarization into context.
    Digest,
    /// Conversation compaction. Split from `digest` because it decides what to
    /// *forget*, and a bad compaction silently corrupts every later turn.
    Compact,
    /// Ranking grep/glob hits.
    Triage,
    /// Write code from a task artifact.
    Edit,
    /// Command construction and output interpretation. Split from `edit`:
    /// "good at diffs" and "safe at `rm`" are different competencies.
    Shell,
    /// Architecture, decomposition, spec authoring.
    Design,
    /// Root-cause on a failure. Split from `design`: needs depth *and* long
    /// context.
    Debug,
    /// Adversarial critique — some users deliberately pick a different vendor
    /// here than the one that wrote the code.
    Review,
}

impl Category {
    /// Every category. AC-2 iterates this; nothing may resolve outside it.
    pub const ALL: [Category; 11] = [
        Category::Route,
        Category::Redact,
        Category::Title,
        Category::Digest,
        Category::Compact,
        Category::Triage,
        Category::Edit,
        Category::Shell,
        Category::Design,
        Category::Debug,
        Category::Review,
    ];

    /// The lowercase wire/display name of this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Route => "route",
            Category::Redact => "redact",
            Category::Title => "title",
            Category::Digest => "digest",
            Category::Compact => "compact",
            Category::Triage => "triage",
            Category::Edit => "edit",
            Category::Shell => "shell",
            Category::Design => "design",
            Category::Debug => "debug",
            Category::Review => "review",
        }
    }

    /// The tier this category inherits its binding from. A **compile-time**
    /// property: a category's tier is not configurable, only the provider its
    /// tier (or its own override) names.
    #[must_use]
    pub const fn tier(self) -> Tier {
        match self {
            Category::Route | Category::Redact | Category::Title => Tier::Reflex,
            Category::Digest | Category::Compact | Category::Triage => Tier::Scan,
            Category::Edit | Category::Shell => Tier::Build,
            Category::Design | Category::Debug | Category::Review => Tier::Think,
        }
    }

    /// Whether this category is tagged at its call site or inferred from user
    /// intent (System Model: `Category.origin`).
    #[must_use]
    pub const fn origin(self) -> CategoryOrigin {
        match self {
            Category::Route
            | Category::Redact
            | Category::Title
            | Category::Digest
            | Category::Compact
            | Category::Triage
            | Category::Shell => CategoryOrigin::HarnessKnown,
            Category::Edit | Category::Design | Category::Debug | Category::Review => {
                CategoryOrigin::IntentClassified
            }
        }
    }

    /// This category's configurable counterpart, or `None` for the one category
    /// a config file cannot name.
    ///
    /// `None` for [`Category::Redact`] and [`Category::Route`] — this is the
    /// fallible direction of ADR-B's asymmetry
    /// ([`From<ConfigurableCategory>`](Category) is total; this is not).
    #[must_use]
    pub const fn configurable(self) -> Option<ConfigurableCategory> {
        match self {
            // BR-4 / ADR-B: `redact` inspects content *before* it is allowed to
            // leave the machine. A binding that could send it elsewhere would
            // send the content it exists to guard, so there is no such binding
            // to express.
            Category::Redact => None,
            // BR-5: `route` MUST resolve to the local tier, and when the local
            // tier cannot serve, classification is *bypassed* rather than routed
            // remotely — "a router that calls a remote model to decide has spent
            // what it was saving" (REQ-544 BR-8).
            //
            // No configuration of `route` can therefore express a valid state:
            // the only provider it may resolve to is the local one, which is not
            // a choice. Left configurable, a user could write a config that
            // violated BR-5 — a remote classifier on every freeform turn — and a
            // test proved they could. Pinning it here rather than screening at
            // the call site is LESSON-484: enforce the rule where the decision is
            // made, not where it is convenient.
            Category::Route => None,
            Category::Title => Some(ConfigurableCategory::Title),
            Category::Digest => Some(ConfigurableCategory::Digest),
            Category::Compact => Some(ConfigurableCategory::Compact),
            Category::Triage => Some(ConfigurableCategory::Triage),
            Category::Edit => Some(ConfigurableCategory::Edit),
            Category::Shell => Some(ConfigurableCategory::Shell),
            Category::Design => Some(ConfigurableCategory::Design),
            Category::Debug => Some(ConfigurableCategory::Debug),
            Category::Review => Some(ConfigurableCategory::Review),
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// ConfigurableCategory
// ---------------------------------------------------------------------------

/// The nine categories a configuration file may bind to a provider.
///
/// `redact` and `route` are **absent by design** (BR-4, BR-5, ADR-B):
/// resolution has no branch for either and cannot be made to have one, because
/// config cannot express the binding. This is also what makes a
/// `[[categories]] name = "redact"` entry fail to deserialize — the rejection is
/// a property of the type, not a hand-written check that could be forgotten.
/// The hand-written [`Deserialize`] impl below changes only the *message*, never
/// the set of representable states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigurableCategory {
    /// See [`Category::Title`].
    Title,
    /// See [`Category::Digest`].
    Digest,
    /// See [`Category::Compact`].
    Compact,
    /// See [`Category::Triage`].
    Triage,
    /// See [`Category::Edit`].
    Edit,
    /// See [`Category::Shell`].
    Shell,
    /// See [`Category::Design`].
    Design,
    /// See [`Category::Debug`].
    Debug,
    /// See [`Category::Review`].
    Review,
}

impl ConfigurableCategory {
    /// Every configurable category — nine, one per [`Category`] except the two
    /// pinned-local ones (`redact`, `route`).
    pub const ALL: [ConfigurableCategory; 9] = [
        ConfigurableCategory::Title,
        ConfigurableCategory::Digest,
        ConfigurableCategory::Compact,
        ConfigurableCategory::Triage,
        ConfigurableCategory::Edit,
        ConfigurableCategory::Shell,
        ConfigurableCategory::Design,
        ConfigurableCategory::Debug,
        ConfigurableCategory::Review,
    ];

    /// The lowercase wire/display name — identical to the [`Category`] it maps
    /// to, so one spelling serves config, CLI, and the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        Category::from_configurable(self).as_str()
    }
}

impl fmt::Display for ConfigurableCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Category {
    /// Total conversion from the configurable ten (ADR-B).
    ///
    /// A `const fn` twin of [`From<ConfigurableCategory> for Category`] so
    /// [`ConfigurableCategory::as_str`] can stay `const`.
    #[must_use]
    pub const fn from_configurable(c: ConfigurableCategory) -> Category {
        match c {
            ConfigurableCategory::Title => Category::Title,
            ConfigurableCategory::Digest => Category::Digest,
            ConfigurableCategory::Compact => Category::Compact,
            ConfigurableCategory::Triage => Category::Triage,
            ConfigurableCategory::Edit => Category::Edit,
            ConfigurableCategory::Shell => Category::Shell,
            ConfigurableCategory::Design => Category::Design,
            ConfigurableCategory::Debug => Category::Debug,
            ConfigurableCategory::Review => Category::Review,
        }
    }
}

impl From<ConfigurableCategory> for Category {
    fn from(c: ConfigurableCategory) -> Self {
        Category::from_configurable(c)
    }
}

/// A string that does not name a [`ConfigurableCategory`].
///
/// The two pinned categories are called out separately because the message
/// quality *is* the acceptance criterion (AC-4): a user who writes
/// `categories.redact` must learn the key is **forbidden**, not that it is
/// misspelled.
///
/// They get one variant each rather than a shared "that category is pinned"
/// sentence for the reason the resolver's `resolve_pinned_local` spells out:
/// each is pinned for its **own** reason, and REQ-544 BR-5's legibility promise
/// is that a decision names the signal that fired, not that it names *a* signal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseCategoryError {
    /// The caller named `redact`, which has no configuration path (BR-4).
    #[error(
        "'redact' is pinned to the local tier by construction and cannot be bound to a provider: \
         it inspects content before that content is allowed to leave the machine. Remove the entry."
    )]
    RedactIsPinned,
    /// The caller named `route`, which has no configuration path (BR-5).
    #[error(
        "'route' is pinned to the local tier by construction and cannot be bound to a provider: \
         a classifier that calls a remote model has spent what the routing decision saves, so no \
         binding for it could name a valid state. Remove the entry."
    )]
    RouteIsPinned,
    /// The caller named something that is not a category at all.
    #[error("unknown category '{name}'; expected one of: {expected}")]
    Unknown {
        /// The unrecognized name, as written.
        name: String,
        /// The accepted names, comma-separated.
        expected: String,
    },
}

impl ParseCategoryError {
    /// The `Unknown` case — the accepted names never advertise a pinned one.
    fn unknown(name: &str) -> Self {
        ParseCategoryError::Unknown {
            name: name.to_owned(),
            expected: ConfigurableCategory::ALL
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

impl FromStr for ConfigurableCategory {
    type Err = ParseCategoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(c) = ConfigurableCategory::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
        {
            return Ok(c);
        }
        // A name that IS a category but has no configurable counterpart is
        // *pinned*, not misspelled. The pinned set is derived from
        // `configurable()` rather than listed here, so the two facts cannot
        // disagree — but the sentence is still chosen per category, because a
        // borrowed justification is the defect this module already corrected
        // once (see `resolve_pinned_local`).
        if let Some(pinned) = Category::ALL
            .into_iter()
            .find(|c| c.as_str() == s && c.configurable().is_none())
        {
            return Err(match pinned {
                Category::Redact => ParseCategoryError::RedactIsPinned,
                Category::Route => ParseCategoryError::RouteIsPinned,
                // Unreachable while those two are the only pinned categories. A
                // third one needs its own sentence rather than a borrowed one,
                // and falling through to the typo message here is what makes
                // `every_pinned_category_names_its_own_pin` fail until it has
                // one.
                other => ParseCategoryError::unknown(other.as_str()),
            });
        }
        Err(ParseCategoryError::unknown(s))
    }
}

/// Deserialized through [`FromStr`] rather than derived, so a config naming a
/// pinned category reads [`ParseCategoryError`]'s sentence instead of serde's
/// bare `unknown variant`, which reads like a typo (AC-4).
///
/// The **rejection** is still structural: there is no `Redact` or `Route`
/// variant to deserialize into, so those states stay unrepresentable however
/// this impl is written (ADR-B). What the hand-written impl buys is only the
/// message — and it buys it for every format at once, config TOML and the
/// JSON-RPC payload that binds a category at runtime alike, rather than at
/// whichever call site remembered to check.
impl<'de> Deserialize<'de> for ConfigurableCategory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// JudgmentCategory
// ---------------------------------------------------------------------------

/// The four categories the `route` classifier may return (AC-3, BR-3).
///
/// **This type is the guarantee.** Because it is the classifier's return type
/// and there is no path from text into [`Category`], assigning `digest` from
/// prompt text does not compile. The seven harness-known categories are
/// therefore verifiable without a live model — a grep-style assertion would be
/// redundant if this holds, and inadequate if it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JudgmentCategory {
    /// Write code from a task artifact. The **declared default** (BR-9) used
    /// when classification is bypassed or fails — today's behavior is that a
    /// non-auxiliary freeform prompt is a coding turn.
    #[default]
    Edit,
    /// Architecture, decomposition, spec authoring.
    Design,
    /// Root-cause on a failure.
    Debug,
    /// Adversarial critique.
    Review,
}

impl JudgmentCategory {
    /// Every judgment category — exactly the four with
    /// [`CategoryOrigin::IntentClassified`].
    pub const ALL: [JudgmentCategory; 4] = [
        JudgmentCategory::Edit,
        JudgmentCategory::Design,
        JudgmentCategory::Debug,
        JudgmentCategory::Review,
    ];

    /// The lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        Category::from_judgment(self).as_str()
    }
}

impl fmt::Display for JudgmentCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Category {
    /// Total conversion from the judgment four.
    #[must_use]
    pub const fn from_judgment(c: JudgmentCategory) -> Category {
        match c {
            JudgmentCategory::Edit => Category::Edit,
            JudgmentCategory::Design => Category::Design,
            JudgmentCategory::Debug => Category::Debug,
            JudgmentCategory::Review => Category::Review,
        }
    }
}

impl From<JudgmentCategory> for Category {
    fn from(c: JudgmentCategory) -> Self {
        Category::from_judgment(c)
    }
}

/// A string that does not name a [`JudgmentCategory`].
///
/// This is the **only** parse that reaches the category space from text, and it
/// admits four names. The classifier parses its model output through here, so a
/// model that answers `"digest"` gets an error rather than a harness-known
/// category (BR-2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a judgment category; expected one of: edit, design, debug, review")]
pub struct ParseJudgmentCategoryError(pub String);

impl FromStr for JudgmentCategory {
    type Err = ParseJudgmentCategoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        JudgmentCategory::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| ParseJudgmentCategoryError(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// The configured table
// ---------------------------------------------------------------------------

/// One row of the tier → provider table — what most users configure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBinding {
    /// The tier this row binds.
    pub tier: Tier,
    /// Primary provider id (FK → [`crate::entities::ModelProvider::id`]).
    pub provider_id: String,
    /// Optional fallback provider id, used when the primary is unavailable or
    /// not routable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_id: Option<String>,
}

/// A per-category override of the tier binding.
///
/// `provider_id` is **required**, not `Option`: a row exists to override, and a
/// row that names no provider would be a half-state where the primary comes
/// from one place and the fallback from another. The fallback always comes from
/// the same row as the primary it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryOverride {
    /// The category this row binds. `redact` is unrepresentable (ADR-B).
    pub name: ConfigurableCategory,
    /// Primary provider id (FK → [`crate::entities::ModelProvider::id`]).
    pub provider_id: String,
    /// Optional fallback provider id for this category specifically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_id: Option<String>,
}

/// Everything [`resolve`] reads: the configured bindings plus the local tier's
/// identity.
///
/// `local_provider_id` is `None` when no local tier exists on this machine.
/// That is a real state, not a placeholder — nothing in this module ever mints
/// a provider id to stand in for an absent one (BR-8, LESSON-456).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategoryTable {
    /// The tier → provider rows.
    pub tiers: Vec<TierBinding>,
    /// The per-category overrides.
    pub categories: Vec<CategoryOverride>,
    /// The local tier's provider id, when this machine has a local tier.
    pub local_provider_id: Option<String>,
}

impl CategoryTable {
    /// An empty table: no tiers bound, no overrides, no local tier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a tier binding.
    #[must_use]
    pub fn with_tier(mut self, binding: TierBinding) -> Self {
        self.tiers.retain(|t| t.tier != binding.tier);
        self.tiers.push(binding);
        self
    }

    /// Add (or replace) a per-category override.
    #[must_use]
    pub fn with_override(mut self, over: CategoryOverride) -> Self {
        self.categories.retain(|c| c.name != over.name);
        self.categories.push(over);
        self
    }

    /// Name the local tier's provider id.
    #[must_use]
    pub fn with_local_provider(mut self, id: impl Into<String>) -> Self {
        self.local_provider_id = Some(id.into());
        self
    }

    /// The binding for `tier`, when one is configured.
    #[must_use]
    pub fn tier_binding(&self, tier: Tier) -> Option<&TierBinding> {
        self.tiers.iter().find(|t| t.tier == tier)
    }

    /// The override for `category`, when one is configured.
    #[must_use]
    pub fn category_override(&self, category: ConfigurableCategory) -> Option<&CategoryOverride> {
        self.categories.iter().find(|c| c.name == category)
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The answer to "where does this category go" (ADR-D).
///
/// Every surface that describes a routing state — `route_decided`'s payload,
/// `teton policy show`, and the turn-failure sentence — is built from **this
/// struct**. None of them computes its own answer (BR-6, AC-11): two surfaces
/// describing one routing state must not be able to drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryResolution {
    /// The category that was resolved.
    pub category: Category,
    /// The tier it inherits from (a compile-time property of the category, so
    /// this is populated even when nothing is bound).
    pub tier: Tier,
    /// The chosen provider id, or `None` when nothing could be selected. Never
    /// a synthesized id: every `Some` here appears verbatim in the table that
    /// was passed in (BR-8).
    pub provider_id: Option<String>,
    /// The fallback provider for this binding, **already screened** for
    /// usability and health, or `None` when there is no usable one.
    ///
    /// Carried on the resolution so a mid-turn failure does not re-read the
    /// configured table to find it — that second read is exactly the shape
    /// BUG-155 found three times, where one path screened providers and another
    /// did not.
    pub fallback_id: Option<String>,
    /// Human-readable sentence naming the signal that fired. Always non-empty,
    /// always names the category (BR-8: an unresolvable category names itself).
    pub reason: String,
    /// Structured outcome for programmatic branching. Shared with phase-policy
    /// evaluation (ADR-J) — one vocabulary, one home.
    pub outcome: RouteOutcome,
}

/// Why a provider was passed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    /// Not registered as routable — in practice a remote provider that declares
    /// no `model` (REQ-557 ADR-E).
    NotRoutable,
    /// Registered, but down / erroring / timing out.
    Unavailable,
}

impl Rejection {
    const fn why(self) -> &'static str {
        match self {
            Rejection::NotRoutable => "is not routable",
            Rejection::Unavailable => "is unavailable",
        }
    }

    /// The clause appended after the sentence, explaining a non-obvious
    /// rejection. Empty for the self-explanatory one.
    const fn note(self) -> &'static str {
        match self {
            Rejection::NotRoutable => {
                " A remote provider that declares no `model` cannot serve a turn."
            }
            Rejection::Unavailable => "",
        }
    }
}

/// Screen one provider id: usability first (ADR-E), then health.
///
/// The usability screen is what keeps this from becoming a fourth dispatch path
/// that bypasses it (BUG-155). It is injected rather than computed so this
/// function stays pure and the daemon's `Router::is_routable` remains the single
/// definition of routable.
fn screen<H, U>(id: &str, health: &H, usable: &U) -> Result<ProviderHealth, Rejection>
where
    H: Fn(&str) -> ProviderHealth,
    U: Fn(&str) -> bool,
{
    if !usable(id) {
        return Err(Rejection::NotRoutable);
    }
    match health(id) {
        ProviderHealth::Unavailable => Err(Rejection::Unavailable),
        h => Ok(h),
    }
}

/// Which row of the table supplied the binding — part of the reason sentence,
/// because "why here" is as much of the answer as "where".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingSource {
    Override,
    TierInheritance,
}

/// Resolve `category` to a provider through override → tier → declared error
/// (BR-8, BR-12).
///
/// Pure: no I/O, no clock, no globals. `health` maps a provider id to its
/// current [`ProviderHealth`] and `usable` reports whether the daemon would
/// route to it at all, both supplied by the caller — the daemon plugs in live
/// probe results and its provider registry, tests plug in fixed tables. This is
/// [`crate::policy::evaluate`]'s shape, with the usability screen added because
/// a new dispatch axis is a new way to bypass it (ADR-E).
///
/// # Precedence
///
/// 1. [`Category::Redact`] returns the local tier unconditionally, reading no
///    part of `table`'s bindings (BR-4, AC-4).
/// 2. A per-category override, if one names this category.
/// 3. The binding of the category's tier.
/// 4. Otherwise the category is unresolvable, and says so **by name**.
///
/// An override that names an unroutable provider does **not** fall through to
/// the tier binding: the user's explicit choice for this category is not
/// quietly replaced by the inherited one. Its own `fallback_id` is the only
/// alternative considered, and if that fails too the failure names both.
#[must_use]
pub fn resolve<H, U>(
    category: Category,
    table: &CategoryTable,
    health: H,
    usable: U,
) -> CategoryResolution
where
    H: Fn(&str) -> ProviderHealth,
    U: Fn(&str) -> bool,
{
    let tier = category.tier();

    // BR-4 / BR-5 / ADR-B / AC-4: `redact` and `route` are pinned to the local
    // tier **by construction**. They are the two variants with no
    // `ConfigurableCategory` counterpart, so this branch is taken for them and
    // only for them, and it returns without consulting a single binding.
    //
    // `redact` because it inspects content before that content is allowed to
    // leave the machine; `route` because a classifier that calls a remote model
    // has spent what the routing decision was meant to save. It is not `if binding.is_none() { local }` — config cannot
    // express a redact binding, so there is no condition here to get wrong the
    // day someone adds a key (LESSON-443).
    let Some(configurable) = category.configurable() else {
        return resolve_pinned_local(
            category,
            table.local_provider_id.as_deref(),
            &health,
            &usable,
        );
    };

    let binding = table
        .category_override(configurable)
        .map(|o| {
            (
                BindingSource::Override,
                o.provider_id.as_str(),
                o.fallback_id.as_deref(),
            )
        })
        .or_else(|| {
            table.tier_binding(tier).map(|t| {
                (
                    BindingSource::TierInheritance,
                    t.provider_id.as_str(),
                    t.fallback_id.as_deref(),
                )
            })
        });

    let Some((source, primary, fallback)) = binding else {
        // BR-8: the category names itself and its unset binding. Nothing is
        // synthesized and no other tier is borrowed from.
        return CategoryResolution {
            category,
            tier,
            provider_id: None,
            fallback_id: None,
            reason: format!(
                "No provider is bound to the '{tier}' tier and the '{category}' category has no \
                 override, so '{category}' cannot be routed. Bind one with `teton policy set-tier \
                 {tier} <provider>` or `teton policy set-category {category} <provider>`."
            ),
            outcome: RouteOutcome::NoPolicy,
        };
    };

    let via = match source {
        BindingSource::Override => "per its per-category override".to_owned(),
        BindingSource::TierInheritance => format!("through its '{tier}' tier binding"),
    };

    match screen(primary, &health, &usable) {
        Ok(ProviderHealth::Degraded) => CategoryResolution {
            category,
            tier,
            provider_id: Some(primary.to_owned()),
            fallback_id: usable_fallback(fallback, &health, &usable),
            reason: format!(
                "Routing the '{category}' category to '{primary}' {via}; its tool-calling is \
                 degraded, so the harness will use a reduced profile."
            ),
            outcome: RouteOutcome::PrimaryDegraded,
        },
        // `screen` maps `Unavailable` to `Err`, so this arm is `Healthy`.
        Ok(_) => CategoryResolution {
            category,
            tier,
            provider_id: Some(primary.to_owned()),
            fallback_id: usable_fallback(fallback, &health, &usable),
            reason: format!("Routing the '{category}' category to '{primary}' {via}."),
            outcome: RouteOutcome::Primary,
        },
        Err(rejected) => resolve_fallback(
            category, tier, primary, rejected, fallback, &health, &usable,
        ),
    }
}

/// The fallback leg: the primary was passed over, so try the fallback from the
/// **same row** and report either the failover or a failure that names both.
fn resolve_fallback<H, U>(
    category: Category,
    tier: Tier,
    primary: &str,
    rejected: Rejection,
    fallback: Option<&str>,
    health: &H,
    usable: &U,
) -> CategoryResolution
where
    H: Fn(&str) -> ProviderHealth,
    U: Fn(&str) -> bool,
{
    let why = rejected.why();
    let note = rejected.note();

    let Some(fallback) = fallback else {
        return CategoryResolution {
            category,
            tier,
            provider_id: None,
            fallback_id: None,
            reason: format!(
                "The '{category}' category resolves to '{primary}', which {why}, and no fallback \
                 provider is configured for it, so '{category}' cannot be routed.{note}"
            ),
            outcome: RouteOutcome::NoHealthyProvider,
        };
    };

    match screen(fallback, health, usable) {
        Ok(_) => CategoryResolution {
            category,
            tier,
            provider_id: Some(fallback.to_owned()),
            // The fallback has itself been used; there is no second one.
            fallback_id: None,
            reason: format!(
                "'{primary}' {why} for the '{category}' category, so it is falling back to \
                 '{fallback}'.{note}"
            ),
            outcome: RouteOutcome::Fallback,
        },
        Err(fallback_rejected) => CategoryResolution {
            category,
            tier,
            provider_id: None,
            fallback_id: None,
            reason: format!(
                "The '{category}' category resolves to '{primary}', which {why}, and its fallback \
                 '{fallback}' {fb_why}, so '{category}' cannot be routed.{note}{fb_note}",
                fb_why = fallback_rejected.why(),
                fb_note = fallback_rejected.note(),
            ),
            outcome: RouteOutcome::NoHealthyProvider,
        },
    }
}

/// The configured fallback, kept only if it would actually be routable when the
/// primary fails mid-turn. Screened here so no caller has to remember to.
fn usable_fallback<H, U>(fallback: Option<&str>, health: &H, usable: &U) -> Option<String>
where
    H: Fn(&str) -> ProviderHealth,
    U: Fn(&str) -> bool,
{
    fallback
        .filter(|id| screen(id, health, usable).is_ok())
        .map(str::to_owned)
}

/// [`Category::Redact`]'s resolution: the local tier, or nothing.
///
/// Reachable for `redact` and nothing else — it takes no category argument, so
/// no future edit can pin a second category through it by accident. It consults
/// no binding: only whether the local tier exists and can serve. When it cannot,
/// the answer is "this call cannot be served", **never** a remote provider —
/// the whole point of the category is to inspect content before it is allowed
/// to leave the machine.
fn resolve_pinned_local<H, U>(
    category: Category,
    local_provider_id: Option<&str>,
    health: &H,
    usable: &U,
) -> CategoryResolution
where
    H: Fn(&str) -> ProviderHealth,
    U: Fn(&str) -> bool,
{
    debug_assert!(
        category.configurable().is_none(),
        "resolve_pinned_local is only for categories config cannot name"
    );
    let tier = category.tier();
    // Each pinned category is pinned for its OWN reason, and the sentence has to
    // say which — REQ-544 BR-5's legibility promise is that a decision names the
    // signal that fired, not that it names *a* signal. This function was written
    // when `redact` was the only pinned category and hardcoded its justification;
    // when `route` joined it, `route` inherited redact's explanation.
    let why_pinned = match category {
        Category::Redact => {
            "it inspects content before that content is allowed to leave the machine"
        }
        Category::Route => {
            "a classifier that calls a remote model has spent what the routing decision saves"
        }
        // Unreachable while `configurable()` returns None for exactly these two;
        // the debug_assert above catches a future third.
        _ => "it is pinned to the local tier by construction",
    };

    let Some(local) = local_provider_id else {
        return CategoryResolution {
            category,
            tier,
            provider_id: None,
            fallback_id: None,
            reason: format!(
                "The '{category}' category is pinned to the local tier by construction and has no \
                 configuration path, but no local provider is registered, so '{category}' cannot \
                 be routed."
            ),
            outcome: RouteOutcome::NoHealthyProvider,
        };
    };

    match screen(local, health, usable) {
        Ok(ProviderHealth::Degraded) => CategoryResolution {
            category,
            tier,
            provider_id: Some(local.to_owned()),
            fallback_id: None,
            reason: format!(
                "The '{category}' category is pinned to the local tier by construction, so it \
                 routes to '{local}' regardless of any tier or per-category binding; its \
                 tool-calling is degraded, so the harness will use a reduced profile."
            ),
            outcome: RouteOutcome::PrimaryDegraded,
        },
        Ok(_) => CategoryResolution {
            category,
            tier,
            provider_id: Some(local.to_owned()),
            fallback_id: None,
            reason: format!(
                "The '{category}' category is pinned to the local tier by construction, so it \
                 routes to '{local}' regardless of any tier or per-category binding."
            ),
            outcome: RouteOutcome::Primary,
        },
        Err(rejected) => CategoryResolution {
            category,
            tier,
            provider_id: None,
            fallback_id: None,
            reason: format!(
                "The '{category}' category is pinned to the local tier and '{local}' {why}. It is \
                 never routed to a remote provider — {why_pinned} — so '{category}' cannot be \
                 routed.{note}",
                why = rejected.why(),
                note = rejected.note(),
            ),
            outcome: RouteOutcome::NoHealthyProvider,
        },
    }
}

// ---------------------------------------------------------------------------
// Phase → category
// ---------------------------------------------------------------------------

/// The category a **structured** turn in `phase` dispatches on (ADR-C).
///
/// A structured turn already knows what it is doing, so it derives its category
/// from its phase with no model call; only a freeform judgment turn consults the
/// classifier.
///
/// This and [`categories_for_phase`] are the *same knowledge* as BR-10's
/// migration table, written once (ADR-F). Written twice they would drift, and
/// the drift would be invisible — one is exercised at config load, the other on
/// every structured turn.
#[must_use]
pub const fn category_for_phase(phase: Phase) -> Category {
    match phase {
        Phase::Spec | Phase::Architect => Category::Design,
        Phase::Implement => Category::Edit,
        Phase::Review => Category::Review,
        Phase::Io => Category::Digest,
    }
}

/// Every category an existing phase→provider binding expands to (BR-10).
///
/// The one-to-many sibling of [`category_for_phase`], used by the migration.
/// `implement` expands to `edit` **and** `shell`, and `io` to four categories,
/// because one old knob becomes several — the migration reports each expansion
/// by name so a user who wanted them to differ knows to split them.
///
/// There is no `freeform` leg: a `[[routing]]` rule targeting it has never
/// loaded (rejected by `Config::validate` before ADR-G, by serde after it), so
/// no such row can exist to migrate — see the architecture's "Corrections to the
/// Requirement". The migration handles the five phases that can appear.
#[must_use]
pub const fn categories_for_phase(phase: Phase) -> &'static [Category] {
    match phase {
        Phase::Spec | Phase::Architect => &[Category::Design],
        Phase::Implement => &[Category::Edit, Category::Shell],
        Phase::Review => &[Category::Review],
        Phase::Io => &[
            Category::Digest,
            Category::Triage,
            Category::Title,
            Category::Compact,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers -----------------------------------------------------------

    const LOCAL: &str = "on-device";

    fn healthy(_: &str) -> ProviderHealth {
        ProviderHealth::Healthy
    }

    fn all_usable(_: &str) -> bool {
        true
    }

    /// A table binding every tier to `provider`, with a local tier registered.
    fn tiers_bound_to(provider: &str) -> CategoryTable {
        Tier::ALL.into_iter().fold(
            CategoryTable::new().with_local_provider(LOCAL),
            |t, tier| {
                t.with_tier(TierBinding {
                    tier,
                    provider_id: provider.to_owned(),
                    fallback_id: None,
                })
            },
        )
    }

    /// BR-5: `route` MUST resolve to the local tier. A user who binds the
    /// `reflex` tier to a remote provider must NOT get a remote classifier — "a
    /// router that calls a remote model to decide has spent what it was saving".
    #[test]
    fn route_never_resolves_to_a_remote_provider() {
        let table = tiers_bound_to("frontier-remote");
        let r = resolve(
            Category::Route,
            &table,
            |_| ProviderHealth::Healthy,
            |_| true,
        );
        assert_eq!(
            r.provider_id.as_deref(),
            Some(LOCAL),
            "route resolved to {:?}, but BR-5 forbids classification ever going \
             remote — the decision costs more than it saves",
            r.provider_id
        );
    }

    /// The category as it appears inside a reason sentence. Quoted, so that
    /// asserting `route` names itself is not satisfied by the word "Routing".
    fn named(category: Category) -> String {
        format!("'{}'", category.as_str())
    }

    /// The provider a category resolves to in `tiers_bound_to(p)` — `p` for the
    /// ten configurable ones, the local tier for the pinned one.
    fn expected_provider(category: Category, bound: &str) -> &str {
        if category.configurable().is_some() {
            bound
        } else {
            LOCAL
        }
    }

    // -- the vocabulary ----------------------------------------------------

    #[test]
    fn every_enum_lists_each_variant_exactly_once() {
        assert_eq!(Category::ALL.len(), 11);
        assert_eq!(ConfigurableCategory::ALL.len(), 9);
        assert_eq!(JudgmentCategory::ALL.len(), 4);
        assert_eq!(Tier::ALL.len(), 4);

        for (i, a) in Category::ALL.iter().enumerate() {
            for b in &Category::ALL[i + 1..] {
                assert_ne!(a, b);
                assert_ne!(a.as_str(), b.as_str());
            }
        }
        for (i, a) in ConfigurableCategory::ALL.iter().enumerate() {
            for b in &ConfigurableCategory::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn redact_and_route_are_the_categories_config_cannot_name() {
        // ADR-B: the pin is a type, not a check. `ConfigurableCategory` has no
        // `Redact` and no `Route` variant, so a binding for either is
        // unrepresentable — `redact` because it inspects content before egress,
        // `route` because BR-5 forbids classification ever going remote, which
        // leaves no valid state a binding could express.
        for c in ConfigurableCategory::ALL {
            assert_ne!(Category::from(c), Category::Redact, "{c} maps to redact");
            assert_ne!(Category::from(c), Category::Route, "{c} maps to route");
        }
        assert!(Category::Redact.configurable().is_none());
        assert!(Category::Route.configurable().is_none());
        for category in Category::ALL {
            if matches!(category, Category::Redact | Category::Route) {
                continue;
            }
            let configurable = category
                .configurable()
                .unwrap_or_else(|| panic!("{category} has no configurable counterpart"));
            // The conversion is total in the configurable → category direction.
            assert_eq!(Category::from(configurable), category);
            assert_eq!(configurable.as_str(), category.as_str());
        }
    }

    #[test]
    fn origin_split_matches_the_requirement_table() {
        // Seven harness-known, four intent-classified — the requirement's table
        // spelled out, so a variant added on the wrong side fails here.
        let harness_known = [
            Category::Route,
            Category::Redact,
            Category::Title,
            Category::Digest,
            Category::Compact,
            Category::Triage,
            Category::Shell,
        ];
        let intent_classified = [
            Category::Edit,
            Category::Design,
            Category::Debug,
            Category::Review,
        ];
        for category in harness_known {
            assert_eq!(
                category.origin(),
                CategoryOrigin::HarnessKnown,
                "{category} must be harness-known"
            );
        }
        for category in intent_classified {
            assert_eq!(
                category.origin(),
                CategoryOrigin::IntentClassified,
                "{category} must be intent-classified"
            );
        }
        assert_eq!(harness_known.len() + intent_classified.len(), 11);
        assert_eq!(
            Category::ALL
                .iter()
                .filter(|c| c.origin() == CategoryOrigin::HarnessKnown)
                .count(),
            7
        );
        assert_eq!(CategoryOrigin::HarnessKnown.as_str(), "harness_known");
        assert_eq!(
            CategoryOrigin::IntentClassified.as_str(),
            "intent_classified"
        );
    }

    #[test]
    fn tiers_match_the_requirement_table() {
        for (category, tier) in [
            (Category::Route, Tier::Reflex),
            (Category::Redact, Tier::Reflex),
            (Category::Title, Tier::Reflex),
            (Category::Digest, Tier::Scan),
            (Category::Compact, Tier::Scan),
            (Category::Triage, Tier::Scan),
            (Category::Edit, Tier::Build),
            (Category::Shell, Tier::Build),
            (Category::Design, Tier::Think),
            (Category::Debug, Tier::Think),
            (Category::Review, Tier::Think),
        ] {
            assert_eq!(category.tier(), tier, "{category}");
        }
    }

    #[test]
    fn the_judgment_four_are_exactly_the_intent_classified_categories() {
        // AC-3's other half: the classifier's return type cannot name a
        // harness-known category, and it covers every one it should.
        let mut from_judgment: Vec<Category> = JudgmentCategory::ALL
            .into_iter()
            .map(Category::from)
            .collect();
        let mut intent: Vec<Category> = Category::ALL
            .into_iter()
            .filter(|c| c.origin() == CategoryOrigin::IntentClassified)
            .collect();
        from_judgment.sort_by_key(|c| c.as_str());
        intent.sort_by_key(|c| c.as_str());
        assert_eq!(from_judgment, intent);
        assert_eq!(Category::from(JudgmentCategory::default()), Category::Edit);
    }

    #[test]
    fn no_harness_known_category_can_be_named_by_text() {
        // AC-3 / BR-2, asserted **per category** rather than once: the only
        // parse into the category space is `JudgmentCategory`, and every
        // harness-known name is rejected by it.
        for category in Category::ALL {
            let parsed = category.as_str().parse::<JudgmentCategory>();
            match category.origin() {
                CategoryOrigin::HarnessKnown => {
                    assert!(
                        parsed.is_err(),
                        "{category} is harness-known but parses from text"
                    );
                }
                CategoryOrigin::IntentClassified => {
                    assert_eq!(Category::from(parsed.expect("judgment")), category);
                }
            }
        }
        // A model that answers with something else is an error, not a guess.
        assert!("summarize this".parse::<JudgmentCategory>().is_err());
    }

    #[test]
    fn parsing_redact_as_a_configurable_category_names_the_pin() {
        // AC-4's message quality: forbidden, not misspelled.
        let err = "redact".parse::<ConfigurableCategory>().unwrap_err();
        assert_eq!(err, ParseCategoryError::RedactIsPinned);
        let text = err.to_string();
        assert!(text.contains("redact"), "{text}");
        assert!(text.contains("pinned"), "{text}");

        let unknown = "nonsense".parse::<ConfigurableCategory>().unwrap_err();
        let text = unknown.to_string();
        assert!(text.contains("nonsense") && text.contains("edit"), "{text}");
        // The list of accepted names never advertises a pinned category.
        assert!(!text.contains("redact"), "{text}");
        assert!(!text.contains("route"), "{text}");
    }

    #[test]
    fn every_pinned_category_names_its_own_pin() {
        // AC-4 asserted per pinned category rather than for `redact` alone
        // (LESSON-479: a subset invariant only holds where you iterate). TWO
        // categories are unrepresentable in config, and a user who writes
        // either must learn the key is forbidden, not misspelled.
        let pinned: Vec<Category> = Category::ALL
            .into_iter()
            .filter(|c| c.configurable().is_none())
            .collect();
        assert_eq!(
            pinned,
            vec![Category::Route, Category::Redact],
            "the pinned set changed; each pinned category needs its own \
             ParseCategoryError sentence"
        );

        for category in pinned {
            let err = category
                .as_str()
                .parse::<ConfigurableCategory>()
                .unwrap_err();
            assert!(
                !matches!(err, ParseCategoryError::Unknown { .. }),
                "{category} is pinned but reads as a typo: {err}"
            );
            let text = err.to_string();
            assert!(text.contains(category.as_str()), "{category}: {text}");
            assert!(text.contains("pinned"), "{category}: {text}");
        }

        // And each names its OWN reason — `route` must not inherit `redact`'s.
        assert_eq!(
            "route".parse::<ConfigurableCategory>().unwrap_err(),
            ParseCategoryError::RouteIsPinned
        );
        let route = ParseCategoryError::RouteIsPinned.to_string();
        assert!(
            route.contains("classifier"),
            "route's pin must cite BR-5's reason, not redact's: {route}"
        );
        assert!(
            !route.contains("leave the machine"),
            "route inherited redact's explanation: {route}"
        );
    }

    #[test]
    fn tiers_and_categories_round_trip_through_their_names() {
        for tier in Tier::ALL {
            assert_eq!(tier.as_str().parse::<Tier>().unwrap(), tier);
            assert_eq!(tier.to_string(), tier.as_str());
        }
        assert!("frontier".parse::<Tier>().is_err());
        for c in ConfigurableCategory::ALL {
            assert_eq!(c.as_str().parse::<ConfigurableCategory>().unwrap(), c);
        }
        for c in JudgmentCategory::ALL {
            assert_eq!(c.as_str().parse::<JudgmentCategory>().unwrap(), c);
        }
    }

    #[test]
    fn tier_and_configurable_category_round_trip_through_toml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            tier: Tier,
            name: ConfigurableCategory,
            judgment_default: JudgmentCategory,
        }
        let wrap = Wrap {
            tier: Tier::Think,
            name: ConfigurableCategory::Design,
            judgment_default: JudgmentCategory::Edit,
        };
        let text = toml::to_string(&wrap).unwrap();
        assert!(text.contains("think") && text.contains("design"), "{text}");
        assert_eq!(toml::from_str::<Wrap>(&text).unwrap(), wrap);

        // A config naming a pinned category cannot deserialize — the pin, as a
        // type — and the message says *pinned*, not "unknown variant" (AC-4).
        for pinned in ["redact", "route"] {
            let err = toml::from_str::<Wrap>(&format!(
                "tier = \"think\"\nname = \"{pinned}\"\njudgment_default = \"edit\"\n"
            ))
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains(pinned), "{text}");
            assert!(
                text.contains("pinned"),
                "a TOML rejection that reads as a typo: {text}"
            );
        }
    }

    // -- resolution: the AC-2 table ----------------------------------------

    #[test]
    fn every_category_resolves_through_override_tier_and_declared_error() {
        // AC-2 / BR-12: all eleven categories × (per-category override / tier
        // inheritance / unresolvable), asserting provider, tier, outcome and a
        // non-empty reason for each.
        for category in Category::ALL {
            // --- 1. tier inheritance -------------------------------------
            let inherited = tiers_bound_to("tier-provider");
            let r = resolve(category, &inherited, healthy, all_usable);
            assert_eq!(r.category, category);
            assert_eq!(r.tier, category.tier(), "{category}");
            assert_eq!(
                r.provider_id.as_deref(),
                Some(expected_provider(category, "tier-provider")),
                "{category} via tier inheritance"
            );
            assert_eq!(r.outcome, RouteOutcome::Primary, "{category}");
            assert!(r.outcome.selected_provider(), "{category}");
            assert!(!r.reason.is_empty(), "{category}");
            assert!(
                r.reason.contains(&named(category)),
                "{category}: {}",
                r.reason
            );
            assert!(r.reason.ends_with('.'), "{category}: {}", r.reason);

            // --- 2. per-category override --------------------------------
            let overridden = match category.configurable() {
                Some(name) => inherited.clone().with_override(CategoryOverride {
                    name,
                    provider_id: "override-provider".to_owned(),
                    fallback_id: None,
                }),
                // `redact` has no representable override — that is the point.
                None => inherited.clone(),
            };
            let r = resolve(category, &overridden, healthy, all_usable);
            assert_eq!(
                r.provider_id.as_deref(),
                Some(expected_provider(category, "override-provider")),
                "{category} via per-category override"
            );
            assert_eq!(r.tier, category.tier(), "{category}");
            assert_eq!(r.outcome, RouteOutcome::Primary, "{category}");
            assert!(
                r.reason.contains(&named(category)),
                "{category}: {}",
                r.reason
            );
            assert!(!r.reason.is_empty(), "{category}");

            // --- 3. unresolvable -----------------------------------------
            // Remove only the binding this category depends on; every other
            // tier stays bound, so a category that "resolves" here has borrowed
            // someone else's provider.
            let mut stripped = tiers_bound_to("tier-provider");
            stripped.tiers.retain(|t| t.tier != category.tier());
            stripped.local_provider_id = None;
            let r = resolve(category, &stripped, healthy, all_usable);
            assert_eq!(r.provider_id, None, "{category} must not synthesize");
            assert_eq!(r.fallback_id, None, "{category}");
            assert_eq!(r.tier, category.tier(), "{category}");
            assert_eq!(
                r.outcome,
                // Nothing is bound for the nine configurable ones; for the two
                // pinned ones there is nothing to bind, only a local tier that
                // is not there.
                if matches!(category, Category::Redact | Category::Route) {
                    RouteOutcome::NoHealthyProvider
                } else {
                    RouteOutcome::NoPolicy
                },
                "{category}"
            );
            assert!(!r.outcome.selected_provider(), "{category}");
            assert!(!r.reason.is_empty(), "{category}");
            assert!(r.reason.ends_with('.'), "{category}: {}", r.reason);
        }
    }

    #[test]
    fn removing_a_binding_makes_each_category_name_itself() {
        // BR-8, asserted **per category** rather than once (LESSON-479: a
        // subset invariant only holds where you iterate).
        for category in Category::ALL {
            let mut table = tiers_bound_to("tier-provider");
            table.tiers.retain(|t| t.tier != category.tier());
            table.local_provider_id = None;

            let r = resolve(category, &table, healthy, all_usable);
            assert!(
                r.reason.contains(&named(category)),
                "{category} does not name itself: {}",
                r.reason
            );
            assert!(r.provider_id.is_none(), "{category}");
            // The unset binding is named too: the tier for the nine
            // configurable ones, the local tier for the two pinned ones.
            if matches!(category, Category::Redact | Category::Route) {
                assert!(
                    r.reason.contains("local"),
                    "{category} must name the local tier: {}",
                    r.reason
                );
            } else {
                assert!(
                    r.reason.contains(&format!("'{}'", category.tier())),
                    "{category} must name its tier: {}",
                    r.reason
                );
            }
        }
    }

    #[test]
    fn resolution_never_names_a_provider_the_table_does_not_contain() {
        // BR-8's "no synthesized provider id at any step", swept across the
        // shapes that historically produced one (BUG-146: a fallback identifier
        // standing in for an absent choice).
        let tables = [
            CategoryTable::new(),
            CategoryTable::new().with_local_provider(LOCAL),
            tiers_bound_to("tier-provider"),
            tiers_bound_to("tier-provider").with_override(CategoryOverride {
                name: ConfigurableCategory::Edit,
                provider_id: "override-provider".to_owned(),
                fallback_id: Some("fallback-provider".to_owned()),
            }),
        ];
        let healths = [
            ProviderHealth::Healthy,
            ProviderHealth::Degraded,
            ProviderHealth::Unavailable,
        ];
        for table in &tables {
            let mut known: Vec<&str> = table.tiers.iter().map(|t| t.provider_id.as_str()).collect();
            known.extend(table.tiers.iter().filter_map(|t| t.fallback_id.as_deref()));
            known.extend(table.categories.iter().map(|c| c.provider_id.as_str()));
            known.extend(
                table
                    .categories
                    .iter()
                    .filter_map(|c| c.fallback_id.as_deref()),
            );
            known.extend(table.local_provider_id.as_deref());
            for health in healths {
                for usable in [true, false] {
                    for category in Category::ALL {
                        let r = resolve(category, table, |_| health, |_| usable);
                        if let Some(id) = r.provider_id.as_deref() {
                            assert!(known.contains(&id), "synthesized '{id}' for {category}");
                        }
                        if let Some(id) = r.fallback_id.as_deref() {
                            assert!(known.contains(&id), "synthesized fallback '{id}'");
                        }
                        assert!(!r.reason.is_empty(), "{category}");
                    }
                }
            }
        }
    }

    // -- resolution: the redact pin (AC-4) ---------------------------------

    #[test]
    fn redact_stays_local_when_every_tier_is_bound_to_a_remote_provider() {
        // AC-4: the resolution function returns the local provider for `redact`
        // even when every tier names a frontier model.
        let table = tiers_bound_to("frontier-remote");
        let r = resolve(Category::Redact, &table, healthy, all_usable);
        assert_eq!(r.provider_id.as_deref(), Some(LOCAL));
        assert_eq!(r.outcome, RouteOutcome::Primary);
        assert_eq!(r.tier, Tier::Reflex);
        assert!(r.reason.contains("pinned"), "{}", r.reason);
        assert!(!r.reason.contains("frontier-remote"), "{}", r.reason);
    }

    #[test]
    fn redact_ignores_every_representable_override() {
        // Nothing a config can say moves `redact`: bind all ten categories and
        // all four tiers to a remote provider and it still resolves local.
        let mut table = tiers_bound_to("frontier-remote");
        for name in ConfigurableCategory::ALL {
            table = table.with_override(CategoryOverride {
                name,
                provider_id: "frontier-remote".to_owned(),
                fallback_id: Some("another-remote".to_owned()),
            });
        }
        let r = resolve(Category::Redact, &table, healthy, all_usable);
        assert_eq!(r.provider_id.as_deref(), Some(LOCAL));
        assert_eq!(r.fallback_id, None, "the pin has no remote fallback");
    }

    #[test]
    fn redact_fails_closed_rather_than_borrowing_a_remote_provider() {
        // An unavailable local tier makes `redact` unroutable. It never falls
        // back to a tier binding — that would send the content it exists to
        // guard.
        let table = tiers_bound_to("frontier-remote");
        for (health, usable) in [
            (ProviderHealth::Unavailable, true),
            (ProviderHealth::Healthy, false),
        ] {
            let r = resolve(
                Category::Redact,
                &table,
                |id| {
                    if id == LOCAL {
                        health
                    } else {
                        ProviderHealth::Healthy
                    }
                },
                |id| if id == LOCAL { usable } else { true },
            );
            assert_eq!(r.provider_id, None, "{health:?}/{usable}");
            assert_eq!(r.outcome, RouteOutcome::NoHealthyProvider);
            assert!(r.reason.contains(&named(Category::Redact)), "{}", r.reason);
            assert!(!r.reason.contains("frontier-remote"), "{}", r.reason);
        }

        // No local tier at all: the same answer, naming the missing local tier
        // rather than borrowing one of the four bound remote providers.
        let mut table = tiers_bound_to("frontier-remote");
        table.local_provider_id = None;
        let r = resolve(Category::Redact, &table, healthy, all_usable);
        assert_eq!(r.provider_id, None);
        assert_eq!(r.outcome, RouteOutcome::NoHealthyProvider);
        assert!(r.reason.contains("no local provider"), "{}", r.reason);
        assert!(!r.reason.contains("frontier-remote"), "{}", r.reason);
    }

    // -- resolution: the usability screen (ADR-E) --------------------------

    #[test]
    fn an_unusable_primary_is_never_selected_for_any_category() {
        // ADR-E / BUG-155: a remote provider that declares no `model` is not
        // routable, and this dispatch path screens it out by construction.
        // Asserted per category so the screen cannot hold for only some.
        for category in Category::ALL {
            let table = tiers_bound_to("no-model").with_local_provider("no-model");
            let r = resolve(category, &table, healthy, |_| false);
            assert_eq!(
                r.provider_id, None,
                "{category} selected an unusable provider"
            );
            assert_eq!(r.outcome, RouteOutcome::NoHealthyProvider, "{category}");
            assert!(
                r.reason.contains(&named(category)),
                "{category}: {}",
                r.reason
            );
            assert!(
                r.reason.contains("not routable"),
                "{category} must say why: {}",
                r.reason
            );
        }
    }

    #[test]
    fn an_unusable_fallback_is_never_selected() {
        let table = tiers_bound_to("primary").with_override(CategoryOverride {
            name: ConfigurableCategory::Edit,
            provider_id: "primary".to_owned(),
            fallback_id: Some("no-model".to_owned()),
        });
        let r = resolve(
            Category::Edit,
            &table,
            |id| {
                if id == "primary" {
                    ProviderHealth::Unavailable
                } else {
                    ProviderHealth::Healthy
                }
            },
            |id| id != "no-model",
        );
        assert_eq!(r.provider_id, None);
        assert_eq!(r.outcome, RouteOutcome::NoHealthyProvider);
        assert!(r.reason.contains("no-model"), "{}", r.reason);
        assert!(r.reason.contains("not routable"), "{}", r.reason);

        // And a healthy-but-unusable fallback is not carried on a successful
        // resolution either — the caller must not fail over to it mid-turn.
        let r = resolve(Category::Edit, &table, healthy, |id| id != "no-model");
        assert_eq!(r.provider_id.as_deref(), Some("primary"));
        assert_eq!(r.fallback_id, None);
    }

    // -- resolution: health, fallback, precedence --------------------------

    #[test]
    fn a_degraded_primary_is_kept_not_failed_over() {
        let table = tiers_bound_to("primary").with_override(CategoryOverride {
            name: ConfigurableCategory::Design,
            provider_id: "primary".to_owned(),
            fallback_id: Some("fallback".to_owned()),
        });
        let r = resolve(
            Category::Design,
            &table,
            |id| {
                if id == "primary" {
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Healthy
                }
            },
            all_usable,
        );
        assert_eq!(r.provider_id.as_deref(), Some("primary"));
        assert_eq!(r.outcome, RouteOutcome::PrimaryDegraded);
        assert_eq!(r.fallback_id.as_deref(), Some("fallback"));
        assert!(r.reason.contains("degraded"), "{}", r.reason);
        assert!(r.reason.contains("reduced profile"), "{}", r.reason);
    }

    #[test]
    fn an_unavailable_primary_falls_back_within_its_own_row() {
        for (tier_fallback, category_fallback, expected) in [
            (Some("tier-fallback"), None, None),
            (
                Some("tier-fallback"),
                Some("cat-fallback"),
                Some("cat-fallback"),
            ),
            (None, Some("cat-fallback"), Some("cat-fallback")),
        ] {
            let table = CategoryTable::new()
                .with_local_provider(LOCAL)
                .with_tier(TierBinding {
                    tier: Tier::Build,
                    provider_id: "tier-primary".to_owned(),
                    fallback_id: tier_fallback.map(str::to_owned),
                })
                .with_override(CategoryOverride {
                    name: ConfigurableCategory::Edit,
                    provider_id: "cat-primary".to_owned(),
                    fallback_id: category_fallback.map(str::to_owned),
                });
            let r = resolve(
                Category::Edit,
                &table,
                |id| {
                    if id == "cat-primary" {
                        ProviderHealth::Unavailable
                    } else {
                        ProviderHealth::Healthy
                    }
                },
                all_usable,
            );
            // The fallback comes from the same row as the primary: an override
            // that fails does not quietly borrow the tier's fallback.
            assert_eq!(r.provider_id.as_deref(), expected, "{tier_fallback:?}");
            if expected.is_some() {
                assert_eq!(r.outcome, RouteOutcome::Fallback);
                assert!(r.reason.contains("falling back"), "{}", r.reason);
            } else {
                assert_eq!(r.outcome, RouteOutcome::NoHealthyProvider);
                assert!(r.reason.contains("no fallback"), "{}", r.reason);
            }
        }
    }

    #[test]
    fn an_override_does_not_fall_through_to_the_tier_binding() {
        // The user's explicit per-category choice is not silently replaced by
        // the inherited one; the failure names the override's provider.
        let table = tiers_bound_to("tier-provider").with_override(CategoryOverride {
            name: ConfigurableCategory::Review,
            provider_id: "chosen".to_owned(),
            fallback_id: None,
        });
        let r = resolve(
            Category::Review,
            &table,
            |id| {
                if id == "chosen" {
                    ProviderHealth::Unavailable
                } else {
                    ProviderHealth::Healthy
                }
            },
            all_usable,
        );
        assert_eq!(r.provider_id, None);
        assert!(r.reason.contains("chosen"), "{}", r.reason);
        assert!(!r.reason.contains("tier-provider"), "{}", r.reason);
    }

    #[test]
    fn an_override_only_binds_its_own_category() {
        let table = tiers_bound_to("tier-provider").with_override(CategoryOverride {
            name: ConfigurableCategory::Debug,
            provider_id: "long-context".to_owned(),
            fallback_id: None,
        });
        let debug = resolve(Category::Debug, &table, healthy, all_usable);
        assert_eq!(debug.provider_id.as_deref(), Some("long-context"));
        assert!(debug.reason.contains("override"), "{}", debug.reason);
        // `design` shares the `think` tier and must be untouched.
        let design = resolve(Category::Design, &table, healthy, all_usable);
        assert_eq!(design.provider_id.as_deref(), Some("tier-provider"));
        assert!(design.reason.contains("'think'"), "{}", design.reason);
    }

    #[test]
    fn a_bound_tier_serves_only_its_own_categories() {
        let table = CategoryTable::new()
            .with_local_provider(LOCAL)
            .with_tier(TierBinding {
                tier: Tier::Think,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
        for category in Category::ALL {
            let r = resolve(category, &table, healthy, all_usable);
            match (category.tier(), category) {
                // Both pinned-local categories ignore the table entirely.
                (_, Category::Redact | Category::Route) => {
                    assert_eq!(r.provider_id.as_deref(), Some(LOCAL))
                }
                (Tier::Think, _) => assert_eq!(r.provider_id.as_deref(), Some("frontier")),
                _ => assert_eq!(r.provider_id, None, "{category} borrowed a binding"),
            }
        }
    }

    // -- phase → category (ADR-F) ------------------------------------------

    #[test]
    fn category_for_phase_is_total_and_matches_the_migration_table() {
        for phase in Phase::ALL {
            // Total: every phase has a category (a `match` with no catch-all
            // makes this a compile-time fact; this asserts the values).
            let category = category_for_phase(phase);
            assert!(Category::ALL.contains(&category), "{phase}");
        }
        assert_eq!(category_for_phase(Phase::Spec), Category::Design);
        assert_eq!(category_for_phase(Phase::Architect), Category::Design);
        assert_eq!(category_for_phase(Phase::Implement), Category::Edit);
        assert_eq!(category_for_phase(Phase::Review), Category::Review);
        assert_eq!(category_for_phase(Phase::Io), Category::Digest);
    }

    #[test]
    fn phase_expansions_match_br_10() {
        assert_eq!(categories_for_phase(Phase::Spec), [Category::Design]);
        assert_eq!(categories_for_phase(Phase::Architect), [Category::Design]);
        assert_eq!(
            categories_for_phase(Phase::Implement),
            [Category::Edit, Category::Shell]
        );
        assert_eq!(categories_for_phase(Phase::Review), [Category::Review]);
        assert_eq!(
            categories_for_phase(Phase::Io),
            [
                Category::Digest,
                Category::Triage,
                Category::Title,
                Category::Compact
            ]
        );
        // Five legs, not six: a `[[routing]]` rule naming `freeform` has never
        // loaded, so there is nothing to migrate, and ADR-G removed the variant
        // that would have named it (architecture: Corrections to the
        // Requirement).
        assert_eq!(Phase::ALL.len(), 5);
    }

    #[test]
    fn dispatch_and_migration_agree_on_every_live_phase() {
        // ADR-F: one knowledge, two uses. The dispatch category is the head of
        // the migration expansion, so the two cannot drift. Every phase is a
        // live phase now that `Freeform` is retired — there is no arm to skip.
        for phase in Phase::ALL {
            assert_eq!(
                categories_for_phase(phase).first(),
                Some(&category_for_phase(phase)),
                "{phase}"
            );
        }
    }
}
