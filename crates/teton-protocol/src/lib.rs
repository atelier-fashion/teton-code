//! teton-protocol — client↔daemon protocol types.
//!
//! The bespoke JSON-RPC 2.0 vocabulary from ADR-002, shared by `tetond` and
//! every client (CLI now, VS Code extension later). The crate is deliberately
//! transport-free: it defines *shapes*, not sockets. All framing, correlation,
//! and negotiation types live here so a future ACP compatibility shim is mostly
//! a rename exercise.
//!
//! Naming borrows ACP (Agent Client Protocol) wherever the concepts overlap —
//! `sessionId`, prompt turns, permission requests, diff shapes — so that the
//! post-MVP `stdio↔socket` ACP adapter stays cheap. Each borrowed name carries
//! an `ACP:` comment at its definition site.
//!
//! Module map:
//! - [`jsonrpc`] — JSON-RPC 2.0 framing, id correlation, error codes.
//! - [`methods`] — typed client→daemon requests and their result types.
//! - [`events`] — the daemon→client event envelope and its payloads.
//! - [`handshake`] — protocol-version negotiation.
//! - [`permissions`] — the named permission levels a session can be in
//!   (REQ-560). The level's *identity* only; the policy table it expands to is
//!   daemon enforcement state and stays in `tetond`.
//! - [`socket_path`] — the shared socket/lock path resolution both the daemon and
//!   every client must agree on.
//! - [`weights`] — the shared on-disk weights directory/filename convention (a
//!   path is never sent over the wire, but both sides derive the same one).

pub mod effort;
pub mod events;
pub mod handshake;
pub mod jsonrpc;
pub mod methods;
pub mod permissions;
pub mod socket_path;
pub mod weights;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Human-readable byte size with one decimal place, in **binary** units
/// (`B`/`KiB`/`MiB`/`GiB`/`TiB`).
///
/// Disk and RAM are conventionally binary, and the daemon's fixed-unit GiB
/// sentences (the probe reason, the insufficient-disk refusal) already use that
/// convention. Sharing one scaling formatter keeps a proposal from labelling the
/// same 1024-based figure two different ways — a client that renders `4.4 GB`
/// beside a daemon reason that says `16 GiB` on the adjacent line.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Wire protocol version.
///
/// A single monotonically increasing integer; bumped on any breaking change to
/// a method or event shape. Clients advertise the `[min, max]` range they can
/// speak and the daemon selects one (see [`handshake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lowest protocol version this build understands.
///
/// Equal to [`PROTOCOL_VERSION_MAX`], and that is the whole point: this build
/// speaks exactly one version, because it has no compatibility path to any
/// other. Widening the range is a claim that the *types in this crate* can read
/// an older daemon's frames — see the note on [`PROTOCOL_VERSION_MAX`].
pub const PROTOCOL_VERSION_MIN: ProtocolVersion = ProtocolVersion(2);
/// Highest protocol version this build understands.
///
/// # Version 2 (REQ-558): the routing table changed shape
///
/// [`methods::ConfigSnapshot`] gained a required `tiers` array and re-typed its
/// `routing` array from a phase-keyed `{phase, provider_id, fallback_id}` to a
/// category-keyed [`methods::CategoryRouteView`]. Neither side can read the
/// other's `config/get` result: a v1 snapshot deserialized by this build fails
/// on `routing[0].category` (pinned by `methods::tests`).
///
/// The socket and lock filenames are stable across releases by design (ADR-007),
/// so an upgraded CLI routinely meets a daemon that has not been restarted yet.
/// With the range left at `1..=1` on both sides, that pairing negotiated
/// *successfully* and then failed at the first `config/get` with a raw serde
/// error — no sentence, no remedy. Bumping **both** ends moves the failure to
/// the handshake, where it is a typed
/// [`jsonrpc::error_code::UNSUPPORTED_PROTOCOL_VERSION`] the client can turn
/// into "restart the daemon" (see [`handshake::VersionSkew`]).
///
/// The rule this encodes: **advertise only what the types can actually read.**
/// A range wider than the shapes support is not backwards compatibility, it is
/// a promise the deserializer will break.
pub const PROTOCOL_VERSION_MAX: ProtocolVersion = ProtocolVersion(2);
/// The version this build prefers to speak (equal to [`PROTOCOL_VERSION_MAX`]).
pub const PROTOCOL_VERSION: ProtocolVersion = PROTOCOL_VERSION_MAX;

