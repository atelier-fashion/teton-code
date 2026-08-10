//! REQ-567 over the real socket: what a carried conversation does at the
//! privacy boundary, and whose conversation a second client joins (TASK-096).
//!
//! Both tests spawn the real `teton-code` binary and drive it as a protocol
//! client. The local tier is a scripted engine, the remote tier is a localhost
//! mock that captures every request body, and the claim in each case is about
//! **bytes that did or did not cross a transport** — which is the only place
//! either claim can be settled (conventions.md: code inspection is not
//! acceptance for a BR-1 claim).
//!
//! - AC-2: boundary content read on prompt 1 blocks prompt 2's remote route,
//!   exactly as a same-turn read does, and never reaches the wire.
//! - AC-9: client A prompts and leaves; client B, still attached, prompts the
//!   same session and its turn carries A's conversation.

use std::time::Duration;

use serde_json::json;

use crate::harness::{
    assert_no_boundary_bytes, openai_turn, remote_provider_block, tier_block, Client, Daemon,
    DaemonOptions, MockProvider, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// A deterministic above-floor hardware profile with a scripted local tier —
/// so the daemon has a local engine to serve (and to reroute onto) without
/// downloading a model.
fn probe_with_local(script: std::path::PathBuf) -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
        .script(script)
}

/// Declare a privacy boundary over the wire — the `config/set` op behind
/// `teton privacy add`.
fn set_boundary(client: &mut Client, glob: &str, mode: &str) {
    let resp = client.call(
        "config/set",
        json!({ "update": {
            "op": "set_privacy_boundary",
            "path_glob": glob,
            "mode": mode,
        }}),
    );
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "declaring the {glob} boundary: {resp}"
    );
}

/// Rebind a tier over the wire — the same `config/set` path `teton tier set`
/// drives.
fn set_tier(client: &mut Client, tier: &str, provider: &str) {
    let resp = client.call(
        "config/set",
        json!({ "update": {
            "op": "set_tier_binding",
            "tier": tier,
            "provider_id": provider,
        }}),
    );
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "binding the {tier} tier: {resp}"
    );
}

fn routes_to(client: &Client, provider: &str) -> usize {
    client
        .events_named("route_decided")
        .iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .count()
}

/// The captured bodies that are **agent turns** rather than duty calls.
///
/// A turn carries the system head; `title` and `route` build their own fixed
/// frames. Filtering on it keeps these assertions about the turn the test is
/// driving, so a future duty that legitimately goes remote cannot silently
/// change what "the provider received" means here.
fn remote_turns(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .into_iter()
        .map(|body| String::from_utf8_lossy(&body).into_owned())
        .filter(|body| body.contains("You are Teton Code"))
        .collect()
}

// ===========================================================================
// AC-2 — a boundary read in an early prompt guards every later one
// ===========================================================================

