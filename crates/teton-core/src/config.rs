//! The on-disk TOML configuration schema and its validation.
//!
//! The config file declares providers, the tier → provider table with its
//! per-category overrides (REQ-558), privacy boundaries, and the opt-in
//! web-lookup ceiling (REQ-563). The pre-REQ-558 phase → provider table
//! (`[[routing]]`) is still *read* here so TASK-055's migration has something to
//! migrate; nothing dispatches on it.
//!
//! It never holds a raw credential (BR-7): providers carry
//! an `auth_ref` — a reference into the OS keychain (or an `env:`/`op://`
//! reference) — and [`Config::validate`] accepts an `auth_ref` only if it matches
//! a recognized reference form (a positive scheme allowlist), rejecting anything
//! else — a raw key or a fake-scheme value — so a credential can never be
//! persisted to a plaintext config. The `[web] search_key_ref` key (REQ-563
//! BR-8) is a second credential-bearing field and is held to the same rule by
//! the *same* predicate — one definition of "this is a reference, not a secret",
//! so the newer field cannot drift into a weaker one.
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
use crate::entities::{BoundaryOrigin, ModelProvider, PrivacyBoundary};
use crate::mcp::{McpServerConfig, McpTransport};
use crate::phase::Phase;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use teton_protocol::permissions::PermissionLevel;

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

/// Privacy behaviour the user opts into (the `[privacy]` table).
///
/// # Why this is not a `[[categories]]` row (REQ-562 BR-10)
///
/// Two different questions live one keystroke apart here, and conflating them
/// would undo REQ-558 ADR-B:
///
/// - *Which provider serves `redact`?* — **unanswerable by configuration.**
///   [`ConfigurableCategory`] has no `Redact` variant, so the binding is
///   unrepresentable rather than rejected.
/// - *Does the scan run at all?* — this table.
///
/// Putting the opt-in in `[[categories]]` would have made `redact`
/// deserializable as a configurable category again, reopening exactly the
/// surface ADR-B deleted. So the switch lives in its own table, and that table
/// deliberately carries **no provider, model, or tier key** — there is nothing
/// here for a binding to hide in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PrivacyConfig {
    /// Run the REQ-562 redaction scan inside the egress choke point.
    ///
    /// **Defaults to `false`** (OQ-3), and "off" means the scan does not exist
    /// rather than that it runs and permits: the daemon installs the gate only
    /// when this is true (ADR-2), so an un-opted-in machine makes zero scanner
    /// calls, loads no weights, and pays no latency.
    ///
    /// The default is load-bearing for a second reason. With the scan enabled, a
    /// redactor that *cannot* run blocks the payload (BR-3, fail-closed) — a
    /// posture that is only affordable because nobody who has not opted in is
    /// affected by it.
    ///
    /// Serialized unconditionally within the table (no `skip_serializing_if`),
    /// like [`LocalModelConfig::auto_accept`]: a config that names `[privacy]`
    /// at all states its posture rather than leaving it to be inferred.
    #[serde(default)]
    pub redact: bool,

    /// Turn off the shipped default boundary set (REQ-597 BR-3).
    ///
    /// **Defaults to `false`**, which is the whole point of REQ-597: on a stock
    /// install the thirteen [`DEFAULT_BOUNDARIES`] globs are in force, so
    /// `~/.ssh/id_rsa`, a project `.env`, and `~/.aws/credentials` are
    /// `local-only` without the user having marked anything.
    ///
    /// This is the one and only route to an empty boundary set — there is no
    /// implicit path, and no heuristic that guesses which machines are safe.
    /// That shape is deliberate: it is what BUG-202 settled on for
    /// `allow_cleartext`, a secure default plus one explicit, greppable opt-out
    /// (LESSON-578).
    ///
    /// Setting it accepts a real consequence, which is why it is a key and not
    /// a flag: with no user rows declared it leaves the session with **no**
    /// boundaries at all, and a session rooted at `$HOME` or `/` in that state
    /// emits `unbounded_root_warning` (BR-5).
    #[serde(default)]
    pub disable_default_boundaries: bool,
}

/// The shipped `local-only` boundary set (REQ-597 BR-1).
///
/// Thirteen repo-root-relative globs covering the credential-shaped paths an
/// agent reads by accident: SSH and signing keys, `.env` files, cloud and
/// registry credentials, and container/cluster configs.
///
/// # Why these match at the repo root
///
/// Every entry is `**/`-prefixed, and under `globset` with
/// `literal_separator(true)` a leading `**/` matches **zero** or more leading
/// directories. So `**/.ssh/**` covers `.ssh/id_rsa` at the root as well as
/// `vendor/fixtures/.ssh/id_rsa` beneath it. This was verified against the real
/// crate before the list was fixed, because the whole REQ rests on it.
///
/// # What they deliberately do not match
///
/// `src/main.rs`, `README.md`, a file literally named `env`, and `notes/.envrc`
/// — pinned by [`tests::default_boundaries_match_credentials_and_spare_sources`].
///
/// # The accepted false positive
///
/// `**/*.pem` and `**/*.key` are broad, and they will match ordinary test
/// fixtures. That is the spec's central judgment, not an oversight: a blocked
/// `.env` that the user wanted to send comes with a clear message and an
/// opt-out ([`PrivacyConfig::disable_default_boundaries`]), and a silent
/// credential leak does not.
///
/// Order matters only relative to the user's rows, never within this list: a
/// path matching two builtins resolves to the earlier one, and both are
/// `local-only`, so the outcome is identical either way.
pub const DEFAULT_BOUNDARIES: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/.ssh/**",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/.aws/**",
    "**/.npmrc",
    "**/.netrc",
    "**/.git-credentials",
    "**/.docker/config.json",
    "**/.kube/config",
];

/// Opt-in spend behaviour (`[cost]`) — today, REQ-588's per-prompt ceiling.
///
/// **Absent means no ceiling**, and "off" means the check does not exist rather
/// than that it runs and permits: with no ceiling configured the choke point
/// builds no accumulator and performs no pricing lookup, so an un-opted-in
/// machine pays nothing — the same posture `[privacy] redact` takes, and for
/// the same reason (REQ-588 ADR-6, OQ-3).
// `PartialEq` without `Eq`, matching `Config` itself: the ceiling is a
// dollar figure and `f64` is not `Eq`. Nothing compares two ceilings for
// exact equality outside `is_unset`, and the *arithmetic* that decides a
// refusal runs on integral micro-cents, never on this field (ADR-3).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CostConfig {
    /// The most one **prompt** may spend, in US dollars (REQ-588 OQ-1/OQ-2).
    ///
    /// Per *prompt* — the unit the user initiates. A turn is an implementation
    /// detail of the loop, and a session is long enough that a ceiling on it
    /// would bind at an arbitrary moment days later.
    ///
    /// Dollars at this edge because that is what a person types; converted to
    /// integral micro-cents immediately (see [`Self::ceiling_micro_cents`]) so
    /// no float reaches the arithmetic that decides a refusal.
    ///
    /// **What it actually promises**, which the docs also say: the ceiling is a
    /// floor crossing, not a prediction. A call's cost depends on its *output*
    /// tokens, which nobody can price in advance, so the rule is "refuse the
    /// next call once this prompt's recorded spend has reached the ceiling" —
    /// and a prompt can therefore overshoot by at most one call (ADR-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ceiling_usd: Option<f64>,
}

impl CostConfig {
    /// Whether every field still holds its default, so an un-opted-in config
    /// carries no `[cost]` table at all.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The ceiling in integral micro-cents, or `None` when none is configured.
    ///
    /// **The one conversion**, at the edge. Dollars are what a person types and
    /// the worst thing to do arithmetic in: a ceiling compared in floating
    /// point would refuse or permit differently depending on how the spend was
    /// accumulated. Micro-cents make the comparison exact and the accumulator
    /// an integer.
    ///
    /// A non-finite or negative value yields `None` rather than a nonsense
    /// ceiling; [`Config::validate`] refuses it outright first, so this is the
    /// belt to that braces.
    #[must_use]
    pub fn ceiling_micro_cents(&self) -> Option<u64> {
        let usd = self.prompt_ceiling_usd?;
        if !usd.is_finite() || usd < 0.0 {
            return None;
        }
        // 1 USD = 100 cents = 100_000 micro-cents.
        Some((usd * 100_000.0).round() as u64)
    }
}

impl PrivacyConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[privacy]` table out of a config that never opted in — the same
    /// treatment [`LocalModelConfig::is_unset`] gives `[local_model]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// How the daemon's shutdown policy is spelled in TOML (`[lifetime] shutdown`).
///
/// The wire spelling is separate from [`crate::lifetime::ShutdownPolicy`]
/// because the runtime type carries the linger window *inside* the `Linger`
/// variant, which is the right shape for the state machine (a `Linger` with no
/// window is unrepresentable) and the wrong shape for TOML, where the mode and
/// the window are two keys. [`LifetimeConfig::policy`] is the one place they
/// are joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShutdownPolicyKind {
    /// Exit as soon as the last client disconnects (the shipped default).
    #[default]
    OnLastDisconnect,
    /// Exit `linger_seconds` after the last client disconnects.
    Linger,
    /// Never self-terminate — the `brew services` always-on opt-in (BR-5).
    Never,
}

impl ShutdownPolicyKind {
    /// The three accepted spellings, for error messages that have to name them.
    pub const SPELLINGS: [&'static str; 3] = ["on-last-disconnect", "linger", "never"];

    /// Parse a flag or environment value.
    ///
    /// Shared by the `--shutdown-policy` flag and `TETON_SHUTDOWN_POLICY` so the
    /// two cannot drift into accepting different spellings of the same mode.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "on-last-disconnect" => Some(Self::OnLastDisconnect),
            "linger" => Some(Self::Linger),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Daemon lifetime behaviour (the `[lifetime]` table, REQ-565 BR-7).
///
/// One knob. BR-7 requires that adding or changing a linger default later cost
/// neither a protocol change nor a packaging change, which is why the mode is a
/// value here rather than, say, an inference from whether launchd started the
/// process — a guard condition derived from incidental facts is exactly the
/// shape LESSON-443 warns about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LifetimeConfig {
    /// What to do when the last client disconnects. Defaults to
    /// `on-last-disconnect`.
    ///
    /// Serialized unconditionally within the table (like
    /// [`PrivacyConfig::redact`]): a config that names `[lifetime]` at all
    /// states its posture rather than leaving it to be inferred.
    #[serde(default)]
    pub shutdown: ShutdownPolicyKind,
    /// The idle window for `shutdown = "linger"`, in seconds.
    ///
    /// Meaningful only in `linger` mode; setting it in any other mode is a
    /// validity error rather than a silently ignored key, because a config that
    /// says `linger_seconds = 300` under `on-last-disconnect` is describing a
    /// belief about the daemon that is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger_seconds: Option<u64>,
}

impl LifetimeConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[lifetime]` table out of a config that never set one.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The runtime policy this table describes.
    #[must_use]
    pub fn policy(&self) -> crate::lifetime::ShutdownPolicy {
        match self.shutdown {
            ShutdownPolicyKind::OnLastDisconnect => {
                crate::lifetime::ShutdownPolicy::OnLastDisconnect
            }
            // A `linger` mode with no window is a 0 s window, which is
            // `on-last-disconnect` by another name — harmless, and validated
            // against separately so the user is told rather than surprised.
            ShutdownPolicyKind::Linger => crate::lifetime::ShutdownPolicy::Linger {
                seconds: self.linger_seconds.unwrap_or(0),
            },
            ShutdownPolicyKind::Never => crate::lifetime::ShutdownPolicy::Never,
        }
    }
}

/// The web-lookup capability ceiling, ordered so that each tier includes the
/// ones below it (REQ-563 BR-3).
///
/// The ordering is the rule, not a convenience. BR-3 grades the capability —
/// `fetch_user_url` < `fetch_any_url` < `search` — and every "may this lookup
/// happen?" question is a comparison against this ceiling, so declaration order
/// (which *is* the derived [`Ord`]) is load-bearing: a variant inserted at the
/// wrong position silently changes what an existing grant permits. A new tier
/// belongs at the position its capability warrants, and
/// `web_tiers_are_ordered_and_each_tier_includes_the_ones_below` fails if the
/// order moves.
///
/// This is the **configured ceiling**, not a grant. BR-3 also requires each tier
/// to be *separately consented*, and that consent is session state carried by
/// the permission gate. A config naming `search` says "search is the most this
/// machine may ever do", never "search is allowed now".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTier {
    /// No web lookup at all — the default (BR-1), and the *only* disabled state
    /// (D-9: there is no separate `enabled` bool to disagree with it).
    #[default]
    Off,
    /// Fetch a URL that appeared verbatim in a user message of the current
    /// session. The floor of the graded capability because the user's paste is
    /// its own authorization — the same reason BR-11's allowlist, which
    /// constrains *model-chosen* destinations, does not constrain this tier.
    FetchUserUrl,
    /// Fetch a URL the model composed.
    FetchAnyUrl,
    /// Query the user-configured search backend and follow its results.
    Search,
}

impl WebTier {
    /// Every tier, lowest first.
    ///
    /// Exists so a sweep over the ladder — "which tiers does this ceiling
    /// permit", a renderer's match, the daemon's mirror test against the wire
    /// twin — cannot miss one a later REQ adds. A hand-kept list at each of
    /// those sites is a list that goes stale at a different time in each of
    /// them; the wire twin (`teton_protocol::events::WebTier::ALL`) carries the
    /// same constant for the same reason, and the daemon's conversion test
    /// sweeps both so the two ladders cannot drift in length either.
    pub const ALL: [WebTier; 4] = [
        WebTier::Off,
        WebTier::FetchUserUrl,
        WebTier::FetchAnyUrl,
        WebTier::Search,
    ];

    /// Whether this ceiling permits a lookup that needs `needed` — BR-3's
    /// each-tier-includes-the-ones-below rule, as one predicate rather than a
    /// comparison every caller re-derives.
    ///
    /// [`WebTier::Off`] is never allowed, *including by itself*. `Off` names the
    /// absence of a capability rather than one, so `Off.allows(Off) == true`
    /// would hand permission to a caller whose required tier came out `Off` — a
    /// default never overwritten, a mapping that fell through — on a machine
    /// that opted into nothing. That is the one comparison here with no honest
    /// affirmative answer, so it fails closed.
    #[must_use]
    pub fn allows(self, needed: Self) -> bool {
        needed != Self::Off && self >= needed
    }
}

/// Opt-in web lookup (the `[web]` table, REQ-563).
///
/// # Why the tier is the only switch (D-9)
///
/// The spec's system model carried an `enabled` bool *and* a `tier`: one fact in
/// two encodings, and therefore two encodings that can disagree. `enabled =
/// false` beside `tier = "search"` has no honest reading — whichever key the
/// code happened to check would be the real setting while the other silently did
/// nothing, which is exactly the "the knob did nothing" defect REQ-558 spent an
/// ADR removing. [`WebTier::Off`] *is* the disabled state, so the contradiction
/// is unrepresentable rather than resolved at read time.
///
/// # Why this is its own table
///
/// The same reason `[privacy]` is (REQ-562 BR-10 — see [`PrivacyConfig`]): this
/// is a capability question, not a routing one. The table names a ceiling, a
/// backend, and a cache window, and deliberately carries **no provider, model,
/// or tier-binding key**. Routing stays in `[[tiers]]`/`[[categories]]`, and
/// BR-10's rule that page reduction is pinned to the local tier *by property* is
/// not something a key here could re-open.
///
/// BR-14's search ⇒ redact-scan coupling is likewise **not** encoded here as a
/// cross-check against `[privacy] redact`. The scan is unconditional for search
/// egress at the choke point (BR-2) — a property no second config writer can
/// bypass. A validation rule saying the same thing would be a weaker copy of a
/// guarantee that already holds, and the weaker copy is the one a reader would
/// trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebConfig {
    /// The capability ceiling. **Defaults to [`WebTier::Off`]**, and that
    /// default is the requirement rather than an implementation detail: BR-1
    /// puts web lookup off on a fresh install, with enabling an explicit user
    /// act.
    ///
    /// Serialized unconditionally within the table (no `skip_serializing_if`),
    /// like [`PrivacyConfig::redact`]: a config that names `[web]` at all states
    /// its posture rather than leaving a reader to infer it from an absent key.
    #[serde(default)]
    pub tier: WebTier,
    /// The tiers a lookup **inside** the ceiling no longer prompts for (BR-4).
    ///
    /// Defaults to **empty**, which is the requirement: BR-4 asks per lookup, and
    /// this list exists only because the consent prompt offers "enable
    /// permanently" and that answer has to become something durable. Serialized
    /// unconditionally within the table, like [`Self::tier`]: a config that names
    /// `[web]` states its consent posture rather than leaving a reader to infer
    /// it from an absent key.
    ///
    /// # Why a set and not a two-valued switch
    ///
    /// BR-3's whole point is that the three tiers are *separately* consented: a
    /// URL the user pasted, a URL the model composed, and a search are three
    /// different capabilities, and one answer about one of them is not an answer
    /// about the other two. A single `permission = "allow"` key made
    /// `enable_permanent` at a `fetch_user_url` prompt permanently stop asking
    /// about `fetch_any_url` and `search` as well — the exact breadth violation
    /// BR-3 forbids, made durable. This list holds precisely the tiers the user
    /// answered for, and [`WebTier::Off`] is not a member any answer can produce.
    ///
    /// It cannot widen [`Self::tier`]. The ceiling is checked before any prompt
    /// is raised, so a listed tier answers only the prompts the ceiling had
    /// already permitted to exist — which is why naming a tier here above `[web]
    /// tier` is not a contradiction to validate away, it is a consent posture for
    /// a capability that is not enabled.
    ///
    /// REQ-560's named permission levels attach here: a level names the tiers it
    /// covers, which is the shape this already is, rather than a second
    /// vocabulary to reconcile.
    #[serde(default)]
    pub permission_allow: Vec<WebTier>,
    /// The search backend's endpoint. **No default ships** (BR-8): there is no
    /// blessed search provider, so an unset endpoint is the ordinary state and
    /// validates cleanly at every tier below `search` — the tier is simply not
    /// offered. Only `tier = "search"` *with* no endpoint is a contradiction,
    /// and [`ConfigError::WebSearchTierWithoutEndpoint`] names the missing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_endpoint: Option<String>,
    /// A **reference** to the search backend's key — never the key itself (BR-7,
    /// BR-8). Checked by the same predicate as a provider's `auth_ref`, and the
    /// rejection names neither the value nor any substring of it.
    ///
    /// Optional even at `tier = "search"`: a self-hosted backend may need no
    /// credential, and requiring one here would make an unauthenticated endpoint
    /// unconfigurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_key_ref: Option<String>,
    /// The header the resolved search credential rides, as a template with
    /// `{key}` marking where the secret goes — e.g.
    /// `"X-Subscription-Token: {key}"` (Brave's shape) or
    /// `"Authorization: Bot {key}"` (Kagi's) (BUG-165).
    ///
    /// **Absent means `Authorization: Bearer {key}`** — the shape an
    /// OpenAI-compatible endpoint expects, and the only shape this machine
    /// spoke before this key existed. BR-8's "no blessed search backend" cuts
    /// both ways: nothing ships a default backend, so nothing can assume every
    /// backend's header either — the shape is a key, not a constant.
    ///
    /// A template, never the header itself: the value carries no secret (BR-7).
    /// `{key}` is replaced with the resolved [`Self::search_key_ref`] only at
    /// the moment the endpoint-bound transport is built, and
    /// [`Config::validate`] refuses a value without `{key}` for the same
    /// reason it refuses a raw key in `search_key_ref`. It is likewise refused
    /// beside an *absent* `search_key_ref`: a shape with no credential to
    /// place is a setting the daemon would silently ignore, which is the
    /// "knob did nothing" defect REQ-558 spent an ADR removing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_auth: Option<String>,
    /// An optional allowlist constraining **model-chosen** destinations only
    /// (BR-11).
    ///
    /// Three states, all valid and all distinct — which is why this is an
    /// `Option<Vec<_>>` rather than a `Vec<_>`:
    ///
    /// - **absent** (`None`): unrestricted; tier grants alone govern. BR-11 is
    ///   explicit that this is a valid configuration and not a warning state.
    /// - **listed**: a model-composed destination outside the list is refused,
    ///   with the allowlist named.
    /// - **present but empty** (`Some([])`): an allowlist that lists nothing
    ///   allows nothing — the most restrictive model-chosen posture, and
    ///   deliberately *not* collapsed into "unrestricted", which is its
    ///   opposite.
    ///
    /// A user-pasted URL is exempt in all three (BR-11: the user's explicit act
    /// is its own authorization).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    /// How long a cached document stays fresh, in seconds (BR-12). Defaults to
    /// 900 — fifteen minutes.
    ///
    /// Zero is valid and means **no caching** rather than "cache forever": every
    /// entry is stale the moment it is written, so a lookup always re-fetches.
    /// That reading is why [`WebConfig`] hand-writes its [`Default`] instead of
    /// deriving it — a derived default would zero this field and quietly mean
    /// something else.
    ///
    /// Serialized unconditionally, for the same reason as [`Self::tier`].
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

/// The default cache freshness window (BR-12): fifteen minutes.
const DEFAULT_CACHE_TTL_SECS: u64 = 900;

/// `serde`'s default for [`WebConfig::cache_ttl_secs`]. Needed as a function
/// because the bare `#[serde(default)]` on a `u64` would supply zero, which this
/// type gives a different meaning to.
const fn default_cache_ttl_secs() -> u64 {
    DEFAULT_CACHE_TTL_SECS
}

impl Default for WebConfig {
    /// Off, with no backend and the default cache window — the fresh-install
    /// state (BR-1).
    fn default() -> Self {
        Self {
            tier: WebTier::Off,
            permission_allow: Vec::new(),
            search_endpoint: None,
            search_key_ref: None,
            search_auth: None,
            allowed_domains: None,
            cache_ttl_secs: DEFAULT_CACHE_TTL_SECS,
        }
    }
}

impl WebConfig {
    /// Whether every field still holds its default, used to keep the `[web]`
    /// table out of a config that never opted in — the same treatment
    /// [`PrivacyConfig::is_unset`] gives `[privacy]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The parsed [`Self::search_auth`] shape: [`SearchAuthShape::bearer`]
    /// when the key is absent or blank ("not configured" is one state, not
    /// two — the reading `validate_web` gives a blank `search_endpoint`), and
    /// `None` when a value is present but does not parse.
    ///
    /// [`Config::validate`] refuses an unparseable value at load, so `None`
    /// is reachable only through a config that skipped validation. A caller
    /// declining to assume validation ran must read `None` as **attach no
    /// credential** — never as "fall back to Bearer": the one thing a
    /// mis-spelled shape must not produce is the credential riding a shape
    /// the user did not write.
    #[must_use]
    pub fn search_auth_shape(&self) -> Option<SearchAuthShape> {
        match self
            .search_auth
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            None => Some(SearchAuthShape::bearer()),
            Some(template) => parse_search_auth(template),
        }
    }
}

// REQ-574 retired `web_table_toml` and its one-key `WebTableDocument`. It
// existed so `/web setup`'s preview and its commit could not disagree about the
// `[web]` section: both rendered it through the same serde path. The seam no
// longer *renders* a section at all — the write applies a delta to the document
// on disk and the preview slices `[web]` back out of that same edited text
// (`config_doc::table_section`, REQ-574 BR-3), so the two cannot disagree
// because there is only one text. A second renderer kept alive beside it would
// be exactly the drift LESSON-451 warns about.

/// The parsed shape of [`WebConfig::search_auth`]: which header the search
/// credential rides, and the scheme word (if any) in front of the secret.
/// Carries no secret itself — the secret enters only through
/// [`Self::header_value`], at the caller's moment of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchAuthShape {
    /// The header name, lowercased: header names are case-insensitive on the
    /// wire, and the transport composes them lowercase.
    pub header: String,
    /// The word in front of the secret (`Bearer`, `Bot`, …), or `None` when
    /// the header carries the bare key.
    pub scheme: Option<String>,
}

impl SearchAuthShape {
    /// What an absent `search_auth` means: `Authorization: Bearer {key}`,
    /// the only shape this machine spoke before BUG-165.
    #[must_use]
    pub fn bearer() -> Self {
        Self {
            header: "authorization".to_owned(),
            scheme: Some("Bearer".to_owned()),
        }
    }

    /// The header value with `secret` in the `{key}` position. The secret
    /// exists only in the return value — callers hand it to the
    /// endpoint-bound transport and drop it.
    #[must_use]
    pub fn header_value(&self, secret: &str) -> String {
        match &self.scheme {
            Some(scheme) => format!("{scheme} {secret}"),
            None => secret.to_owned(),
        }
    }
}

