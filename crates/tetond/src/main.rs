//! tetond — the Teton Code daemon binary.
//!
//! Startup wiring only: resolve the socket/lock paths, take the single-instance
//! lock, bind the socket, and run the JSON-RPC server (see the `tetond` library
//! crate for the daemon's logic). Session state, routing, cost accounting, and
//! provider adapters build on this spine in later tasks.

use std::process::ExitCode;
use std::sync::Arc;

use teton_protocol::socket_path;
use tetond::broadcast::EventBus;
use tetond::runtime::DaemonRuntime;
use tetond::single_instance::SingleInstance;
use tetond::{server, Daemon};

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

    let base_dir = paths
        .socket
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);

    runtime.block_on(async move {
        // Assemble the runtime (local tier, providers, cost ledger, MCP) from
        // config and the environment, sharing the event bus so cost and privacy
        // events reach attached clients.
        //
        // H-1 (E-4): this happens **before** the socket is bound, and the order
        // is load-bearing. `from_env` refuses to start on a present-but-invalid
        // config rather than falling open to an empty one — but bound first, a
        // client's `connect` succeeded into the listen backlog and then died at
        // the handshake with a bare EOF, so the diagnostic the refusal exists to
        // deliver never reached anyone. With no socket, the CLI's autostart poll
        // fails cleanly and reports the daemon's own stderr instead.
        let events = Arc::new(EventBus::new());
        let daemon_runtime = Arc::new(DaemonRuntime::from_env(&base_dir, &events)?);

        let listener = server::bind_listener(&paths.socket)?;
        eprintln!("tetond listening on {}", paths.socket.display());

        // REQ-547 BR-1/D-3: drive the first-run consent flow on its own task. It
        // may await a client's `model/confirm` indefinitely, and while it does
        // the daemon must keep serving sessions remote-only — so it is spawned
        // beside `serve`, never awaited before it.
        if daemon_runtime.first_run_consent_applies() {
            let consent_runtime = Arc::clone(&daemon_runtime);
            tokio::spawn(async move {
                consent_runtime.run_model_consent().await;
            });
        }

        let daemon = Arc::new(Daemon::with_runtime(events, daemon_runtime));
        server::serve(listener, daemon).await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(ExitCode::SUCCESS)
}
