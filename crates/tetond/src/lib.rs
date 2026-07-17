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
//! - [`harness`] — the agentic tool-use loop (local-first: read/edit/verify).
//! - [`single_instance`] — the `flock`-based single-instance guard.
//! - [`socket_path`] — socket and lock path resolution.

pub mod auth;
pub mod broadcast;
pub mod egress;
pub mod harness;
pub mod server;
pub mod sessions;
pub mod single_instance;
pub mod socket_path;

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