/// Defines a transparent `String` newtype used as a stable wire identifier.
///
/// Transparent serde means the wire form is just the string — the newtype only
/// buys type-safety on the Rust side and is invisible to the TypeScript mirror.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype!(
    /// Stable identifier for a session. ACP: `sessionId`.
    SessionId
);
id_newtype!(
    /// Identifier of a configured model provider (spec entity `ModelProvider.id`).
    ProviderId
);
id_newtype!(
    /// Correlates a permission request event with its later response.
    /// ACP: the `requestId` of `session/request_permission`.
    RequestId
);
id_newtype!(
    /// Identifier for one prompt turn. ACP: a prompt-turn handle.
    TurnId
);

/// Lifecycle phase of a structured (ADLC) session.
///
/// **Not a routing key** (REQ-558 BR-1/AC-9) — [`Category`] is. Phase travels on
/// the wire for cost attribution (`cost_recorded`, BR-11) and to report which
/// gate a structured session sits behind. Structured mode pins a session to a
/// phase; freeform mode leaves every `phase` field `None`, which is why there is
/// no `Freeform` variant (ADR-G).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Requirement authoring.
    Spec,
    /// Architecture / task decomposition.
    Architect,
    /// Implementation from task artifacts.
    Implement,
    /// Code review.
    Review,
    /// Mechanical I/O (summaries, commit messages).
    Io,
}

/// The routing tier a category inherits its provider binding from (REQ-558).
///
/// Mirrors `teton_core::category::Tier` variant-for-variant, so the daemon's
/// resolution and this wire form cannot drift apart — the precedent
/// [`ProviderKind`] and [`crate::events::SelectionSource`] set.
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
    /// The lowercase wire/display name — identical to the serde form.
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

/// The routing category a model call was made for — the dispatch key in both
/// session modes (REQ-558 BR-1).
///
/// Mirrors **all eleven** variants of `teton_core::category::Category`,
/// including the two `route` and `redact` that are pinned to the local tier and
/// have no configurable counterpart: this type reports what *happened*, not what
/// a config file may bind, and a pinned category still routes a call.
///
/// # The conversion is one-way on purpose
///
/// `teton_core::category::Category` deliberately carries no `FromStr` and no
/// `Deserialize` — that absence is AC-3's type-level guarantee that no
/// prompt-text path can name a harness-known category. This wire twin *is*
/// deserializable, because a client has to read the event it appears in, and
/// that stays safe only while the conversion runs core → wire and never back.
/// Adding a `teton_protocol::Category -> teton_core::Category` conversion (or a
/// `FromStr` on either side) reopens the hole the core type closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Classifies the four judgment categories; pinned local (BR-5).
    Route,
    /// Secret/PII scan before egress; pinned local (BR-4).
    Redact,
    /// Session and branch naming.
    Title,
    /// File and diff summarization into context.
    Digest,
    /// Conversation compaction.
    Compact,
    /// Ranking grep/glob hits.
    Triage,
    /// Write code from a task artifact.
    Edit,
    /// Command construction and output interpretation.
    Shell,
    /// Architecture, decomposition, spec authoring.
    Design,
    /// Root-cause on a failure.
    Debug,
    /// Adversarial critique.
    Review,
}

impl Category {
    /// The lowercase wire/display name — identical to the serde form.
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
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The nine categories a client may **bind** to a provider (REQ-558 ADR-B).
///
/// Mirrors `teton_core::category::ConfigurableCategory` variant-for-variant, and
/// mirrors its *absences* too: `route` and `redact` have no variant here either,
/// so `config/set` cannot carry a binding for them. The pin is a property of the
/// wire type rather than a check the daemon performs on arrival — the same
/// argument ADR-B makes about the config file, applied to the protocol, because
/// a check is something a fourth code path can forget (BUG-155).
///
/// [`Category`] is the *reporting* type and keeps all eleven variants: a pinned
/// category still routes a call, and an event has to be able to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigurableCategory {
    /// Session and branch naming.
    Title,
    /// File and diff summarization into context.
    Digest,
    /// Conversation compaction.
    Compact,
    /// Ranking grep/glob hits.
    Triage,
    /// Write code from a task artifact.
    Edit,
    /// Command construction and output interpretation.
    Shell,
    /// Architecture, decomposition, spec authoring.
    Design,
    /// Root-cause on a failure.
    Debug,
    /// Adversarial critique.
    Review,
}

