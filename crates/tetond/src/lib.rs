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
//! - [`auth`] — socket permissions and the peer-credential uid/pid check.
//! - [`peer`] — process ancestry (REQ-569 ADR-A/ADR-B): a thin per-platform
//!   `pid -> ppid` seam under a pure, table-testable "is this peer a descendant
//!   of this daemon?" decision. Answers the question; gates nothing.
//! - [`grants`] — session grants (REQ-569 BR-1/BR-2, ADR-C/ADR-D): the
//!   in-memory, daemon-lifetime registry keyed by
//!   `(connection, session, scope)`, and the pure `may_attach`/`may_monitor`
//!   predicates over it. Decides who may attach; [`server`] applies it.
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
//! - [`call_sites`] — which categories the harness actually dispatches on today
//!   (REQ-558 ADR-A), and the source-scanning test that keeps the
//!   `declared, no call site yet` marker from rotting into a hand-maintained
//!   list.
//! - [`classify`] — the `route` classifier (REQ-558 BR-3): the one small local
//!   model call that assigns a *freeform* turn to one of the four judgment
//!   categories, with a bypass that issues no call at all when the local tier
//!   cannot serve (BR-5). It lives beside the router rather than inside it
//!   because no routing function may see prompt text.
//! - [`provider_recipes`] — the vendor `provider add` recipes Teton's own
//!   guidance hands a user (REQ-577 ADR-1), as one pure factory over static
//!   product data verified against the vendors' current docs. Same posture as
//!   [`web_setup_catalog`] and for the same reason: the guide, the README and
//!   the bundled docs topic are gated against this list rather than each keeping
//!   a copy that goes stale unnoticed.
//! - [`router`] — category routing (REQ-558 BR-1), BR-6 degradation, remote
//!   wiring through egress (BR-1/BR-2), and provider fallback on failure (AC-7).
//! - [`runtime`] — the assembled engine/router/egress/cost/MCP state the JSON-RPC
//!   handlers drive: `session/prompt` execution, config, and the cost query.
//! - [`structured`] — structured (ADLC) mode (D-4, BR-3): the phase state machine,
//!   artifact gates, `.teton/` artifact storage, and bundled generic templates.
//! - [`model_consent`] — the first-run consent gate (REQ-547 BR-1): probe →
//!   propose → await an answer → only then download. Gates the local *tier*,
//!   never the session (D-3).
//! - [`selection_store`] — the recorded decision as machine state (REQ-547 D-4),
//!   which is what makes "a decision is not re-litigated" a state read.
//! - [`mcp`] — user-registered MCP servers as egress-gated tool providers
//!   (ADR-003): the protocol client, the server registry, and the tool bridge.
//! - [`web`] — opt-in web lookup's local, pure support pieces (REQ-563): the
//!   on-machine document cache, the dependency-free HTML→text reducer, the
//!   per-session user-pasted-URL set, and the domain allowlist. Nothing here
//!   opens a socket and nothing here decides whether a lookup is allowed —
//!   egress belongs to [`egress`], the gates beside it.
//! - [`web_setup_catalog`] — the search backends `/web setup` suggests (REQ-573
//!   ADR-A), as one pure factory over static product data. It lives daemon-side
//!   so a backend's endpoint and header shape are written down once and clients
//!   render what they were handed, rather than each keeping a copy that drifts
//!   from the others — which is what BUG-165 was.
//! - [`session_root`] — the session-root probe (REQ-583 ADR-1): the I/O half
//!   of "what ground does this session stand on" — is a project marker present,
//!   what does `.git/HEAD` say — over the pure classification in
//!   `teton_core::session_root`. Called per use, never cached.
//! - [`fs_util`] — the one bounded file reader (REQ-583 verify): type and size
//!   checked off `metadata` before the open, the read `take`n — shared by the
//!   probe's `.git`/`HEAD` reads and `grep`'s per-file reads, so a FIFO or a
//!   giant file holds neither hostage and the two cannot drift in order.
//! - [`skills`] — user-defined slash commands read from `SKILL.md` (REQ-585
//!   BR-1): four fixed globs one level deep behind a listing seam, a total
//!   frontmatter parser, and the name contest that decides which file a
//!   spelling reaches. Pure over the seam; the turn that runs one is
//!   [`runtime`]'s.
//! - [`single_instance`] — the `flock`-based single-instance guard.
//!
//! Socket and lock path resolution lives in the shared
//! [`teton_protocol::socket_path`] module so the daemon and every client resolve
//! the same path (REQ-544 — was a byte-identical copy in each binary).

pub mod attest;
pub mod auth;
pub mod broadcast;
pub mod call_sites;
pub mod carry;
pub mod child_env;
pub mod classify;
pub mod consent;
pub mod cost;
pub mod download;
pub mod egress;
pub mod env_path;
pub mod fs_util;
pub mod grants;
pub mod harness;
pub mod install;
pub mod keychain;
pub mod lifetime;
pub mod mcp;
pub mod model_consent;
pub mod peer;
pub mod projects;
pub mod provider_recipes;
pub mod router;
pub mod runtime;
pub mod selection_store;
pub mod server;
pub mod session_root;
pub mod sessions;
pub mod turn_context;
pub mod single_instance;
pub mod skills;
pub mod structured;
pub mod web;
pub mod web_setup_catalog;

pub use server::{bind_listener, serve, Daemon};

/// Returns the crate version (equal to the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Mint a [`ProvenanceId`](teton_core::ProvenanceId) for a **test fixture**,
/// panicking on a path that is not one (REQ-571).
///
/// Test-only and deliberately so, the same posture as
/// [`RetainedContext::from_blocks`](crate::harness::context::RetainedContext):
/// production code that reached for it does not compile, which is what keeps
/// ADR-A's "only a resolved identity enters the provenance channel" a property of
/// the shipped binary rather than a convention. A fixture naming an un-mintable
/// path is a broken fixture, so a panic is the right failure.
///
/// One helper for the whole crate rather than one per test module: several
/// modules need it, and a second copy is a second place the fixture form could
/// drift from the real one.
#[cfg(test)]
#[must_use]
pub(crate) fn fixture_id(path: &str) -> teton_core::ProvenanceId {
    teton_core::ProvenanceId::claimed(path)
        .unwrap_or_else(|e| panic!("fixture path {path:?} is not an identity: {e}"))
}

/// Create a FIFO at `path` — the test fixture for "an entry that is not a
/// regular file and whose open would block for a writer" (REQ-583 verify).
/// `mkfifo(2)` through libc, panicking on failure so a fixture that could not be
/// made is a failed test and never a vacuous one. Test-only, one helper for
/// every FIFO test in the crate (the probe's `HEAD`, `grep`'s tree, `read` and
/// `edit`'s path, the bounded reader) rather than one `unsafe` block per
/// module.
#[cfg(test)]
pub(crate) fn mkfifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `mkfifo` reads a NUL-terminated path and a mode; nothing else.
    let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(
        made,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// Run `work` on a worker thread and return its result, failing the test if it
/// has not returned within five seconds — so a test whose regression is a hang
/// (a FIFO opened for reading) fails instead of stalling the suite. `what`
/// names the call in the failure message.
#[cfg(test)]
pub(crate) fn with_deadline<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => {
            panic!("{what} must return: it was still running after 5 s (a blocking open?)")
        }
        Err(RecvTimeoutError::Disconnected) => {
            panic!("{what} panicked on its worker thread; see the panic above")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_reported() {
        assert!(!version().is_empty());
    }
}
