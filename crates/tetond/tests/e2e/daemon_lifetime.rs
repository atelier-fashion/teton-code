//! REQ-565 acceptance: the daemon exits with its last client.
//!
//! Every test here spawns the **real** `teton-code` binary under its **real**
//! shipped lifetime (`DaemonOptions::real_lifetime()`) and asserts on the
//! process. That is deliberate: the claim is "no `teton-code` process remains",
//! and only the process can answer it. Every other suite in this binary pins
//! its daemon with `--shutdown-policy never`, because it uses the daemon as a
//! fixture rather than as the thing under test.
//!
//! A scripted local engine (`TETON_LOCAL_SCRIPT`) is used for the tests that do
//! not need a turn, so no real model is downloaded, verified, or loaded. That is
//! not just speed: `first_run_consent_applies()` is false for a scripted engine,
//! so the consent flow never runs and BR-2's model deferral cannot make exit
//! timing depend on multi-gigabyte I/O.
//!
//! The two AC-3 tests need a turn that is genuinely still running when its
//! client leaves, so they use a **delayed mock provider** instead. An instant
//! engine cannot give that window reliably — the first version of this suite
//! tried, and the race it left failed on a loaded macOS runner after passing
//! locally and on Linux.

use std::time::Duration;

use super::harness::{openai_turn, Daemon, DaemonOptions, MockProvider, MockResponse, Workspace};

/// How long a daemon is given to notice it is idle and go. Generous for CI, but
/// finite: a test that passed by waiting forever would assert nothing.
const EXIT_WINDOW: Duration = Duration::from_secs(20);

/// A workspace with a scripted local tier and no providers — enough to run a
/// turn without touching a network or a model file.
fn workspace(tag: &str) -> (Workspace, std::path::PathBuf) {
    let workspace = Workspace::new(tag);
    workspace.write_config("");
    let script = workspace.write_script("ok\n");
    (workspace, script)
}

fn spawn(workspace: &Workspace, script: &std::path::Path) -> Daemon {
    Daemon::spawn(
        workspace,
        DaemonOptions::default()
            .real_lifetime()
            .script(script.to_path_buf()),
    )
}

// ---------------------------------------------------------------------------
// AC-1: autostart → exit
// ---------------------------------------------------------------------------

