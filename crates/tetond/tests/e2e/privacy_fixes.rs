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
    assert_no_boundary_bytes, openai_turn, remote_provider_block_with_window, Client, Daemon,
    DaemonOptions, MockProvider, MockResponse, Workspace,
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

/// A `[[providers]]` entry. `model` is the model the provider *calls* — REQ-557
/// BR-1/BR-2 make it a required declaration for every remote kind, and a remote
/// provider without one is unusable (ADR-E), so a fixture that omits it has no
/// routable remote provider at all.
fn provider_block(id: &str, kind: &str, endpoint: &str, model: &str) -> String {
    format!(
        "[[providers]]\nid = \"{id}\"\nkind = \"{kind}\"\nendpoint = \"{endpoint}\"\n\
         model = \"{model}\"\n\n"
    )
}

/// A `[[tiers]]` row (REQ-558). The tier — not the lifecycle phase — is the
/// configured routing surface: `build` serves `edit`/`shell`, `think` serves
/// `design`/`debug`/`review`, and a structured turn maps its phase to a category
/// which inherits one of them.
fn tier_block(tier: &str, provider: &str) -> String {
    format!("[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"{provider}\"\n\n")
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
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek"));
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

// ===========================================================================
// REQ-586 AC-15a — the privacy reroute re-fits the context before the local
// pin serves the turn
// ===========================================================================

/// Whitespace words in the AC-15a paste, at **4 bytes per word** (`"abc "`).
///
/// 30,000 words / 120,000 bytes fits the 128k-derived pair (84,650 words /
/// 253,952 bytes) with room to spare — under *both* of its 70% soft thresholds,
/// so the `compact` duty never fires on the remote leg and the paste reaches the
/// provider whole. It is 2.9× the local pair's words and 3.66× its bytes (10,240
/// / 32,768 since REQ-590 ADR-9 — the word half window-derived, the byte half the
/// unchanged constant), so the local pin cannot take it unchanged in either
/// currency.
///
/// The density is the point (REQ-586 Phase-3 F-19). At more than 4 B/word the
/// **byte** guard would bind while the turn was still being assembled for the
/// remote route, the paste would already be clamped before any reroute happened,
/// and this test would be quietly measuring the assembly gate instead of the
/// refit.
const AC15A_PASTE_WORDS: usize = 30_000;

/// A marker in the middle of that paste. `truncate_middle_with` keeps a head and
/// a tail; only the middle proves the whole thing travelled.
const AC15A_MIDDLE: &str = "MIDDLE-OF-THE-PASTE-MARKER";

fn ac15a_paste() -> String {
    let half = "abc ".repeat(AC15A_PASTE_WORDS / 2);
    format!("HEAD-OF-THE-PASTE-MARKER {half}{AC15A_MIDDLE} {half}TAIL-OF-THE-PASTE-MARKER")
}

/// **REQ-586 AC-15a / BR-1.** A turn assembled against a **128k** provider's
/// window is privacy-blocked mid-turn and pinned to the local tier: the context
/// is re-fitted to the local pair with `context_pressure { kind:
/// refit_on_reroute }` **before** the local `route_decided`, and the turn
/// completes.
///
/// ## What was wrong before, and what this catches
///
/// `CarriedTurn::begin` seeds the turn's manager once, from the *first*
/// attempt's pair. While every route's budget was equal that stale seed was
/// invisible. With a route-derived budget it is a wedge: a 30,000-word context
/// assembled for a 128k window, re-sent unchanged to a local engine with a
/// 16,384-token one, is the typed over-window refusal — and a refused turn never
/// commits (REQ-567 BR-6), so the session replays it into the same refusal
/// forever. "The turn completes" is therefore the load-bearing assertion here,
/// and the ordering is what makes it explicable: choose route, refit, announce,
/// retry.
///
/// ## Why the mock's received bytes are read
///
/// That the paste reached the provider *whole* is what makes this a test of the
/// refit rather than of the assembly gate (see the byte-density note on
/// [`AC15A_PASTE_WORDS`]), and it is only true because the budget followed the
/// route in the first place — on the pre-REQ default pair this same prompt would
/// have arrived at the provider already clamped.
#[test]
fn a_128k_turn_blocked_by_privacy_is_refitted_before_the_local_pin_serves_it() {
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(("c1", "read", r#"{"path":"secrets/prod.env"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );

    let mut config = String::new();
    config.push_str(&remote_provider_block_with_window(
        "deepseek",
        &provider.openai_endpoint(),
        "deepseek-chat",
        128_000,
    ));
    config.push_str(&tier_block("build", "deepseek"));
    config.push_str(&boundary_block("secrets/**", "local-only"));

    let ws = Workspace::new("px-refit");
    ws.write_config(&config);
    let script = ws.write_script("Rerouted locally; done.");
    let daemon = Daemon::spawn(&ws, probe_16gb_with_local(script));
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let paste = ac15a_paste();
    let resp = client.prompt(
        &session,
        &format!("Read the production configuration and summarize it.\n\n{paste}"),
    );

    // The load-bearing claim: it finished. A context re-sent unchanged to the
    // local pin is an over-window refusal, and the reroute has no fallback left.
    assert_eq!(
        resp["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted-to-local turn must complete, not die over-window: {resp}"
    );
    client.drain_events(Duration::from_millis(400));

    // Exactly one block, and the blocked provider was never retried (REQ-544).
    assert_eq!(client.events_named("privacy_block").len(), 1);
    assert_eq!(count_route_decided_to(&client, "deepseek"), 1);

    // The paste reached the 128k provider whole — so the budget really did
    // follow the route, and what the refit below cut is a context that was
    // assembled at full size rather than one already clamped on the way out.
    let sent = String::from_utf8_lossy(&provider.requests()[0]).into_owned();
    assert!(
        sent.contains(AC15A_MIDDLE),
        "a 128k route must carry a 120 KB paste whole; a clamped one here would \
         mean the fixture never tested the refit"
    );

    // --- The refit, and where it sits in the stream. ---
    let blocked = client
        .event_index_from(0, |e| e["event"] == "privacy_block")
        .expect("the turn was blocked");
    let refit = client
        .event_index_from(blocked, |e| {
            e["event"] == "context_pressure" && e["kind"] == "refit_on_reroute"
        })
        .unwrap_or_else(|| {
            panic!(
                "no refit_on_reroute after the block: {:?}",
                client.event_names()
            )
        });
    let local_route = client
        .event_index_from(blocked, |e| {
            e["event"] == "route_decided" && e["provider_id"] == "local"
        })
        .expect("the blocked turn must reroute to the local tier (M-1)");
    assert!(
        refit < local_route,
        "the refit must precede the route it re-budgeted for: refit at {refit}, \
         local route_decided at {local_route}"
    );

    // It names the local pair and its bound, and it really cut something.
    let event = &client.events()[refit];
    assert_eq!(
        event["bound"].as_str(),
        Some("local_engine"),
        "the pin is the local tier, whatever the blocked provider declared: {event}"
    );
    // The local tier's pair (REQ-590). The **word** half is derived: the
    // engine's 16,384-token window less the 1,024-token generation reservation,
    // run through the same formula a declared window runs. The **byte** half is
    // `LOCAL_BUDGET_BYTES`, unchanged — D-4 took the window derivation for it
    // too (30,720) and ADR-9 reversed that. Spelled as literals because what
    // this test is entitled to read is the *wire* — a client sees these two
    // numbers and nothing about how they were made.
    assert_eq!(event["budget_tokens"].as_u64(), Some(10_240), "{event}");
    assert_eq!(event["budget_bytes"].as_u64(), Some(32_768), "{event}");
    assert!(
        event["dropped_blocks"].as_u64().unwrap_or(0) > 0
            || event["elided_bytes"].as_u64().unwrap_or(0) > 0,
        "a refit that cut nothing would leave this vacuous — 120 KB does not fit \
         32,768 bytes: {event}"
    );
    assert_eq!(
        client.events_named("context_pressure").len(),
        1,
        "one reroute, one refit, one announcement"
    );

    // The boundary file's content never reached the mock provider.
    assert_no_boundary_bytes();
}
