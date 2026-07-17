//! tetond — the Teton Code daemon binary.
//!
//! Startup wiring only: resolve the socket/lock paths, take the single-instance
//! lock, bind the socket, and run the JSON-RPC server (see the `tetond` library
//! crate for the daemon's logic). Session state, routing, cost accounting, and
//! provider adapters build on this spine in later tasks.

use std::process::ExitCode;
use std::sync::Arc;

use tetond::single_instance::SingleInstance;
use tetond::{server, socket_path, Daemon};

/// Returns `true` when the process was asked to print its version.
fn wants_version() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

fn main() -> anyhow::Result<ExitCode> {
    let version = env!("CARGO_PKG_VERSION");

    if wants_version() {
        println!("tetond {version}");
        return Ok(ExitCode::SUCCESS);
    }

    let paths = socket_path::daemon_paths();

    // Single-instance: a second daemon exits cleanly rather than fighting over
    // the socket.
    let _instance = match SingleInstance::acquire(&paths.lock)? {
        Some(instance) => instance,
        None => {
            eprintln!(
                "tetond: already running (lock held at {})",
                paths.lock.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
    };

    // Reference every library crate so the dependency edges stay real (the
    // daemon must fail to compile if a layer is missing).
    eprintln!(
        "tetond {version} — core {}, protocol {}, providers {}, inference {}",
        teton_core::version(),
        teton_protocol::version(),
        teton_providers::version(),
        teton_inference::version(),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let listener = server::bind_listener(&paths.socket)?;
        eprintln!("tetond listening on {}", paths.socket.display());
        let daemon = Arc::new(Daemon::new());
        server::serve(listener, daemon).await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(ExitCode::SUCCESS)
}