/// **AC-2 / BR-3.** A file read on prompt 1 and carried into prompt 2 raises
/// the **same** `privacy_block` and the same reroute a same-turn read raises,
/// and no byte of it reaches the wire.
///
/// This is the provenance half of BR-3: the block fires because the carried
/// `ContextBlock` still says which file its text came from, three registry
/// hops and one fresh `ContextManager` later. A carry that kept the text and
/// dropped the tag would leave the choke point looking at ordinary
/// conversation, which is exactly the failure this test exists to catch.
///
/// ## Why the boundary arrives between the two prompts
///
/// It has to, and the reason is worth stating because it looks like a
/// contrivance and is not. With the boundary already configured, prompt 1's own
/// completion trips REQ-544 C-2's taint backstop — a successful turn whose
/// context intersects a boundary pins the session local — so prompt 2 never
/// attempts a remote route and the choke point is never consulted. That
/// backstop is real protection and `privacy_fixes.rs` already pins it, but a
/// test riding it would pass with every provenance tag stripped: it reads a
/// session flag, not the conversation. So this fixture takes the sequence where
/// the choke point is the thing that answers — the user reads a file, *then*
/// declares it sensitive, then routes remote — which is an ordinary way to
/// discover you needed a boundary, and the only shape in which the carried
/// provenance is what stands between the file and the network.
///
/// ## The non-vacuity leg is a second session
///
/// A blocked turn and an unreachable provider look identical from the outside,
/// so the same daemon, at the same moment, on the same binding, runs a control
/// session that never touched the file — and its turn goes remote and
/// completes. The provider is reachable; what stopped the first session was the
/// conversation it was carrying.
#[test]
fn boundary_content_read_in_prompt_one_blocks_the_remote_prompt_two() {
    let provider = MockProvider::always(openai_turn("Fine by me.", None, 120, 20));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "deepseek",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    // A declared engine-backed tier, and prompt 1's binding. Declared rather
    // than left implicit because a tier binding names a **registered**
    // provider: config load refuses one that does not exist, which is REQ-557's
    // "no implicit default" holding at the config gate.
    config.push_str("[[providers]]\nid = \"on-device\"\nkind = \"local\"\n\n");
    config.push_str(&tier_block("build", "on-device"));
    // No boundary yet — see the doc comment. It is declared between the two
    // prompts, over the wire, exactly as `teton privacy add` declares one.

    let ws = Workspace::new("carry-privacy");
    ws.write_config(&config);
    let script = ws.write_script(
        &[
            // Prompt 1, locally: read the boundary file and answer from it.
            "I'll read the production configuration.\n\
             {\"tool\": \"read\", \"arguments\": {\"path\": \"secrets/prod.env\"}}",
            "Read it. Keeping that here.",
            // Prompt 2, after the block reroutes it back to this tier.
            "Still local; answering from what we have.",
        ]
        .join("\n---\n"),
    );
    let daemon = Daemon::spawn(&ws, probe_with_local(script));
    let mut client = daemon.connect();

    // --- Prompt 1: local, and it reads the boundary file. -------------------
    let session = client.create_session("structured", Some("implement"));
    let first = client.prompt(&session, "Summarize the production configuration.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the local read turn should complete cleanly: {first}"
    );
    client.drain_events(Duration::from_millis(300));
    assert!(
        routes_to(&client, "on-device") >= 1,
        "prompt 1 must have been served locally: {:?}",
        client.events_named("route_decided")
    );
    assert!(
        remote_turns(&provider).is_empty(),
        "prompt 1 was supposed to be local, but the provider received a turn"
    );
    assert!(
        !client.saw_event("privacy_block"),
        "nothing was blocked yet — a block here would mean prompt 1 tried to \
         egress, and the AC-2 claim below would be about the wrong turn"
    );

    // --- The user declares the file sensitive, and rebinds the tier. --------
    set_boundary(&mut client, "secrets/**", "local_only");
    set_tier(&mut client, "build", "deepseek");

    let second = client.prompt(&session, "Now describe what you read.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the blocked turn must reroute and complete, not fail: {second}"
    );
    client.drain_events(Duration::from_millis(300));

    // The carried read is what the choke point saw: one block, naming the
    // boundary file and the provider it was about to go to.
    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one privacy_block for the carried boundary content, \
         got {blocks:?}"
    );
    assert_eq!(
        blocks[0]["path"].as_str(),
        Some("secrets/prod.env"),
        "the block must name the file the conversation is carrying: {}",
        blocks[0]
    );
    assert_eq!(blocks[0]["provider_id"].as_str(), Some("deepseek"));
    assert_eq!(
        routes_to(&client, "deepseek"),
        1,
        "the blocked provider must be attempted once and never retried (M-1)"
    );
    assert!(
        routes_to(&client, "on-device") >= 2,
        "the blocked turn must reroute to the local tier"
    );

    // --- The control: a session that read nothing goes remote and lands. ----
    let clean = client.create_session("structured", Some("implement"));
    let control = client.prompt(&clean, "Say something agreeable.");
    assert_eq!(
        control["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the control session must complete on the remote provider: {control}"
    );
    let turns = remote_turns(&provider);
    assert_eq!(
        turns.len(),
        1,
        "exactly one turn should have reached the provider — the control's. \
         The carrying session's turn was blocked before transport."
    );
    assert!(
        turns[0].contains("Say something agreeable."),
        "the one turn that reached the provider is not the control's: {}",
        &turns[0][..turns[0].len().min(400)]
    );

    // The whole-suite claim: no byte of the boundary file ever reached a mock.
    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-9 — a second client joins a conversation, not an empty session
// ===========================================================================

/// **AC-9 / BR-2.** Client A prompts a session and disconnects while client B
/// stays attached; B's prompt on the same session carries A's conversation —
/// A's user message and the reply A got — into the context the model receives.
///
/// Read off the **captured request body**, because that is the turn context: a
/// client-visible answer could be produced by a model that was shown nothing.
///
/// The daemon runs under its real shipped lifetime (REQ-565) rather than pinned
/// open, so B's connection is what keeps it alive across A's departure. Under
/// the default policy the conversation dies with the daemon (BR-9), and this is
/// the case where it must not: a client left, not the last one.
#[test]
fn client_bs_prompt_carries_the_conversation_client_a_left_behind() {
    let provider =
        MockProvider::always(openai_turn("Three attempts, as configured.", None, 120, 20));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "deepseek",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek"));

    let ws = Workspace::new("carry-clients");
    ws.write_config(&config);
    // A scripted local tier the daemon never needs for a turn here — present so
    // the consent flow stays out of the way (`first_run_consent_applies` is
    // false for a scripted engine).
    let script = ws.write_script("Unused by this fixture.");
    let mut daemon = Daemon::spawn(&ws, probe_with_local(script).real_lifetime());

    let mut a = daemon.connect();
    let mut b = daemon.connect();

    // --- A prompts, then leaves. --------------------------------------------
    let session = a.create_session("structured", Some("implement"));
    let attached = b.call("session/attach", json!({ "session_id": session.clone() }));
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(session.as_str()),
        "client B must be attached to the same session: {attached}"
    );

    let first = a.prompt(&session, "How many retry attempts does the router allow?");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "client A's turn should complete: {first}"
    );

    drop(a);
    // The survivor holds the daemon open. A fixed wait is right here: the
    // assertion is that nothing happened, and there is no event to poll for.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        daemon.is_running(),
        "one client left and one remains — the daemon (and the conversation it \
         holds) must survive; log:\n{}",
        daemon.log()
    );

    // --- B prompts the same session. ----------------------------------------
    let second = b.prompt(&session, "And did we say the fallback shares it?");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "client B's turn should complete: {second}"
    );

    let turns = remote_turns(&provider);
    assert_eq!(turns.len(), 2, "one remote turn per prompt");
    // Non-vacuity: A's turn saw only its own message. Without this, a body that
    // contains everything would pass the assertion below for the wrong reason.
    assert!(
        !turns[0].contains("And did we say the fallback shares it?"),
        "client A's turn cannot have seen a prompt that had not been sent"
    );

    let bs_turn = &turns[1];
    assert!(
        bs_turn.contains("How many retry attempts does the router allow?"),
        "client B's turn context is missing the message client A sent — B \
         joined a session, not a conversation"
    );
    assert!(
        bs_turn.contains("Three attempts, as configured."),
        "client B's turn context is missing the reply client A got — half of \
         what BR-1 carries is the assistant's side"
    );
    assert!(
        bs_turn.contains("And did we say the fallback shares it?"),
        "client B's own message is missing from its own turn context"
    );

    assert_no_boundary_bytes();
}