#[test]
fn ac1_the_daemon_exits_cleanly_when_its_only_client_leaves() {
    let (workspace, script) = workspace("ac1");
    let mut daemon = spawn(&workspace, &script);

    let client = daemon.connect();
    drop(client);

    let status = daemon
        .wait_for_exit(EXIT_WINDOW)
        .expect("the daemon must exit on its own after its last client leaves");
    assert!(
        status.success(),
        "the exit must be clean, got {status}; log:\n{}",
        daemon.log()
    );

    // BR-8: no stale socket for the next autostart to trip over.
    assert!(
        !daemon.socket().exists(),
        "the socket must be unlinked on the way out, still at {}",
        daemon.socket().display()
    );

    let log = daemon.log();
    // `daemon_shutdown (` with the paren, not a bare prefix: `daemon_shutdown`
    // is also the start of `daemon_shutdown_armed` and `_deferred`, so the
    // loose form would pass against a daemon that armed and never left.
    assert!(
        log.contains("daemon_shutdown ("),
        "the exit itself must be reported, not just the arming; log:\n{log}"
    );
    assert!(
        log.contains("last_client"),
        "the reason must be last_client; log:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// AC-2: two clients
// ---------------------------------------------------------------------------

#[test]
fn ac2_only_the_second_disconnect_stops_a_two_client_daemon() {
    let (workspace, script) = workspace("ac2");
    let mut daemon = spawn(&workspace, &script);

    let first = daemon.connect();
    let second = daemon.connect();

    drop(first);
    // The survivor keeps it alive. A fixed wait is right here: the assertion is
    // that nothing happens, and there is no event to poll for.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        daemon.is_running(),
        "one client left, one remains — the daemon must not exit; log:\n{}",
        daemon.log()
    );

    let log = daemon.log();
    assert!(
        log.contains("client_disconnected (live_connection_count=1)"),
        "the disconnect must report one client still connected; log:\n{log}"
    );

    drop(second);
    assert!(
        daemon.wait_for_exit(EXIT_WINDOW).is_some(),
        "the last disconnect must stop it; log:\n{}",
        daemon.log()
    );
}

// ---------------------------------------------------------------------------
// AC-3: an in-flight turn defers, and its ledger row survives
// ---------------------------------------------------------------------------

/// The regression this REQ is proudest of. Before it, client teardown called
/// `task.abort()` on in-flight prompt turns, so a turn killed mid-flight never
/// reached its `record_call` and its cost row was simply lost. "The ledger row
/// for that turn is intact" was false.
///
/// Asserted on the **ledger**, not on the streamed output: the writer half is
/// gone by the time the turn finishes, so the text goes nowhere by design. The
/// durable row is the claim.
#[test]
fn ac3_a_turn_in_flight_defers_the_exit_until_it_completes() {
    // A provider that holds its response open for two seconds, so the turn is
    // unambiguously still executing when the client disconnects.
    //
    // The first version of this test used the instant scripted engine and argued
    // the ordering was structural — EOF is already buffered, while the turn must
    // make a `spawn_blocking` round trip. That reasoning was wrong: it held
    // locally over six runs and on Linux CI, then failed on a loaded macOS
    // runner where the turn finished first and no deferral was ever recorded.
    // The window is now a duration this test owns, not a scheduling accident
    // (LESSON-433: verification on one machine is not verification).
    let provider = MockProvider::start_delayed(
        vec![MockResponse::ok(openai_turn("done", None, 100, 20))],
        MockResponse::ok(openai_turn("done", None, 100, 20)),
        Duration::from_secs(2),
    );

    let workspace = Workspace::new("ac3");
    workspace.write_config(&format!(
        "default_provider = \"mock\"\n\n\
         [[providers]]\nid = \"mock\"\nkind = \"openai-compatible\"\n\
         endpoint = \"{}\"\nmodel = \"deepseek-chat\"\n\n",
        provider.openai_endpoint()
    ));
    let script = workspace.write_script("ok\n");
    let mut daemon = spawn(&workspace, &script);

    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    // Fire and forget, then leave immediately: the turn is still executing when
    // its client's socket closes, which is precisely AC-3's scenario and the one
    // `prompt` (which waits for the response) cannot produce.
    client.prompt_no_wait(&session, "hello");
    drop(client);

    let status = daemon
        .wait_for_exit(EXIT_WINDOW)
        .expect("the daemon must still exit once the turn finishes");
    assert!(status.success(), "the exit must be clean, got {status}");

    let log = daemon.log();
    // The daemon armed, found work, and said so — rather than exiting through
    // the middle of a running turn.
    assert!(
        log.contains("daemon_shutdown_deferred"),
        "an in-flight turn must defer the exit; log:\n{log}"
    );
    assert!(
        log.contains("\"turn\""),
        "the deferral must name the turn as the blocking activity; log:\n{log}"
    );
    // …and only then left.
    assert!(
        log.contains("daemon_shutdown ") && log.contains("last_client"),
        "the daemon must exit after the turn completes; log:\n{log}"
    );

    // BR-8: the deferred path unlinks the socket too.
    assert!(
        !daemon.socket().exists(),
        "the socket must be unlinked even on the deferred exit path"
    );
}

/// The ledger half of AC-3, asserted on an actual row.
///
/// This is the claim the REQ turns on: *"the ledger row for that turn is
/// intact"*. Before this change, client teardown called `task.abort()` on
/// in-flight prompt turns, killing the turn at whatever await point it had
/// reached — so it never reached its `record_call` and the row was simply lost.
/// The statement was false.
///
/// A real (mock) remote provider is used rather than the scripted local engine
/// because only a priced remote call produces a cost row at all; a scripted
/// turn records nothing, so it could never distinguish the fix from the bug.
///
/// A second client stays attached throughout. That is what makes the assertion
/// possible — someone has to be able to *ask* — and it isolates the property
/// under test: this is about the first client's teardown not killing its turn,
/// not about the daemon's exit, which the test above covers.
#[test]
fn ac3_a_disconnect_no_longer_kills_the_turn_that_writes_its_ledger_row() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn("done", None, 100, 20))],
        MockResponse::ok(openai_turn("done", None, 100, 20)),
    );

    let workspace = Workspace::new("ac3b");
    workspace.write_config(&format!(
        "default_provider = \"mock\"\n\n\
         [[providers]]\nid = \"mock\"\nkind = \"openai-compatible\"\n\
         endpoint = \"{}\"\nmodel = \"deepseek-chat\"\n\n",
        provider.openai_endpoint()
    ));
    let script = workspace.write_script("ok\n");
    let daemon = spawn(&workspace, &script);

    // The observer, attached first so the daemon cannot exit when the worker
    // leaves — the exit path is a different test's concern.
    let mut observer = daemon.connect();

    let mut worker = daemon.connect();
    let session = worker.create_session("freeform", None);
    worker.prompt_no_wait(&session, "hello");
    // Leave while the turn is still executing. Under the old `abort()` teardown
    // this is precisely where the turn died and its row was lost.
    drop(worker);

    // Poll: the turn finishes after its client is gone, so there is no response
    // to wait on — the row appearing is the only signal.
    let deadline = std::time::Instant::now() + EXIT_WINDOW;
    let mut calls = 0;
    while std::time::Instant::now() < deadline {
        calls = observer.cost_query()["total_calls"].as_u64().unwrap_or(0);
        if calls > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        calls > 0,
        "the turn's cost row must survive its client disconnecting mid-turn — \
         this is AC-3's ledger claim, and it was false before the teardown \
         stopped aborting in-flight turns; log:\n{}",
        daemon.log()
    );

    assert!(
        !daemon.log().contains("did not finish within"),
        "the turn must have been awaited to completion, not abandoned; log:\n{}",
        daemon.log()
    );
}

