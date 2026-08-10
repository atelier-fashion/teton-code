//! `teton-code` — the Teton Code daemon binary (crate `tetond`).
//!
//! Startup wiring only: resolve the socket/lock paths, take the single-instance
//! lock, bind the socket, and run the JSON-RPC server (see the `tetond` library
//! crate for the daemon's logic). Session state, routing, cost accounting, and
//! provider adapters build on this spine in later tasks.
//!
//! Since REQ-565 this file also owns the *end* of the daemon's life: the
//! shutdown policy is resolved here (flag > env > config > default) and the
//! ordered teardown runs here, because the ordering constraint it exists to
//! satisfy — the socket is unlinked while the single-instance lock is still
//! held — can only be expressed where that lock's guard lives.

use std::process::ExitCode;
use std::sync::Arc;

use teton_protocol::socket_path;
use tetond::broadcast::EventBus;
use tetond::lifetime::{self, LifetimeSupervisor};
use tetond::runtime::DaemonRuntime;
use tetond::single_instance::{SingleInstance, DEFAULT_ACQUIRE_WINDOW};
use tetond::{server, Daemon};

use teton_core::lifetime::{BlockingActivity, ExitReason};
use teton_protocol::events::{DaemonLifetime, DaemonLifetimeStage, Event};

/// Returns `true` when the process was asked to print its version.
fn wants_version() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
}

