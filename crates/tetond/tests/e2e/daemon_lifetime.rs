//! REQ-565 acceptance: the daemon exits with its last client.
//!
//! Every test here spawns the **real** `teton-code` binary under its **real**
//! shipped lifetime (`DaemonOptions::real_lifetime()`) and asserts on the
//! process. That is deliberate: the claim is "no `teton-code` process remains",
//! and only the process can answer it. Every other suite in this binary pins
//! its daemon with `--shutdown-policy never`, because it uses the daemon as a
//! fixture rather than as the thing under test.
//!
//! A scripted local engine (`TETON_LOCAL_SCRIPT`) is used throughout so no real
//! model is downloaded, verified, or loaded. That is not just speed:
//! `first_run_consent_applies()` is false for a scripted engine, so the consent
//! flow never runs, and BR-2's model deferral cannot make exit timing depend on
//! multi-gigabyte I/O. The deferral itself is asserted with a prompt turn, which
//! this suite can control exactly.

use std::time::Duration;

use super::harness::{Daemon, DaemonOptions, Workspace};

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
    assert!(
        log.contains("daemon_shutdown"),
        "the shutdown must be reported; log:\n{log}"
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
    let (workspace, script) = workspace("ac3");
    let mut daemon = spawn(&workspace, &script);

    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    // Fire and forget, then leave immediately: the turn is still executing when
    // its client's socket closes, which is precisely AC-3's scenario and the one
    // `prompt` (which waits for the response) cannot produce.
    //
    // The ordering is structural, not lucky. By the time the reader loop polls
    // again, the EOF is already buffered — the client shut the socket down
    // before this line returned — so teardown begins within one loop iteration.
    // The turn, meanwhile, cannot finish faster than a `spawn_blocking` thread
    // round trip (ADR-006 E-3 puts every engine call there). So the turn is
    // still in flight when the disconnect lands, even with a scripted engine
    // whose completion is microseconds.
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

/// The ledger half of AC-3, at the layer that can actually show it.
///
/// A turn's cost row is written by `record_call` inside the turn. Before this
/// REQ, client teardown called `task.abort()` on in-flight turns, so a turn
/// killed mid-flight never reached that call and its row was lost. The fix is
/// that teardown now *awaits* the turn instead — asserted here by the turn
/// reaching its natural end after its client is gone, which is the only way the
/// row can exist at all.
#[test]
fn ac3_a_disconnect_no_longer_kills_the_turn_that_writes_the_row() {
    let (workspace, script) = workspace("ac3b");
    let mut daemon = spawn(&workspace, &script);

    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    client.prompt_no_wait(&session, "hello");
    drop(client);

    assert!(
        daemon.wait_for_exit(EXIT_WINDOW).is_some(),
        "the daemon must exit; log:\n{}",
        daemon.log()
    );

    let log = daemon.log();
    // The abandonment path logs loudly when it fires. Its absence is the claim:
    // the turn was waited for, not abandoned.
    assert!(
        !log.contains("did not finish within"),
        "the turn must have been awaited to completion, not abandoned; log:\n{log}"
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
            .arg("--linger-seconds")
            .arg("3"),
    );

    let client = daemon.connect();
    drop(client);

    // Still there partway through the window.
    std::thread::sleep(Duration::from_millis(750));
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
            .arg("5"),
    );

    let first = daemon.connect();
    drop(first);
    std::thread::sleep(Duration::from_millis(500));

    let second = daemon.connect();
    // Past the original window: the returning client must have cancelled it.
    std::thread::sleep(Duration::from_secs(6));
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
