//! teton — the Teton Code CLI (stub).
//!
//! A thin client that renders daemon events and forwards input over the
//! bespoke JSON-RPC protocol (ADR-002). This stub only reports its version
//! and exits.

/// Returns `true` when the process was asked to print its version.
fn wants_version() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

fn main() {
    let version = env!("CARGO_PKG_VERSION");

    if wants_version() {
        println!("teton {version}");
        return;
    }

    // Reference the protocol crate so the client/protocol edge is real.
    eprintln!(
        "teton {version} (stub) — protocol {}",
        teton_protocol::version(),
    );
}