/// Parse a [`WebConfig::search_auth`] template, or `None` when the value is
/// not one of the two accepted spellings:
///
/// - `Header-Name: {key}` — the bare secret in a header of that name.
/// - `Header-Name: Scheme {key}` — one scheme word, one space, the secret.
///
/// The header name must be an RFC 7230 token and the scheme a single token,
/// deliberately narrow: a template is a *shape*, and the moment it accepts
/// arbitrary bytes it can carry the secret itself — the thing
/// `search_key_ref` exists to keep out of this file. The strictness also
/// keeps the template honest about the wire: `"Bot{key}"` (no space) is
/// refused rather than silently rendered with one.
#[must_use]
pub fn parse_search_auth(template: &str) -> Option<SearchAuthShape> {
    let (name, value) = template.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || !name.bytes().all(is_http_token_byte) {
        return None;
    }
    let value = value.trim();
    if value.matches("{key}").count() != 1 {
        return None;
    }
    let prefix = value.strip_suffix("{key}")?;
    let scheme = if prefix.is_empty() {
        None
    } else {
        // One space between the scheme and the secret; the token check
        // refuses whitespace, so `"Two words {key}"` and `"Bot  {key}"`
        // both fail here rather than parse to something surprising.
        let word = prefix.strip_suffix(' ')?;
        if word.is_empty() || !word.bytes().all(is_http_token_byte) {
            return None;
        }
        Some(word.to_owned())
    };
    Some(SearchAuthShape {
        header: name.to_ascii_lowercase(),
        scheme,
    })
}

/// An RFC 7230 `tchar` — the bytes a header name or auth-scheme word may
/// contain.
const fn is_http_token_byte(byte: u8) -> bool {
    matches!(byte,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

/// Tool permissions (the `[permissions]` table, REQ-560).
///
/// One knob, and deliberately only one: levels are *presets*, and a user who
/// wants finer control edits the per-tool table directly rather than growing a
/// second vocabulary here.
///
/// ## This is a starting value, not a setting
///
/// The permission level is **session-scoped**. This field is the value a new
/// session is seeded with; `/permissions <level>` changes the running session
/// and writes nothing back (BR-6). The asymmetry with REQ-559's persisted
/// reasoning effort is the point: an effort level that survives a restart costs
/// money predictably, while a `full` that survives a restart removes a guardrail
/// invisibly, in a session the user does not remember configuring.
///
/// ## It grants no egress
///
/// No level, including `full`, affects the `local-only` boundary or the
/// session-taint pin (BR-3). A level governs which tools may run; the boundary
/// governs what leaves the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PermissionsConfig {
    /// The level a new session starts at. Defaults to `guarded`.
    ///
    /// Serialized unconditionally within the table (like
    /// [`LifetimeConfig::shutdown`]): a config that names `[permissions]` at all
    /// states its posture rather than leaving a reader to infer it from an
    /// absent key.
    ///
    /// An unrecognised spelling is a **deserialization** failure, not a silent
    /// fallback to `guarded` — the daemon refuses to start and the error names
    /// the four valid levels. A posture nobody chose is the shape of a guard
    /// that has quietly stopped guarding, and it would be worst in exactly the
    /// case that matters: a typo in `full` leaving a user who asked for one
    /// thing running as another.
    #[serde(default)]
    pub default_level: PermissionLevel,
}

impl PermissionsConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[permissions]` table out of a config that never set one — the same
    /// treatment [`LifetimeConfig::is_unset`] gives `[lifetime]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// Durable skill consent (the `[skills]` table, REQ-589 D-13).
///
/// # This table is a deliberate security widening, not a convenience
///
/// REQ-589 D-10 put a trust gate on the user-typed `/name` path, so a project
/// skill's body is acknowledged before it reaches the model labelled
/// *instructions*. That gate has no unattended answer: a piped session has no
/// human to ask, and the shadowing case is asked even at `full`, so an
/// automated run could not invoke a typed project skill at all. D-13 chose to
/// preserve automation, and this table is the price — the guarantee that
/// **every** project-authored body is acknowledged *in the session that sends
/// it* is traded for a human decision made **once, out of band**, and consulted
/// later without a prompt.
///
/// The half that is *not* traded away is the whole point: a human still decides.
/// An unattended session at a root this list does not name refuses exactly as it
/// did before D-13. This list is consulted; it is never written by the
/// unattended path, and nothing here invents a decision nobody made.
///
/// # The precedent
///
/// `[web] permission_allow` (REQ-563 BR-4) is the same shape and is deliberately
/// mirrored rather than reinvented: a durable, human-made consent recorded in
/// config by an option on an interactive prompt whose **label names the key it
/// writes**, and consulted by later sessions without re-asking.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// The project roots whose skills a human has durably acknowledged
    /// (REQ-589 D-13).
    ///
    /// Each entry is the **canonical** root name minted by
    /// `tetond::harness::tools::skill::durable_trust_root_name`: the session
    /// root with every symlink resolved, spelled as an **absolute** path and
    /// percent-escaped. Canonical, because an entry naming a path rather than a
    /// tree is a bypass — a symlink dropped at a listed path would hand a
    /// repository nobody acknowledged the trust of one somebody did. Absolute
    /// since REQ-591 D-4, because a `$HOME`-relative row means a different tree
    /// under a different `HOME` and a row is documented as naming a tree. See
    /// that function for the full rule and for what this identity deliberately
    /// does *not* defend against.
    ///
    /// The **prompt** still reads home-relative (`~/dev/repo`); that is
    /// `TrustRoot::display`, and it is a rendering concern rather than an
    /// identity.
    ///
    /// **Matched by exact equality, never by prefix.** Trusting `~/dev/repo`
    /// says nothing about `~/dev/repo/vendor/other`, which is a different
    /// repository with different authors; a prefix test would extend one
    /// answer over every tree nested under it, including one a dependency
    /// update dropped there.
    ///
    /// An entry that matches no root is inert rather than an error: it can only
    /// ever fail to allow, which is the direction to be wrong in, so a stale
    /// row left behind by a moved repository costs one prompt and nothing else.
    ///
    /// A **malformed** entry is different, and since REQ-591 D-5 it is fatal at
    /// load. The failure it prevents is specific and silent: a user hand-edits
    /// `~/dev/repo` — an entirely reasonable-looking thing to write — the
    /// minter produces the canonical absolute form, the two never match, and
    /// their automation keeps refusing with no indication why. The allowlist
    /// *appears* to contain their repository and does not. Inertness is the
    /// right answer for a row that names a real tree the daemon cannot see; it
    /// is the wrong answer for a row that could never have named one.
    /// [`is_canonical_trust_root`] is the rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_project_roots: Vec<String>,
}

impl SkillsConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[skills]` table out of a config that never set one — the same treatment
    /// [`PermissionsConfig::is_unset`] gives `[permissions]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// What the `shell` tool's child is allowed beyond the default (the `[shell]`
/// table, REQ-607).
///
/// # One key, and why it is not a list
///
/// REQ-596 gave the `shell` child a twelve-name positive allowlist and recorded
/// its rejections. One of them — `SSH_AUTH_SOCK` — is the rejection users
/// actually feel, because a `git push` over ssh needs it. This table is the one
/// way to get it back, and it is deliberately **not** the general
/// `[shell] extra_env = [...]` that REQ-596's OQ-2 left open: a list lets a user
/// admit a name holding a bare-token secret the daemon was never told about,
/// which is the class REQ-596 closed. A `bool` cannot express that.
///
/// The shape is BUG-202's, mirrored rather than reinvented (LESSON-578): a
/// secure default plus one explicit, greppable key beats both a permissive
/// default and a heuristic that guesses when the agent is wanted. Nothing in
/// this daemon reads the environment, the command text, or a prior failure to
/// decide — the key is the only input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ShellConfig {
    /// Admit `SSH_AUTH_SOCK` to the `shell` tool's child environment
    /// (REQ-607 BR-5).
    ///
    /// **Defaults to `false`**, and the default is the whole security posture:
    /// the variable is a handle to an agent that *lends* credentials, so
    /// admitting it grants every model-issued command the ability to
    /// authenticate as the user, to any host, for the life of that command.
    /// REQ-596 weighed that and withheld it; this key does not overturn the
    /// judgement, it makes the consequence escapable by someone who has read it.
    ///
    /// It admits that one name and nothing else, and it reaches the `shell`
    /// path only. A spawned MCP server's environment is composed from
    /// `MCP_BASE_ENV_ALLOW` and never consults this (REQ-596 BR-7.1,
    /// REQ-607 BR-7) — turning the agent on for a command you are watching must
    /// not turn it on for a third-party `npx` package you are not.
    ///
    /// Serialized unconditionally within the table (no `skip_serializing_if`),
    /// like [`PrivacyConfig::redact`]: a config that names `[shell]` at all
    /// states its posture rather than leaving it to be inferred.
    #[serde(default)]
    pub allow_ssh_agent: bool,
}

impl ShellConfig {
    /// Whether every field still holds its default, used to keep the `[shell]`
    /// table out of a config that never opted in — the same treatment
    /// [`PrivacyConfig::is_unset`] gives `[privacy]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// The default `[transcript] retain_days` — thirty days (REQ-611 BR-13).
///
/// A free function rather than a literal in one place, because serde needs a
/// path for the field-level `default` and [`TranscriptConfig::default`] needs
/// the same number: two spellings of a retention policy is one policy the day
/// they disagree.
fn default_retain_days() -> u32 {
    30
}

/// The default `[transcript] max_record_bytes` — 64 KiB (REQ-611 BR-12).
///
/// Here for [`default_retain_days`]'s reason: serde's field default and
/// [`TranscriptConfig::default`] must be the same number by construction.
fn default_max_record_bytes() -> usize {
    65_536
}

/// The smallest `max_record_bytes` a transcript will accept (REQ-611 BR-12).
///
/// A truncation marker plus the record's own envelope keys (`n`, `ts`,
/// `session_id`, `kind`, `truncated`, `original_bytes`) already run to a few
/// hundred bytes, so a budget below a kilobyte cannot hold a record that says
/// anything about what it cut. Refused at load rather than clamped silently:
/// a clamp would leave the user reading a number in their own file that the
/// daemon does not use.
const MIN_MAX_RECORD_BYTES: usize = 1024;

/// Opt-in session transcripts (the `[transcript]` table, REQ-611).
///
/// **Off by default, and "off" means the sink does not exist** (BR-1) rather
/// than a sink that runs and discards: with `enabled = false` and no
/// `/transcript on`, the daemon opens no file and creates no directory. That is
/// the posture [`PrivacyConfig::redact`] takes, for the same reason — an
/// un-opted-in machine pays nothing, and the surface it does not have cannot
/// leak.
///
/// # Two switches, and only one of them is here
///
/// This table is the **durable default** read when a session is created (BR-2).
/// The session-lifetime override (`/transcript on|off`) is deliberately not a
/// field here and is never written to disk — the same split `/permissions`
/// already makes between a level and `[permissions]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptConfig {
    /// Record a transcript for every session created from this config.
    ///
    /// **Defaults to `false`** (BR-1). Serialized unconditionally within the
    /// table (no `skip_serializing_if`), like [`PrivacyConfig::redact`]: a
    /// config that names `[transcript]` at all states its posture rather than
    /// leaving a reader to infer it from an absence.
    #[serde(default)]
    pub enabled: bool,

    /// Where transcripts are written, overriding the data-directory default.
    ///
    /// `None` — the ordinary case — means `<data dir>/transcripts`, derived by
    /// [`TranscriptConfig::effective_dir`] from
    /// [`teton_protocol::socket_path::resolve_data_dir`]. The derived path is
    /// **never** written back into the user's file: it is a function of the
    /// machine, not a setting, and AC-19 requires that an unrelated config
    /// write never grow a `dir` key the user did not type.
    ///
    /// Must be absolute when set ([`Config::validate`]). A relative path would
    /// resolve against the daemon's working directory, which is not a place any
    /// user means, and it would resolve differently for a daemon started from a
    /// different shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,

    /// Days a transcript file is kept before the daemon prunes it (BR-13).
    ///
    /// Defaults to `30`; `0` means never prune, and is a valid setting rather
    /// than an error — "keep everything" is a policy, not a malformed number.
    /// Serialized unconditionally for [`Self::enabled`]'s reason: BR-13 calls
    /// retention a *stated* policy, and a retention window that vanishes from
    /// the file whenever it holds its default is a hidden constant.
    #[serde(default = "default_retain_days")]
    pub retain_days: u32,

    /// The per-field content budget, in bytes, before a record is truncated
    /// with an explicit marker (BR-12).
    ///
    /// Defaults to 64 KiB, and must be at least [`MIN_MAX_RECORD_BYTES`].
    /// Serialized unconditionally, for [`Self::retain_days`]'s reason.
    #[serde(default = "default_max_record_bytes")]
    pub max_record_bytes: usize,
}

impl Default for TranscriptConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: None,
            retain_days: default_retain_days(),
            max_record_bytes: default_max_record_bytes(),
        }
    }
}

impl TranscriptConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[transcript]` table out of a config that never opted in — the same
    /// treatment [`PrivacyConfig::is_unset`] gives `[privacy]`.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The directory this config's transcripts are written to (REQ-611 ADR-4).
    ///
    /// The user's [`Self::dir`] when they set one, else `<data dir>/transcripts`.
    /// `data_dir` is the caller's — [`teton_protocol::socket_path::data_dir`] in
    /// the daemon — because this crate performs no I/O and reads no environment.
    ///
    /// **Pure**: it touches no filesystem, creates nothing, and canonicalizes
    /// nothing. That is what lets `teton doctor` print the effective directory
    /// without a daemon round-trip for the default case (AC-20), and what keeps
    /// the derived path out of every code path that could write it back into the
    /// user's config.
    #[must_use]
    pub fn effective_dir(&self, data_dir: &Path) -> PathBuf {
        match &self.dir {
            Some(dir) => dir.clone(),
            None => data_dir.join("transcripts"),
        }
    }
}

/// The default `[context] repo_file` — **on** (REQ-612 BR-2).
///
/// A free function rather than a bare `#[serde(default)]` on the field, and the
/// distinction is the whole feature: serde's *field*-level `default` calls
/// `bool::default()`, which is `false`, and it wins over the container's
/// `#[serde(default)]`. A `[context]` table written for some other key would
/// then silently turn the notes off — the "on arriving by omission" failure
/// [`TranscriptConfig::enabled`] guards against, inverted. This is
/// [`default_retain_days`]'s pattern for [`default_retain_days`]'s reason: serde's
/// field default and [`ContextConfig::default`] must be one number by
/// construction.
fn default_repo_file() -> bool {
    true
}

/// Repository context notes (the `[context]` table, REQ-612).
///
/// **On by default** (BR-2), which is the opposite posture from
/// [`TranscriptConfig`] and deliberately so: a `TETON.md` at the repository root
/// is a file an author wrote *in order to be read*, and a feature that has to be
/// switched on before the file it names does anything is a feature nobody
/// installs. The bound on the trade is a byte cap (BR-3), not an opt-in.
///
/// "Off" still means the mechanism does not run (the REQ-611 BR-1 posture):
/// `repo_file = false` means the daemon never opens the file — no `stat`, no
/// read, no block, no event — rather than reading it and discarding the result.
///
/// # Two switches, and only one of them is here
///
/// This table is the **durable default** read when a session is created. The
/// session-lifetime override (`/context on|off`) is deliberately not a field
/// here and is never written to disk — the same split
/// [`TranscriptConfig`] makes for `/transcript on|off`, and `/permissions`
/// before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Read the repository's notes file at the session root into the system
    /// prompt.
    ///
    /// **Defaults to `true`** (BR-2). Serialized unconditionally within the
    /// table (no `skip_serializing_if`), like [`PrivacyConfig::redact`] and
    /// [`TranscriptConfig::enabled`]: a config that names `[context]` at all
    /// states its posture rather than leaving a reader to infer it from an
    /// absence — and here the absence would read as the *wrong* posture, since
    /// the shipped default is on.
    #[serde(default = "default_repo_file")]
    pub repo_file: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            repo_file: default_repo_file(),
        }
    }
}

impl ContextConfig {
    /// Whether every field still holds its default, used to keep the
    /// `[context]` table out of a config that never named it — the same
    /// treatment [`PrivacyConfig::is_unset`] gives `[privacy]`.
    ///
    /// Note which way round this reads: an *unset* `[context]` is one with the
    /// feature **on**, because on is the default. The table reaches a user's
    /// file only when they turned the notes off.
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
    /// The global reasoning-effort setting applied to every model call
    /// (REQ-559 BR-2), persisted across sessions (BR-8).
    ///
    /// **One** global setting: there is deliberately no per-category,
    /// per-tier or per-provider effort configuration. The category bindings
    /// (REQ-558) already carry the per-workload cost distinction; effort is the
    /// orthogonal "how hard am I thinking right now" dial. The single exception
    /// is the per-provider clamp, which is a capability constraint rather than a
    /// user setting — see [`crate::effort::EffortLadder::clamp`].
    ///
    /// Serialized **unconditionally** (no `skip_serializing_if`), for the same
    /// reason as [`Config::judgment_default`] and
    /// [`LocalModelConfig::auto_accept`]: a declared default that vanishes from
    /// a written-out config whenever it holds its default value is precisely the
    /// hidden constant that configuration-visibility rules out. A user must be
    /// able to see what they are spending on.
    ///
    /// Declared here among the scalars, **before** the array-of-table fields,
    /// for the TOML-ordering reason above.
    #[serde(default)]
    pub effort: crate::effort::EffortLevel,
    /// Local-model tier inputs (`[local_model]`): the pin, the auto-accept
    /// opt-in, and the catalog base-URL override.
    #[serde(default, skip_serializing_if = "LocalModelConfig::is_unset")]
    pub local_model: LocalModelConfig,
    /// Opt-in privacy behaviour (`[privacy]`): today, the REQ-562 redaction
    /// scan. Absent means off (BR-10) — see [`PrivacyConfig`] for why the
    /// switch is here rather than in [`Config::categories`].
    #[serde(default, skip_serializing_if = "PrivacyConfig::is_unset")]
    pub privacy: PrivacyConfig,
    /// Opt-in spend behaviour (`[cost]`): today, REQ-588's per-prompt ceiling.
    /// Absent means no ceiling — see [`CostConfig`].
    #[serde(default, skip_serializing_if = "CostConfig::is_unset")]
    pub cost: CostConfig,
    /// Opt-in web lookup (`[web]`): the capability ceiling, the search backend,
    /// and the cache window (REQ-563). Absent means `tier = "off"` and no code
    /// path performs a lookup (BR-1) — see [`WebConfig`] for why the ceiling is
    /// the only switch. Declared here among the tables, before the
    /// array-of-table fields, for the TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "WebConfig::is_unset")]
    pub web: WebConfig,
    /// Daemon lifetime (`[lifetime]`): what happens when the last client
    /// disconnects (REQ-565). Absent means `on-last-disconnect` — the daemon
    /// exits with its last client. Declared here among the tables, before the
    /// array-of-table fields, for the TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "LifetimeConfig::is_unset")]
    pub lifetime: LifetimeConfig,
    /// Tool permissions (`[permissions]`): the level a **new** session starts at
    /// (REQ-560). Absent means `guarded` — reads run freely, edits and shell
    /// commands ask. Declared here among the tables, before the array-of-table
    /// fields, for the TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "PermissionsConfig::is_unset")]
    pub permissions: PermissionsConfig,
    /// Durable skill consent (`[skills]`, REQ-589 D-13): the project roots a
    /// human has acknowledged out of band, so an unattended session may run
    /// their skills. Absent means the empty list — every unattended session
    /// refuses a typed project skill exactly as it did before D-13. Declared
    /// here among the tables, before the array-of-table fields, for the
    /// TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "SkillsConfig::is_unset")]
    pub skills: SkillsConfig,
    /// What the `shell` tool's child may inherit beyond REQ-596's twelve
    /// (`[shell]`, REQ-607). Absent means `allow_ssh_agent = false` — the
    /// default REQ-596 shipped, unchanged. Declared here among the tables,
    /// before the array-of-table fields, for the TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "ShellConfig::is_unset")]
    pub shell: ShellConfig,
    /// Opt-in session transcripts (`[transcript]`, REQ-611): the durable
    /// default every new session starts from, where the files go, how long they
    /// are kept, and the per-field truncation budget. Absent means `enabled =
    /// false` and no sink is constructed at all (BR-1) — see
    /// [`TranscriptConfig`]. Declared here among the tables, before the
    /// array-of-table fields, for the TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "TranscriptConfig::is_unset")]
    pub transcript: TranscriptConfig,
    /// Repository context notes (`[context]`, REQ-612): whether the daemon
    /// reads the repository's notes file at the session root into the system
    /// prompt. Absent means `repo_file = true` — the notes are on, because a
    /// file written to be read is no use behind an opt-in (BR-2) — and `false`
    /// means the file is never opened at all. See [`ContextConfig`]. Declared
    /// here among the tables, before the array-of-table fields, for the
    /// TOML-ordering reason above.
    #[serde(default, skip_serializing_if = "ContextConfig::is_unset")]
    pub context: ContextConfig,
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

