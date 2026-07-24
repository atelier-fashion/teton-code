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
//! - [`socket_path`] — the shared socket/lock path resolution both the daemon and
//!   every client must agree on.
//! - [`weights`] — the shared on-disk weights directory/filename convention (a
//!   path is never sent over the wire, but both sides derive the same one).

pub mod events;
pub mod handshake;
pub mod jsonrpc;
pub mod methods;
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
pub const PROTOCOL_VERSION_MIN: ProtocolVersion = ProtocolVersion(1);
/// Highest protocol version this build understands.
pub const PROTOCOL_VERSION_MAX: ProtocolVersion = ProtocolVersion(1);
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

/// Lifecycle phase that drives workflow-aware routing (spec `RoutingPolicy.phase`).
///
/// Structured mode pins a session to a phase; freeform mode leaves it `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Requirement authoring — routes to a frontier model.
    Spec,
    /// Architecture / task decomposition — routes to a frontier model.
    Architect,
    /// Implementation from task artifacts — routes to a cheap/mid model.
    Implement,
    /// Code review — routes to a frontier model.
    Review,
    /// Mechanical I/O (summaries, commit messages) — routes to the local tier.
    Io,
    /// No structured phase; heuristic routing applies.
    Freeform,
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

    #[test]
    fn protocol_version_serializes_as_bare_number() {
        let json = serde_json::to_string(&ProtocolVersion(3)).unwrap();
        assert_eq!(json, "3");
        let back: ProtocolVersion = serde_json::from_str("3").unwrap();
        assert_eq!(back, ProtocolVersion(3));
    }

    #[test]
    fn id_newtype_is_transparent_string() {
        let id = SessionId::from("sess-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"sess-1\"");
        assert_eq!(id.to_string(), "sess-1");
        let back: SessionId = serde_json::from_str("\"sess-1\"").unwrap();
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

    #[test]
    fn provider_kind_uses_kebab_case_wire_form() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenaiCompatible).unwrap(),
            "\"openai-compatible\""
        );
    }
}