impl ConfigurableCategory {
    /// Every bindable category, in the order `teton policy show` prints them.
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

    /// The lowercase wire/display name — identical to the [`Category`] it names.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.reported().as_str()
    }

    /// The reporting [`Category`] this binds. Total, like its core twin (ADR-B).
    #[must_use]
    pub const fn reported(self) -> Category {
        match self {
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

impl fmt::Display for ConfigurableCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that does not name a [`ConfigurableCategory`].
///
/// The two pinned categories get a variant each rather than falling into
/// `Unknown`, because AC-4's acceptance criterion *is* the message: a user who
/// types `teton policy set-category redact anthropic` must learn the category is
/// **forbidden**, not that it is misspelled.
///
/// # Why these sentences exist twice
///
/// `teton_core::category::ParseCategoryError` says the same two things, and the
/// CLI cannot read it — the CLI depends on this crate alone, deliberately (it
/// holds no routing logic and no HTTP client). Duplicating the *variants* is
/// already this crate's contract with `teton-core` ([`Category`], [`Tier`]); the
/// duplication of the *sentences* is pinned by a test in `tetond`, the one crate
/// that can see both, which fails if either side is reworded alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseCategoryError {
    /// The caller named `redact`, which has no configuration path (BR-4).
    #[error(
        "'redact' is pinned to the local tier by construction and cannot be bound to a provider: \
         it inspects content before that content is allowed to leave the machine."
    )]
    RedactIsPinned,
    /// The caller named `route`, which has no configuration path (BR-5).
    #[error(
        "'route' is pinned to the local tier by construction and cannot be bound to a provider: \
         a classifier that calls a remote model has spent what the routing decision saves, so no \
         binding for it could name a valid state."
    )]
    RouteIsPinned,
    /// The caller named something that is not a bindable category at all.
    #[error("unknown category '{name}'; expected one of: {expected}")]
    Unknown {
        /// The unrecognized name, as written.
        name: String,
        /// The accepted names, comma-separated. Never advertises a pinned one.
        expected: String,
    },
}

impl std::str::FromStr for ConfigurableCategory {
    type Err = ParseCategoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(c) = ConfigurableCategory::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
        {
            return Ok(c);
        }
        // A name that IS a category but has no bindable counterpart is *pinned*,
        // not misspelled. Derived from the reporting enum rather than listed, so
        // the two cannot disagree about which names are categories at all.
        match s {
            _ if s == Category::Redact.as_str() => Err(ParseCategoryError::RedactIsPinned),
            _ if s == Category::Route.as_str() => Err(ParseCategoryError::RouteIsPinned),
            _ => Err(ParseCategoryError::Unknown {
                name: s.to_owned(),
                expected: ConfigurableCategory::ALL
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }
}

/// Where a category's effective provider came from (REQ-558 ADR-A's table).
///
/// Mirrors `teton_core::category::BindingSource`. Carried structurally so
/// `teton policy show` reports the resolver's answer instead of re-deriving one
/// from the reason sentence (BR-6, AC-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingSource {
    /// A per-category override named this category.
    Override,
    /// The binding of the category's tier.
    TierInheritance,
    /// Neither: the category is pinned to the local tier by construction.
    PinnedLocal,
    /// Neither: nothing is bound.
    Unbound,
}

/// Where an **unbound** tier's provider came from (REQ-557 BR-4).
///
/// A tier with no `[[tiers]]` row still resolves, because the daemon fills it —
/// and `reflex` fills differently from the other three, which is the fact
/// `teton_core::category::Tier::inherits_default_provider` owns. This enum is
/// how `teton policy show` reports that fill instead of restating the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierBindingSource {
    /// A `[[tiers]]` row the user configured.
    Configured,
    /// Unbound, filled from `default_provider`.
    DefaultProvider,
    /// Unbound, filled from the local tier — every tier's last resort, and
    /// `reflex`'s only one.
    LocalTier,
    /// Unbound, with nothing to inherit.
    Unbound,
}

/// Session interaction mode (spec entity `Session.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Default experience; heuristic routing, no phase gates (BR-3).
    Freeform,
    /// Opt-in ADLC mode; phase-driven routing by policy (BR-5).
    Structured,
}

/// Provider family (spec entity `ModelProvider.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// On-device llama.cpp tier.
    Local,
    /// Any OpenAI-compatible chat/completions endpoint (DeepSeek, Kimi, Ollama…).
    OpenaiCompatible,
    /// Anthropic Messages API.
    Anthropic,
    /// Adapter-specific / bespoke integration.
    Custom,
}