fn main() -> anyhow::Result<ExitCode> {
    let version = env!("CARGO_PKG_VERSION");

    if wants_version() {
        println!("teton-code {version}");
        return Ok(ExitCode::SUCCESS);
    }

    // REQ-565 BR-7: resolve the lifetime policy before anything else observable
    // happens. A bad value refuses to start rather than defaulting — a typo'd
    // `--shutdown-policy` that silently fell back to exit-on-last-disconnect
    // would give the `brew services` always-on daemon the one lifetime it must
    // not have, and it would flap against launchd's keep-alive (BR-5).
    let policy_flags = match lifetime::parse_policy_flags(std::env::args().skip(1)) {
        Ok(flags) => flags,
        Err(err) => {
            eprintln!("teton-code: {err}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let policy_env = match lifetime::PolicyEnv::from_env() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("teton-code: {err}");
            return Ok(ExitCode::FAILURE);
        }
    };

    let paths = socket_path::daemon_paths();

    // Single-instance, with a bounded wait (REQ-565 D-6). Under exit-on-last-
    // client a departing daemon and an arriving one share an instant: the
    // departing one holds this lock until after it unlinks the socket, so a
    // successor spawned during that teardown would otherwise see "already
    // running", exit, and leave the CLI polling a socket nobody will bind.
    let _instance = match SingleInstance::acquire_within(&paths.lock, DEFAULT_ACQUIRE_WINDOW)? {
        Some(instance) => instance,
        None => {
            eprintln!(
                "teton-code: already running (lock held at {})",
                paths.lock.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
    };

    // Reference every library crate so the dependency edges stay real (the
    // daemon must fail to compile if a layer is missing).
    eprintln!(
        "teton-code {version} — core {}, protocol {}, providers {}, inference {}",
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

    let socket_path_for_teardown = paths.socket.clone();

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

        let (policy, source) =
            lifetime::resolve_policy(policy_flags, policy_env, daemon_runtime.lifetime_config());
        let supervisor = Arc::new(LifetimeSupervisor::new(policy, source, Arc::clone(&events)));
        eprintln!(
            "teton-code: shutdown policy {} (from {})",
            lifetime::policy_label(policy),
            source.as_str()
        );

        let listener = server::bind_listener(&paths.socket)?;
        eprintln!("tetond listening on {}", paths.socket.display());

        // The startup grace runs from the bind, not from process start: a slow
        // assembly above happens with no socket for anyone to connect to, so
        // only time *after* this point is time a client could have used.
        supervisor.spawn_startup_grace(lifetime::STARTUP_GRACE);
        spawn_signal_handlers(&supervisor);

        // REQ-547 BR-1/D-3: drive the first-run consent flow on its own task. It
        // may await a client's `model/confirm` indefinitely, and while it does
        // the daemon must keep serving sessions remote-only — so it is spawned
        // beside `serve`, never awaited before it.
        //
        // REQ-565 BR-2: it holds a `ModelLoad` claim for its whole duration —
        // the flow spans the deep verify, the load, and the benchmark, and
        // ADR-006 already treats those as one in-flight claim. A client that
        // disconnects mid-verify must not take a multi-gigabyte read down with
        // it.
        if daemon_runtime.first_run_consent_applies() {
            let consent_runtime = Arc::clone(&daemon_runtime);
            let consent_guard = supervisor.activity(BlockingActivity::ModelLoad);
            tokio::spawn(async move {
                let _consent_guard = consent_guard;
                consent_runtime.run_model_consent().await;
            });
        }

        let daemon = Arc::new(Daemon::with_lifetime(
            events,
            daemon_runtime,
            Arc::clone(&supervisor),
        ));
        // Returns when the lifetime supervisor commits — the accept loop races
        // `accept` against the shutdown signal.
        server::serve(listener, Arc::clone(&daemon)).await?;

        shutdown(&daemon, &supervisor, &socket_path_for_teardown);
        Ok::<(), anyhow::Error>(())
    })?;

    // `_instance` drops here, *after* `shutdown` unlinked the socket — the
    // ordering BR-3 depends on. See `shutdown`.
    Ok(ExitCode::SUCCESS)
}

/// The ordered teardown (REQ-565 BR-8).
///
/// The order is the requirement, not housekeeping:
///
/// 1. **stop accepting** — already done; `serve` returned because the lifetime
///    committed.
/// 2. **close sessions**, counting them for the event payload.
/// 3. **the cost ledger needs no flush.** It is SQLite in autocommit, so every
///    recorded row was durable the moment `record` returned. BR-8's "no
///    half-written ledger rows" is a property of the storage engine here; what
///    actually threatened it was a turn killed mid-flight, which `server.rs`
///    teardown now waits for instead of aborting.
/// 4. **unlink the socket** — while the single-instance lock is still held by
///    `main`'s `_instance`. A successor that wins the lock therefore cannot
///    find a stale socket, because the only process that could have left one
///    still owns the lock (BR-3).
/// 5. **report** the exit.
///
/// The lock is released last, by `_instance` dropping at the end of `main`.
fn shutdown(daemon: &Arc<Daemon>, supervisor: &Arc<LifetimeSupervisor>, socket: &std::path::Path) {
    let sessions_closed = u32::try_from(daemon.sessions.list().len()).unwrap_or(u32::MAX);

    // An unlink failure is reported, never fatal: a missing socket is the
    // desired end state either way, and the next daemon's `bind_listener`
    // removes a stale one it finds while holding the lock.
    if let Err(err) = std::fs::remove_file(socket) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "teton-code: could not remove the socket at {}: {err}",
                socket.display()
            );
        }
    }

    let reason = supervisor.exit_reason().unwrap_or(ExitReason::Signal);
    let uptime_seconds = supervisor.uptime().as_secs();
    let stage = DaemonLifetimeStage::Shutdown {
        reason,
        uptime_seconds,
        sessions_closed,
    };
    let reason_text = serde_json::to_string(&reason).unwrap_or_else(|_| "\"unknown\"".to_owned());
    eprintln!(
        "teton-code: daemon_shutdown (reason={reason_text}, uptime_seconds={uptime_seconds}, \
         sessions_closed={sessions_closed})"
    );
    daemon
        .events
        .publish(None, Event::DaemonLifetime(DaemonLifetime { stage }));
}

/// SIGTERM/SIGINT run the same ordered teardown as a last-client exit.
///
/// Without this, `brew services stop` under the `never` policy would SIGTERM a
/// daemon whose only exit path was self-termination, and the process would die
/// where it stood — leaving the socket behind for the next autostart to trip
/// over. Signals are the *only* way an always-on daemon ever stops, so they
/// have to reach the same code (BR-5, BR-8).
fn spawn_signal_handlers(supervisor: &Arc<LifetimeSupervisor>) {
    use tokio::signal::unix::{signal, SignalKind};

    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        let Ok(mut stream) = signal(kind) else {
            // A daemon that cannot install a handler still runs; it just exits
            // the hard way on a signal, exactly as it did before this REQ.
            continue;
        };
        let supervisor = Arc::clone(supervisor);
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                supervisor.signal();
            }
        });
    }
}