    /// A `[skills] trusted_project_roots` row that could never name a tree
    /// (REQ-591 D-5).
    ///
    /// **Structural, so it is fatal at load**, for `UnusableSpendCeiling`'s
    /// reason: this is not an incomplete record the daemon can refuse at the
    /// point of use — it is a security allowlist the user believes they added a
    /// repository to. A row that is merely *stale* stays inert, because it can
    /// only fail to allow; a row that is **malformed** never matched anything
    /// and never will, and leaving it inert means a user's automation refuses
    /// forever with the allowlist apparently naming their repository.
    ///
    /// The message names the correct form and the way to obtain it, because the
    /// only thing wrong is the spelling.
    #[error(
        "[skills] trusted_project_roots contains {0}, which cannot name a repository. A row is \
         the canonical absolute path of the tree — every symlink resolved, no `.` or `..` \
         component, no trailing slash, and `~` is not expanded — for example \
         \"/Users/you/dev/repo\". Teton writes it for you when you answer `p` \
         (\"trust this repository permanently\") at the acknowledgment prompt; a path typed by \
         hand only matches if it is already in that form."
    )]
    /// Carries the row **as written**, quoted, so the user can find the line.
    MalformedTrustedProjectRoot(String),

    /// `[cost] prompt_ceiling_usd` is not a usable amount (REQ-588 BR-5).
    ///
    /// **Structural, so it is fatal at load**, per conventions.md's split: a
    /// ceiling that cannot be compared is not an incomplete record the daemon
    /// can refuse at the point of use — it is a spend limit the user believes
    /// they set. Starting with it silently ignored is the worst of the three
    /// outcomes.
    #[error(
        "[cost] prompt_ceiling_usd = {0} is not a usable amount; it must be a finite number \
         greater than zero (remove the key for no ceiling)"
    )]
    /// Carries the **rendered** figure rather than the `f64`: `ConfigError`
    /// derives `Eq` (every other variant is comparable), and the message only
    /// ever echoes what the user wrote back at them.
    UnusableSpendCeiling(String),

    /// `[lifetime] shutdown = "linger"` with no `linger_seconds`.
    #[error(
        "[lifetime] shutdown = \"linger\" needs a linger_seconds window. Without one the daemon \
         exits the instant the last client leaves, which is what shutdown = \"on-last-disconnect\" \
         already means — say which you want."
    )]
    LingerWithoutWindow,

    /// `linger_seconds` set under a mode that never lingers.
    #[error(
        "[lifetime] linger_seconds is set, but shutdown = \"{shutdown:?}\" never lingers, so the \
         window would be ignored. Set shutdown = \"linger\" to use it, or remove linger_seconds."
    )]
    LingerWindowWithoutLingerMode {
        /// The mode that makes the window meaningless.
        shutdown: ShutdownPolicyKind,
    },

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

    /// `[web] tier = "search"` names no `search_endpoint` (REQ-563 BR-8, AC-7).
    ///
    /// BR-8's "with no endpoint configured, the search tier is simply not
    /// offered — its absence is not an error" governs the config that does not
    /// *ask* for search: below that tier an unset endpoint is the ordinary
    /// state and validates cleanly. This variant is the other case — a config
    /// that names the tier while naming no backend to serve it. That is a
    /// request for something unserveable rather than an absence, and it is
    /// reported at load with the missing key named, rather than surfacing later
    /// as a tier that mysteriously never appears in the consent prompt.
    #[error(
        "[web] tier = \"search\" requires a `search_endpoint`, and none is set. Add your search \
         backend's URL as `[web] search_endpoint`, or lower `[web] tier` to \"fetch_any_url\" — \
         no default search endpoint ships (BR-8)."
    )]
    WebSearchTierWithoutEndpoint,

    /// `[web] search_endpoint` is set to something that is not an absolute
    /// http(s) URL (REQ-563 BR-8).
    ///
    /// Checked wherever the key is set rather than only at `tier = "search"`: a
    /// value that could never be requested is a mistake at the moment it is
    /// written, and finding out about it when the tier is *raised* — the one
    /// moment the user is trying to do something else — is the worse of the two
    /// times to be told.
    ///
    /// The value is not echoed, for [`ConfigError::InvalidAllowedDomain`]'s
    /// reason: the likeliest malformed endpoint is one carrying a key in its
    /// query string, and this message is loggable.
    #[error(
        "[web] search_endpoint is not a usable URL. It must be an absolute http/https URL \
         including a host, e.g. \"https://search.example.com/search\". (The value is not echoed: \
         an endpoint can carry a credential in its query string, and this message is loggable — \
         BR-7.)"
    )]
    InvalidWebSearchEndpoint,

    /// `[web] search_key_ref` is set beside a cleartext `http://` endpoint on a
    /// non-loopback host (REQ-563 BR-7).
    ///
    /// The key resolved from that reference is sent as a bearer credential, so
    /// an `http://` endpoint puts it on the wire in the clear for every hop to
    /// read. A config that names both is asking for that, almost certainly
    /// without meaning to — and the honest place to say so is the load, not a
    /// packet capture.
    ///
    /// Loopback is exempt because there is no wire: a self-hosted backend on
    /// `http://127.0.0.1:8888` is an ordinary setup, and refusing it would push
    /// people toward a self-signed certificate for no gain.
    #[error(
        "[web] search_key_ref is set, but search_endpoint is a cleartext http:// URL on a \
         non-loopback host — the search key would be sent in the clear. Use https://, or point \
         search_endpoint at a loopback address if the backend runs on this machine (BR-7)."
    )]
    WebSearchKeyOverCleartextEndpoint,

    /// A provider's `auth_ref` sits beside a cleartext `http://` endpoint on a
    /// non-loopback host, and the provider has not opted out (BUG-202).
    ///
    /// The sibling of [`ConfigError::WebSearchKeyOverCleartextEndpoint`]: the
    /// credential resolved from that reference is sent as a request header on
    /// every turn, so a cleartext endpoint puts it on the wire for every hop to
    /// read. `[web]` has refused this pair since REQ-563; the provider half was
    /// only ever a warning inside the guided `teton provider add` flow, which
    /// left a hand-edited config and a migrated one with no check at all.
    ///
    /// Unlike the `[web]` rule this one is **escapable**, because provider
    /// topologies are broader than `[web]`'s. A self-hosted model server on a
    /// trusted LAN with a token in front of it is a legitimate setup, and
    /// [`is_cleartext_to_a_remote_host`] exempts only *loopback* — it cannot
    /// tell a LAN host from a public one, and no reliable rule tells
    /// `models.corp.example.com` from `models.example.com`. So the default is
    /// secure and the judgment is handed to the person who knows their own
    /// network, via `allow_cleartext = true` on that provider.
    #[error(
        "provider '{provider_id}': auth_ref is set, but endpoint is a cleartext http:// URL — \
         the credential would travel to {host} in the clear on every turn, for every hop \
         between this machine and that host to read. Use https:// if {host} serves it, or a \
         loopback address if the provider runs on this machine. If {host} is on a network you \
         trust (a self-hosted model server on a LAN), re-run with \
         `teton provider add --allow-cleartext` — or set `allow_cleartext = true` on this \
         provider's row — to say so deliberately (BUG-202, BUG-205)."
    )]
    AuthRefOverCleartextEndpoint {
        /// The offending provider's id.
        provider_id: String,
        /// The host the credential would travel to, named so the message is
        /// actionable without a second lookup — the registration warning this
        /// rule grew out of named it too.
        host: String,
    },

    /// `[web] search_endpoint` already carries a `q` query parameter (REQ-563
    /// BR-2, BR-8).
    ///
    /// The search seam appends the query as `q`, so an endpoint that already has
    /// one produces a URL with two — and which of them the backend honours is
    /// its business, not something this machine can know. That matters beyond
    /// tidiness: the redaction scan (BR-14) runs on the query string this daemon
    /// composed, and if the backend answers the *other* `q` then the string that
    /// was scanned is not the string that decided the request. A parameter name
    /// the seam owns is not one a config may also set.
    #[error(
        "[web] search_endpoint already carries a `q` parameter, and the search query is sent as \
         `q`. Two would leave which one the backend honours undefined — and the scanned query \
         would not be the effective one. Remove `q` from the endpoint URL (its other parameters \
         are kept)."
    )]
    WebSearchEndpointCarriesQueryParam,

    /// `[web] permission_allow` names `"off"` (REQ-563 BR-3, BR-4).
    ///
    /// The list names tiers the user has answered "stop asking" for, and
    /// [`WebTier::Off`] names the *absence* of a tier — there is no consent key
    /// for it, no prompt that could produce it, and nothing an entry would
    /// switch off. Left unvalidated it would be a member the daemon silently
    /// drops, which is the shape of a setting that does nothing (REQ-558's
    /// lesson). Refused at load instead, where the user is still looking at the
    /// file they just edited.
    #[error(
        "[web] permission_allow lists \"off\", which is not a tier a lookup can be consented at. \
         Remove it — an empty list is the default and means \"ask about every lookup\". Valid \
         members are \"fetch_user_url\", \"fetch_any_url\" and \"search\"."
    )]
    WebPermissionAllowNamesOff,

    /// `[web] search_key_ref` is not a recognized credential *reference*
    /// (REQ-563 BR-8). Like [`ConfigError::UnrecognizedAuthRef`], the message
    /// names the accepted forms and never the value.
    #[error(
        "[web] search_key_ref is not a recognized credential reference. Config files must store \
         only a reference to the secret, never the credential itself: use a keychain reference \
         (\"keychain://<service>/<account>\" or \"keychain:<account>\"), an environment reference \
         (\"env:<VAR>\"), or a 1Password reference (\"op://<vault>/<item>\"). Put the search key \
         in your OS keychain and name it here (BR-7)."
    )]
    UnrecognizedWebSearchKeyRef,

    /// `[web] search_auth` does not parse as a credential-header template
    /// (BUG-165).
    ///
    /// The message teaches both accepted spellings rather than only naming
    /// the rule, because the likeliest author is someone transcribing a
    /// backend's documentation — and refuses to echo the value: the likeliest
    /// *malformed* template is one where the user pasted the key itself in
    /// place of `{key}`, and this message is loggable (BR-7).
    #[error(
        "[web] search_auth is not a usable credential-header template. Write the one header the \
         search key rides, with `{{key}}` where the secret goes: \"X-Subscription-Token: {{key}}\" \
         (bare key) or \"Authorization: Bot {{key}}\" (one scheme word in front). The key itself \
         stays in the OS keychain under search_key_ref — a template without `{{key}}` is refused \
         for the same reason a raw key in search_key_ref is (BR-7). When unset, the credential is \
         sent as \"Authorization: Bearer {{key}}\". (The value is not echoed: a malformed \
         template may carry the key itself, and this message is loggable.)"
    )]
    InvalidWebSearchAuth,

    /// `[web] search_auth` is set while no `search_key_ref` names a credential
    /// (BUG-165).
    ///
    /// The template says how a credential rides; `search_key_ref` names the
    /// credential. One without the other is a setting the daemon would
    /// silently ignore — the "knob did nothing" shape (REQ-558's lesson, the
    /// same reading [`ConfigError::WebPermissionAllowNamesOff`] gives a member
    /// that switches nothing off) — and the author almost certainly believes
    /// they configured auth.
    #[error(
        "[web] search_auth is set, but no search_key_ref names a credential to place in it. Add \
         `[web] search_key_ref` (a keychain/env/op reference — never the key itself, BR-7), or \
         remove search_auth if the backend needs no credential."
    )]
    WebSearchAuthWithoutKeyRef,

    /// A `[web] allowed_domains` entry is not shaped like a bare domain pattern
    /// (REQ-563 BR-11).
    ///
    /// The entry is located by **position, not by value**, which is the one
    /// place this enum's no-echo rule differs from
    /// [`ConfigError::InvalidBoundaryGlob`]: a *rejected* allowlist entry is by
    /// definition not a domain, and the likeliest thing it is instead is a
    /// pasted URL — which can carry a credential in its query string. The
    /// position is the locator the user actually needs to find the line.
    #[error(
        "[web] allowed_domains entry {position} is not a bare domain pattern. Entries are hosts \
         or wildcards such as \"docs.rs\" or \"*.example.com\" — letters, digits, '.', '-' and \
         '*' only, with no scheme, no path, and no \"..\". (The value is not echoed: a mis-pasted \
         URL can carry a credential in its query string, and this message is loggable — BR-7.)"
    )]
    InvalidAllowedDomain {
        /// The 1-based position of the offending entry — the locator, since the
        /// value itself is deliberately not echoed.
        position: usize,
    },

    /// `[transcript] max_record_bytes` is below the floor a truncated record
    /// needs to describe itself (REQ-611 BR-12).
    ///
    /// **Structural, so it is fatal at load**, for [`ConfigError::UnusableSpendCeiling`]'s
    /// reason: the user set a budget, and starting with it silently raised to
    /// something else means every truncation marker in their file reports a
    /// number they did not choose. The value *is* echoed here — it is an
    /// integer the user typed, with nothing on the right-hand side that could
    /// be a secret, and it is the fastest way to recognize the line.
    #[error(
        "[transcript] max_record_bytes = {bytes} is too small; a truncated record still carries \
         its own `n`, `ts`, `session_id`, `kind`, `truncated` and `original_bytes` keys, so the \
         budget must be at least 1024 bytes. The default is 65536."
    )]
    TranscriptRecordSizeTooSmall {
        /// The budget as written, so the user can find the line.
        bytes: usize,
    },

    /// `[transcript] dir` is set to a relative path (REQ-611 BR-8).
    ///
    /// A relative directory resolves against the daemon's working directory —
    /// which is not a place any user means, and is a *different* place for a
    /// daemon autostarted by the CLI than for one started by hand. Left
    /// unchecked it would scatter transcripts across the filesystem and hand the
    /// tool jail a denied prefix that names a different tree on every start.
    ///
    /// The value is **not** echoed, unlike
    /// [`ConfigError::MalformedTrustedProjectRoot`]: that one locates a row
    /// inside a list, whereas this names a single key the user can find by its
    /// own name, and a transcript path is boundary content (REQ-569 BR-10) in a
    /// message that reaches the daemon log.
    #[error(
        "[transcript] dir must be an absolute path. A relative one would resolve against \
         whatever directory the daemon happened to start in, which differs between an \
         autostarted daemon and one you launched yourself. Write the full path (`~` is not \
         expanded), or remove the key to use the default transcripts directory."
    )]
    TranscriptDirNotAbsolute,
}

impl Config {
    /// The boundary set this config actually enforces: the user's rows, then
    /// the shipped defaults (REQ-597 BR-2, BR-2.1, BR-3).
    ///
    /// **This is the one place the builtin set is composed** (AC-8). Every
    /// reader of a boundary list — the egress choke point, the session-taint
    /// check, `config/get`'s report — calls this. The single *writer*
    /// (`config/set`'s `SetPrivacyBoundary`) deliberately does not: it appends
    /// to [`Self::boundaries`], the user's own table, which is what keeps a
    /// user's config file free of rows they never wrote (AC-10).
    ///
    /// # Why the builtins are appended rather than prepended
    ///
    /// [`crate::boundary::BoundaryMatcher::match_path`] resolves an overlap by
    /// **earliest declaration wins** (it takes `.min()` over the matched
    /// indices). Appending therefore leaves every user row strictly ahead of
    /// every builtin, so a user row that already matches a builtin path keeps
    /// its own mode and its own identity — which is BR-7, true by construction
    /// rather than by a tie-break rule written on top. Prepending would make a
    /// builtin override the user's own row, the direct contradiction of BR-7
    /// (BR-2.1).
    ///
    /// The residual BR-2.2 accepts: a user row *can* therefore select a weaker
    /// mode for a builtin path. It can never remove the protection — the
    /// composed set still matches — and the residual is inert today because
    /// both [`BoundaryMode`] arms fail closed at the egress inspector.
    ///
    /// # No deduplication
    ///
    /// A user glob byte-identical to a builtin leaves **both** rows in the
    /// result. BR-7's "one block, not two" is a statement about enforcement,
    /// which `match_path` already guarantees by returning exactly one row;
    /// collapsing the pair here would instead hide the builtin from
    /// `boundary list` and destroy the only evidence of which row governs.
    ///
    /// # Returns a fresh list, and never mutates
    ///
    /// The builtin rows must not exist inside a [`Config`] value, because
    /// [`crate::config_doc::apply_config_delta`] diffs a `Config` against the
    /// user's real TOML document. A builtin row living in [`Self::boundaries`]
    /// would be diffed as a row the user is missing and written to their file
    /// on the next unrelated `config/set`.
    #[must_use]
    pub fn effective_boundaries(&self) -> Vec<PrivacyBoundary> {
        let mut composed = self.boundaries.clone();
        if !self.privacy.disable_default_boundaries {
            composed.extend(
                DEFAULT_BOUNDARIES
                    .iter()
                    .map(|g| PrivacyBoundary::builtin(*g)),
            );
        }
        composed
    }

    /// How many builtin rows [`Self::effective_boundaries`] contributed — the
    /// `count` payload of `boundary_defaults_applied` (REQ-597 System Model).
    ///
    /// Counted **from the composed set itself**, not by re-deriving the opt-out
    /// condition beside it. The first version of this did re-derive it, and
    /// AC-8's region check caught it: two readings of `DEFAULT_BOUNDARIES` are
    /// two places the rule lives, and the day one changes, this event starts
    /// reporting a number the enforced set does not have. Reading the composer's
    /// own output makes that unrepresentable rather than merely unlikely.
    ///
    /// The cost is one composed `Vec` per call, and the caller is session
    /// creation.
    #[must_use]
    pub fn builtin_boundary_count(&self) -> usize {
        self.effective_boundaries()
            .iter()
            .filter(|b| b.origin == BoundaryOrigin::Builtin)
            .count()
    }

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

    /// **REQ-591 D-5.** Every `[skills] trusted_project_roots` row is a
    /// well-formed canonical mint.
    ///
    /// One rule, and deliberately no caps. An entry-length cap and a list-length
    /// cap were considered and dropped: they guard nothing real — a long row
    /// simply fails to match, a long list is a user's own file, and neither is
    /// reachable by anything but the person who owns the config. A security
    /// allowlist earns a rule about **meaning**, not about size.
    fn validate_skills(&self) -> Result<(), ConfigError> {
        for row in &self.skills.trusted_project_roots {
            if !is_canonical_trust_root(row) {
                return Err(ConfigError::MalformedTrustedProjectRoot(format!("{row:?}")));
            }
        }
        Ok(())
    }

    /// `[cost]`'s structural check (REQ-588 BR-5).
    ///
    /// Only structure: a ceiling that is absent is the ordinary case and a
    /// ceiling that is present must be a number this can compare. Whether the
    /// figure is *sensible* is the user's business — a $0.01 ceiling is a
    /// choice, not an error.
    fn validate_cost(&self) -> Result<(), ConfigError> {
        if let Some(usd) = self.cost.prompt_ceiling_usd {
            if !usd.is_finite() || usd <= 0.0 {
                return Err(ConfigError::UnusableSpendCeiling(usd.to_string()));
            }
        }
        Ok(())
    }

    /// `[transcript]`'s structural check (REQ-611 BR-12, BR-13).
    ///
    /// **Structure only**, and the omissions are the interesting half. It does
    /// not ask whether the directory exists, whether it is writable, or whether
    /// its permissions are owner-only: those are runtime conditions the sink
    /// answers where it opens the file, degrading that one session per BR-6
    /// while the daemon keeps running — and `validate` is fail-closed and gates
    /// daemon *startup*, so enforcing them here would make an unplugged external
    /// drive a machine that will not start (conventions.md, "config validity vs
    /// usability"; REQ-557 ADR-E).
    ///
    /// It also does not police `retain_days`: `0` is "never prune", every other
    /// value is a window, and there is no unusable number to catch.
    fn validate_transcript(&self) -> Result<(), ConfigError> {
        let transcript = &self.transcript;
        if transcript.max_record_bytes < MIN_MAX_RECORD_BYTES {
            return Err(ConfigError::TranscriptRecordSizeTooSmall {
                bytes: transcript.max_record_bytes,
            });
        }
        if let Some(dir) = &transcript.dir {
            if !dir.is_absolute() {
                return Err(ConfigError::TranscriptDirNotAbsolute);
            }
        }
        Ok(())
    }

