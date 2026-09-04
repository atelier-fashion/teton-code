//! Core domain entities from REQ-544's System Model.
//!
//! These are pure data types with serde derives; they hold no behavior that
//! performs I/O. Credential *resolution* (turning [`ModelProvider::auth_ref`]
//! into a live secret) is `tetond`'s job — this crate only ever sees the
//! reference, never the secret itself (BR-7).

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

/// The wire kind, read as the domain kind.
///
/// [`teton_protocol::ProviderKind`] is the vocabulary the socket speaks; this
/// one is what the domain rules are written against. They carry the same four
/// variants by construction — same names, same `kebab-case` rule (see above) —
/// and this is the **one** place the two meet, so the daemon's `to_core_kind`
/// and the CLI's registration flow cannot drift into two different mappings.
/// They used to each spell the match themselves; a typo in either would compose
/// an Anthropic registration with the OpenAI-compatible request path, which is a
/// wrong URL written into a user's config that no type checker can see
/// (REQ-578).
///
/// An exhaustive match rather than a `_` arm, for the same reason
/// [`crate::canonical_request_path`] is one: a fifth kind has to be *decided*
/// here rather than silently mapped onto a neighbour whose protocol it does not
/// share.
impl From<teton_protocol::ProviderKind> for ProviderKind {
    fn from(kind: teton_protocol::ProviderKind) -> Self {
        match kind {
            teton_protocol::ProviderKind::Local => ProviderKind::Local,
            teton_protocol::ProviderKind::OpenaiCompatible => ProviderKind::OpenaiCompatible,
            teton_protocol::ProviderKind::Anthropic => ProviderKind::Anthropic,
            teton_protocol::ProviderKind::Custom => ProviderKind::Custom,
        }
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

/// `skip_serializing_if` predicate for [`ProviderCapabilities::context_budget_cap`]:
/// zero is "no cap", and no cap is no line.
fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// `skip_serializing_if` predicate for [`ModelProvider::allow_cleartext`]:
/// `false` is the secure default, and a default is no line. A config that never
/// opted in keeps no `allow_cleartext` key, so the field is greppable exactly
/// where somebody deliberately turned the protection off (BUG-202).
fn is_false(v: &bool) -> bool {
    !*v
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
    ///
    /// REQ-586: the window is what a remote route's context budget derives
    /// from. `0` = unknown → the default budget applies, and the fact is
    /// stated in `/doctor` and `/provider list` rather than hidden (BR-3).
    #[serde(default)]
    pub max_context: u32,
    /// A user ceiling on the context budget, in tokens, below the window
    /// (REQ-586 BR-5). `0` means "no cap"; the effective budget derives from
    /// `min(window, cap)`, so a cap above the window is inert, not invalid
    /// (architecture ADR-7) — [`crate::config::Config::validate`] stays
    /// structural-only.
    ///
    /// Skipped from the serialized form when zero, so the canonical
    /// `[providers.capabilities]` rendering of a record without a cap does not
    /// grow a line — REQ-574's preservation witnesses list the rendered keys.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_budget_cap: u32,
    /// Which reasoning field(s) this provider's request body accepts (REQ-559
    /// BR-4). `None` means **not declared**, and
    /// [`crate::effort::default_shape_for`] supplies the per-kind default —
    /// which for every remote kind is `effort_only`, not `none` (ADR-E/OQ-2).
    ///
    /// Declared per provider and **never sniffed from a response**: a capability
    /// conclusion drawn from one HTTP status outlives the condition that
    /// produced it. The runtime degradation for a provider that refuses the
    /// field is session-scoped and deliberately leaves this field alone
    /// (`EffortOmission::RefusedThisSession`, ADR-F).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_shape: Option<crate::effort::ReasoningShape>,
    /// The canonical effort levels this provider actually accepts (REQ-559
    /// BR-5). `None` means **not declared**, and
    /// [`crate::effort::default_ladder_for`] supplies the per-kind default.
    ///
    /// A **bitset**, not a `Vec`, so this struct stays [`Copy`] — see REQ-559
    /// ADR-C. A `Vec` here would ripple across ~30
    /// `ProviderCapabilities::default()` sites in the daemon for no benefit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_ladder: Option<crate::effort::EffortLadder>,
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
    /// The exact model identifier sent on the wire (REQ-557 BR-1) — e.g.
    /// `claude-opus-5`, `deepseek-chat`. This is the **declared** routing
    /// identity; nothing derives it from the price table, the provider id, or
    /// the endpoint (REQ-557 ADR-A).
    ///
    /// `Option` is load-bearing at two layers, and neither is a style choice
    /// (REQ-557 ADR-B / ADR-E):
    ///
    /// 1. A bare `String` makes every pre-REQ config fail to **deserialize**,
    ///    and a config that cannot be opened cannot be migrated.
    /// 2. The requirement is likewise **not** enforced in
    ///    [`crate::config::Config::validate`]. `Config::load` validates
    ///    internally and the daemon turns a load error into a refusal to start,
    ///    so a validation-level rule would block a pre-REQ config from starting
    ///    long enough to migrate — and would make one unresolvable provider
    ///    prevent startup entirely. It is enforced by the non-fatal usability
    ///    pass ([`crate::config::Config::unusable_providers`]) instead.
    ///
    /// Absent for `local`, whose model is owned by the REQ-547 consent flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reference to an OS-keychain entry holding the credential. Never the raw
    /// credential itself (BR-7); config validation rejects raw-key-shaped
    /// values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
    /// Permit an `auth_ref` beside a cleartext `http://` endpoint on a
    /// non-loopback host (BUG-202).
    ///
    /// `false` — the default — means such a pair is a **validation error**:
    /// the credential resolved from `auth_ref` is sent as a request header on
    /// every turn, so a cleartext endpoint puts it on the wire for every hop
    /// between here and that host to read. `[web]` has refused the identical
    /// pair since REQ-563; this is that rule's provider half.
    ///
    /// The opt-out exists because provider topologies are broader than
    /// `[web]`'s. A self-hosted model server on a trusted LAN
    /// (`http://10.0.1.50:8000`) with a token in front of it is a legitimate
    /// setup, and `is_cleartext_to_a_remote_host` exempts only *loopback* — it
    /// cannot tell a LAN host from a public one, and no reliable rule can tell
    /// `models.corp.example.com` from `models.example.com`. So the judgment is
    /// handed to the person who knows their own network, in one explicit,
    /// auditable line, rather than guessed at by a heuristic.
    ///
    /// Setting this does not silence the registration warning: `teton provider
    /// add` still names the host the credential travels to.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_cleartext: bool,
    /// Capability profile (tool-call tier, parallel support, context window).
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
}

impl ModelProvider {
    /// The model this provider declares it calls, or `None` when it declares
    /// none — treating a blank or whitespace-only value as no declaration.
    ///
    /// **This is the one place that decides what "declared" means** (BUG-155).
    /// Before it existed, three call sites answered the question separately and
    /// two of them disagreed: `Config::unusable_providers` and
    /// `Config::migrate_models` trimmed and treated `""` / `"   "` as absent,
    /// while `build_router` matched on `Some(_)` alone. A provider with
    /// `model = " "` was therefore reported unusable at startup, named in the
    /// turn-failure message, and rendered `UNUSABLE` by `teton provider list`,
    /// while simultaneously being registered in the router and sending a blank
    /// model string to a real vendor API.
    ///
    /// That is the drift LESSON-456 is about — a state classified by one
    /// component and acted on by another, with nothing observing the
    /// disagreement — so the predicate lives on the entity and every caller
    /// reads it from here.
    #[must_use]
    pub fn declared_model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
    }

    /// Whether this provider cannot serve turns because it declares no model
    /// (REQ-557 ADR-E). Always `false` for the local kind, whose model is owned
    /// by the REQ-547 consent flow rather than by this field.
    #[must_use]
    pub fn is_unusable_for_lacking_a_model(&self) -> bool {
        self.kind.is_remote() && self.declared_model().is_none()
    }
}

// REQ-558 TASK-055: `RoutingPolicy` — the phase → provider routing table's row
// type — is gone. It was the System Model's configured routing entity; the
// configured table is now `TierBinding` + `CategoryOverride` in `category.rs`,
// beside the resolver that reads them, and a category is the dispatch key in
// both session modes (BR-1).
//
// What remains of the old table is `config::LegacyRoutingRule`: a row shape the
// migration reads once from an existing `[[routing]]` block and never writes
// back. It lives in `config.rs` rather than here on purpose — this module holds
// the entities the system runs on, and that one is a file format we are
// retiring, not an entity anything dispatches on.

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

/// Where a [`PrivacyBoundary`] came from (System Model: `PrivacyBoundary.origin`,
/// REQ-597).
///
/// Exists for **reporting** (BR-6) and for the composition order BR-2.1 names.
/// It is never read at enforcement time: [`crate::boundary::BoundaryMatcher`]
/// does not branch on it, and neither does the egress inspector. A builtin
/// boundary blocks exactly as a user boundary does (BR-4).
///
/// [`Self::User`] is the default, and that is load-bearing rather than
/// arbitrary: the only rows that can reach the config *writer* are the user's
/// own (the builtin set is composed on read and never enters
/// [`crate::Config::boundaries`] — REQ-597 ADR-1), and every config authored
/// before this REQ has no `origin` key at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoundaryOrigin {
    /// A row the user wrote in their own `[[boundaries]]` table.
    #[default]
    User,
    /// A row from the shipped default set ([`crate::config::DEFAULT_BOUNDARIES`]).
    Builtin,
}

