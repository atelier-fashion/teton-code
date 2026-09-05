//! BUG-211 — through the real binary: the daemon keeps its durable state under
//! `$XDG_DATA_HOME/teton`, not under `$XDG_RUNTIME_DIR/teton`, and moves what
//! an older daemon left there on its first start.
//!
//! The harness sets both variables to different directories, which is the
//! Linux shape; every other suite here now exercises the data-dir layout for
//! free (the consent suite's restarts, in particular, find their weights and
//! their recorded decision across a respawn). This file pins the two claims
//! those suites only imply:
//!
//! | Claim | Test |
//! |---|---|
//! | a fresh daemon writes its stores under the data dir and nothing durable beside the socket | [`a_fresh_daemon_keeps_nothing_durable_beside_the_socket`] |
//! | a daemon that finds legacy state beside the socket moves it once, says so, and uses it | [`legacy_state_beside_the_socket_is_moved_once_and_announced`] |
//!
//! Mutation record (run 2026-09-05): disabling `migrate_durable_state` (an
//! unconditional early return) reddens the legacy claim here and in
//! `tests/state_dir_migration.rs` while the fresh claim stays green; pointing
//! the stores back at the runtime directory (`let base_dir = runtime_dir` in
//! `from_dirs`) reddens both claims here and the legacy one there, while
//! `from_env_over_one_directory_is_unchanged` stays green — the split is the
//! finding, since that test is the one shape the fix must not disturb.

use std::time::Duration;

use crate::harness::{Daemon, DaemonOptions, Workspace};

const GIB: u64 = 1024 * 1024 * 1024;

fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Everything durable the daemon creates on a plain start, by name under
/// the state directory — the list the runtime-dir assertion is the absence
/// of.
const DURABLE_ON_START: &[&str] = &["cost.db", "projects.json"];

#[test]
fn a_fresh_daemon_keeps_nothing_durable_beside_the_socket() {
    let ws = Workspace::new("state-fresh");
    ws.write_config("");
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();
    // One session, so the project registry has something to record.
    let _ = client.create_session("freeform", None);
    client.drain_events(Duration::from_millis(200));

    for entry in DURABLE_ON_START {
        assert!(
            ws.state_dir().join(entry).exists(),
            "{entry} must be created under the data directory: {:?}",
            std::fs::read_dir(ws.state_dir()).map(|d| d.count())
        );
        assert!(
            !ws.runtime_state_dir().join(entry).exists(),
            "{entry} must not be created beside the socket"
        );
    }
    assert!(
        ws.runtime_state_dir().join("tetond.sock").exists(),
        "the socket is what the runtime directory is for"
    );
}

#[test]
fn legacy_state_beside_the_socket_is_moved_once_and_announced() {
    let ws = Workspace::new("state-legacy");
    ws.write_config("");
    // What a pre-BUG-211 daemon left beside its socket: a recorded model
    // decision and a project registry. Planted before the first start.
    let legacy = ws.runtime_state_dir();
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("model-selection.toml"),
        "model_name = \"none\"\nsource = \"probe\"\ndeclined_local = true\ndecided_at_ms = 1\n",
    )
    .unwrap();
    std::fs::write(legacy.join("projects.json"), "{\"entries\":{}}\n").unwrap();

    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();
    let _ = client.create_session("freeform", None);
    client.drain_events(Duration::from_millis(200));

    for entry in ["model-selection.toml", "projects.json"] {
        assert!(
            ws.state_dir().join(entry).exists(),
            "{entry} was not moved to the data directory"
        );
        assert!(
            !legacy.join(entry).exists(),
            "{entry} was left beside the socket"
        );
    }
    let log = daemon.log();
    assert!(
        log.contains("tetond: state — moved model-selection.toml")
            && log.contains("tetond: state — moved projects.json"),
        "the move is announced on the daemon's stderr, once per entry: {log}"
    );
    assert!(
        log.contains("BUG-211"),
        "the announcement names the bug a reader can look up: {log}"
    );

    // A second daemon on the same workspace has nothing to move and says
    // nothing about it.
    drop(client);
    drop(daemon);
    let again = Daemon::spawn(&ws, probe_16gb());
    let mut client = again.connect();
    let _ = client.create_session("freeform", None);
    client.drain_events(Duration::from_millis(200));
    assert!(
        !again.log().contains("tetond: state —"),
        "a second start moves nothing: {}",
        again.log()
    );
}
