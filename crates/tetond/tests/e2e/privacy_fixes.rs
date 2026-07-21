//! End-to-end regression tests for the REQ-544 Group A privacy fixes.
//!
//! These spawn the **real** `tetond` binary and drive it over the socket, so they
//! exercise the daemon-level behaviors the loop/egress unit and integration tests
//! cannot: the session-taint backstop (C-2), the reroute-to-local on a privacy
//! block (M-1), and the guarantee that a tainted session's *subsequent* turns are
//! pinned to the local tier regardless of phase policy.
//!
//! Each remote-touching test also asserts the suite-wide BR-1 egress capture
//! stayed clean (the boundary secret never reached a mock provider).

use std::time::Duration;

use crate::harness::{
    assert_no_boundary_bytes, openai_turn, Client, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// A 16 GiB Apple-Silicon probe **with** a local script, so the daemon has a
/// local tier to reroute a blocked remote turn onto (REQ-544 M-1).
fn probe_16gb_with_local(script: std::path::PathBuf) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
        .script(script)
}

fn provider_block(id: &str, kind: &str, endpoint: &str) -> String {
    format!("[[providers]]\nid = \"{id}\"\nkind = \"{kind}\"\nendpoint = \"{endpoint}\"\n\n")
}

fn routing_block(phase: &str, provider: &str) -> String {
    format!("[[routing]]\nphase = \"{phase}\"\nprovider_id = \"{provider}\"\n\n")
}

fn boundary_block(glob: &str, mode: &str) -> String {
    format!("[[boundaries]]\npath_glob = \"{glob}\"\nmode = \"{mode}\"\n\n")
}

/// A local-engine script: two plain end-of-turn replies (one for the reroute of
/// the first prompt, one for the tainted second prompt).
fn local_done_script() -> String {
    ["Rerouted locally; done.", "Still local; done."].join("\n---\n")
}

fn count_route_decided_to(client: &Client, provider: &str) -> usize {
    client
        .events_named("route_decided")
        .iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .count()
}

/// The shared shape: a structured session routes `implement` to a remote mock,
/// the remote model reads a `local-only` file with `tool` (a boundary-touching
/// built-in), the next remote turn is blocked, the daemon reroutes to local, and
/// a *second* prompt on the same (now tainted) session is pinned to local even
/// though the phase policy still says remote.
fn taint_and_reroute(tag: &str, tool_call: (&str, &str, &str)) {
    // Turn 1 asks to read the boundary file; the daemon runs it locally (via the
    // jailed built-in), folds the result, and the next remote turn is blocked
    // BEFORE it reaches the mock — so the mock only ever sees turn 1.
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(tool_call),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );

    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &provider.openai_endpoint(),
    ));
    config.push_str(&routing_block("implement", "deepseek"));
    config.push_str(&boundary_block("secrets/**", "local-only"));

    let ws = Workspace::new(tag);
    ws.write_config(&config);
    let script = ws.write_script(&local_done_script());
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));

    // --- First prompt: remote turn blocked → tainted → rerouted to local. ---
    let first = client.prompt(
        &session,
        "Read the production configuration and summarize it.",
    );
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted-to-local turn should complete cleanly: {first}"
    );
    client.drain_events(Duration::from_millis(300));

    // Exactly one privacy_block for the whole logical block (REQ-544 M-1: not
    // retried against the blocked provider, so no duplicate events).
    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one privacy_block, got {blocks:?}"
    );
    assert_eq!(blocks[0]["provider_id"].as_str(), Some("deepseek"));

    // The remote provider was selected exactly once (the initial attempt); after
    // the block the turn rerouted to local, and a route_decided named it.
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        1,
        "the blocked remote provider must not be retried"
    );
    assert!(
        count_route_decided_to(&client, "local") >= 1,
        "the blocked turn must reroute to the local tier (M-1)"
    );

    // --- Second prompt: the session is tainted, so it is pinned to local even
    // though the `implement` policy routes to the remote provider (C-2). ---
    let second = client.prompt(&session, "Now describe the file you just read.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the tainted session's subsequent turn should complete on local: {second}"
    );
    client.drain_events(Duration::from_millis(300));

    // The remote provider was STILL only ever selected that one first time — the
    // subsequent turn never tried it (pinned local by taint).
    assert_eq!(
        count_route_decided_to(&client, "deepseek"),
        1,
        "a tainted session's later turn must not route remote (C-2)"
    );
    assert!(
        count_route_decided_to(&client, "local") >= 2,
        "the subsequent turn must route local"
    );
    // Still exactly one privacy_block overall — the second turn never egressed.
    assert_eq!(
        client.events_named("privacy_block").len(),
        1,
        "the tainted second turn must not produce another privacy_block"
    );

    // The boundary file's content never reached any mock provider.
    assert_no_boundary_bytes();
}

#[test]
fn shell_cat_taints_the_session_and_reroutes_to_local() {
    // The classic bypass: `shell {command: "cat secrets/prod.env"}`. Its result is
    // UNKNOWN provenance, fail-closed at egress.
    taint_and_reroute(
        "px-shell",
        ("c1", "shell", r#"{"command":"cat secrets/prod.env"}"#),
    );
}

#[test]
fn grep_surfacing_boundary_content_taints_and_reroutes() {
    // grep whose only match is inside the boundary file: the matched file is the
    // result's provenance.
    taint_and_reroute(
        "px-grep",
        ("c1", "grep", r#"{"pattern":"sk-live-DO-NOT-LEAK"}"#),
    );
}

#[test]
fn glob_enumerating_boundary_files_taints_and_reroutes() {
    // glob that enumerates the boundary file: the enumerated file is the result's
    // provenance.
    taint_and_reroute("px-glob", ("c1", "glob", r#"{"pattern":"secrets/**"}"#));
}