    /// Validate cross-field invariants and the BR-7 no-raw-keys rule.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_local_model()?;
        self.validate_web()?;
        self.validate_lifetime()?;
        self.validate_cost()?;
        self.validate_skills()?;
        self.validate_transcript()?;

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
            // BUG-202: the `[web]` rule's sibling, by the same predicate rather
            // than a second spelling of it (LESSON-494). A credential beside a
            // cleartext endpoint is refused as a *pair* — `http://` to a
            // provider that wants no credential is the user's own call, exactly
            // as it is for `[web]`.
            //
            // `allow_cleartext` is the deliberate escape hatch, and it is
            // checked first so the opt-out costs nothing to read. See the
            // variant's doc comment for why this rule is escapable where
            // `[web]`'s is not.
            //
            // `is_cleartext_to_a_remote_host` answers `false` for anything that
            // is not an absolute http(s) URL, so a `Custom`/`Local` endpoint
            // that is not a URL at all passes through untouched.
            if !p.allow_cleartext {
                if let Some(endpoint) = p.endpoint.as_deref() {
                    if p.auth_ref.is_some() && is_cleartext_to_a_remote_host(endpoint) {
                        return Err(ConfigError::AuthRefOverCleartextEndpoint {
                            provider_id: p.id.clone(),
                            host: url_host(endpoint).unwrap_or(endpoint).to_owned(),
                        });
                    }
                }
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

    /// Validates the `[local_model]` inputs (REQ-547).
    ///
    /// The pin's *shape* is what a config-time check can honestly assert: this
    /// crate holds no catalog (that is `teton-inference`), so it rejects values
    /// that could never name a catalog entry — a path, a URL, a blank string —
    /// and leaves "is there such a model?" to the daemon, which has the catalog
    /// and can list the alternatives.
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

    /// Validates the `[web]` table (REQ-563).
    ///
    /// The rules, each of which is as interesting for what it does *not* check:
    ///
    /// - `tier = "search"` requires a `search_endpoint`. An unset endpoint below
    ///   that tier is not checked at all — BR-8 makes it the ordinary state.
    /// - a `search_endpoint` that *is* set must be an absolute http(s) URL, must
    ///   not already carry the `q` parameter the search seam appends, and must
    ///   not be cleartext to a remote host when a `search_key_ref` sits beside
    ///   it. These are checked at every tier, because a value that could never
    ///   be requested is wrong when it is written, not when it is first used.
    /// - `search_key_ref`, when present, must be a reference rather than a
    ///   secret, by the same predicate a provider `auth_ref` faces. It is not
    ///   *required* at any tier: an unauthenticated backend is a legitimate
    ///   configuration, and demanding a key would make one unconfigurable.
    /// - every `allowed_domains` entry must be shaped like a domain pattern. An
    ///   absent list is not checked (BR-11: unrestricted is valid), and an empty
    ///   one is not an error — it is the most restrictive setting available, not
    ///   a malformed one.
    /// - every `permission_allow` member must be a tier a lookup can actually be
    ///   consented at. An unknown spelling is refused by `serde` before this runs
    ///   (the field is typed as [`WebTier`], not as a string), and `"off"` — the
    ///   one spelling that parses but names nothing — is refused here.
    ///
    /// Nothing here validates the *tier* against the rest of the machine (is
    /// there a local tier for BR-10's reduction? is the redact scan on?). Those
    /// are runtime conditions, answered where the lookup happens — a tier that
    /// cannot be served today is a stated absence, not a config that fails to
    /// load (BR-8, BR-14).
    /// `[lifetime]` coherence (REQ-565 BR-7).
    ///
    /// Both checks exist to stop a config from *describing* a lifetime it will
    /// not get. A `linger_seconds` under a non-linger mode, and a `linger` mode
    /// with no window, are each a statement the daemon would silently ignore —
    /// and a silently ignored lifetime setting is how an operator ends up
    /// believing a daemon lingers when it exits instantly.
    fn validate_lifetime(&self) -> Result<(), ConfigError> {
        let lifetime = &self.lifetime;
        match lifetime.shutdown {
            ShutdownPolicyKind::Linger => {
                if lifetime.linger_seconds.is_none() {
                    return Err(ConfigError::LingerWithoutWindow);
                }
            }
            _ => {
                if lifetime.linger_seconds.is_some() {
                    return Err(ConfigError::LingerWindowWithoutLingerMode {
                        shutdown: lifetime.shutdown,
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_web(&self) -> Result<(), ConfigError> {
        let web = &self.web;

        // A blank endpoint is as unset as an absent one — the same reading
        // `MissingEndpoint` gives a provider's, so `search_endpoint = ""` cannot
        // satisfy the tier by being technically present. The same reading is why
        // the shape checks below skip a blank value rather than calling it
        // malformed: "not configured" is one state, not two.
        let endpoint = web
            .search_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if web.tier == WebTier::Search && endpoint.is_none() {
            return Err(ConfigError::WebSearchTierWithoutEndpoint);
        }

        if let Some(endpoint) = endpoint {
            if !is_absolute_http_url(endpoint) {
                return Err(ConfigError::InvalidWebSearchEndpoint);
            }
            if url_query_names(endpoint).any(|name| name == SEARCH_QUERY_PARAM) {
                return Err(ConfigError::WebSearchEndpointCarriesQueryParam);
            }
            // A key beside a cleartext endpoint is the credential going out in
            // the clear, so the pair is refused rather than the endpoint alone —
            // `http://` to a backend that wants no key is a user's own call.
            if web.search_key_ref.is_some() && is_cleartext_to_a_remote_host(endpoint) {
                return Err(ConfigError::WebSearchKeyOverCleartextEndpoint);
            }
        }

        // BR-7, by the provider path's own predicate rather than a second
        // spelling of it: a raw key here would be a plaintext secret exactly as
        // it would be in an `auth_ref`.
        if let Some(key_ref) = &web.search_key_ref {
            if !is_recognized_auth_ref(key_ref) {
                return Err(ConfigError::UnrecognizedWebSearchKeyRef);
            }
        }

        // BUG-165: the credential-header template, parsed by the same function
        // the daemon reads the shape through — so the shape that validates is
        // the shape that rides. A blank value is as unset as an absent one,
        // the `endpoint` reading above.
        let search_auth = web
            .search_auth
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(template) = search_auth {
            if parse_search_auth(template).is_none() {
                return Err(ConfigError::InvalidWebSearchAuth);
            }
            if web.search_key_ref.is_none() {
                return Err(ConfigError::WebSearchAuthWithoutKeyRef);
            }
        }

        if let Some(domains) = &web.allowed_domains {
            for (index, domain) in domains.iter().enumerate() {
                if !is_domain_pattern_shaped(domain) {
                    return Err(ConfigError::InvalidAllowedDomain {
                        position: index + 1,
                    });
                }
            }
        }

        if web.permission_allow.contains(&WebTier::Off) {
            return Err(ConfigError::WebPermissionAllowNamesOff);
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

/// Strip `prefix` from `value` ASCII-case-insensitively.
///
/// A URL scheme is case-insensitive (RFC 3986 §3.1) and every real parser folds
/// it, so a check that does not is a check a different reading of the same string
/// passes — the split-parser hazard this file exists to avoid, in miniature.
fn strip_scheme_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

/// Split an absolute http(s) URL into "is this cleartext" and the text after the
/// scheme, or `None` when it is neither.
///
/// The **one** place the two schemes are recognized, so
/// [`is_absolute_http_url`], [`url_host`] and [`is_cleartext_to_a_remote_host`]
/// cannot disagree about whether a given string is an `http://` URL. They used
/// to each spell the prefix test themselves, and making only one of them
/// case-insensitive would have been strictly worse than leaving all three
/// case-sensitive: `HTTP://evil.example` would validate as a URL and then fail
/// the cleartext test, putting a search key on the wire in the clear.
fn split_http_scheme(value: &str) -> Option<(bool, &str)> {
    if let Some(rest) = strip_scheme_ci(value, "https://") {
        return Some((false, rest));
    }
    strip_scheme_ci(value, "http://").map(|rest| (true, rest))
}

/// The authority region of an absolute http(s) URL — everything after the scheme
/// and before the path, query or fragment.
///
/// **A backslash ends the authority too**, and that is the load-bearing part.
/// WHATWG treats `\` as `/` in a special scheme, so `http://evil.example\@127.0.0.1/x`
/// is a request to `evil.example` with `\@127.0.0.1/x` as its path — while a
/// splitter that only knows `/?#` reads the whole thing as an authority, takes
/// the userinfo off at the last `@`, and concludes the host is `127.0.0.1`. That
/// is two parsers disagreeing about the destination, which is exactly how a
/// cleartext-loopback exemption gets handed a remote host. Rather than teach this
/// crate the WHATWG grammar — it is the pure-logic core and carries no URL
/// dependency — the shape is refused outright by [`is_absolute_http_url`], so no
/// later reader has to be right about it.
fn url_authority(rest: &str) -> &str {
    rest.split(['/', '?', '#', '\\']).next().unwrap_or_default()
}

/// Whether `row` is a well-formed `[skills] trusted_project_roots` entry — the
/// shape `tetond::harness::tools::skill::durable_trust_root_name` mints
/// (REQ-591 D-5).
///
/// # What it checks, and what it deliberately does not
///
/// **Form only. It touches no filesystem.** Whether the tree exists, is
/// mounted, or still holds a repository is not a validity question: a laptop
/// with an unplugged drive would otherwise refuse to start a daemon over a
/// perfectly good config, and a row for a tree that is merely absent is exactly
/// the *inert* case the field's doc keeps inert. What this rejects is a row that
/// could never have named a tree on any machine — a spelling the minter cannot
/// produce, so a comparison against it can only ever fail.
///
/// The rule is the canonical mint's own shape:
///
/// - **absolute** — `canonicalize` returns an absolute path, and `~` is not
///   expanded anywhere on this path, so a `~/dev/repo` row is the exact
///   hand-written mistake this rule exists to catch;
/// - **no `.` or `..` component, and no empty one** — `canonicalize` resolves
///   them, so their presence means the row was not minted;
/// - **no trailing slash** except for the root itself, for the same reason;
/// - **well-formed percent escapes** — the mint writes `%XX` in upper-case hex
///   for each byte outside a valid UTF-8 sequence and `%25` for a literal `%`,
///   and nothing else in the string can contain a bare `%`.
///
/// # Why the rule lives here and the mint lives in `tetond`
///
/// This crate is I/O-free by construction and the mint reads the filesystem, so
/// they cannot be one function. What binds them is a test rather than a call:
/// `tetond`'s `every_name_the_minter_produces_is_a_row_this_config_accepts`
/// feeds real minted names through this predicate, so a change to either side
/// that separates them is caught where a shared function would have caught it.
#[must_use]
pub fn is_canonical_trust_root(row: &str) -> bool {
    if row.is_empty() || !row.starts_with('/') {
        return false;
    }
    if row.len() > 1 && row.ends_with('/') {
        return false;
    }
    // `split('/')` on an absolute path yields a leading empty segment, which is
    // the root and is the only empty one allowed.
    if row[1..]
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        && row != "/"
    {
        return false;
    }
    let mut bytes = row.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            continue;
        }
        // Upper-case hex, because that is what `format!("%{byte:02X}")` writes:
        // accepting lower-case here would admit a spelling the minter never
        // produces and therefore a row that never matches.
        let ok = |b: Option<u8>| matches!(b, Some(b'0'..=b'9' | b'A'..=b'F'));
        if !ok(bytes.next()) || !ok(bytes.next()) {
            return false;
        }
    }
    true
}

/// Whether `value` is an absolute `http`/`https` URL with a non-empty host.
///
/// Deliberately hand-rolled rather than pulling in a URL parser: this crate is
/// the pure-logic core and the check it needs is narrow — a scheme, a host, and
/// no embedded whitespace. Full URL semantics are the download client's problem
/// (`tetond`), which parses it for real before fetching anything.
///
/// The one place that narrowness is not enough is the backslash: see
/// [`url_authority`]. An authority containing one is refused here rather than
/// interpreted, because the two available interpretations name two different
/// hosts and this crate is not the parser that settles it.
///
/// **Public since REQ-578, for a second consumer.** `teton provider add` gates
/// its registration seam on exactly this predicate before it composes or stores
/// anything, so a provider endpoint and a `[web]` search endpoint are held to one
/// shape rule rather than two. It is deliberately *not* wired into
/// [`Config::validate`]: a hand-written config that already carries an odd
/// endpoint must keep loading (REQ-578 BR-6), and the shapes this refuses are
/// ones the CLI can still refuse at the moment they are typed.
///
/// What that buys the registration seam is the family of strings a URL parser
/// reads as an authority while a string-splitter does not: `http:/host`,
/// `http:\\host`, `http:/\host`, `http:\/host` (all of which `url` 2.5 resolves
/// to `http://host`), and `https://evil.example\@127.0.0.1/x`, whose host is
/// `evil.example` under WHATWG and `127.0.0.1` under a naive read. Requiring the
/// literal `://` and refusing a backslash in the authority removes the whole
/// family in one rule.
#[must_use]
pub fn is_absolute_http_url(value: &str) -> bool {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((_, rest)) = split_http_scheme(value) else {
        return false;
    };
    // Computed with `/?#` only, so a `\` *inside* the region those delimiters
    // bound is seen and refused rather than silently ending the authority early.
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !host.starts_with(':') && !host.contains('\\')
}

/// The query parameter the search seam sends the user's query as (REQ-563).
///
/// Named here rather than only at the seam because the config check and the
/// request builder have to agree about which name is spoken for: a constant in
/// one crate and a string literal in the other is exactly the pair that drifts.
const SEARCH_QUERY_PARAM: &str = "q";

/// The parameter *names* in `url`'s query string, in order.
///
/// Names only — a value is never yielded, because a query string is the likeliest
/// place in a URL for a credential to be sitting and this iterator feeds error
/// paths. The fragment is cut first: `#` ends the query, and a `?` after one
/// belongs to the fragment and is never sent.
fn url_query_names(url: &str) -> impl Iterator<Item = &str> {
    url.split('#')
        .next()
        .and_then(|before_fragment| before_fragment.split_once('?'))
        .map_or("", |(_, query)| query)
        .split('&')
        .map(|pair| pair.split('=').next().unwrap_or_default())
        .filter(|name| !name.is_empty())
}

/// The host of an absolute http(s) URL, without userinfo, port, or brackets.
///
/// Deliberately not a URL parser: it answers one question — which host would be
/// contacted — for a string [`is_absolute_http_url`] has already accepted. The
/// userinfo strip uses the *last* `@`, because a password may contain one and
/// the authority ends at the last.
///
/// The authority is taken by [`url_authority`], which ends at a backslash as well
/// — belt and braces, since `is_absolute_http_url` has already refused a URL
/// whose authority contains one, and the two readings must not be able to drift
/// apart if that order ever changes.
///
/// **Public since REQ-578**, so `teton provider add`'s cleartext warning can name
/// the host a key would travel to rather than carrying a second copy of this
/// reading. Callers owe it the same precondition the `[web]` path meets:
/// [`is_absolute_http_url`] first.
#[must_use]
pub fn url_host(url: &str) -> Option<&str> {
    let (_, rest) = split_http_scheme(url)?;
    let authority = url_authority(rest);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, after)| after);
    let host = host_port.strip_prefix('[').map_or_else(
        || host_port.split(':').next().unwrap_or_default(),
        |bracketed| bracketed.split(']').next().unwrap_or_default(),
    );
    (!host.is_empty()).then_some(host)
}

/// Whether `url` would put bytes on a wire in the clear.
///
/// `http://` to loopback is not that: nothing leaves the machine, so a
/// self-hosted backend on `http://127.0.0.1:8888` is an ordinary configuration
/// and not a credential exposure. Anything else `http://` is.
///
/// A host that cannot be extracted counts as remote — the failing-safe reading,
/// though [`is_absolute_http_url`] has already refused the hostless shapes.
///
/// The loopback set is `localhost` plus anything [`std::net::IpAddr`] calls
/// loopback. It is deliberately *narrower* than the set of strings that reach
/// loopback: `http://127.1`, `http://2130706433` and `http://[::ffff:127.0.0.1]`
/// all land on this machine and none of them parse as loopback here, so all three
/// are called remote. That errs towards saying "this is exposed" about something
/// that is not — noise, not a hole — which is the direction a credential warning
/// should fail in.
///
/// **Public since REQ-578**, so `teton provider add` can warn before a key is
/// typed into an `http://` registration using this rule rather than a copy of it.
/// Callers owe it [`is_absolute_http_url`] first, exactly as the `[web]` path
/// does.
#[must_use]
pub fn is_cleartext_to_a_remote_host(url: &str) -> bool {
    split_http_scheme(url).is_some_and(|(cleartext, _)| cleartext)
        && !url_host(url).is_some_and(is_loopback_host)
}

/// Whether `host` names this machine: `localhost`, or any address in a loopback
/// range (`127.0.0.0/8` is loopback in its entirety, not just `127.0.0.1`).
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Whether `value` is shaped like a bare domain pattern for the BR-11 allowlist
/// — a host (`docs.rs`) or a wildcard (`*.example.com`).
///
/// The charset `[A-Za-z0-9.*-]` does the work of several rules at once, which is
/// why it is stated as one: "no scheme" and "no path" fall out of it because
/// `:` and `/` are not in it, and so do the credential-bearing parts a
/// mis-pasted URL would bring along (`?`, `#`, `@`). Two rules the charset
/// cannot express are checked separately — a non-empty value, and no `..`, an
/// empty label that matches no host and is likelier a half-finished edit or a
/// relative-path fragment than an intent.
///
/// The 253-byte cap is the maximum length of a DNS name, so no pattern that
/// could name a real host is refused by it.
///
/// This is a *shape* check, in the spirit of [`is_model_name_shaped`]: whether a
/// pattern matches a given destination is the matcher's question, and whether
/// the host resolves is the network's.
fn is_domain_pattern_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '*'))
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
///
/// **Public since REQ-579**, for a second consumer with a stronger need than
/// convenience: `provider/setup_preview` refuses a candidate whose `key_ref` is
/// not a reference **before** it builds a candidate `Config` at all, so a raw
/// key that reached the wire is never cloned into a config, never serialized by
/// the delta engine, and never present in a document any refusal path could
/// quote. Deferring to [`Config::validate`] would refuse the same candidate one
/// step later, with the secret already inside the value being validated. One
/// definition either way: the daemon asks this function rather than carrying a
/// second opinion about what a reference is.
#[must_use]
pub fn is_recognized_auth_ref(value: &str) -> bool {
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
    use crate::category::{Category, ParseCategoryError};
    use crate::entities::{
        BoundaryMode, BoundaryOrigin, ModelProvider, ProviderCapabilities, ProviderKind,
        ToolCallTier,
    };
    use std::collections::{BTreeMap, BTreeSet};

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
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities::default(),
                },
                ModelProvider {
                    id: "anthropic".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com/v1/messages".to_owned()),
                    model: None,
                    auth_ref: Some("keychain:anthropic".to_owned()),
                    allow_cleartext: false,
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
                    allow_cleartext: false,
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

    // ---- REQ-559: effort key + per-provider reasoning declaration ----------

    /// The pre-REQ-559 config shape: no top-level `effort` key, and a
    /// `[providers.capabilities]` table carrying only the three fields that
    /// existed before. Written as raw TOML on purpose — the claim is that bytes
    /// authored before this REQ still parse and mean what they meant.
    const PRE_REQ_559_TOML: &str = r#"
[[providers]]
id = "anthropic"
kind = "anthropic"
endpoint = "https://api.anthropic.com"
model = "claude-opus-5"
auth_ref = "keychain:anthropic"

[providers.capabilities]
tool_call_tier = "native"
parallel_calls = true
max_context = 200000
"#;

    #[test]
    fn a_pre_req_559_config_still_loads_and_defaults_to_high() {
        let cfg = Config::load(PRE_REQ_559_TOML).expect("pre-REQ-559 config must load");
        // BR-1: the absence of a user setting resolves to the declared default,
        // never to an absent field.
        assert_eq!(cfg.effort, crate::effort::EffortLevel::High);
        let p = &cfg.providers[0];
        assert_eq!(p.capabilities.max_context, 200_000);
        // Undeclared, so `resolve_effort` applies the per-kind default rather
        // than a value materialized at load time.
        assert!(p.capabilities.reasoning_shape.is_none());
        assert!(p.capabilities.effort_ladder.is_none());
    }

    #[test]
    fn a_declared_shape_and_ladder_round_trip_through_toml() {
        let src = r#"
effort = "xhigh"

[[providers]]
id = "kimi"
kind = "openai-compatible"
endpoint = "https://api.moonshot.example"
model = "kimi-k2.6"
auth_ref = "keychain:kimi"

[providers.capabilities]
reasoning_shape = "thinking_flag_only"
effort_ladder = ["low", "high", "xhigh", "max"]
"#;
        let cfg = Config::load(src).expect("must load");
        assert_eq!(cfg.effort, crate::effort::EffortLevel::Xhigh);
        let caps = cfg.providers[0].capabilities;
        assert_eq!(
            caps.reasoning_shape,
            Some(crate::effort::ReasoningShape::ThinkingFlagOnly),
        );
        assert_eq!(
            caps.effort_ladder,
            Some(crate::effort::EffortLadder::from_levels(&[
                crate::effort::EffortLevel::Low,
                crate::effort::EffortLevel::High,
                crate::effort::EffortLevel::Xhigh,
                crate::effort::EffortLevel::Max,
            ])),
        );

        // load -> serialize -> load is lossless. The ladder has a hand-written
        // serde pair, so this is the path a silent drift would take.
        let round = Config::load(&toml::to_string(&cfg).expect("serialize")).expect("reload");
        assert_eq!(round, cfg);
    }

    // ---- REQ-586: per-provider context budget cap ---------------------------

    /// REQ-586 BR-5: a declared `context_budget_cap` loads, survives a
    /// serialize → load round trip, and — because zero is "no cap" — a record
    /// without one writes **no line** for it, so the canonical
    /// `[providers.capabilities]` rendering of every existing record is
    /// unchanged (the REQ-574 preservation witnesses list those keys byte for
    /// byte).
    ///
    /// No `validate()` rule, deliberately: a cap above the window is inert,
    /// not invalid (architecture ADR-7 — the derivation takes the minimum, so
    /// it cannot bind), and REQ-557 ADR-E keeps `validate` structural-only. So
    /// the over-window case is asserted to load, validate and leave the
    /// provider usable, rather than asserted to refuse.
    #[test]
    fn a_declared_context_budget_cap_round_trips_and_zero_writes_no_line() {
        let src = r#"
[[providers]]
id = "kimi"
kind = "openai-compatible"
endpoint = "https://api.moonshot.example"
model = "kimi-k2.6"
auth_ref = "keychain:kimi"

[providers.capabilities]
max_context = 131072
context_budget_cap = 65536
"#;
        let cfg = Config::load(src).expect("must load");
        let caps = cfg.providers[0].capabilities;
        assert_eq!(caps.max_context, 131_072);
        assert_eq!(caps.context_budget_cap, 65_536);

        let text = toml::to_string(&cfg).expect("serialize");
        assert!(
            text.contains("context_budget_cap = 65536"),
            "a declared cap must be visible in the written config, got:\n{text}",
        );
        let round = Config::load(&text).expect("reload");
        assert_eq!(round, cfg);

        // Pre-REQ-586 bytes: no cap key at all reads as "no cap", and writes
        // back out without one — the canonical rendering did not grow a line.
        let cfg = Config::load(PRE_REQ_559_TOML).expect("pre-REQ-586 config must load");
        assert_eq!(cfg.providers[0].capabilities.context_budget_cap, 0);
        let text = toml::to_string(&cfg).expect("serialize");
        assert!(
            !text.contains("context_budget_cap"),
            "zero is \"no cap\" and no cap is no line, got:\n{text}",
        );
        assert!(
            text.contains("max_context = 200000"),
            "the window is still written out as it always was, got:\n{text}",
        );

        // A cap above the window is inert, not invalid: it loads, it
        // validates, and the provider stays usable.
        let src = r#"
[[providers]]
id = "kimi"
kind = "openai-compatible"
endpoint = "https://api.moonshot.example"
model = "kimi-k2.6"
auth_ref = "keychain:kimi"

[providers.capabilities]
max_context = 32000
context_budget_cap = 1000000
"#;
        let cfg = Config::load(src).expect("an over-window cap must not refuse startup");
        assert_eq!(cfg.providers[0].capabilities.context_budget_cap, 1_000_000);
        assert!(
            cfg.unusable_providers().is_empty(),
            "an inert cap is not a reason to stop serving turns",
        );
    }

    /// A declared default that vanishes from a written-out config whenever it
    /// holds its default value is the hidden constant configuration-visibility
    /// rules out — the same reason `judgment_default` is unconditional.
    #[test]
    fn the_effort_key_is_always_written_out_even_at_its_default() {
        let cfg = Config::load("").expect("empty config loads");
        assert_eq!(cfg.effort, crate::effort::EffortLevel::High);
        let text = toml::to_string(&cfg).expect("serialize");
        assert!(
            text.contains("effort = \"high\""),
            "the default must be visible in the written config, got:\n{text}",
        );
    }

    /// ADR-E: `validate` is fail-closed and gates daemon startup, so it carries
    /// structural errors only. An effort misconfiguration must not refuse to
    /// start the daemon, and must not mark the provider unusable either — an
    /// empty ladder resolves to `Omit(EmptyLadder)` and is reported on the
    /// surface instead.
    #[test]
    fn an_explicitly_empty_ladder_loads_validates_and_leaves_the_provider_usable() {
        let src = r#"
[[providers]]
id = "weird"
kind = "openai-compatible"
endpoint = "https://api.weird.example"
model = "weird-1"
auth_ref = "keychain:weird"

[providers.capabilities]
effort_ladder = []
"#;
        let cfg = Config::load(src).expect("an empty ladder must not refuse startup");
        assert_eq!(
            cfg.providers[0].capabilities.effort_ladder,
            Some(crate::effort::EffortLadder::EMPTY),
        );
        assert!(
            cfg.unusable_providers().is_empty(),
            "an effort misconfiguration is not a reason to stop serving turns",
        );
    }

    fn sample_config() -> Config {
        Config {
            pinned_local_model: None,
            // REQ-559: the sample carries a non-default level on purpose, so a
            // serialization round-trip that silently dropped the key would fail
            // rather than coincide with the default.
            effort: crate::effort::EffortLevel::Xhigh,
            default_provider: Some("anthropic-prod".to_owned()),
            local_model: LocalModelConfig {
                pinned: Some("qwen2.5-coder-3b".to_owned()),
                auto_accept: false,
                base_url: Some("https://hf-mirror.example.com".to_owned()),
            },
            // REQ-562: the shared fixture stays on the default (off) posture —
            // the opt-in has its own tests, and every caller here is asserting
            // something else.
            privacy: PrivacyConfig::default(),
            // REQ-588: default (no ceiling), so the round trip proves the table
            // stays out of a config that never opted in.
            cost: CostConfig::default(),
            // REQ-563: likewise off, for the same reason.
            web: WebConfig::default(),
            // REQ-565: the shipped default (exit with the last client); the
            // policy modes have their own tests.
            lifetime: LifetimeConfig::default(),
            // REQ-560: likewise the shipped default (guarded); the levels have
            // their own tests.
            permissions: PermissionsConfig::default(),
            // REQ-589 D-13: likewise the shipped default (nothing durably
            // acknowledged), so the round trip proves `[skills]` stays out of a
            // config that never trusted a repository.
            skills: SkillsConfig::default(),
            // REQ-607: likewise the shipped default (the ssh agent withheld),
            // so the round trip proves `[shell]` stays out of a config that
            // never opted in.
            shell: ShellConfig::default(),
            // REQ-611 BR-1: likewise the shipped default (no transcript), so
            // the round trip proves `[transcript]` stays out of a config that
            // never opted in.
            transcript: TranscriptConfig::default(),
            // REQ-612 BR-2: likewise the shipped default — which here is the
            // feature *on*, so the round trip proves `[context]` stays out of a
            // config that never turned the notes off.
            context: ContextConfig::default(),
            providers: vec![
                ModelProvider {
                    id: "local".to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    // Local: model is owned by the REQ-547 consent flow, not here.
                    model: None,
                    auth_ref: None,
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities {
                        tool_call_tier: ToolCallTier::Degraded,
                        parallel_calls: false,
                        max_context: 8192,
                        ..ProviderCapabilities::default()
                    },
                },
                ModelProvider {
                    id: "anthropic-prod".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com".to_owned()),
                    model: Some("claude-opus-5".to_owned()),
                    auth_ref: Some("keychain:anthropic-prod".to_owned()),
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities {
                        tool_call_tier: ToolCallTier::Native,
                        parallel_calls: true,
                        max_context: 200_000,
                        ..ProviderCapabilities::default()
                    },
                },
                ModelProvider {
                    id: "deepseek".to_owned(),
                    kind: ProviderKind::OpenaiCompatible,
                    endpoint: Some("https://api.deepseek.com".to_owned()),
                    model: Some("deepseek-chat".to_owned()),
                    auth_ref: Some("keychain:deepseek".to_owned()),
                    allow_cleartext: false,
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
                PrivacyBoundary::user("secrets/**", BoundaryMode::LocalOnly),
                PrivacyBoundary::user("docs/**", BoundaryMode::RedactThenRemote),
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

    /// **A provider credential beside a cleartext endpoint is refused by
    /// default, exactly as `[web]`'s is** (BUG-202).
    ///
    /// The pair is what is refused: `http://` to a provider that wants no
    /// credential is the user's own call, and loopback is exempt because
    /// nothing leaves the machine.
    ///
    /// **Mutation (run, not assumed):** disabling the guard in `validate`'s
    /// provider loop turns **4** tests red — this one,
    /// `allow_cleartext_permits_the_pair_it_is_set_on_and_nothing_else` (its
    /// falsification half), `the_provider_cleartext_rule_folds_scheme_case`, and
    /// `the_provider_cleartext_refusal_names_the_provider_and_the_escape_hatch`.
    ///
    /// The number that matters is the **fourth**:
    /// `a_search_key_beside_a_cleartext_remote_endpoint_is_refused` stays
    /// **green** under that mutation. The `[web]` rule and this one are separate
    /// enforcement points of one invariant, and the web test never covered the
    /// provider path — which is precisely how this hole survived to be found by
    /// audit instead of by CI (conventions.md, "an invariant with more than one
    /// enforcement point needs a sweep").
    #[test]
    fn a_provider_credential_beside_a_cleartext_remote_endpoint_is_refused() {
        for remote in [
            "http://api.example.com/v1/chat/completions",
            "http://192.0.2.10:8888/v1/chat/completions",
            "http://user@api.example.com/v1/chat/completions",
            "http://[2001:db8::1]/v1/chat/completions",
        ] {
            let mut cfg = sample_config();
            cfg.providers[2].endpoint = Some((*remote).to_owned());
            cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::AuthRefOverCleartextEndpoint {
                    provider_id: "deepseek".to_owned(),
                    host: url_host(cfg.providers[2].endpoint.as_deref().expect("endpoint"))
                        .expect("host")
                        .to_owned(),
                },
                "a credential was allowed to travel in the clear to {remote}"
            );
        }

        // Loopback is exempt: nothing leaves the machine, and refusing it would
        // push a local Ollama or llama.cpp server toward a self-signed
        // certificate for no gain.
        for local in [
            "http://localhost:11434/v1/chat/completions",
            "http://LOCALHOST:11434/v1/chat/completions",
            "http://127.0.0.1:11434/v1/chat/completions",
            "http://127.9.9.9/v1/chat/completions",
            "http://[::1]:11434/v1/chat/completions",
        ] {
            let mut cfg = sample_config();
            cfg.providers[2].endpoint = Some((*local).to_owned());
            cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
            cfg.validate()
                .unwrap_or_else(|e| panic!("{local} is loopback and needs no TLS: {e}"));
        }

        // https is fine anywhere, and cleartext with no credential is not this
        // rule's business.
        for (endpoint, auth) in [
            (
                "https://api.example.com/v1/chat/completions",
                Some("keychain:deepseek"),
            ),
            ("http://api.example.com/v1/chat/completions", None),
        ] {
            let mut cfg = sample_config();
            cfg.providers[2].endpoint = Some((*endpoint).to_owned());
            cfg.providers[2].auth_ref = auth.map(str::to_owned);
            cfg.validate()
                .unwrap_or_else(|e| panic!("{endpoint} with auth {auth:?} must validate: {e}"));
        }
    }

    /// **`allow_cleartext` is the deliberate escape hatch, and it works for the
    /// case it exists for** (BUG-202): a self-hosted model server on a LAN,
    /// which `is_cleartext_to_a_remote_host` cannot tell from a public host.
    ///
    /// Falsification is the second half: the same config with the flag back off
    /// is refused, so this asserts the flag rather than asserting that these
    /// endpoints were permitted all along.
    #[test]
    fn allow_cleartext_permits_the_pair_it_is_set_on_and_nothing_else() {
        for lan in [
            "http://10.0.1.50:8000/v1/chat/completions",
            "http://192.168.1.20:8000/v1/chat/completions",
            "http://models.corp.example.com/v1/chat/completions",
        ] {
            let mut cfg = sample_config();
            cfg.providers[2].endpoint = Some((*lan).to_owned());
            cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
            cfg.providers[2].allow_cleartext = true;
            cfg.validate()
                .unwrap_or_else(|e| panic!("{lan} was opted in and must validate: {e}"));

            // Falsification: the flag is what permitted it.
            cfg.providers[2].allow_cleartext = false;
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::AuthRefOverCleartextEndpoint {
                    provider_id: "deepseek".to_owned(),
                    host: url_host(cfg.providers[2].endpoint.as_deref().expect("endpoint"))
                        .expect("host")
                        .to_owned(),
                },
                "{lan} passed without the opt-out, so the opt-out proved nothing"
            );
        }

        // The opt-out is per provider, not global: setting it on one leaves
        // another's cleartext pair refused.
        let mut cfg = sample_config();
        cfg.providers[2].endpoint = Some("http://10.0.1.50:8000/v1".to_owned());
        cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
        cfg.providers[2].allow_cleartext = true;
        cfg.providers[1].endpoint = Some("http://api.example.com/v1".to_owned());
        cfg.providers[1].auth_ref = Some("keychain:anthropic-prod".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::AuthRefOverCleartextEndpoint {
                provider_id: "anthropic-prod".to_owned(),
                host: "api.example.com".to_owned(),
            },
            "one provider's opt-out excused another's cleartext credential"
        );
    }

    /// The scheme fold that `[web]` pins for its own readers has to hold here
    /// too: `HTTP://` is cleartext, and a rule reading `starts_with("http://")`
    /// would send the credential out believing it was TLS (BUG-202).
    #[test]
    fn the_provider_cleartext_rule_folds_scheme_case() {
        for shouty in [
            "HTTP://api.example.com/v1/chat/completions",
            "Http://api.example.com/v1/chat/completions",
        ] {
            let mut cfg = sample_config();
            cfg.providers[2].endpoint = Some((*shouty).to_owned());
            cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::AuthRefOverCleartextEndpoint {
                    provider_id: "deepseek".to_owned(),
                    host: url_host(cfg.providers[2].endpoint.as_deref().expect("endpoint"))
                        .expect("host")
                        .to_owned(),
                },
                "{shouty} was read as a URL by one check and not by the other"
            );
        }
    }

    /// The refusal names the provider, the remedy, and the escape hatch, and
    /// echoes no credential (conventions.md: no credential in an error message;
    /// LESSON-557: compose the sentence where the facts are).
    #[test]
    fn the_provider_cleartext_refusal_names_the_provider_and_the_escape_hatch() {
        let mut cfg = sample_config();
        cfg.providers[2].endpoint = Some("http://api.example.com/v1".to_owned());
        cfg.providers[2].auth_ref = Some("keychain:deepseek".to_owned());
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(msg.contains("deepseek"), "must name the provider: {msg}");
        assert!(
            msg.contains("api.example.com"),
            "must name the host the credential travels to: {msg}"
        );
        assert!(msg.contains("https://"), "must name the remedy: {msg}");
        assert!(
            msg.contains("loopback"),
            "must name the loopback exemption: {msg}"
        );
        assert!(
            msg.contains("allow_cleartext"),
            "must name the escape hatch, or the refusal is a dead end: {msg}"
        );
        // BUG-205: naming the config *field* was not enough — `provider add` is
        // the only command that stores a keychain entry, so a refusal that names
        // only a hand-edit leaves no supported way to register at all. The
        // remedy has to be a command the user can run.
        assert!(
            msg.contains("teton provider add --allow-cleartext"),
            "must name a remedy the CLI can actually perform: {msg}"
        );
        assert!(
            !msg.contains("keychain:deepseek"),
            "the error echoed the credential reference: {msg}"
        );
    }

    /// `allow_cleartext` stays out of a config that never opted in, and comes
    /// back when it did (BUG-202).
    ///
    /// The absent half is the point: a security-relevant opt-out that
    /// serialized as `allow_cleartext = false` into every provider table would
    /// be unreadable noise, and the field is meant to be greppable exactly
    /// where somebody deliberately turned the protection off.
    #[test]
    fn allow_cleartext_round_trips_and_defaults_to_absent() {
        let cfg = sample_config();
        let rendered = toml::to_string(&cfg).expect("serialize");
        assert!(
            !rendered.contains("allow_cleartext"),
            "a config that never opted in carries no allow_cleartext line:\n{rendered}"
        );
        let back: Config = toml::from_str(&rendered).expect("round trip");
        assert!(back.providers.iter().all(|p| !p.allow_cleartext));

        let mut opted = sample_config();
        opted.providers[2].allow_cleartext = true;
        let rendered = toml::to_string(&opted).expect("serialize");
        assert!(
            rendered.contains("allow_cleartext = true"),
            "the opt-in must survive a round trip:\n{rendered}"
        );
        let back: Config = toml::from_str(&rendered).expect("round trip");
        assert!(back.providers[2].allow_cleartext);
        assert!(!back.providers[1].allow_cleartext);
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

    // ---- REQ-562: the `[privacy]` opt-in (BR-10, AC-13, AC-14) -------------

    /// A config carrying the opt-in *and* a full routing table — the fixture the
    /// AC-14 test needs, because the interesting claim is that the two coexist
    /// without the switch reopening the binding surface.
    const PRIVACY_OPT_IN_TOML: &str = r#"
[privacy]
redact = true

[[providers]]
id = "on-device"
kind = "local"

[[tiers]]
tier = "reflex"
provider_id = "on-device"
"#;

    #[test]
    fn a_config_with_no_privacy_table_leaves_redaction_off() {
        // AC-13's "off by default" leg, asserted on documents that predate the
        // table entirely — a pre-REQ-562 config is the common case, and it must
        // load to the off state rather than failing on a missing key.
        for (label, toml_text) in [
            ("pre-REQ-557", PRE_REQ_557_TOML),
            ("the tier table", TIER_TABLE_TOML),
        ] {
            let cfg = Config::load(toml_text).expect("must load");
            assert!(
                !cfg.privacy.redact,
                "{label}: a config that never named [privacy] opted in"
            );
        }
        assert!(
            !Config::default().privacy.redact,
            "the struct default must agree with the parsed default"
        );
        assert!(Config::default().privacy.is_unset());
    }

    #[test]
    fn the_privacy_table_reads_the_opt_in_rather_than_defaulting_it() {
        // LESSON-485: a fixture that cannot discriminate is not a test. "It
        // loads" would pass against a `redact` field wired to a constant, so
        // parse the two documents that differ only in this value and assert
        // they disagree — the read is what is under test, not the parse.
        let on = Config::load("[privacy]\nredact = true\n").expect("must load");
        let off = Config::load("[privacy]\nredact = false\n").expect("must load");
        assert!(
            on.privacy.redact,
            "`redact = true` did not survive the load"
        );
        assert!(!off.privacy.redact);
        assert_ne!(
            on.privacy, off.privacy,
            "the parsed value does not depend on the document"
        );

        // A present-but-empty table is the off state too: `#[serde(default)]` on
        // the field means an author who writes the header and nothing else has
        // opted into nothing.
        let empty = Config::load("[privacy]\n").expect("must load");
        assert!(!empty.privacy.redact);
        assert_eq!(empty.privacy, off.privacy);
    }

    #[test]
    fn the_privacy_table_round_trips_and_stays_out_of_a_config_that_never_opted_in() {
        // The REQ-557 round-trip rule: what the daemon writes back must be what
        // a user could have authored. The opt-in survives a write/read cycle...
        let cfg = Config::load(PRIVACY_OPT_IN_TOML).expect("must load");
        let toml_text = cfg.to_toml().expect("serialize");
        assert!(toml_text.contains("[privacy]"), "toml: {toml_text}");
        assert!(toml_text.contains("redact = true"), "toml: {toml_text}");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        assert!(back.privacy.redact);
        Config::load(&toml_text).expect("a re-serialized config must still validate");

        // ...and a config that never opted in does not grow the table, exactly
        // as `[local_model]` stays out of a config that never set one.
        let untouched = Config::load(TIER_TABLE_TOML).expect("must load");
        let written = untouched.to_toml().expect("serialize");
        assert!(
            !written.contains("[privacy]"),
            "an unset opt-in was written into the user's config: {written}"
        );
        assert_eq!(
            Config::from_toml(&written).expect("deserialize").privacy,
            PrivacyConfig::default()
        );
    }

    #[test]
    fn the_privacy_opt_in_does_not_make_redact_a_bindable_category() {
        // AC-14. BR-10's whole warning is that the switch could be built as a
        // `[[categories]]` row, which would make `redact` deserializable as a
        // configurable category again and undo REQ-558 ADR-B. This test goes red
        // if a later change relocates the switch or adds the variant.
        let cfg = Config::load(PRIVACY_OPT_IN_TOML).expect("must load");
        assert!(cfg.privacy.redact, "the fixture must have the switch ON");
        assert!(
            cfg.categories.is_empty(),
            "the opt-in must not have materialized a category override"
        );

        // The pin as a type: ten bindable categories, none of them `redact`.
        // The census is stated rather than derived so that *adding* one shows up
        // as a diff a reviewer reads — REQ-613 TASK-381's `draft` is the tenth,
        // and this line is where it had to be admitted.
        assert_eq!(ConfigurableCategory::ALL.len(), 10);
        for c in ConfigurableCategory::ALL {
            assert_ne!(Category::from(c), Category::Redact, "{c} maps to redact");
        }
        assert_eq!(
            "redact".parse::<ConfigurableCategory>().unwrap_err(),
            ParseCategoryError::RedactIsPinned
        );

        // And the file path, with the opt-in present in the same document: a
        // `[[categories]]` entry naming `redact` is still refused at load, and
        // still says *pinned* rather than reading as a typo.
        let err = Config::load(&format!(
            "{PRIVACY_OPT_IN_TOML}\n[[categories]]\nname = \"redact\"\nprovider_id = \"on-device\"\n"
        ))
        .expect_err("the opt-in must not have made `redact` bindable");
        assert!(matches!(err, LoadError::Parse(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("redact"), "{msg}");
        assert!(
            msg.contains("pinned"),
            "the rejection must name the pin, not read as a typo: {msg}"
        );
    }

    #[test]
    fn an_unknown_key_in_the_privacy_table_is_ignored_like_any_other_unknown_key() {
        // No new unknown-key posture: `Config` has no `deny_unknown_fields`
        // anywhere, so a stray key is ignored, and `[privacy]` matches that
        // rather than inventing a stricter rule of its own.
        let stray_top_level = Config::load("nonsense_key = 1\n").expect("must load");
        assert_eq!(stray_top_level, Config::default());

        // The specific stray key worth naming: BR-10 forbids a provider/model/
        // tier key here, so one written anyway must not bind anything. It is
        // dropped on the floor exactly like the top-level case above.
        let cfg = Config::load("[privacy]\nredact = true\nprovider_id = \"anthropic\"\n")
            .expect("must load");
        assert!(cfg.privacy.redact);
        assert!(cfg.categories.is_empty() && cfg.tiers.is_empty());
        assert_eq!(
            cfg.privacy,
            Config::load("[privacy]\nredact = true\n")
                .expect("must load")
                .privacy,
            "an unknown key changed what [privacy] means"
        );
        // The table has exactly one key, and it is not a binding: a serialized
        // opt-in cannot smuggle a provider back in.
        let written = cfg.to_toml().expect("serialize");
        assert!(!written.contains("anthropic"), "toml: {written}");
    }

    // ---- REQ-611: the `[transcript]` table (BR-1, BR-13, AC-19) ------------

    /// **REQ-611 BR-1 / TASK-360.** Off by default, from every direction a
    /// config can arrive, and a written-out table states the posture rather
    /// than leaving it to be inferred.
    ///
    /// The two arrivals are deliberately separate cases: a file with **no**
    /// `[transcript]` table and a file that names the table but writes no
    /// `enabled` key are different serde paths (the struct-level `default`
    /// versus the field-level one), and a schema that gets one right and the
    /// other wrong is the shape BR-1 has to rule out — "on" arriving by
    /// omission is exactly the failure the default exists to prevent.
    ///
    /// **Mutation** (LESSON-441): set `enabled: true` in
    /// `impl Default for TranscriptConfig` — this goes red on the first four
    /// assertions (`assert!(!…enabled)`), while nothing else in
    /// `teton-core` notices. Restored.
    #[test]
    fn transcript_table_defaults_to_off_and_states_its_posture() {
        // No table at all — the stock install, and a config authored before
        // this REQ existed.
        for (label, document) in [
            ("empty document", ""),
            ("an unrelated table", "[privacy]\nredact = true\n"),
        ] {
            let cfg = Config::load(document).expect("must load");
            assert!(
                !cfg.transcript.enabled,
                "{label}: a config that never named [transcript] opted in"
            );
            assert!(cfg.transcript.is_unset(), "{label}");
        }
        assert!(
            !Config::default().transcript.enabled,
            "the in-memory default opted in"
        );

        // The table named, the key omitted: still off, and indistinguishable
        // from the absence.
        let named = Config::load("[transcript]\n").expect("must load");
        assert!(!named.transcript.enabled);
        assert_eq!(named.transcript, TranscriptConfig::default());
        assert_eq!(
            named.transcript,
            Config::load("[transcript]\nenabled = false\n")
                .expect("must load")
                .transcript,
            "writing the default explicitly is not a third state"
        );

        // Reading the opt-in rather than defaulting it.
        let on = Config::load("[transcript]\nenabled = true\n").expect("must load");
        assert!(on.transcript.enabled);

        // "States its posture": whenever the table is emitted at all, `enabled`
        // is in it — including when the thing that made it non-default was some
        // other key. A reader of the file never has to infer the switch.
        let written = on.to_toml().expect("serialize");
        assert!(written.contains("[transcript]"), "toml: {written}");
        assert!(written.contains("enabled = true"), "toml: {written}");
        let only_retention = Config::load("[transcript]\nretain_days = 7\n").expect("must load");
        let written = only_retention.to_toml().expect("serialize");
        assert!(
            written.contains("enabled = false"),
            "an emitted [transcript] table must state the switch: {written}"
        );

        // And BR-1's other half: a config that never opted in does not grow the
        // table on a write, exactly as `[privacy]` does not.
        let untouched = Config::load("[privacy]\nredact = true\n").expect("must load");
        let written = untouched.to_toml().expect("serialize");
        assert!(
            !written.contains("[transcript]"),
            "an unset table was written into the user's config: {written}"
        );
    }

    /// **REQ-611 BR-13 / TASK-360.** The retention window and the record
    /// budget carry the declared defaults, and `retain_days = 0` — "never
    /// prune" — is a setting rather than an error.
    ///
    /// The table-present-key-absent case is repeated from BR-1's test on
    /// purpose: `retain_days` and `max_record_bytes` reach their defaults
    /// through a *named function* (`#[serde(default = "…")]`), which is a
    /// different mechanism from `bool`'s `Default`, and it is the mechanism
    /// that silently yields `0` if the attribute is ever dropped.
    ///
    /// **Mutation** (LESSON-441): change `default_retain_days` to return `7`
    /// — three assertions go red (the absent table, the named-but-empty table,
    /// and the in-memory default). Restored.
    #[test]
    fn transcript_retention_and_record_size_defaults() {
        for (label, document) in [
            ("no table", ""),
            (
                "table present, keys absent",
                "[transcript]\nenabled = true\n",
            ),
        ] {
            let cfg = Config::load(document).expect("must load");
            assert_eq!(cfg.transcript.retain_days, 30, "{label}");
            assert_eq!(cfg.transcript.max_record_bytes, 65_536, "{label}");
        }
        assert_eq!(TranscriptConfig::default().retain_days, 30);
        assert_eq!(TranscriptConfig::default().max_record_bytes, 65_536);

        // `0` is "never prune" (BR-13), not a malformed window: it parses, it
        // validates, and it is not silently promoted to the default.
        let forever = Config::load("[transcript]\nenabled = true\nretain_days = 0\n")
            .expect("retain_days = 0 must load");
        assert_eq!(forever.transcript.retain_days, 0);
        forever
            .validate()
            .expect("never-prune is a policy, not a validation failure");

        // Both keys are read from the file when written.
        let set = Config::load("[transcript]\nretain_days = 1\nmax_record_bytes = 4096\n")
            .expect("must load");
        assert_eq!(set.transcript.retain_days, 1);
        assert_eq!(set.transcript.max_record_bytes, 4096);
        set.validate().expect("a valid table passes");
    }

    /// **REQ-611 ADR-4 / TASK-360.** `effective_dir` is the user's `dir` when
    /// set and `<data dir>/transcripts` otherwise — and it is pure.
    ///
    /// Purity is asserted by handing it a data directory that does not exist
    /// and checking nothing appeared: `teton-core` performs no I/O, and this is
    /// the one method in it that holds a filesystem path and would be tempting
    /// to make create its own directory.
    #[test]
    fn the_effective_transcript_dir_is_the_users_dir_or_the_data_dir_default() {
        let data_dir = Path::new("/does/not/exist/teton");

        let default = TranscriptConfig::default();
        assert_eq!(
            default.effective_dir(data_dir),
            PathBuf::from("/does/not/exist/teton/transcripts")
        );

        let chosen = TranscriptConfig {
            dir: Some(PathBuf::from("/srv/records/teton")),
            ..TranscriptConfig::default()
        };
        assert_eq!(
            chosen.effective_dir(data_dir),
            PathBuf::from("/srv/records/teton"),
            "a configured dir is used as written, not joined under the data dir"
        );

        assert!(
            !data_dir.exists(),
            "effective_dir must not create anything — teton-core performs no I/O"
        );
    }

    /// **REQ-611 BR-12 / TASK-360.** A record budget too small to describe its
    /// own truncation is a structural error, refused at load.
    ///
    /// **Mutation** (LESSON-441, "invert the gate and count what fails"): drop
    /// `self.validate_transcript()?` from `Config::validate`. **Three** tests go
    /// red — this one, `a_relative_transcript_dir_is_refused_by_validate`, and
    /// `every_config_error_variant_validate_can_raise_is_asserted_by_a_test`,
    /// which sees the two raises leave the call tree it walks. Restored.
    #[test]
    fn a_transcript_record_budget_below_the_floor_is_refused_by_validate() {
        let mut cfg = sample_config();
        cfg.transcript.max_record_bytes = 10;
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::TranscriptRecordSizeTooSmall { bytes: 10 }
        );

        // The message names the key the user has to edit, and the floor.
        let err = Config::load("[transcript]\nmax_record_bytes = 10\n")
            .expect_err("a sub-kilobyte budget must be refused at load");
        let rendered = format!("{err}");
        assert!(rendered.contains("max_record_bytes"), "{rendered}");
        assert!(rendered.contains("1024"), "{rendered}");

        // The boundary itself is valid — the rule is "below 1024", not "below
        // or at".
        cfg.transcript.max_record_bytes = 1024;
        cfg.validate().expect("the floor itself is a valid budget");
    }

    /// **REQ-611 BR-8 / TASK-360.** A relative `dir` is a structural error: it
    /// would name a different tree depending on where the daemon was started,
    /// and the tool jail's denied prefix is derived from it.
    ///
    /// **Mutation**: the gate inversion recorded on
    /// `a_transcript_record_budget_below_the_floor_is_refused_by_validate`
    /// reddens this one too — it is the second of that mutation's three.
    #[test]
    fn a_relative_transcript_dir_is_refused_by_validate() {
        let mut cfg = sample_config();
        cfg.transcript.dir = Some(PathBuf::from("transcripts"));
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::TranscriptDirNotAbsolute
        );

        // `~` is not expanded by this crate, so a tilde path is relative too —
        // the case a user is most likely to write by hand.
        cfg.transcript.dir = Some(PathBuf::from("~/teton-transcripts"));
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::TranscriptDirNotAbsolute
        );

        // An absolute one passes, and so does an absent one.
        cfg.transcript.dir = Some(PathBuf::from("/srv/records/teton"));
        cfg.validate().expect("an absolute dir is valid");
        cfg.transcript.dir = None;
        cfg.validate().expect("an absent dir is the ordinary case");
    }

    /// **REQ-611 AC-19 (schema half) / TASK-360.** The table round-trips, and
    /// the *effective* directory never reaches the file — only a `dir` the user
    /// wrote does.
    ///
    /// The rendering half of AC-19 lives in
    /// `config_doc::tests::the_transcript_table_is_written_only_when_the_user_named_it`;
    /// this is the schema's own share of it, which is what makes the derived
    /// path unwritable rather than merely unwritten.
    #[test]
    fn the_transcript_table_round_trips_and_never_carries_the_derived_directory() {
        let cfg =
            Config::load("[transcript]\nenabled = true\nretain_days = 7\n").expect("must load");
        let toml_text = cfg.to_toml().expect("serialize");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        Config::load(&toml_text).expect("a re-serialized config must still validate");

        // `dir` was not written, so it is not emitted — and neither is the
        // directory `effective_dir` would derive for it.
        assert!(!toml_text.contains("dir ="), "toml: {toml_text}");
        let derived = cfg.transcript.effective_dir(Path::new("/var/lib/teton"));
        assert!(
            !toml_text.contains(&derived.display().to_string()),
            "the derived transcript directory reached the user's config: {toml_text}"
        );

        // A `dir` the user did write survives verbatim.
        let with_dir = Config::load("[transcript]\nenabled = true\ndir = \"/srv/records/teton\"\n")
            .expect("must load");
        let toml_text = with_dir.to_toml().expect("serialize");
        assert!(
            toml_text.contains("/srv/records/teton"),
            "toml: {toml_text}"
        );
        assert_eq!(
            Config::from_toml(&toml_text)
                .expect("deserialize")
                .transcript,
            with_dir.transcript
        );
    }

    // ---- REQ-612: the `[context]` table (BR-2) -----------------------------

    /// **REQ-612 BR-2 / TASK-369.** The repository notes are **on** by default,
    /// from every direction a config can arrive, and a written-out table states
    /// the posture rather than leaving it to be inferred.
    ///
    /// The three arrivals are separate cases because they are three different
    /// serde paths, and a schema that gets one right and another wrong is
    /// exactly what BR-2 has to rule out. The middle one — the table **named**
    /// with no `repo_file` key — is the trap: serde's *field*-level
    /// `#[serde(default)]` resolves to `bool::default()`, which is `false`, and
    /// it outranks the container's `#[serde(default)]`. Written that way, naming
    /// `[context]` for any other reason would silently turn the feature off.
    /// [`default_repo_file`] exists so it cannot.
    ///
    /// The last leg is the write posture, and it is the one that keeps the table
    /// out of a file whose author never mentioned it — the [`PrivacyConfig`]
    /// rule, and the integration half of it is
    /// `tetond/tests/config_preservation.rs::a_named_context_table_survives_an_unrelated_write_and_an_unnamed_one_is_not_added`.
    ///
    /// **Mutations** (LESSON-441), all three run 2026-09-03 and restored:
    /// 1. `default_repo_file` returns `false` — the first three legs go red
    ///    (every default arrival reads as off);
    /// 2. `#[serde(default = "default_repo_file")]` on `ContextConfig::repo_file`
    ///    replaced by a bare `#[serde(default)]` — only the named-but-empty-table
    ///    leg goes red, which is the point of writing that leg down;
    /// 3. `skip_serializing_if = "ContextConfig::is_unset"` dropped from
    ///    `Config::context` — only the last leg goes red (an untouched config
    ///    grows a `[context]` table on write).
    #[test]
    fn context_table_defaults_repo_file_to_true() {
        // No table at all — the stock install, and a config authored before
        // this REQ existed. Both must read as "on".
        for (label, document) in [
            ("empty document", ""),
            ("an unrelated table", "[privacy]\nredact = true\n"),
        ] {
            let cfg = Config::load(document).expect("must load");
            assert!(
                cfg.context.repo_file,
                "{label}: a config that never named [context] opted out"
            );
            assert!(cfg.context.is_unset(), "{label}");
        }
        assert!(
            Config::default().context.repo_file,
            "the in-memory default opted out"
        );

        // The table named, the key omitted: still on, and indistinguishable
        // from the absence. This is the leg the field-level default breaks.
        let named = Config::load("[context]\n").expect("must load");
        assert!(
            named.context.repo_file,
            "naming [context] without writing `repo_file` turned the notes off"
        );
        assert_eq!(named.context, ContextConfig::default());
        assert_eq!(
            named.context,
            Config::load("[context]\nrepo_file = true\n")
                .expect("must load")
                .context,
            "writing the default explicitly is not a third state"
        );

        // Reading the opt-out rather than defaulting it.
        let off = Config::load("[context]\nrepo_file = false\n").expect("must load");
        assert!(!off.context.repo_file);
        assert!(
            !off.context.is_unset(),
            "an opted-out table is not the shipped default"
        );
        off.validate()
            .expect("the table is structural only — nothing to refuse");

        // "States its posture": whenever the table is emitted at all,
        // `repo_file` is in it, so a reader of the file never has to infer the
        // switch from an absence — which here would read as the wrong answer,
        // the default being on.
        let written = off.to_toml().expect("serialize");
        assert!(written.contains("[context]"), "toml: {written}");
        assert!(written.contains("repo_file = false"), "toml: {written}");
        assert_eq!(
            Config::from_toml(&written).expect("deserialize").context,
            off.context,
            "the table must round-trip; toml was:\n{written}"
        );

        // And BR-2's other half: a config that never named the table does not
        // grow it on a write, exactly as `[privacy]` does not.
        let untouched = Config::load("[privacy]\nredact = true\n").expect("must load");
        let written = untouched.to_toml().expect("serialize");
        assert!(
            !written.contains("[context]"),
            "an unset table was written into the user's config: {written}"
        );
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
                    allow_cleartext: false,
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
            allow_cleartext: false,
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

    // ---- REQ-563: the `[web]` ceiling (BR-3, BR-8, BR-11, AC-7) ------------

    /// A fully-configured `[web]` table at the top tier — the fixture for the
    /// round-trip test, which needs every key present at once.
    const WEB_SEARCH_TOML: &str = r#"
[web]
tier = "search"
search_endpoint = "https://search.example/api"
search_key_ref = "keychain:teton-search"
allowed_domains = ["docs.rs", "*.example.com"]
cache_ttl_secs = 300
"#;

    /// A config whose only setting is the `[web]` table. The validation tests
    /// have nothing to say about providers or routing, and struct-update syntax
    /// keeps them honest about which field they are actually varying.
    fn web_config(web: WebConfig) -> Config {
        Config {
            web,
            ..Config::default()
        }
    }

    #[test]
    fn web_tiers_are_ordered_and_each_tier_includes_the_ones_below() {
        // BR-3 grades the capability. The order is asserted directly because
        // `Ord` here is derived from *declaration* order: a variant moved or
        // inserted mid-list changes what an existing grant permits, silently.
        assert!(WebTier::Off < WebTier::FetchUserUrl);
        assert!(WebTier::FetchUserUrl < WebTier::FetchAnyUrl);
        assert!(WebTier::FetchAnyUrl < WebTier::Search);

        // Inclusion: a ceiling allows its own tier and every tier below it.
        assert!(WebTier::Search.allows(WebTier::Search));
        assert!(WebTier::Search.allows(WebTier::FetchAnyUrl));
        assert!(WebTier::Search.allows(WebTier::FetchUserUrl));
        assert!(WebTier::FetchAnyUrl.allows(WebTier::FetchAnyUrl));
        assert!(WebTier::FetchAnyUrl.allows(WebTier::FetchUserUrl));
        assert!(WebTier::FetchUserUrl.allows(WebTier::FetchUserUrl));

        // And nothing above it — the half BR-3 actually legislates: "a grant at
        // a lower tier never implies a higher tier".
        assert!(!WebTier::FetchUserUrl.allows(WebTier::FetchAnyUrl));
        assert!(!WebTier::FetchUserUrl.allows(WebTier::Search));
        assert!(!WebTier::FetchAnyUrl.allows(WebTier::Search));
    }

    #[test]
    fn the_off_tier_allows_nothing_including_itself() {
        for needed in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            assert!(!WebTier::Off.allows(needed), "off permitted {needed:?}");
        }

        // The deliberate case: `Off` names the absence of a capability, not one,
        // so it is never *allowed* either. A caller whose required tier came out
        // `Off` — an unset default, a mapping that fell through — must not read
        // permission out of a machine that opted into nothing.
        assert!(!WebTier::Off.allows(WebTier::Off));
        for ceiling in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            assert!(!ceiling.allows(WebTier::Off), "{ceiling:?} allowed off");
        }
    }

    #[test]
    fn a_config_with_no_web_table_leaves_web_lookup_off() {
        // BR-1's off-by-default leg, asserted on documents that predate the
        // table entirely — every config authored before this REQ is in exactly
        // that state, and must load to "off" rather than fail on a missing key.
        for (label, toml_text) in [
            ("pre-REQ-557", PRE_REQ_557_TOML),
            ("the tier table", TIER_TABLE_TOML),
            ("the empty document", ""),
        ] {
            let cfg = Config::load(toml_text).expect("must load");
            assert_eq!(cfg.web, WebConfig::default(), "{label}");
            assert_eq!(cfg.web.tier, WebTier::Off, "{label}: web lookup was on");
            assert!(
                !cfg.web.tier.allows(WebTier::FetchUserUrl),
                "{label}: a config that never named [web] permitted a lookup"
            );
        }
        // The struct default must agree with the parsed default.
        assert_eq!(Config::default().web.tier, WebTier::Off);
        assert!(Config::default().web.is_unset());
    }

    #[test]
    fn the_web_table_reads_its_values_rather_than_defaulting_them() {
        // LESSON-485: a fixture that cannot discriminate is not a test. "It
        // loads" would pass against fields wired to constants, so read a
        // document that sets every key to a non-default and assert each value
        // survived the load.
        let configured = Config::load(
            r#"
[web]
tier = "fetch_any_url"
search_endpoint = "https://search.example/api"
search_key_ref = "keychain:teton-search"
allowed_domains = ["docs.rs", "*.example.com"]
cache_ttl_secs = 60
"#,
        )
        .expect("must load");
        assert_eq!(configured.web.tier, WebTier::FetchAnyUrl);
        assert_eq!(
            configured.web.search_endpoint.as_deref(),
            Some("https://search.example/api")
        );
        assert_eq!(
            configured.web.search_key_ref.as_deref(),
            Some("keychain:teton-search")
        );
        assert_eq!(
            configured.web.allowed_domains,
            Some(vec!["docs.rs".to_owned(), "*.example.com".to_owned()])
        );
        assert_eq!(configured.web.cache_ttl_secs, 60);
        assert_ne!(configured.web, WebConfig::default());

        // Every tier spelling parses to its variant, so the ceiling a user
        // writes is the ceiling the system holds.
        for (spelling, tier) in [
            ("off", WebTier::Off),
            ("fetch_user_url", WebTier::FetchUserUrl),
            ("fetch_any_url", WebTier::FetchAnyUrl),
            ("search", WebTier::Search),
        ] {
            let cfg = Config::load(&format!(
                "[web]\ntier = \"{spelling}\"\nsearch_endpoint = \"https://search.example/api\"\n"
            ))
            .unwrap_or_else(|e| panic!("{spelling} must load: {e}"));
            assert_eq!(cfg.web.tier, tier, "{spelling} parsed to the wrong tier");
        }

        // A present-but-empty table is the off state: every key carries a serde
        // default, so an author who writes the header and nothing else has
        // configured nothing.
        assert_eq!(
            Config::load("[web]\n").expect("must load").web,
            WebConfig::default()
        );
    }

    /// **`[web] permission_allow` defaults to empty and holds tiers, one each.**
    ///
    /// BR-4 asks per lookup, so "ask about everything" is the requirement rather
    /// than a taste, and an empty list is what says it. A member exists only
    /// because the consent prompt offers "enable permanently" and that answer
    /// needs something durable to become — and it is a *list of tiers* rather
    /// than a two-valued switch because BR-3 grades the capability into three
    /// separately-consented tiers. It is orthogonal to the ceiling: a tier listed
    /// here with `tier = "off"` is not a contradiction, it is a consent posture
    /// for a capability nobody enabled.
    #[test]
    fn the_web_permission_allow_list_defaults_to_empty_and_holds_one_tier_per_answer() {
        assert!(WebConfig::default().permission_allow.is_empty());
        assert!(
            Config::load("[web]\n")
                .expect("must load")
                .web
                .permission_allow
                .is_empty(),
            "a table that names no consent list asks about everything (BR-4)"
        );

        let cfg = Config::load(
            "[web]\ntier = \"fetch_any_url\"\npermission_allow = [\"fetch_user_url\"]\n",
        )
        .expect("must load");
        assert_eq!(cfg.web.permission_allow, vec![WebTier::FetchUserUrl]);
        // It never widens the ceiling: that is a separate key and stays put.
        assert_eq!(cfg.web.tier, WebTier::FetchAnyUrl);

        // Every tier above `off` is a legal member, and members do not imply
        // each other — the list holds exactly what was answered for.
        let all = Config::load(
            "[web]\npermission_allow = [\"fetch_user_url\", \"fetch_any_url\", \"search\"]\n",
        )
        .expect("must load");
        assert_eq!(all.web.tier, WebTier::Off);
        assert_eq!(
            all.web.permission_allow,
            vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search]
        );

        // And it survives a round-trip, written unconditionally like `tier`.
        let toml_text = all.to_toml().expect("serialize");
        assert!(
            toml_text.contains("permission_allow = [") && toml_text.contains("\"search\","),
            "toml: {toml_text}"
        );
        assert_eq!(
            Config::from_toml(&toml_text).expect("deserialize").web,
            all.web
        );
    }

    /// **A member that names no tier is refused at load, not silently dropped.**
    ///
    /// Two spellings reach this. An unknown one (`"fetch"`, a typo) is refused by
    /// `serde` because the field is typed as [`WebTier`] rather than as a string
    /// — which is the reason it is typed that way. `"off"` parses and then names
    /// the *absence* of a tier: there is no consent key for it and no prompt that
    /// could produce it, so an entry would be a setting that does nothing, which
    /// is the defect REQ-558 spent an ADR removing.
    #[test]
    fn a_permission_allow_member_that_names_no_tier_is_refused_at_load() {
        for spelling in ["fetch", "FETCH_USER_URL", "web_search", "\"\""] {
            let err = Config::load(&format!("[web]\npermission_allow = [{spelling:?}]\n"));
            assert!(
                err.is_err(),
                "{spelling:?} loaded as a consent tier: {err:?}"
            );
        }

        let err = Config::load("[web]\ntier = \"fetch_any_url\"\npermission_allow = [\"off\"]\n")
            .expect_err("\"off\" is not a tier a lookup can be consented at");
        assert!(
            format!("{err}").contains("permission_allow"),
            "the message must name the key the user has to edit: {err}"
        );

        // And the same list without `off` is fine — the rule is about the
        // member, not about the key being present.
        assert!(Config::load(
            "[web]\ntier = \"fetch_any_url\"\npermission_allow = [\"fetch_user_url\"]\n"
        )
        .is_ok());
    }

    #[test]
    fn the_web_table_round_trips_and_stays_out_of_a_config_that_never_configured_it() {
        // The REQ-557 round-trip rule: what the daemon writes back must be what
        // a user could have authored.
        let cfg = Config::load(WEB_SEARCH_TOML).expect("must load");
        let toml_text = cfg.to_toml().expect("serialize");
        assert!(toml_text.contains("[web]"), "toml: {toml_text}");
        // The two unconditionally-serialized keys: a written-out `[web]` states
        // its posture rather than leaving a reader to infer it from an absence.
        assert!(toml_text.contains("tier = \"search\""), "toml: {toml_text}");
        assert!(toml_text.contains("cache_ttl_secs"), "toml: {toml_text}");
        let back = Config::from_toml(&toml_text).expect("deserialize");
        assert_eq!(cfg, back, "round-trip mismatch; toml was:\n{toml_text}");
        Config::load(&toml_text).expect("a re-serialized config must still validate");

        // And a config that never configured web lookup does not grow the
        // table, exactly as `[privacy]` stays out of one that never opted in.
        let untouched = Config::load(TIER_TABLE_TOML).expect("must load");
        let written = untouched.to_toml().expect("serialize");
        assert!(
            !written.contains("[web]"),
            "an unset ceiling was written into the user's config: {written}"
        );
        assert_eq!(
            Config::from_toml(&written).expect("deserialize").web,
            WebConfig::default()
        );
    }

    /// REQ-607 BR-8 — the default is `false`, and the default is the security
    /// posture. `SSH_AUTH_SOCK` is a handle to an agent that lends credentials,
    /// so a machine that has not opted in must not hand it to a model-issued
    /// command. REQ-596 decided that; this asserts REQ-607 did not quietly
    /// undo it while adding the escape hatch.
    #[test]
    fn shell_config_defaults_to_withholding_the_agent() {
        assert!(
            !Config::default().shell.allow_ssh_agent,
            "the shipped default admitted the ssh agent"
        );
        assert!(ShellConfig::default().is_unset());
        // A config that says nothing about `[shell]` gets the same answer as
        // one that does not exist — the absence is not a third state.
        let quiet = Config::from_toml("").expect("an empty config parses");
        assert!(!quiet.shell.allow_ssh_agent);
    }

    /// REQ-607 BR-5 / BR-6 — the opt-in is **one boolean key**, not a list.
    ///
    /// This is the shape assertion, and it is the one that keeps
    /// `allow_ssh_agent` from becoming the general `[shell] extra_env` REQ-596's
    /// OQ-2 left open and this REQ's Out of Scope refuses. A list lets a user
    /// admit a name holding a bare-token secret the daemon was never told
    /// about — the class REQ-596 closed — and a `bool` cannot express it.
    ///
    /// Asserted through TOML rather than on the Rust type, because the wire
    /// form is what a user actually writes: a field widened to `Vec<String>`
    /// would still compile and still be one field, but this test's
    /// `allow_ssh_agent = true` would stop parsing.
    #[test]
    fn shell_config_carries_one_boolean_key() {
        let cfg = Config::from_toml("[shell]\nallow_ssh_agent = true\n")
            .expect("the boolean spelling is the one the key accepts");
        assert!(cfg.shell.allow_ssh_agent);

        // A list is not a spelling of this key. If this ever starts parsing,
        // the escape hatch has been widened and BR-5's "and nothing else" is
        // no longer true.
        assert!(
            Config::from_toml("[shell]\nallow_ssh_agent = [\"SSH_AUTH_SOCK\"]\n").is_err(),
            "a list parsed as the opt-in — the key has become an extra_env"
        );

        // Round-trip: a written-out `[shell]` states its posture, and a config
        // that never opted in does not grow the table.
        let toml_text = cfg.to_toml().expect("serialize");
        assert!(toml_text.contains("[shell]"), "toml: {toml_text}");
        assert!(
            toml_text.contains("allow_ssh_agent = true"),
            "toml: {toml_text}"
        );
        assert_eq!(
            Config::from_toml(&toml_text).expect("deserialize"),
            cfg,
            "round-trip mismatch; toml was:\n{toml_text}"
        );
        let written = Config::default().to_toml().expect("serialize");
        assert!(
            !written.contains("[shell]"),
            "an unset opt-in was written into the user's config: {written}"
        );
    }

    #[test]
    fn the_search_tier_without_an_endpoint_is_rejected_naming_the_missing_field() {
        // AC-7 / BR-8: a config that asks for search while naming no backend is
        // a contradiction, caught at load rather than surfacing later as a tier
        // that mysteriously never appears in the consent prompt.
        let err = Config::load("[web]\ntier = \"search\"\n")
            .expect_err("the search tier with no endpoint must not validate");
        assert!(
            matches!(
                err,
                LoadError::Validate(ConfigError::WebSearchTierWithoutEndpoint)
            ),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("search_endpoint"),
            "the error must name the missing field: {msg}"
        );

        // A blank endpoint is as unset as an absent one — it cannot satisfy the
        // tier by being technically present.
        for blank in ["\"\"", "\"   \""] {
            let err = Config::load(&format!(
                "[web]\ntier = \"search\"\nsearch_endpoint = {blank}\n"
            ))
            .expect_err("a blank endpoint is not an endpoint");
            assert!(
                matches!(
                    err,
                    LoadError::Validate(ConfigError::WebSearchTierWithoutEndpoint)
                ),
                "{blank}: {err:?}"
            );
        }

        // With an endpoint, the same tier is a valid configuration.
        Config::load(
            "[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n",
        )
        .expect("search with an endpoint must validate");
    }

    #[test]
    fn a_missing_search_endpoint_below_the_search_tier_is_not_an_error() {
        // BR-8's other half, and the easy one to over-implement into a warning:
        // with no endpoint the search tier is simply not offered, and every
        // lower tier is a perfectly valid configuration without one.
        for tier in ["off", "fetch_user_url", "fetch_any_url"] {
            let cfg = Config::load(&format!("[web]\ntier = \"{tier}\"\n"))
                .unwrap_or_else(|e| panic!("{tier} must load without a search endpoint: {e}"));
            assert!(cfg.web.search_endpoint.is_none());
            assert!(
                !cfg.web.tier.allows(WebTier::Search),
                "{tier} reached the search tier"
            );
        }
    }

    #[test]
    fn a_credential_shaped_search_key_ref_is_rejected_without_echoing_it() {
        // BR-7 reaching the second credential-bearing field. Same posture as
        // `rejection_message_points_at_keychain_and_never_echoes_the_secret`,
        // because it is the same rule.
        let secret = "sk-ant-api03-TOPSECRETshouldNeverLeak0000";
        let err = Config::load(&format!(
            "[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n\
             search_key_ref = \"{secret}\"\n"
        ))
        .expect_err("a raw key in search_key_ref must be refused");
        assert!(
            matches!(
                err,
                LoadError::Validate(ConfigError::UnrecognizedWebSearchKeyRef)
            ),
            "{err:?}"
        );

        let msg = err.to_string();
        assert!(
            !msg.contains(secret),
            "the error echoed the raw credential: {msg}"
        );
        assert!(
            !msg.contains("TOPSECRET"),
            "the error echoed part of the credential: {msg}"
        );
        assert!(
            msg.contains("keychain"),
            "the fix must be readable from the message: {msg}"
        );
        assert!(msg.contains("BR-7"), "message should cite BR-7: {msg}");

        // The same predicate as a provider's `auth_ref`, so the shapes REQ-544
        // MED-3 named are refused here too rather than only there. Checked at
        // `tier = "off"`: a secret written into a plaintext config is a leak
        // whether or not the capability it belongs to is enabled.
        for raw in [
            "sk-1234567890abcdefghijklmnop",
            "AKIAIOSFODNN7EXAMPLE",
            "foo:AKIAIOSFODNN7EXAMPLE",
            "a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6q7r8s9t0",
            "keychain:",
            "env:",
        ] {
            let cfg = web_config(WebConfig {
                search_key_ref: Some(raw.to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::UnrecognizedWebSearchKeyRef,
                "accepted as a reference: {raw}"
            );
        }
    }

    #[test]
    fn a_reference_shaped_search_key_ref_is_accepted() {
        for good in [
            "keychain://teton/search", // the shape the CLI emits
            "keychain:teton-search",   // shorthand
            "env:TETON_SEARCH_KEY",
            "op://vault/search-key", // 1Password
        ] {
            let cfg = web_config(WebConfig {
                search_key_ref: Some(good.to_owned()),
                ..WebConfig::default()
            });
            cfg.validate()
                .unwrap_or_else(|e| panic!("{good} is a recognized reference: {e}"));
        }

        // And a key reference is never *required*: an unauthenticated backend
        // (a self-hosted one) is a legitimate configuration, so demanding a key
        // at the search tier would make it unconfigurable.
        Config::load("[web]\ntier = \"search\"\nsearch_endpoint = \"https://searx.internal\"\n")
            .expect("an unauthenticated search backend must be configurable");
    }

    /// BUG-165 — the `search_auth` template parses to the shape that rides,
    /// through the one function validation and the daemon share.
    #[test]
    fn a_search_auth_template_parses_to_its_header_and_scheme() {
        // The REQ's own example backends' spellings, which are the reason the
        // key exists: neither is Bearer.
        let brave =
            parse_search_auth("X-Subscription-Token: {key}").expect("Brave's shape must parse");
        assert_eq!(brave.header, "x-subscription-token", "names are lowercased");
        assert_eq!(brave.scheme, None);
        assert_eq!(brave.header_value("s3cret"), "s3cret");

        let kagi = parse_search_auth("Authorization: Bot {key}").expect("Kagi's shape must parse");
        assert_eq!(kagi.header, "authorization");
        assert_eq!(kagi.scheme.as_deref(), Some("Bot"));
        assert_eq!(kagi.header_value("s3cret"), "Bot s3cret");

        // An absent (or blank) key means the pre-BUG-165 constant, spelled by
        // the same accessor the daemon reads.
        for unset in [None, Some(String::new()), Some("   ".to_owned())] {
            let web = WebConfig {
                search_auth: unset.clone(),
                ..WebConfig::default()
            };
            assert_eq!(
                web.search_auth_shape(),
                Some(SearchAuthShape::bearer()),
                "{unset:?} must mean the Bearer default"
            );
        }
        assert_eq!(
            SearchAuthShape::bearer().header_value("s3cret"),
            "Bearer s3cret"
        );

        // A present-but-unparseable value is `None` — attach nothing — never
        // a silent fall-back to Bearer.
        let web = WebConfig {
            search_auth: Some("not a template".to_owned()),
            ..WebConfig::default()
        };
        assert_eq!(web.search_auth_shape(), None);
    }

    /// The template grammar is a *shape*, and everything that is not one of
    /// its two spellings is refused — most importantly a value where the user
    /// pasted the key itself in place of `{key}`.
    #[test]
    fn a_malformed_search_auth_is_rejected_without_echoing_it() {
        let pasted_key = "X-Subscription-Token: BSAj4f1c9TOPSECRETshouldNeverLeak";
        for (bad, why) in [
            (pasted_key, "a pasted key in place of {key}"),
            ("Authorization: Bearer", "no {key} at all"),
            ("{key}", "no header name"),
            (": {key}", "an empty header name"),
            ("X Subscription Token: {key}", "spaces in the header name"),
            ("Authorization: Two words {key}", "a multi-word scheme"),
            ("Authorization: Bot{key}", "no space before {key}"),
            ("Authorization: {key} Bot", "{key} not at the end"),
            ("Authorization: {key} {key}", "two {key} markers"),
            ("Authorization: Bot  {key}", "two spaces before {key}"),
        ] {
            assert_eq!(parse_search_auth(bad), None, "parsed anyway: {why}");
            let err = web_config(WebConfig {
                search_key_ref: Some("keychain:search".to_owned()),
                search_auth: Some(bad.to_owned()),
                ..WebConfig::default()
            })
            .validate()
            .expect_err(why);
            assert_eq!(err, ConfigError::InvalidWebSearchAuth, "{why}");
        }

        // The message teaches both accepted spellings and echoes nothing: the
        // likeliest malformed template carries the credential itself, and the
        // message is loggable (the `UnrecognizedWebSearchKeyRef` posture).
        let msg = ConfigError::InvalidWebSearchAuth.to_string();
        assert!(
            msg.contains("{key}"),
            "the placeholder must be taught: {msg}"
        );
        assert!(
            msg.contains("X-Subscription-Token: {key}") && msg.contains("Authorization: Bot {key}"),
            "both accepted spellings must be readable from the message: {msg}"
        );
        assert!(
            !msg.contains("TOPSECRET"),
            "the error must never echo the value: {msg}"
        );
    }

    /// `search_auth` beside an absent `search_key_ref` is a knob that does
    /// nothing — the daemon would build a credential-free transport and
    /// silently ignore the shape — so it is refused at load instead.
    #[test]
    fn a_search_auth_without_a_key_ref_is_rejected() {
        let err = web_config(WebConfig {
            search_auth: Some("X-Subscription-Token: {key}".to_owned()),
            ..WebConfig::default()
        })
        .validate()
        .expect_err("a shape with no credential to place must be refused");
        assert_eq!(err, ConfigError::WebSearchAuthWithoutKeyRef);

        // A blank value is as unset as an absent one, so it demands nothing.
        web_config(WebConfig {
            search_auth: Some("  ".to_owned()),
            ..WebConfig::default()
        })
        .validate()
        .expect("a blank search_auth is not configured, so it requires nothing");

        // And the pair together is the configuration this key exists for.
        Config::load(
            "[web]\ntier = \"search\"\n\
             search_endpoint = \"https://api.search.brave.com/res/v1/web/search\"\n\
             search_key_ref = \"keychain:brave-search\"\n\
             search_auth = \"X-Subscription-Token: {key}\"\n",
        )
        .expect("the spec's own example backend must be configurable");
    }

    /// A `search_endpoint` that could never be requested is a mistake at the
    /// moment it is written. Every one of these used to load cleanly and fail
    /// later — at daemon start, or at the first search, whichever came first.
    #[test]
    fn a_search_endpoint_that_is_not_an_absolute_http_url_is_rejected() {
        for bad in [
            "search.example/api",             // no scheme
            "//search.example/api",           // protocol-relative
            "ftp://search.example/api",       // wrong scheme
            "file:///etc/passwd",             // wrong scheme, and a local read
            "https://",                       // no host
            "https:///api",                   // no host, with a path
            "https://:8443/api",              // port with no host
            "javascript:alert(1)",            // not a URL at all
            "https://search example/api",     // whitespace
            "https://search.example/\u{7}pi", // control character
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*bad).to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::InvalidWebSearchEndpoint,
                "accepted as an endpoint: {bad:?}"
            );
        }

        // The rejection is checked at every tier, not only at `search`: an
        // endpoint written today is wrong today, whatever tier it is waiting for.
        for tier in [WebTier::Off, WebTier::FetchUserUrl, WebTier::FetchAnyUrl] {
            let cfg = web_config(WebConfig {
                tier,
                search_endpoint: Some("not-a-url".to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::InvalidWebSearchEndpoint,
                "{tier:?} skipped the endpoint check"
            );
        }

        // ...and the shapes that are usable endpoints.
        for good in [
            "https://search.example",
            "https://search.example/search",
            "http://127.0.0.1:8888/search",
            "https://search.example/search?format=json&safe=1",
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*good).to_owned()),
                ..WebConfig::default()
            });
            cfg.validate()
                .unwrap_or_else(|e| panic!("{good} is a usable endpoint: {e}"));
        }
    }

    /// The rejection is loggable, and a malformed endpoint is most often one
    /// with a key pasted into its query string — so it names the field and
    /// nothing else, the same trade [`ConfigError::InvalidAllowedDomain`] makes.
    #[test]
    fn the_endpoint_rejection_names_the_field_and_never_the_value() {
        let leaky = "search.example/api?api_key=sk-live-DO-NOT-LOG";
        let cfg = web_config(WebConfig {
            search_endpoint: Some(leaky.to_owned()),
            ..WebConfig::default()
        });
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(
            !msg.contains("sk-live-DO-NOT-LOG"),
            "the error echoed a credential: {msg}"
        );
        assert!(!msg.contains(leaky), "the error echoed the value: {msg}");
        assert!(
            msg.contains("search_endpoint"),
            "the error must name the field: {msg}"
        );
    }

    /// The key resolved from `search_key_ref` goes out as a bearer credential,
    /// so a cleartext endpoint puts it on the wire for every hop to read. The
    /// pair is what is refused — `http://` with no key is the user's own call.
    #[test]
    fn a_search_key_beside_a_cleartext_remote_endpoint_is_refused() {
        for remote in [
            "http://search.example/api",
            "http://192.0.2.10:8888/search",
            "http://user@search.example/api",
            "http://[2001:db8::1]/search",
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*remote).to_owned()),
                search_key_ref: Some("keychain:teton-search".to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::WebSearchKeyOverCleartextEndpoint,
                "a key was allowed to travel in the clear to {remote}"
            );
        }