/// Privacy-boundary enforcement mode (spec entity `PrivacyBoundary.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// Content never leaves the machine; only the local tier may read it (BR-1).
    LocalOnly,
    /// Content may go remote after redaction (MVP-optional; see OQ-7).
    RedactThenRemote,
}

/// Which kind of client is attached. Used by the handshake and the
/// `daemon_client_attach` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// The `teton` CLI.
    Cli,
    /// The VS Code extension (phase 2).
    Extension,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layering rule as a test rather than a habit: this crate is the shared
    /// vocabulary, so no crate above it may become a dependency of it.
    ///
    /// The manifest is where the assertion belongs, because the manifest is
    /// where the rule can actually be broken. A stray `use teton_core::…` does
    /// not compile; a dependency added "just for one type" compiles fine, and
    /// then these types stop being mirrorable in TypeScript (ADR-002) because
    /// the mirror cannot follow a link into the daemon's crates. REQ-561's
    /// `session_titled` is the occasion for writing it down: its payload is a
    /// `String` scoped by a `SessionId`, both already here, so the event costs
    /// this crate no new edge — and that is a property worth being told about if
    /// it ever stops holding.
    #[test]
    fn the_protocol_crate_depends_on_no_other_teton_crate() {
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("teton-protocol's own manifest is readable");

        let mut in_dependencies = false;
        let mut declared: Vec<String> = Vec::new();
        for raw in manifest.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_dependencies = header.contains("dependencies");
                // `[dependencies.foo]` and `[target.'cfg(unix)'.dependencies.foo]`
                // name the crate in the header instead of on a key line.
                if in_dependencies && !header.ends_with("dependencies") {
                    if let Some((_, name)) = header.rsplit_once('.') {
                        declared.push(name.to_owned());
                    }
                }
                continue;
            }
            if !in_dependencies || line.is_empty() {
                continue;
            }
            declared.push(
                line.split(['=', '.'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            );
        }

        // Non-vacuity: a scan that read nothing would pass every assertion below
        // while checking nothing at all (LESSON-485).
        assert!(
            declared.iter().any(|d| d == "serde"),
            "the scan did not find the dependencies it is meant to read: {declared:?}"
        );
        for name in &declared {
            assert!(
                !name.starts_with("teton"),
                "teton-protocol must depend on no teton crate; found '{name}' in {declared:?}"
            );
        }
    }

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }

    #[test]
    fn format_bytes_scales_in_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
    }

    /// This build advertises exactly the one version its types can read.
    ///
    /// Not a tautology: the range and the shapes are edited in different files,
    /// and the failure mode of letting them drift is silent — a successful
    /// handshake followed by a serde error mid-command. Widening this range is
    /// only honest once some shape actually has a compatibility path, so the
    /// assertion is here to be *deliberately* changed alongside one.
    #[test]
    fn this_build_advertises_only_the_version_its_types_can_read() {
        assert_eq!(
            PROTOCOL_VERSION_MIN, PROTOCOL_VERSION_MAX,
            "no protocol type in this crate deserializes an older frame, so offering a range \
             wider than one version promises compatibility the deserializer will break"
        );
        assert_eq!(PROTOCOL_VERSION, PROTOCOL_VERSION_MAX);
        assert!(
            PROTOCOL_VERSION_MIN > ProtocolVersion(1),
            "v1 is the pre-REQ-558 ConfigSnapshot shape this build cannot read"
        );
    }

    #[test]
    fn protocol_version_serializes_as_bare_number() {
        let json = serde_json::to_string(&ProtocolVersion(3)).unwrap();
        assert_eq!(json, "3");
        let back: ProtocolVersion = serde_json::from_str("3").unwrap();
        assert_eq!(back, ProtocolVersion(3));
    }

    #[test]
    fn id_newtype_is_transparent_string() {
        let id = SessionId::from("sess-wire");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"sess-wire\"");
        assert_eq!(id.to_string(), "sess-wire");
        let back: SessionId = serde_json::from_str("\"sess-wire\"").unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn phase_uses_snake_case_wire_form() {
        assert_eq!(serde_json::to_string(&Phase::Io).unwrap(), "\"io\"");
        assert_eq!(
            serde_json::to_string(&Phase::Architect).unwrap(),
            "\"architect\""
        );
    }

    /// The display name and the serde form are two renderings of one wire
    /// spelling, so they are asserted to be the same string rather than
    /// maintained in parallel. A variant whose `as_str` drifts from its serde
    /// rename fails here.
    #[test]
    fn category_and_tier_display_names_are_their_wire_form() {
        let categories = [
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
        for category in categories {
            let json = serde_json::to_string(&category).unwrap();
            assert_eq!(json, format!("\"{}\"", category.as_str()));
            assert_eq!(category.to_string(), category.as_str());
            assert_eq!(
                serde_json::from_str::<Category>(&json).unwrap(),
                category,
                "{category} must round-trip"
            );
        }
        // All eleven, including the two pinned-local ones: the event reports
        // what happened, and a pinned category still routes a call.
        assert_eq!(categories.len(), 11);

        for tier in [Tier::Reflex, Tier::Scan, Tier::Build, Tier::Think] {
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{}\"", tier.as_str()));
            assert_eq!(tier.to_string(), tier.as_str());
            assert_eq!(serde_json::from_str::<Tier>(&json).unwrap(), tier);
        }
    }

    /// ADR-B on the wire: a binding for a pinned category is unrepresentable,
    /// and the nine that *are* representable share their spelling with the
    /// reporting enum, so one name serves config, CLI, and the wire.
    #[test]
    fn a_binding_can_name_nine_categories_and_only_nine() {
        assert_eq!(ConfigurableCategory::ALL.len(), 9);
        for c in ConfigurableCategory::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            assert_eq!(
                serde_json::from_str::<ConfigurableCategory>(&json).unwrap(),
                c
            );
            assert_eq!(c.as_str(), c.reported().as_str());
            assert_eq!(c.as_str().parse::<ConfigurableCategory>().unwrap(), c);
            assert_ne!(c.reported(), Category::Redact);
            assert_ne!(c.reported(), Category::Route);
        }
        // The pins are a deserialization failure, not a check to remember.
        for pinned in ["redact", "route"] {
            assert!(
                serde_json::from_str::<ConfigurableCategory>(&format!("\"{pinned}\"")).is_err(),
                "{pinned} must not deserialize into a binding"
            );
        }
    }

    /// AC-4: a pinned name reads as *forbidden*, not misspelled — and each names
    /// its own reason, because a borrowed justification tells a user why some
    /// other category is pinned.
    #[test]
    fn parsing_a_pinned_category_names_the_pin_and_its_own_reason() {
        let redact = "redact".parse::<ConfigurableCategory>().unwrap_err();
        assert_eq!(redact, ParseCategoryError::RedactIsPinned);
        assert!(redact.to_string().contains("leave the machine"));

        let route = "route".parse::<ConfigurableCategory>().unwrap_err();
        assert_eq!(route, ParseCategoryError::RouteIsPinned);
        assert!(route.to_string().contains("classifier"));
        assert!(
            !route.to_string().contains("leave the machine"),
            "route inherited redact's explanation: {route}"
        );

        for err in [&redact, &route] {
            let text = err.to_string();
            assert!(text.contains("pinned"), "{text}");
            assert!(!matches!(err, ParseCategoryError::Unknown { .. }));
        }

        let typo = "reviw".parse::<ConfigurableCategory>().unwrap_err();
        let text = typo.to_string();
        assert!(text.contains("reviw") && text.contains("review"), "{text}");
        // The accepted list never advertises a name that cannot be accepted.
        assert!(
            !text.contains("redact") && !text.contains("route"),
            "{text}"
        );
    }

    #[test]
    fn binding_sources_use_snake_case_wire_form() {
        for (source, wire) in [
            (BindingSource::Override, "override"),
            (BindingSource::TierInheritance, "tier_inheritance"),
            (BindingSource::PinnedLocal, "pinned_local"),
            (BindingSource::Unbound, "unbound"),
        ] {
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{wire}\"")
            );
        }
        for (source, wire) in [
            (TierBindingSource::Configured, "configured"),
            (TierBindingSource::DefaultProvider, "default_provider"),
            (TierBindingSource::LocalTier, "local_tier"),
            (TierBindingSource::Unbound, "unbound"),
        ] {
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{wire}\"")
            );
        }
    }

    #[test]
    fn provider_kind_uses_kebab_case_wire_form() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenaiCompatible).unwrap(),
            "\"openai-compatible\""
        );
    }
}