// ---------------------------------------------------------------------------
// AC-4: the connect-vs-shutdown race
// ---------------------------------------------------------------------------

/// Either arm of BR-3 is a pass; what must never happen is a *third* outcome —
/// a client that believes it has a session on a daemon that is leaving.
#[test]
fn ac4_a_client_arriving_during_shutdown_ends_up_on_a_working_daemon() {
    let (workspace, script) = workspace("ac4");
    let mut daemon = spawn(&workspace, &script);

    let first = daemon.connect();
    // Race a new connection against the departure of the old one.
    drop(first);
    let mut racer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = daemon.connect();
        let session = client.create_session("freeform", None);
        client.prompt(&session, "hello")
    }));

    // Arm one: the shutdown was cancelled and the turn ran. Arm two (the `Err`
    // case, deliberately unasserted): the daemon was already gone, so the
    // connect itself failed. In production the CLI autostarts a successor
    // there; the harness client does not, so an unwind is that arm's shape.
    if let Ok(response) = racer.as_mut() {
        assert!(
            response.get("error").is_none() || response["error"]["code"].as_i64() == Some(-32007),
            "a racing client must either be served or be refused with \
             DAEMON_SHUTTING_DOWN, got: {response}"
        );
    }

    // Whichever arm ran, exactly one daemon existed throughout — the flock
    // guarantees it, and a second daemon would have had to bind this socket.
    let _ = daemon.wait_for_exit(EXIT_WINDOW);
}

// ---------------------------------------------------------------------------
// AC-8: the policy modes
// ---------------------------------------------------------------------------

#[test]
fn ac8_never_survives_the_last_disconnect() {
    let (workspace, script) = workspace("ac8n");
    // The default `DaemonOptions` pins with `never`, which is exactly the mode
    // under test — spelled out here rather than inherited, so the test says what
    // it asserts.
    let mut daemon = Daemon::spawn(
        &workspace,
        DaemonOptions::default()
            .real_lifetime()
            .script(script.clone())
            .arg("--shutdown-policy")
            .arg("never"),
    );

    let client = daemon.connect();
    drop(client);
    std::thread::sleep(Duration::from_secs(2));

    assert!(
        daemon.is_running(),
        "`never` must survive the last disconnect; log:\n{}",
        daemon.log()
    );
    // And it still serves.
    let mut again = daemon.connect();
    let session = again.create_session("freeform", None);
    assert!(!session.is_empty(), "the daemon must still serve sessions");
}

#[test]
fn ac8_linger_waits_out_its_window_then_exits() {
    let (workspace, script) = workspace("ac8l");
    let mut daemon = Daemon::spawn(
        &workspace,
        DaemonOptions::default()
            .real_lifetime()
            .script(script.clone())
            .arg("--shutdown-policy")
            .arg("linger")
            // Six seconds, checked at one: the margin is deliberate. A loaded CI
            // runner has already caught this suite out once by finishing work
            // sooner than the test assumed, and a linger test that trips on
            // scheduling noise would be read as a lifetime bug.
            .arg("--linger-seconds")
            .arg("6"),
    );

    let client = daemon.connect();
    drop(client);

    // Still there partway through the window.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        daemon.is_running(),
        "a linger daemon must not exit before its window elapses; log:\n{}",
        daemon.log()
    );

    assert!(
        daemon.wait_for_exit(EXIT_WINDOW).is_some(),
        "a linger daemon must exit once its window elapses; log:\n{}",
        daemon.log()
    );
    assert!(
        daemon.log().contains("daemon_shutdown_armed"),
        "the linger arm must be reported; log:\n{}",
        daemon.log()
    );
}

