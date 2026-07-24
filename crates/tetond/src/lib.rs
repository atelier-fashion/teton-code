//! tetond — the Teton Code daemon library.
//!
//! The daemon's spine (ADR-002): a bespoke JSON-RPC 2.0 server over a Unix
//! domain socket that multiple thin clients attach to and detach from, with
//! sessions that outlive any client (BR-4). This crate exposes the daemon as a
//! library so the same server logic that `main` runs can be driven directly by
//! integration tests.
//!
//! Module map:
//! - [`server`] — the UDS listener, per-client tasks, and JSON-RPC dispatch.
//! - [`auth`] — socket permissions and the peer-credential uid check.
//! - [`sessions`] — the authoritative, client-independent session registry.
//! - [`broadcast`] — the bounded, slow-client-evicting event bus.
//! - [`egress`] — the single egress choke point: privacy-boundary enforcement
//!   (BR-1), the sole HTTP client, and the cost-recording hook (BR-2).
//! - [`cost`] — the append-only cost ledger, price table, and report (BR-2): one
//!   `CostRecord` per remote call, recorded at the egress hook.
//! - [`download`] — the model-download HTTP client (REQ-547 D-2): the *second*
//!   trust context — credential-free and redirect-following, deliberately
//!   separate from [`egress`]'s credentialed, redirect-refusing client.
//! - [`install`] — the weights install pipeline (REQ-547 BR-7/BR-9): disk
//!   preflight before a byte is fetched, download to a temporary path, verify,
//!   then an atomic rename into place.
//! - [`harness`] — the agentic tool-use loop (local-first: read/edit/verify).
//! - [`router`] — phase-policy routing (BR-5), BR-6 degradation, remote wiring
//!   through egress (BR-1/BR-2), and provider fallback on failure (AC-7).
//! - [`runtime`] — the assembled engine/router/egress/cost/MCP state the JSON-RPC
//!   handlers drive: `session/prompt` execution, config, and the cost query.
//! - [`structured`] — structured (ADLC) mode (D-4, BR-3): the phase state machine,
//!   artifact gates, `.teton/` artifact storage, and bundled generic templates.
//! - [`heuristics`] — freeform-mode routing (BR-5): local for auxiliary duties,
//!   the configured default for coding turns, with a BR-8 local-tier bypass.
//! - [`model_consent`] — the first-run consent gate (REQ-547 BR-1): probe →
//!   propose → await an answer → only then download. Gates the local *tier*,
//!   never the session (D-3).
//! - [`selection_store`] — the recorded decision as machine state (REQ-547 D-4),
//!   which is what makes "a decision is not re-litigated" a state read.
//! - [`mcp`] — user-registered MCP servers as egress-gated tool providers
//!   (ADR-003): the protocol client, the server registry, and the tool bridge.
//! - [`single_instance`] — the `flock`-based single-instance guard.
//!
//! Socket and lock path resolution lives in the shared
//! [`teton_protocol::socket_path`] module so the daemon and every client resolve
//! the same path (REQ-544 — was a byte-identical copy in each binary).

pub mod auth;
pub mod broadcast;
pub mod cost;
pub mod download;
pub mod egress;
pub mod harness;
pub mod heuristics;
pub mod install;
pub mod keychain;
pub mod mcp;
pub mod model_consent;
pub mod router;
pub mod runtime;
pub mod selection_store;
pub mod server;
pub mod sessions;
pub mod single_instance;
pub mod structured;

pub use server::{bind_listener, serve, Daemon};

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
