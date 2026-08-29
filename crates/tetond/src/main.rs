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
use tetond::lifetime::{self, LifetimeSupervisor, LifetimeWorkClaim};
use tetond::runtime::DaemonRuntime;
use tetond::single_instance::{SingleInstance, DEFAULT_ACQUIRE_WINDOW};
use tetond::{server, Daemon};

use teton_core::lifetime::ExitReason;
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

    let served = runtime.block_on(async move {
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
        // REQ-565 BR-2: an install (download → verify → load) now defers the
        // daemon's exit. Wired here rather than at construction because the
        // policy above is read from the config the runtime loads, so the runtime
        // — and its consent gate — necessarily exists first.
        daemon_runtime
            .consent()
            .set_work_claim(Arc::new(LifetimeWorkClaim::new(Arc::clone(&supervisor))));

        // REQ-596 BR-1: tell the child-environment composer which variable names
        // hold configured credentials, so a `shell` command can never read one
        // back out of its own environment. A closure over the runtime rather
        // than a value, because the answer must be the *live* config's at the
        // moment of the spawn — a snapshot taken here would not know about a
        // provider added later in the session (LESSON-539).
        //
        // The closure keeps an `Arc<DaemonRuntime>` for the life of the
        // process. That is not a teardown hazard: the ordered shutdown below is
        // an explicit `shutdown(...)` call, not a `Drop` impl, so nothing is
        // skipped by the runtime outliving this scope.
        {
            let runtime_for_credentials = Arc::clone(&daemon_runtime);
            tetond::child_env::set_credential_env_names_provider(move || {
                runtime_for_credentials.credential_env_var_names()
            });
        }
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
        // REQ-565 BR-2: the flow is deliberately NOT wrapped in a lifetime claim
        // here. That indefinite await is a wait for a *human*, not in-flight
        // work, and a claim spanning it would mean a proposal nobody answers
        // pins the daemon forever — the standing-resident-daemon harm this REQ
        // removes, reintroduced by its own deferral rule. The claim lives one
        // level down instead, on the install itself, where bytes are actually
        // being written (`ModelConsentGate::with_work_claim`).
        if daemon_runtime.first_run_consent_applies() {
            let consent_runtime = Arc::clone(&daemon_runtime);
            tokio::spawn(async move {
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
    });

    // BUG-169: die by `_exit`, never by `exit()`.
    //
    // A llama-build daemon that has loaded the model cannot survive libc
    // `exit()`: ggml's Metal backend holds its device registry in a
    // function-local C++ static whose destructor — run by `__cxa_finalize`
    // inside `exit()` — hard-asserts that every Metal buffer was already
    // freed (`GGML_ASSERT([rsets->data count] == 0)`, ggml-metal-device.m).
    // With the model weights still resident that assert aborts, turning
    // every routine shutdown (exit-with-last-client, SIGTERM) into SIGABRT
    // plus a macOS crash report. `std::process::exit` runs the same
    // handlers; only `_exit` skips them.
    //
    // Nothing this skips is load-bearing. Everything that must outlive the
    // process already does: the cost ledger is SQLite in autocommit (durable
    // per `record`), `shutdown` unlinked the socket, and the daemon reports
    // on stderr, which Rust does not buffer. `_instance`'s flock releases at
    // process death — the same instant, relative to the unlink, that BR-3
    // got from its drop at the end of `main`.
    let code = match served {
        Ok(()) => 0,
        Err(err) => {
            // The stderr a `?`-return printed before this path existed; the
            // CLI surfaces this tail when an autostart fails (E-4).
            eprintln!("Error: {err:?}");
            1
        }
    };
    unsafe { libc::_exit(code) }
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
/// The lock is released last, when the process dies — `main` ends in `_exit`
/// (BUG-169), so `_instance`'s flock is released by process death rather than
/// by its drop, at the same point in the order.
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