        // Loopback is exempt: there is no wire, and refusing it would push a
        // self-hosted backend toward a self-signed certificate for no gain.
        for local in [
            "http://localhost:8888/search",
            "http://LOCALHOST:8888/search",
            "http://127.0.0.1:8888/search",
            "http://127.9.9.9/search",
            "http://[::1]:8888/search",
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*local).to_owned()),
                search_key_ref: Some("keychain:teton-search".to_owned()),
                ..WebConfig::default()
            });
            cfg.validate()
                .unwrap_or_else(|e| panic!("{local} is loopback and needs no TLS: {e}"));
        }

        // https is fine anywhere, and cleartext with no key is not this rule's
        // business.
        for (endpoint, key) in [
            ("https://search.example/api", Some("keychain:teton-search")),
            ("http://search.example/api", None),
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some(endpoint.to_owned()),
                search_key_ref: key.map(str::to_owned),
                ..WebConfig::default()
            });
            cfg.validate()
                .unwrap_or_else(|e| panic!("{endpoint} with key {key:?} must validate: {e}"));
        }

        let msg = ConfigError::WebSearchKeyOverCleartextEndpoint.to_string();
        assert!(msg.contains("search_key_ref"), "{msg}");
        assert!(msg.contains("search_endpoint"), "{msg}");
    }

    /// **The cleartext exemption cannot be reached by a second reading of the
    /// authority.**
    ///
    /// `http://evil.example\@127.0.0.1/x` is two URLs depending on who parses it.
    /// WHATWG treats `\` as `/` in a special scheme, so the authority ends at the
    /// backslash and the host is `evil.example` — which is what `reqwest` binds
    /// the search key to, and what the packet reaches. A splitter that only knows
    /// `/?#` reads the whole thing as an authority, takes the userinfo off at the
    /// last `@`, and concludes the host is `127.0.0.1` — loopback, therefore
    /// exempt from the cleartext rule, therefore a bearer key on the open wire to
    /// a host the validator never saw.
    ///
    /// This crate carries no URL parser and is not going to grow one for this, so
    /// the shape is refused rather than interpreted: a backslash in the authority
    /// means the two available readings disagree, and neither is this crate's to
    /// pick.
    #[test]
    fn an_endpoint_whose_authority_carries_a_backslash_is_refused_outright() {
        for split_brain in [
            "http://evil.example\\@127.0.0.1/x",
            "https://evil.example\\@localhost/search",
            "http://evil.example\\127.0.0.1/x",
            "http://\\@127.0.0.1/x",
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*split_brain).to_owned()),
                // With a key beside it, so a validator that let this through
                // would be letting the credential out too.
                search_key_ref: Some("keychain:teton-search".to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::InvalidWebSearchEndpoint,
                "two parsers were allowed to disagree about the host of {split_brain:?}"
            );
        }

        // A backslash *after* the authority is somebody's path and no business
        // of this rule — the two readings agree about the host there.
        let pathy = web_config(WebConfig {
            search_endpoint: Some("https://search.example/a\\b".to_owned()),
            ..WebConfig::default()
        });
        pathy
            .validate()
            .expect("a backslash in the path names no second host");
    }

    /// **A URL scheme is case-insensitive, and every reader here agrees about
    /// it.**
    ///
    /// The hazard is not `HTTP://` being rejected — it is one reader folding the
    /// case and another not. `is_absolute_http_url` accepting `HTTP://evil.example`
    /// while `is_cleartext_to_a_remote_host` tested `starts_with("http://")` would
    /// have validated a cleartext endpoint as if it were TLS and sent the search
    /// key out in the clear. The three readers share one scheme split, and this
    /// pins the shared answer at both ends.
    #[test]
    fn the_scheme_check_folds_case_for_every_reader() {
        // Accepted as a URL...
        let upper = web_config(WebConfig {
            search_endpoint: Some("HTTPS://search.example/api".to_owned()),
            search_key_ref: Some("keychain:teton-search".to_owned()),
            ..WebConfig::default()
        });
        upper
            .validate()
            .expect("HTTPS:// is https:// — the scheme is case-insensitive");

        // ...and the cleartext rule reads the same fold, so an upper-case
        // `http://` is still cleartext.
        for shouty in ["HTTP://search.example/api", "Http://search.example/api"] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*shouty).to_owned()),
                search_key_ref: Some("keychain:teton-search".to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::WebSearchKeyOverCleartextEndpoint,
                "{shouty} was read as a URL by one check and not by the other"
            );
        }

        // The loopback exemption folds the same way.
        let local = web_config(WebConfig {
            search_endpoint: Some("HTTP://127.0.0.1:8888/search".to_owned()),
            search_key_ref: Some("keychain:teton-search".to_owned()),
            ..WebConfig::default()
        });
        local
            .validate()
            .expect("loopback is loopback whatever case the scheme is written in");
    }

    /// The seam appends the query as `q`. An endpoint that already carries one
    /// would produce two, and which the backend honours is its business — so the
    /// string this daemon scanned would not be the string that decided the
    /// request. The name the seam owns is not one a config may also set.
    #[test]
    fn an_endpoint_that_already_carries_a_q_parameter_is_rejected() {
        for bad in [
            "https://search.example/api?q=",
            "https://search.example/api?q=preset",
            "https://search.example/api?format=json&q=preset",
            "https://search.example/api?format=json&q=preset&safe=1",
            "https://search.example/api?q", // valueless, still the name
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*bad).to_owned()),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::WebSearchEndpointCarriesQueryParam,
                "accepted an endpoint that already sets q: {bad:?}"
            );
        }

        // Other parameters are the backend's business and are kept — only the
        // one name the seam owns is refused. A `q` inside a *value*, a path, or
        // a fragment is not a parameter named `q`.
        for good in [
            "https://search.example/api?format=json&safe=1",
            "https://search.example/api?query=preset",
            "https://search.example/api?format=q",
            "https://search.example/q/api",
            "https://search.example/api#q=notsent",
        ] {
            let cfg = web_config(WebConfig {
                search_endpoint: Some((*good).to_owned()),
                ..WebConfig::default()
            });
            cfg.validate()
                .unwrap_or_else(|e| panic!("{good} sets no q parameter: {e}"));
        }

        let msg = ConfigError::WebSearchEndpointCarriesQueryParam.to_string();
        assert!(msg.contains("search_endpoint"), "{msg}");
        assert!(
            !msg.contains("preset"),
            "the message must not echo the value: {msg}"
        );
    }

    #[test]
    fn allowlist_entries_are_charset_checked() {
        // BR-11 entries are bare hosts or wildcards. The charset does most of
        // the work — a scheme brings `:`, a path brings `/`, and a mis-pasted
        // URL brings both plus its query string.
        for bad in [
            "https://docs.rs",             // scheme
            "docs.rs/std",                 // path
            "docs.rs:443",                 // port
            "example..com",                // empty label
            "..",                          // relative-path fragment
            "",                            // empty
            "exa mple.com",                // whitespace
            "user@example.com",            // userinfo
            "example.com?key=sk-not-real", // query string
            "exämple.com",                 // non-ASCII (punycode is the spelling)
        ] {
            let cfg = web_config(WebConfig {
                allowed_domains: Some(vec![bad.to_owned()]),
                ..WebConfig::default()
            });
            assert_eq!(
                cfg.validate().unwrap_err(),
                ConfigError::InvalidAllowedDomain { position: 1 },
                "accepted as a domain pattern: {bad:?}"
            );
        }

        // ...and the shapes that are patterns.
        let cfg = web_config(WebConfig {
            allowed_domains: Some(
                [
                    "docs.rs",
                    "example.com",
                    "*.example.com",
                    "sub.domain.example-host.io",
                    "*",
                ]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ),
            ..WebConfig::default()
        });
        cfg.validate()
            .expect("bare hosts and wildcards must be accepted");
    }

    #[test]
    fn the_allowlist_rejection_locates_the_entry_without_echoing_it() {
        // The one place this enum trades the offending value for a position: a
        // *rejected* allowlist entry is by definition not a domain, and the
        // likeliest thing it is instead is a pasted URL — which can carry a
        // credential in its query string, into a message that gets logged.
        let leaky = "https://search.example/api?key=sk-live-DO-NOT-LOG";
        let cfg = web_config(WebConfig {
            allowed_domains: Some(vec![
                "docs.rs".to_owned(),
                "*.example.com".to_owned(),
                leaky.to_owned(),
            ]),
            ..WebConfig::default()
        });
        let err = cfg.validate().unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidAllowedDomain { position: 3 },
            "the position must locate the third entry"
        );

        let msg = err.to_string();
        assert!(
            !msg.contains("sk-live-DO-NOT-LOG"),
            "the error echoed a credential: {msg}"
        );
        assert!(!msg.contains(leaky), "the error echoed the entry: {msg}");
        // The position is what replaces the value, so it has to be *in* the
        // message — an unlocatable rejection in a twenty-entry list is worse
        // than useless.
        assert!(
            msg.contains('3'),
            "the message must locate the entry: {msg}"
        );
        assert!(
            msg.contains("allowed_domains"),
            "the message must name the key: {msg}"
        );
    }

    #[test]
    fn an_absent_allowlist_and_an_empty_one_are_valid_and_are_not_the_same_thing() {
        // BR-11: absent means unrestricted, and is explicitly a valid
        // configuration rather than a warning state.
        let unrestricted = Config::load("[web]\ntier = \"fetch_any_url\"\n").expect("must load");
        assert!(unrestricted.web.allowed_domains.is_none());

        // An empty list is the opposite posture — an allowlist that lists
        // nothing allows nothing — and is the most restrictive model-chosen
        // setting, not a malformed one. The two must not collapse together.
        let nothing_allowed =
            Config::load("[web]\ntier = \"fetch_any_url\"\nallowed_domains = []\n")
                .expect("an empty allowlist is a setting, not an error");
        assert_eq!(nothing_allowed.web.allowed_domains, Some(Vec::new()));
        assert_ne!(
            unrestricted.web, nothing_allowed.web,
            "unrestricted and allow-nothing collapsed into one state"
        );

        // And the distinction survives a write/read cycle, which is where an
        // `Option` that is really a `Vec` usually loses it.
        let written = nothing_allowed.to_toml().expect("serialize");
        assert_eq!(
            Config::from_toml(&written)
                .expect("deserialize")
                .web
                .allowed_domains,
            Some(Vec::new()),
            "an explicit empty allowlist came back as unrestricted: {written}"
        );
    }

    #[test]
    fn the_cache_ttl_defaults_to_fifteen_minutes_and_zero_is_a_valid_setting() {
        assert_eq!(WebConfig::default().cache_ttl_secs, 900);
        assert_eq!(
            Config::load("[web]\ntier = \"fetch_user_url\"\n")
                .expect("must load")
                .web
                .cache_ttl_secs,
            900,
            "a table that omits the key must get the declared default, not zero"
        );

        // Zero is a *setting*, not an absence: it means no caching (every entry
        // is stale as written), which is why `Default` is hand-written rather
        // than derived and why the key is serialized unconditionally.
        let no_cache = Config::load("[web]\ntier = \"fetch_user_url\"\ncache_ttl_secs = 0\n")
            .expect("must load");
        assert_eq!(no_cache.web.cache_ttl_secs, 0);
        assert_ne!(no_cache.web, WebConfig::default());
        let written = no_cache.to_toml().expect("serialize");
        assert_eq!(
            Config::from_toml(&written)
                .expect("deserialize")
                .web
                .cache_ttl_secs,
            0,
            "an explicit zero came back as the default: {written}"
        );
    }

    // -----------------------------------------------------------------------
    // [web] table sectioning — REQ-572 BR-7, REQ-574 BR-3
    // -----------------------------------------------------------------------

    /// The `[web]` section of a rendered document: its header line and every
    /// line up to the next top-level table, with the blank line the serializer
    /// puts between tables dropped (it belongs to the document's layout, not to
    /// the section).
    ///
    /// A deliberately naive line reader, kept as the *independent* answer
    /// [`crate::config_doc::table_section`] is checked against: two ways of
    /// finding the same bytes, so agreement is evidence rather than a tautology.
    fn web_section_of(document: &str) -> String {
        let mut section = String::new();
        let mut inside = false;
        for line in document.lines() {
            if line.starts_with('[') {
                if inside {
                    break;
                }
                inside = line == "[web]";
            }
            if inside {
                section.push_str(line);
                section.push('\n');
            }
        }
        while section.ends_with("\n\n") {
            section.pop();
        }
        section
    }

    /// The three shapes the setup flow can write, each a document the validator
    /// accepts (a fixture the daemon would refuse proves nothing about what
    /// gets written).
    fn web_rendering_fixtures() -> [(&'static str, WebConfig); 3] {
        [
            (
                "fetch-only",
                WebConfig {
                    tier: WebTier::FetchAnyUrl,
                    permission_allow: vec![WebTier::FetchUserUrl],
                    ..WebConfig::default()
                },
            ),
            (
                "keyless search",
                WebConfig {
                    tier: WebTier::Search,
                    search_endpoint: Some(
                        "https://searx.example.com/search?format=json".to_owned(),
                    ),
                    allowed_domains: Some(vec!["docs.rs".to_owned()]),
                    cache_ttl_secs: 0,
                    ..WebConfig::default()
                },
            ),
            (
                "search with a key reference and an auth template",
                WebConfig {
                    tier: WebTier::Search,
                    search_endpoint: Some(
                        "https://api.search.brave.com/res/v1/web/search".to_owned(),
                    ),
                    search_key_ref: Some("keychain://teton/web-search".to_owned()),
                    search_auth: Some("X-Subscription-Token: {key}".to_owned()),
                    ..WebConfig::default()
                },
            ),
        ]
    }

    #[test]
    fn the_sliced_web_section_is_the_documents_own_bytes() {
        // REQ-574 BR-3: `/web setup`'s preview is the `[web]` section sliced
        // out of the document the commit writes, so "what the user confirmed is
        // what is written" holds only while the slicer really returns the
        // document's own bytes — for a table with an endpoint, a key reference,
        // an auth template, an allowlist and a permission list, not just for the
        // easy default.
        //
        // These fixtures are the schema's side of that claim: `table_section`
        // has its own unit tests over a hand-written document, and this one
        // asks the same question of every shape `WebConfig` can actually
        // serialize into.
        for (label, web) in web_rendering_fixtures() {
            let cfg = Config {
                web: web.clone(),
                ..Config::default()
            };
            cfg.validate()
                .unwrap_or_else(|e| panic!("the {label} fixture must be a loadable config: {e}"));
            let document = cfg.to_toml().expect("serialize");
            let sliced = crate::config_doc::table_section(&document, "web")
                .unwrap_or_else(|| panic!("{label}: the document names [web]:\n{document}"));

            // The strict claim: the section appears in the document verbatim,
            // not merely equivalently.
            assert!(
                document.contains(sliced.trim_end()),
                "{label}: the sliced table is not a substring of the document.\n\
                 sliced:\n{sliced}\ndocument:\n{document}"
            );
            // And the converse, read by an independent line walk, so a key the
            // document carries but the slice drops (or the reverse) cannot hide
            // behind the substring check.
            assert_eq!(
                web_section_of(&document),
                sliced,
                "{label}: the document's [web] section and the sliced table differ"
            );
        }
    }

    #[test]
    fn an_unset_web_table_is_the_one_section_the_document_omits() {
        // `Config.web` is `skip_serializing_if = "WebConfig::is_unset"`, so a
        // table holding every default is left out of the document entirely —
        // and, since REQ-574, there is then no section to slice out of it
        // either, which is what `/web setup` would have to show. Recorded as a
        // test rather than as a comment because it is the single input for
        // which the flow has nothing to preview, and its protection against
        // reaching that state is that it always writes a tier above `off`.
        let unset = WebConfig::default();
        assert!(unset.is_unset());
        let document = Config::default().to_toml().expect("serialize");
        assert!(
            !document.contains("[web]"),
            "an unset [web] table must not be written: {document}"
        );
        assert!(
            crate::config_doc::table_section(&document, "web").is_none(),
            "a document with no [web] table has no [web] section: {document}"
        );
    }

    // -----------------------------------------------------------------------
    // [permissions] — REQ-560
    // -----------------------------------------------------------------------

    #[test]
    fn an_absent_permissions_table_means_a_new_session_starts_guarded() {
        let cfg = Config::load("").expect("an empty config must load");
        assert_eq!(cfg.permissions.default_level, PermissionLevel::Guarded);
        assert!(cfg.permissions.is_unset());
    }

    #[test]
    fn every_level_is_nameable_from_config() {
        // Driven off `ALL` so a fifth level cannot ship unreachable from config
        // (REQ-560 AC-17).
        for level in PermissionLevel::ALL {
            let toml = format!("[permissions]\ndefault_level = \"{}\"\n", level.name());
            let cfg = Config::load(&toml)
                .unwrap_or_else(|err| panic!("`{}` must load: {err}", level.name()));
            assert_eq!(cfg.permissions.default_level, *level);
            cfg.validate()
                .unwrap_or_else(|err| panic!("`{}` must validate: {err}", level.name()));
        }
    }

    /// REQ-560: an unrecognised level is refused at load, not silently defaulted.
    ///
    /// The refusal is a **deserialization** failure rather than a
    /// [`Config::validate`] error, which is the same treatment every other
    /// enum-valued key gets (`[web] tier`, `[lifetime] shutdown`) — it fails one
    /// step earlier, and serde's message already enumerates the valid spellings.
    /// What matters for the requirement is that the daemon refuses to start:
    /// quietly falling back to `guarded` would leave a user who typed `full`
    /// running as something else, and quietly falling back to `full` would be
    /// worse.
    #[test]
    fn an_unrecognised_level_is_refused_rather_than_defaulted() {
        let err = Config::load("[permissions]\ndefault_level = \"unrestricted\"\n")
            .expect_err("an unknown level must not load");
        let rendered = err.to_string();
        for level in PermissionLevel::ALL {
            assert!(
                rendered.contains(level.name()),
                "the refusal should name `{}`: {rendered}",
                level.name()
            );
        }
    }

    #[test]
    fn a_permissions_table_holding_only_the_default_is_not_serialised() {
        let cfg = Config::load("[permissions]\ndefault_level = \"guarded\"\n")
            .expect("guarded must load");
        let rendered = toml::to_string(&cfg).expect("config serialises");
        assert!(
            !rendered.contains("[permissions]"),
            "an unset table should stay out of the document: {rendered}"
        );

        // A non-default level round-trips through the document.
        let cfg =
            Config::load("[permissions]\ndefault_level = \"plan\"\n").expect("plan must load");
        let rendered = toml::to_string(&cfg).expect("config serialises");
        assert!(rendered.contains("[permissions]"), "{rendered}");
        let back = Config::load(&rendered).expect("the rendered document reloads");
        assert_eq!(back.permissions.default_level, PermissionLevel::Plan);
    }

    // -----------------------------------------------------------------------
    // [lifetime] — REQ-565
    // -----------------------------------------------------------------------

    #[test]
    fn an_absent_lifetime_table_means_exit_with_the_last_client() {
        let cfg = Config::load("").expect("an empty config must load");
        assert_eq!(cfg.lifetime.shutdown, ShutdownPolicyKind::OnLastDisconnect);
        assert_eq!(cfg.lifetime.linger_seconds, None);
        assert_eq!(
            cfg.lifetime.policy(),
            crate::lifetime::ShutdownPolicy::OnLastDisconnect
        );
        assert!(cfg.lifetime.is_unset());
    }

    #[test]
    fn a_config_that_never_set_a_lifetime_does_not_grow_the_table() {
        let cfg = Config::load("").expect("an empty config must load");
        let written = toml::to_string(&cfg).expect("serialize");
        assert!(
            !written.contains("[lifetime]"),
            "an unset lifetime must not be written out: {written}"
        );
    }

    #[test]
    fn the_three_modes_round_trip_through_toml() {
        let never = Config::load("[lifetime]\nshutdown = \"never\"\n").expect("never must load");
        assert_eq!(
            never.lifetime.policy(),
            crate::lifetime::ShutdownPolicy::Never
        );

        let linger = Config::load("[lifetime]\nshutdown = \"linger\"\nlinger_seconds = 45\n")
            .expect("linger must load");
        assert_eq!(
            linger.lifetime.policy(),
            crate::lifetime::ShutdownPolicy::Linger { seconds: 45 }
        );

        let default = Config::load("[lifetime]\nshutdown = \"on-last-disconnect\"\n")
            .expect("the default must load");
        assert_eq!(
            default.lifetime.policy(),
            crate::lifetime::ShutdownPolicy::OnLastDisconnect
        );
    }

    /// A window nobody will honour is a belief about the daemon that is false;
    /// say so at load rather than ignoring the key.
    #[test]
    fn a_linger_window_under_a_non_linger_mode_is_rejected() {
        let err = Config::load("[lifetime]\nshutdown = \"never\"\nlinger_seconds = 30\n")
            .expect_err("a window under `never` must not validate");
        assert!(
            matches!(
                err,
                LoadError::Validate(ConfigError::LingerWindowWithoutLingerMode { .. })
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("linger_seconds"), "{err}");
    }

    #[test]
    fn linger_without_a_window_is_rejected_and_names_the_alternative() {
        let err = Config::load("[lifetime]\nshutdown = \"linger\"\n")
            .expect_err("linger with no window must not validate");
        assert!(
            matches!(err, LoadError::Validate(ConfigError::LingerWithoutWindow)),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("linger_seconds"), "{msg}");
        assert!(msg.contains("on-last-disconnect"), "{msg}");
    }

    #[test]
    fn an_unknown_shutdown_spelling_is_a_parse_error() {
        let err = Config::load("[lifetime]\nshutdown = \"forever\"\n")
            .expect_err("an unknown mode must not load");
        assert!(matches!(err, LoadError::Parse(_)), "{err:?}");
    }

    /// The flag/env parser and the TOML spellings must not drift apart — one
    /// accepting a mode the other rejects is how `--shutdown-policy never`
    /// silently becomes the default.
    #[test]
    fn the_flag_parser_accepts_exactly_the_toml_spellings() {
        for spelling in ShutdownPolicyKind::SPELLINGS {
            let kind = ShutdownPolicyKind::parse(spelling)
                .unwrap_or_else(|| panic!("`{spelling}` must parse as a mode"));
            let toml = format!("[lifetime]\nshutdown = \"{spelling}\"\n");
            let from_toml = Config::from_toml(&toml)
                .unwrap_or_else(|e| panic!("`{spelling}` must deserialize: {e}"))
                .lifetime
                .shutdown;
            assert_eq!(kind, from_toml, "`{spelling}` disagreed");
        }
        assert_eq!(ShutdownPolicyKind::parse("keep-alive"), None);
        assert_eq!(ShutdownPolicyKind::parse(""), None);
    }

    // ---- REQ-571 BR-10: every ConfigError `validate` can raise is asserted ----
    //
    // `Config::validate` is fail-closed and gates daemon startup, so a variant it
    // can raise with nothing asserting it is an unguarded startup gate. The four
    // tests below close the gaps that existed (AC-10); the enumeration after them
    // is what keeps the set honest for variants added later (AC-11).

    #[test]
    fn a_default_provider_naming_an_unregistered_id_is_rejected() {
        // REQ-557 BR-6: a dangling `default_provider` is a validity error, not a
        // route that fails later, further from the cause, with the wrong name
        // attached (LESSON-456).
        let mut cfg = sample_config();
        cfg.default_provider = Some("ghost".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownDefaultProvider {
                default_provider: "ghost".to_owned(),
                registered: "anthropic-prod, deepseek, local".to_owned(),
            }
        );
    }

    #[test]
    fn a_tier_fallback_naming_an_unregistered_provider_is_rejected() {
        // `the_dangling_binding_error_names_the_tier_or_category_it_came_from`
        // pins two of the table's four dangling-reference slots by variant; this
        // is a third. The four checks are copies of the same six lines, which is
        // exactly where a wrong one hides — the shared-message test above them
        // cannot tell `UnknownTierFallback` from `UnknownTierProvider`, because
        // both name the id and list the registered ones.
        let mut cfg = sample_config();
        cfg.tiers[3].fallback_id = Some("ghost".to_owned());
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownTierFallback {
                tier: Tier::Think,
                fallback_id: "ghost".to_owned(),
                registered: "anthropic-prod, deepseek, local".to_owned(),
            }
        );
    }

    #[test]
    fn a_category_override_naming_an_unregistered_provider_is_rejected() {
        // The fourth slot: the category's own provider, as opposed to its
        // fallback.
        let mut cfg = sample_config();
        cfg.categories[0].provider_id = "ghost".to_owned();
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::UnknownCategoryProvider {
                category: ConfigurableCategory::Review,
                provider_id: "ghost".to_owned(),
                registered: "anthropic-prod, deepseek, local".to_owned(),
            }
        );
    }

    #[test]
    fn a_permission_allow_list_naming_off_is_rejected_by_validate() {
        // `a_permission_allow_member_that_names_no_tier_is_refused_at_load`
        // drives the same rule through `Config::load` and asserts on the
        // message. This pins the variant, so the rule cannot come to be served
        // by some other error whose text happens to mention the key — and it
        // pins the *member* rule rather than the list: a legitimate tier sits
        // beside the rejected one here.
        let mut cfg = sample_config();
        cfg.web.permission_allow = vec![WebTier::FetchUserUrl, WebTier::Off];
        assert_eq!(
            cfg.validate().unwrap_err(),
            ConfigError::WebPermissionAllowNamesOff
        );
    }

    /// This file's own source, embedded at compile time.
    ///
    /// `include_str!` rather than a runtime read of `src/`: BUG-159 is the trap
    /// where a source-scanning test panics because something rewrote the file
    /// between the walk and the read — which is precisely what this repo's
    /// mutation-check convention (LESSON-441) does between `cargo test` runs.
    /// Embedding removes the race rather than tolerating it: there is no runtime
    /// read left to lose, and the bytes scanned are by construction the ones
    /// that produced the binary running the scan.
    const THIS_SOURCE: &str = include_str!("config.rs");

    /// This file split at the `#[cfg(test)]` boundary: `(production, tests)`.
    fn source_halves() -> (&'static str, &'static str) {
        let at = THIS_SOURCE
            .find("\n#[cfg(test)]\n")
            .expect("config.rs must still carry its `#[cfg(test)]` boundary");
        THIS_SOURCE.split_at(at)
    }

    /// Every `ConfigError` variant named in `text`, ignoring comment lines.
    ///
    /// Comments are dropped because a doc comment linking a variant is not a
    /// raise, and the test half carries two that name a variant ADR-G retired.
    fn config_error_variants(text: &str) -> BTreeSet<String> {
        const PREFIX: &str = "ConfigError::";
        let mut named = BTreeSet::new();
        for line in text
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
        {
            let mut rest = line;
            while let Some(at) = rest.find(PREFIX) {
                rest = &rest[at + PREFIX.len()..];
                let variant: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                // A bare `ConfigError::` (this function's own needle, quoted
                // above) names nothing; a variant starts with a capital.
                if variant.starts_with(|c: char| c.is_ascii_uppercase()) {
                    named.insert(variant);
                }
            }
        }
        named
    }

    /// The body of `fn <name>` in `lines`, delimited by rustfmt's indentation: a
    /// method's closing brace is the first later line that is exactly its own
    /// indent followed by `}`. `None` when there is no such function.
    fn fn_body(lines: &[&str], name: &str) -> Option<String> {
        let signature = format!("fn {name}(");
        let (start, indent) = lines.iter().enumerate().find_map(|(index, line)| {
            let trimmed = line.trim_start();
            let at = trimmed.find(&signature)?;
            // Only visibility/qualifier keywords may precede it, so a line that
            // merely mentions the call is not mistaken for its definition.
            let is_definition = trimmed[..at].split_whitespace().all(|word| {
                matches!(word, "pub" | "const" | "async" | "unsafe") || word.starts_with("pub(")
            });
            if is_definition {
                Some((index, line.len() - trimmed.len()))
            } else {
                None
            }
        })?;
        let closer = format!("{}}}", " ".repeat(indent));
        let end = lines[start..].iter().position(|line| *line == closer)? + start;
        Some(lines[start..=end].join("\n"))
    }

    /// The `self.<name>(` calls made in a function body.
    fn self_calls(body: &str) -> Vec<String> {
        const PREFIX: &str = "self.";
        let mut called = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find(PREFIX) {
            rest = &rest[at + PREFIX.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            // `self.providers.len()` names a field, not a call.
            if rest[name.len()..].starts_with('(') {
                called.push(name);
            }
        }
        called
    }

    /// Every function reachable from `Config::validate` through `self.…()`
    /// calls, as name -> body.
    ///
    /// Walked rather than listed: a validation helper added later is swept
    /// without anyone remembering to name it here. A hand-kept list of helpers
    /// would fail in the same way a hand-kept list of variants does, which is
    /// the failure AC-11 exists to remove.
    fn validate_call_tree(production: &str) -> BTreeMap<String, String> {
        let lines: Vec<&str> = production.lines().collect();
        let mut bodies: BTreeMap<String, String> = BTreeMap::new();
        let mut pending = vec!["validate".to_owned()];
        while let Some(name) = pending.pop() {
            if bodies.contains_key(&name) {
                continue;
            }
            let Some(body) = fn_body(&lines, &name) else {
                continue;
            };
            pending.extend(self_calls(&body));
            bodies.insert(name, body);
        }
        bodies
    }

    /// AC-11: the check that keeps AC-10 from being a snapshot of today's
    /// variants.
    ///
    /// It walks `Config::validate`'s call tree in this file's own source,
    /// collects every `ConfigError` variant constructed anywhere in it, and
    /// fails naming any the test half never references. A variant added to a
    /// validation helper next year is therefore covered on the day it is
    /// written, with no list to update.
    ///
    /// What it can and cannot see: a reference from the test half is the proxy
    /// for "asserted", and it cannot tell an assertion from a mention. So it is
    /// a floor rather than a proof — but the hole it closes is a variant with
    /// *no* test touching it at all, which is the one BR-10 is about.
    #[test]
    fn every_config_error_variant_validate_can_raise_is_asserted_by_a_test() {
        let (production, tests) = source_halves();
        let tree = validate_call_tree(production);
        let raised: BTreeSet<String> = tree
            .values()
            .flat_map(|body| config_error_variants(body))
            .collect();

        // Floors first, so a scan that silently sees nothing fails instead of
        // passing vacuously — BUG-159's lesson is that a source scan without a
        // floor turns into a green that means nothing.
        assert!(
            tree.contains_key("validate") && tree.len() >= 5,
            "the walk did not reach `validate` and its helpers, so the extractor \
             is broken rather than the config: {:?}",
            tree.keys().collect::<Vec<_>>()
        );
        assert!(
            raised.contains("UnknownDefaultProvider"),
            "the walk read `validate`'s own body but found nothing it raises"
        );
        assert!(
            raised.contains("WebPermissionAllowNamesOff"),
            "the walk did not follow `validate` into its helpers"
        );
        assert!(
            raised.len() >= 20,
            "only {} variants found in validate's call tree; the scan is broken",
            raised.len()
        );

        // `ConfigError` is `validate`'s error type and nothing else's, so a
        // construction in this file that the walk did not see means the walk
        // missed a function — or a raise has escaped `validate`, which needs a
        // human either way rather than silent under-coverage.
        assert_eq!(
            raised,
            config_error_variants(production),
            "a `ConfigError` is constructed outside the call tree walked from \
             `validate`: either a helper the walk missed, or a raise that has \
             escaped `validate` and needs its own coverage rule"
        );

        let asserted = config_error_variants(tests);
        let unasserted: Vec<&str> = raised
            .iter()
            .filter(|variant| !asserted.contains(*variant))
            .map(String::as_str)
            .collect();
        assert!(
            unasserted.is_empty(),
            "`Config::validate` can raise these, and no test names them: \
             {unasserted:?}. `validate` is fail-closed and gates daemon startup, so \
             each is an unguarded startup gate — give each one a test asserting \
             `cfg.validate().unwrap_err()` equals it (BR-10)."
        );
    }
    /// **REQ-588 ADR-6 / TASK-234.** `[cost]` is absent by default and a config
    /// with no such table is unchanged.
    #[test]
    fn a_config_without_a_cost_table_has_no_ceiling_and_serializes_none() {
        let cfg: Config = toml::from_str("").expect("an empty config loads");
        assert!(cfg.cost.is_unset());
        assert_eq!(cfg.cost.prompt_ceiling_usd, None);
        assert_eq!(cfg.cost.ceiling_micro_cents(), None);
        assert!(
            !toml::to_string(&cfg).unwrap().contains("[cost]"),
            "a config that never opted in must not grow the table"
        );
        cfg.validate().expect("no ceiling is not an error");
    }

    /// The dollar figure converts to **integral** micro-cents at the edge, so
    /// no float reaches the comparison that decides a refusal (ADR-3).
    #[test]
    fn the_ceiling_converts_to_integral_micro_cents() {
        let parse = |t: &str| toml::from_str::<Config>(t).expect("loads").cost;
        assert_eq!(
            parse("[cost]\nprompt_ceiling_usd = 5.0\n").ceiling_micro_cents(),
            Some(500_000)
        );
        // A figure with cents, and one with sub-cent precision that must round
        // rather than truncate toward a *smaller* ceiling than the user set.
        assert_eq!(
            parse("[cost]\nprompt_ceiling_usd = 0.01\n").ceiling_micro_cents(),
            Some(1_000)
        );
        assert_eq!(
            parse("[cost]\nprompt_ceiling_usd = 1.234567\n").ceiling_micro_cents(),
            Some(123_457)
        );
    }

    /// **Structural refusal, fatal at load.** A ceiling the daemon cannot
    /// compare is a limit the user believes they set — starting with it
    /// silently ignored is the worst of the three outcomes.
    #[test]
    fn an_unusable_ceiling_is_refused_at_load() {
        for bad in ["0.0", "-1.0", "nan", "inf"] {
            let cfg: Config = toml::from_str(&format!("[cost]\nprompt_ceiling_usd = {bad}\n"))
                .expect("it parses as TOML; the refusal is validate's");
            assert!(
                matches!(cfg.validate(), Err(ConfigError::UnusableSpendCeiling(_))),
                "`{bad}` must be refused rather than silently ignored"
            );
        }
        // Non-vacuity: a usable one passes.
        let ok: Config = toml::from_str("[cost]\nprompt_ceiling_usd = 2.50\n").unwrap();
        ok.validate().expect("a usable ceiling loads");
        assert_eq!(ok.cost.ceiling_micro_cents(), Some(250_000));
    }

    /// **REQ-591 D-5: a row that could never name a tree is refused at load,
    /// and the error names the correct form.**
    ///
    /// The failure this prevents is specific and silent. A user hand-edits
    /// `~/dev/repo` into `[skills] trusted_project_roots` — an entirely
    /// reasonable-looking thing to write, and what the *prompt* shows them —
    /// the minter produces the canonical absolute form, the two never match,
    /// and their automation keeps refusing with no indication why. The
    /// allowlist appears to contain their repository and does not. That is a
    /// silent no-op, and this converts it into a loud error at load time.
    ///
    /// **Paired on one fixture** (LESSON-520): every rejected spelling sits
    /// beside a well-formed row in the same document, so a rule that refused
    /// *everything* fails the accepting leg and a rule that refused nothing
    /// fails the rejecting ones. An unpaired rejection test proves only that
    /// some string was refused.
    ///
    /// The error's **words** are asserted, not just its variant. Naming the
    /// correct form is the whole remedy here — the row is not wrong about which
    /// repository the user meant, only about how to spell it — so a message
    /// that said "invalid" would leave them exactly as stuck as the silence
    /// did.
    #[test]
    fn a_row_that_could_never_name_a_tree_is_refused_at_load() {
        // The one that matters: what a user types by hand, and what the prompt
        // shows them.
        for bad in [
            "~/dev/repo",
            "dev/repo",
            "",
            "/dev/repo/",
            "/dev/../repo",
            "/dev/./repo",
            "/dev//repo",
            // A `%` the mint never writes: its escapes are upper-case hex pairs,
            // so a bare or lower-case one is a spelling nothing produces.
            "/dev/re%po",
            "/dev/re%ffpo",
            "/dev/repo%",
        ] {
            let document =
                format!("[skills]\ntrusted_project_roots = [\"/Users/you/dev/ok\", {bad:?}]\n");
            let err = Config::load(&document).expect_err("a malformed row must not load: {bad:?}");
            let LoadError::Validate(ConfigError::MalformedTrustedProjectRoot(named)) = err else {
                panic!("`{bad}` was refused for the wrong reason: {err:?}");
            };
            assert_eq!(
                named,
                format!("{bad:?}"),
                "the error must quote the row as written, or the user cannot \
                 find the line"
            );
            let message = ConfigError::MalformedTrustedProjectRoot(named).to_string();
            for phrase in [
                "canonical absolute path",
                "no trailing slash",
                "`~` is not expanded",
                "answer `p`",
            ] {
                assert!(
                    message.contains(phrase),
                    "the message must name the correct form and how to obtain \
                     it — missing `{phrase}`: {message}"
                );
            }
        }

        // The accepting leg, on the same shapes the minter really produces:
        // an ordinary tree, the escape for a non-UTF-8 byte, the escape for a
        // literal `%`, and the root itself.
        let ok = Config::load(
            "[skills]\ntrusted_project_roots = [\
             \"/Users/you/dev/repo\", \"/tmp/re%FFpo\", \"/tmp/100%25\", \"/\"]\n",
        )
        .expect("a well-formed list loads");
        assert_eq!(ok.skills.trusted_project_roots.len(), 4);
    }

    // -----------------------------------------------------------------------
    // REQ-597 — the builtin boundary set and its one composition site
    // -----------------------------------------------------------------------

    /// BR-1. The shipped list is exactly the thirteen globs the spec names, in
    /// the spec's order, every one `local-only` and `builtin`.
    ///
    /// **Mutation**: drop or reorder any entry of `DEFAULT_BOUNDARIES`, or give
    /// one a non-`LocalOnly` mode, and this fails.
    #[test]
    fn default_boundaries_are_the_thirteen_specified_globs() {
        assert_eq!(
            DEFAULT_BOUNDARIES,
            [
                "**/.env",
                "**/.env.*",
                "**/.ssh/**",
                "**/*.pem",
                "**/*.key",
                "**/id_rsa*",
                "**/id_ed25519*",
                "**/.aws/**",
                "**/.npmrc",
                "**/.netrc",
                "**/.git-credentials",
                "**/.docker/config.json",
                "**/.kube/config",
            ]
        );
        let cfg = Config::default();
        let effective = cfg.effective_boundaries();
        // Count first. Without this the loop below is vacuous over an empty set
        // and the test survives the very mutation it exists to catch.
        assert_eq!(
            effective.len(),
            DEFAULT_BOUNDARIES.len(),
            "a stock config carries every builtin row"
        );
        for (b, expected) in effective.iter().zip(DEFAULT_BOUNDARIES) {
            assert_eq!(
                &b.path_glob, expected,
                "composed order follows the spec's order"
            );
            assert_eq!(
                b.mode,
                BoundaryMode::LocalOnly,
                "{} is not local-only",
                b.path_glob
            );
            assert_eq!(
                b.origin,
                BoundaryOrigin::Builtin,
                "{} is not builtin",
                b.path_glob
            );
        }
    }

    /// BR-1, and the assumption the whole REQ rests on: a leading `**/` matches
    /// **zero** leading directories under `globset` with `literal_separator`,
    /// so every glob catches its target at the repo root — and none of them
    /// catches an ordinary source file.
    ///
    /// **Mutation**: strip the `**/` prefix from any glob (making it root-only)
    /// or widen `**/.env` to `**/.env*` (which would swallow `notes/.envrc`)
    /// and this fails.
    #[test]
    fn default_boundaries_match_credentials_and_spare_sources() {
        let cfg = Config::default();
        let effective = cfg.effective_boundaries();
        let matcher = BoundaryMatcher::new(&effective).expect("builtin globs compile");

        for caught in [
            ".ssh/id_rsa",
            ".ssh/keys/id_rsa",
            "vendor/fixtures/.ssh/id_rsa",
            ".env",
            ".env.local",
            "sub/.env",
            ".aws/credentials",
            ".netrc",
            ".npmrc",
            ".git-credentials",
            ".docker/config.json",
            ".kube/config",
            "certs/server.pem",
            "a/b/c.key",
            "id_rsa",
            "id_ed25519.pub",
        ] {
            assert!(
                matcher.match_path(caught).is_some(),
                "{caught} should be covered by the builtin set"
            );
        }

        for spared in [
            "src/main.rs",
            "README.md",
            "env",
            "notes/.envrc",
            "Cargo.toml",
        ] {
            assert!(
                matcher.match_path(spared).is_none(),
                "{spared} must not be caught by the builtin set"
            );
        }
    }

    /// BR-2 / BR-2.1. User rows come first and builtin rows are appended after
    /// — asserted **by index**, because the position is what makes BR-7 true
    /// and set membership would pass under either ordering.
    ///
    /// **Mutation**: prepend the builtins instead of extending, and this fails.
    #[test]
    fn user_rows_precede_appended_builtin_rows() {
        let cfg = Config {
            boundaries: vec![
                PrivacyBoundary::user("src/vendor/**", BoundaryMode::LocalOnly),
                PrivacyBoundary::user("docs/**", BoundaryMode::RedactThenRemote),
            ],
            ..Default::default()
        };

        let effective = cfg.effective_boundaries();
        assert_eq!(effective.len(), 2 + DEFAULT_BOUNDARIES.len());
        assert_eq!(effective[0].path_glob, "src/vendor/**");
        assert_eq!(effective[0].origin, BoundaryOrigin::User);
        assert_eq!(effective[1].path_glob, "docs/**");
        assert_eq!(effective[1].origin, BoundaryOrigin::User);
        assert_eq!(effective[2].path_glob, DEFAULT_BOUNDARIES[0]);
        assert_eq!(effective[2].origin, BoundaryOrigin::Builtin);
    }

    /// BR-3. The opt-out is the only route to a set without builtins, and it
    /// leaves the user's own rows untouched.
    ///
    /// **Mutation**: ignore `disable_default_boundaries` in the composer and
    /// this fails.
    #[test]
    fn the_opt_out_is_the_only_route_to_no_builtins() {
        let mut cfg = Config::default();
        assert_eq!(
            cfg.effective_boundaries().len(),
            DEFAULT_BOUNDARIES.len(),
            "a stock config is protected by the builtin set"
        );
        assert_eq!(cfg.builtin_boundary_count(), DEFAULT_BOUNDARIES.len());

        cfg.privacy.disable_default_boundaries = true;
        assert!(
            cfg.effective_boundaries().is_empty(),
            "the opt-out with no user rows is the empty set BR-5 keys on"
        );
        assert_eq!(cfg.builtin_boundary_count(), 0);

        cfg.boundaries = vec![PrivacyBoundary::user(
            "src/vendor/**",
            BoundaryMode::LocalOnly,
        )];
        let effective = cfg.effective_boundaries();
        assert_eq!(
            effective.len(),
            1,
            "the opt-out drops builtins, not user rows"
        );
        assert_eq!(effective[0].path_glob, "src/vendor/**");
    }

    /// BR-7 + BR-2.2. A user row that collides with a builtin **governs** the
    /// path, keeping its own mode and origin.
    ///
    /// The assertion is on the governing row's identity, not on whether the
    /// path matched: both `BoundaryMode` arms fail closed at egress today, so
    /// an outcome assertion cannot distinguish the two orderings (LESSON-550).
    ///
    /// **Mutation**: prepend the builtins, and the returned row becomes the
    /// builtin `local-only` one.
    #[test]
    fn a_colliding_user_row_governs_and_keeps_its_own_mode() {
        let cfg = Config {
            boundaries: vec![PrivacyBoundary::user(
                "**/.env",
                BoundaryMode::RedactThenRemote,
            )],
            ..Default::default()
        };

        let effective = cfg.effective_boundaries();
        let matcher = BoundaryMatcher::new(&effective).expect("globs compile");
        let governing = matcher.match_path(".env").expect(".env is governed");

        assert_eq!(
            governing.origin,
            BoundaryOrigin::User,
            "the user's row wins"
        );
        assert_eq!(governing.mode, BoundaryMode::RedactThenRemote);
    }

    /// BR-7's other half: the collision leaves **both** rows in the composed
    /// set. Deduping would hide the builtin from `boundary list` and destroy
    /// the evidence of which row governs.
    ///
    /// **Mutation**: dedupe by `path_glob` in the composer, and this fails.
    #[test]
    fn a_colliding_user_row_does_not_remove_the_builtin() {
        let cfg = Config {
            boundaries: vec![PrivacyBoundary::user(
                "**/.env",
                BoundaryMode::RedactThenRemote,
            )],
            ..Default::default()
        };

        let effective = cfg.effective_boundaries();
        assert_eq!(effective.len(), 1 + DEFAULT_BOUNDARIES.len());
        let env_rows: Vec<_> = effective
            .iter()
            .filter(|b| b.path_glob == "**/.env")
            .collect();
        assert_eq!(
            env_rows.len(),
            2,
            "the user's row and the builtin both survive"
        );
        assert_eq!(env_rows[0].origin, BoundaryOrigin::User);
        assert_eq!(env_rows[1].origin, BoundaryOrigin::Builtin);
    }

    /// AC-10's protection, asserted at the type rather than only end to end: a
    /// user row must serialize with **no** `origin` key, or every config that
    /// takes an unrelated `config/set` grows `origin = "user"` lines.
    ///
    /// **Mutation**: remove `skip_serializing_if` from `PrivacyBoundary::origin`
    /// and this fails.
    #[test]
    fn a_user_row_serializes_without_an_origin_key() {
        let cfg = Config {
            boundaries: vec![PrivacyBoundary::user("secrets/**", BoundaryMode::LocalOnly)],
            ..Default::default()
        };

        let toml = cfg.to_toml().expect("a well-formed config serializes");
        assert!(toml.contains("secrets/**"), "the user's row is written");
        assert!(
            !toml.contains("origin"),
            "a user row must not grow an `origin` key on disk:\n{toml}"
        );

        // And the builtin set never reaches the writer at all (ADR-1).
        for glob in DEFAULT_BOUNDARIES {
            assert!(
                !toml.contains(glob),
                "builtin {glob} must never be serialized into a user's config"
            );
        }
    }

    /// The additive-field contract on disk: a config written before REQ-597 has
    /// no `origin` key, and must load as a user row.
    #[test]
    fn a_pre_req_config_loads_its_boundaries_as_user_rows() {
        let cfg = Config::from_toml(
            "[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n",
        )
        .expect("a pre-REQ config still loads");
        assert_eq!(cfg.boundaries.len(), 1);
        assert_eq!(cfg.boundaries[0].origin, BoundaryOrigin::User);
    }
}
