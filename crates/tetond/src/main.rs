//! tetond — the Teton Code daemon (stub).
//!
//! Full daemon (session state, routing policy, cost ledger, privacy egress,
//! provider adapters, local-model lifecycle) lands in later tasks. This stub
//! only reports its version and exits.

/// Returns `true` when the process was asked to print its version.
fn wants_version() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

fn main() {
    let version = env!("CARGO_PKG_VERSION");

    if wants_version() {
        println!("tetond {version}");
        return;
    }

    // Reference every library crate so the dependency edges are real and the
    // daemon fails to compile if a layer is missing.
    eprintln!(
        "tetond {version} (stub) — core {}, protocol {}, providers {}, inference {}",
        teton_core::version(),
        teton_protocol::version(),
        teton_providers::version(),
        teton_inference::version(),
    );
}