impl BoundaryOrigin {
    /// Whether this is a user-authored row.
    ///
    /// Used as `skip_serializing_if` on [`PrivacyBoundary::origin`]. Without
    /// that skip, `config_doc::canonical_document` emits `origin = "user"` into
    /// every `[[boundaries]]` table and the next unrelated `config/set` writes
    /// those lines into the user's file — the AC-10 failure this predicate
    /// exists to prevent.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

/// A repo-relative glob marking files under a privacy rule (System Model:
/// `PrivacyBoundary`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyBoundary {
    /// Repo-relative glob (e.g. `secrets/**`).
    pub path_glob: String,
    /// The privacy mode for matching files. Defaults to `local-only`.
    #[serde(default)]
    pub mode: BoundaryMode,
    /// Whether this row is user-authored or shipped (REQ-597 BR-4/BR-6).
    ///
    /// Skipped on serialize when [`BoundaryOrigin::User`] so a user's config
    /// file never grows an `origin` key it did not write (AC-10).
    #[serde(default, skip_serializing_if = "BoundaryOrigin::is_user")]
    pub origin: BoundaryOrigin,
}

impl PrivacyBoundary {
    /// A user-authored boundary — the shape every config row and every caller
    /// outside the builtin set has.
    #[must_use]
    pub fn user(path_glob: impl Into<String>, mode: BoundaryMode) -> Self {
        Self {
            path_glob: path_glob.into(),
            mode,
            origin: BoundaryOrigin::User,
        }
    }