#[test]
fn ac8_a_client_returning_inside_the_linger_window_keeps_the_daemon() {
    let (workspace, script) = workspace("ac8r");
    let mut daemon = Daemon::spawn(
        &workspace,
        DaemonOptions::default()
            .real_lifetime()
            .script(script.clone())
            .arg("--shutdown-policy")
            .arg("linger")
            .arg("--linger-seconds")
            .arg("8"),
    );

    let first = daemon.connect();
    drop(first);
    std::thread::sleep(Duration::from_secs(1));

    let second = daemon.connect();
    // Well past the original window: the returning client must have cancelled
    // it, so the daemon is alive for a reason and not merely slow.
    std::thread::sleep(Duration::from_secs(10));
    assert!(
        daemon.is_running(),
        "a client that returned inside the window must cancel the exit; log:\n{}",
        daemon.log()
    );
    drop(second);
}

// ---------------------------------------------------------------------------
// The probe that must not count (D-1)
// ---------------------------------------------------------------------------

/// Asserted explicitly rather than relied on silently. The harness's own
/// readiness check — and the CLI's autostart poll — open the socket and drop it
/// without handshaking. If those counted as clients, every daemon in this
/// binary would race its own liveness probe to the exit.
#[test]
fn a_readiness_probe_that_never_handshakes_does_not_start_the_clock() {
    let (workspace, script) = workspace("probe");
    let mut daemon = spawn(&workspace, &script);

    for _ in 0..3 {
        let probe = std::os::unix::net::UnixStream::connect(daemon.socket())
            .expect("the daemon must still be accepting");
        drop(probe);
    }

    std::thread::sleep(Duration::from_secs(2));
    assert!(
        daemon.is_running(),
        "bare probes must not arm a shutdown; log:\n{}",
        daemon.log()
    );

    // A real client still stops it, so the daemon is not merely stuck.
    let client = daemon.connect();
    drop(client);
    assert!(
        daemon.wait_for_exit(EXIT_WINDOW).is_some(),
        "a handshaked client leaving must still stop it; log:\n{}",
        daemon.log()
    );
}

// ---------------------------------------------------------------------------
// BR-5/BUG-166: a signal runs the ordered teardown, and the exit is clean
// ---------------------------------------------------------------------------

/// SIGTERM must end in exit status 0 with the socket unlinked — the `brew
/// services stop` path (BR-5), and the path BUG-166 found broken in llama
/// builds: leaving `main` through libc `exit()` ran ggml's Metal static
/// destructors, which abort when the model is still resident, turning every
/// routine stop into SIGABRT plus a macOS crash report. `main` now ends in
/// `_exit`, which skips them. This build has no llama engine, so the abort
/// itself cannot reproduce here; what this pins is the contract the fix
/// preserves — signal → ordered teardown, reported and attributed → status 0,
/// socket gone.
#[test]
fn a_sigterm_runs_the_ordered_teardown_and_exits_zero() {
    let (workspace, script) = workspace("term");
    // Pinned `never`: the only way this daemon can exit is the signal, so the
    // clean status below is the signal path's doing and nobody else's.
    let mut daemon = Daemon::spawn(
        &workspace,
        DaemonOptions::default()
            .real_lifetime()
            .script(script.clone())
            .arg("--shutdown-policy")
            .arg("never"),
    );

    // A handshaked client first, so the daemon under the signal is one that
    // has actually served, not one still in its startup grace.
    let client = daemon.connect();
    drop(client);

    let pid = i32::try_from(daemon.pid()).expect("a pid fits in i32");
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM must be deliverable to the daemon");

    let status = daemon
        .wait_for_exit(EXIT_WINDOW)
        .expect("a SIGTERM'd daemon must exit");
    assert!(
        status.success(),
        "the exit must be clean, got {status}; log:\n{}",
        daemon.log()
    );

    let log = daemon.log();
    assert!(
        log.contains("daemon_shutdown") && log.contains("\"signal\""),
        "the teardown must run and attribute the exit to the signal; log:\n{log}"
    );
    assert!(
        !daemon.socket().exists(),
        "the ordered teardown must unlink the socket; log:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// Policy diagnostics (BR-7)
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_policy_refuses_to_start_and_names_the_valid_spellings() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_teton-code"))
        .args(["--shutdown-policy", "nevr"])
        .output()
        .expect("run teton-code");

    assert!(
        !output.status.success(),
        "an unknown policy must refuse to start rather than defaulting"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nevr"),
        "the typo must be visible: {stderr}"
    );
    assert!(
        stderr.contains("on-last-disconnect") && stderr.contains("never"),
        "the valid spellings must be named: {stderr}"
    );
}