    /// A row from the shipped default set. Always [`BoundaryMode::LocalOnly`]
    /// (BR-1) — the builtin set names no other mode, so this constructor does
    /// not take one.
    #[must_use]
    pub fn builtin(path_glob: impl Into<String>) -> Self {
        Self {
            path_glob: path_glob.into(),
            mode: BoundaryMode::LocalOnly,
            origin: BoundaryOrigin::Builtin,
        }
    }
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
    /// A `[local_model] pinned` config key named the model the proposal offered.
    ///
    /// The pin overrides the probe's pick (REQ-544 BR-9) but does **not** decide
    /// on the user's behalf: post-C-1 (REQ-547 review) a pin proposes and the user
    /// still answers, so an accepted pin is recorded as [`Self::Probe`]. Retained
    /// for wire/state compatibility; the daemon no longer records a selection with
    /// this source.
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
    /// The KV cache element type the engine was loaded at — `f16` or `q8_0`
    /// (REQ-616 BR-10, AC-11). `None` until a load records one.
    ///
    /// Recorded here rather than re-derived on read because it is a fact about
    /// *what happened*, not a decision that can be recomputed: the probe's
    /// inputs (free RAM, the `[inference]` table) can change between a load and
    /// the next `teton model status`, and a status line that re-ran the probe
    /// would report what the daemon *would* choose rather than what it is
    /// serving.
    ///
    /// Additive and optional, so a `model-selection.toml` written by an earlier
    /// release still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_cache_type: Option<String>,
    /// The context window the engine was loaded with, in engine tokens
    /// (REQ-616 BR-10). `None` until a load records one, and — like
    /// [`Self::kv_cache_type`] — a record of what is being served rather than a
    /// derivation that could disagree with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_n_ctx: Option<u32>,
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
            kv_cache_type: None,
            served_n_ctx: None,
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
            kv_cache_type: None,
            served_n_ctx: None,
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

    /// The wire enum and the domain enum are the same four kinds, and the
    /// conversion between them is total.
    ///
    /// Not a formality: the daemon reads a registration through this mapping and
    /// the CLI composes an endpoint through it, so a pair that drifted would
    /// write the wrong vendor's request path into a user's config. Stated once
    /// here, now that both callers share the one `impl` (REQ-578).
    #[test]
    fn every_wire_provider_kind_becomes_its_own_domain_kind() {
        for (wire, domain) in [
            (teton_protocol::ProviderKind::Local, ProviderKind::Local),
            (
                teton_protocol::ProviderKind::OpenaiCompatible,
                ProviderKind::OpenaiCompatible,
            ),
            (
                teton_protocol::ProviderKind::Anthropic,
                ProviderKind::Anthropic,
            ),
            (teton_protocol::ProviderKind::Custom, ProviderKind::Custom),
        ] {
            assert_eq!(
                ProviderKind::from(wire),
                domain,
                "{wire:?} maps to the wrong domain kind"
            );
        }
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
