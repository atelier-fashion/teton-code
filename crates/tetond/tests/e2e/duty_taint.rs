//! **AC-5 — a tainted session runs its duties on this machine, whatever the
//! bindings say** (REQ-561 BR-5), asserted by the bytes a mock provider
//! received rather than by reading a resolved route.
//!
//! ## Why this is here rather than beside the resolver
//!
//! Every duty already carries a resolver-level taint test with its own
//! non-vacuity pair (`runtime::tests::dispatch::{digest,triage,shell,title,
//! compact}::a_tainted_session_*`). Those read `route.provider()`, and AC-5 is
//! explicit that reading the route is not the assertion it wants: a route may
//! name `local` while something else still puts bytes on a wire. The only place
//! that claim can be settled is against a real daemon with a real HTTP transport
//! in front of a server that records what it was sent.
//!
//! So this file spawns the **real** `tetond` binary with the duties bound to a
//! separate mock provider from the turns — which is what makes
//! `duties.request_count()` a clean count of duty calls and nothing else — and
//! asserts:
//!
//! 1. a tainted session's `title` and `triage` duties put **zero** bytes on that
//!    provider, and demonstrably ran anyway (the stand-in engine's own answers
//!    reach the model's visible reply), and
//! 2. **the non-vacuity pair**: an untainted session on the same daemon, the same
//!    bindings and the same prompt sends both duty prompts off the machine.
//!
//! Order is load-bearing. The tainted session runs first, so its zero is a zero
//! against a provider nothing had reached yet; the control then makes the same
//! counter non-zero. Reversed, "the count did not grow" would be indistinguishable
//! from a provider that had gone away (LESSON-485).
//!
//! ## Two of the five duties cannot be shown this way, and why
//!
//! Recorded rather than omitted:
//!
//! - **`shell`** — the taint backstop has to be armed for any of this, and arming
//!   it means configuring a privacy boundary. A `shell` result is always
//!   `ToolProvenance::Unknown`, which the choke point fail-closes on **whenever a
//!   boundary exists**, so an untainted session's `shell` duty sends zero bytes
//!   too. There is no non-vacuity pair to be had: the control leg would be zero
//!   as well. Its taint override is asserted at the resolver, and its zero-bytes
//!   claim is `harness::shell_duty`'s own AC-4 pair. What *is* asserted here is
//!   the other half — that a tainted session's `shell` duty genuinely **ran**, on
//!   this machine, which is the part a resolver test cannot show.
//! - **`compact`** — reaching the soft threshold end to end means driving a
//!   conversation past a fraction of the daemon's real context budget, which is a
//!   fixture about context sizing rather than about taint. Its taint override is
//!   asserted at the resolver with its own non-vacuity pair
//!   (`a_tainted_session_compacts_on_the_local_tier`), and its egress refusal by
//!   captured bytes in `tests/duty_egress.rs`.

use std::time::Duration;

use tetond::harness::title::TITLE_OUTPUT_CONTRACT;
use tetond::harness::triage::TRIAGE_OUTPUT_CONTRACT;

use crate::harness::{
    assert_no_boundary_bytes, category_block, openai_turn, remote_provider_block, tier_block,
    Client, Daemon, DaemonOptions, MockProvider, MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// The stand-in engine's fixed answer to a `triage` duty is the identity
/// ranking, which `GrepTool::refine` renders under this frame. Its presence in a
/// turn's visible reply is proof the duty ran **and** that what served it was
/// this machine — a remote `triage` would have put the mock provider's own
/// numbers there instead.
///
/// Spelled here rather than imported because `render_ranked` is private to the
/// tool; the count is the fixture's own (`grep` for `fn ` finds exactly two
/// matches in the demo repo), so a change to either shows up as a failure here
/// rather than as a silently weaker assertion.
const LOCAL_TRIAGE_FRAME: &str = "[triage: 2 of 2 matches, most useful first]";

/// The stand-in engine's fixed answer to a `shell` duty
/// (`runtime::SCRIPTED_SHELL_INTERPRETATION`, private). Same argument as above.
const LOCAL_SHELL_INTERPRETATION: &str = "[scripted interpretation of the command output]";

/// The concatenated assistant-message text this client has streamed.
fn agent_message_text(client: &Client) -> String {
    client
        .events_named("session_update")
        .iter()
        .filter_map(|e| {
            let update = &e["update"];
            if update["kind"].as_str() == Some("agent_message_chunk") {
                update["text"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect()
}

/// How many of `provider`'s captured request bodies carry `needle`.
fn bodies_carrying(provider: &MockProvider, needle: &str) -> usize {
    provider
        .requests()
        .iter()
        .filter(|body| String::from_utf8_lossy(body).contains(needle))
        .count()
}

/// **AC-5 / BR-5.** A session tainted by boundary content runs `title` and
/// `triage` on the local tier despite both being bound to a remote provider —
/// asserted by that provider's captured bytes — and an untainted session on the
/// same daemon and the same bindings genuinely sends both.
#[test]
fn a_tainted_sessions_duties_never_reach_the_provider_they_are_bound_to() {
    // The TURN provider. Its script is consumed only by turns, never by duties,
    // which is the whole reason the duties get a provider of their own.
    let turns = MockProvider::start(
        vec![
            // Turn 1 of the tainted session: read the boundary file. The
            // follow-up call carrying that result is blocked before a byte
            // leaves, which is what marks the session.
            MockResponse::ok(openai_turn(
                "Reading the production config.",
                Some(("c1", "read", r#"{"path":"secrets/prod.env"}"#)),
                120,
                20,
            )),
            // The control session's turn: a `grep` whose matches are worth
            // ranking, so `triage` fires.
            MockResponse::ok(openai_turn(
                "Let me look for the functions.",
                Some(("c2", "grep", r#"{"pattern":"fn "}"#)),
                120,
                20,
            )),
        ],
        MockResponse::ok(openai_turn("Done.", None, 10, 5)),
    );
    // The DUTY provider. Every request it receives is a harness duty.
    let duties = MockProvider::always(openai_turn("2 1", None, 5, 2));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "turns",
        &turns.openai_endpoint(),
        "claude-opus-4",
    ));
    config.push_str(&remote_provider_block(
        "duties",
        &duties.openai_endpoint(),
        "deepseek-chat",
    ));
    // `edit` inherits `build`, so turns go to the turn provider.
    config.push_str(&tier_block("build", "turns"));
    // `triage`, `digest` and `compact` inherit `scan`.
    config.push_str(&tier_block("scan", "duties"));
    // `title` is `reflex`, which never inherits `default_provider` — a
    // per-category override is the one way a user can bind it off the machine,
    // and binding it is what makes the taint override load-bearing here.
    config.push_str(&category_block("title", "duties"));
    // The backstop only arms when a boundary exists (`context_is_sensitive`
    // returns false with none configured).
    config.push_str("[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n\n");

    let ws = Workspace::new("ac5");
    ws.write_config(&config);
    // Five blocks: one for the rerouted turn 1, two each for the tainted
    // session's turns 2 and 3. Duties are answered off-script and consume none
    // of them (BR-10), which is what keeps this sequence meaning what it says.
    let script = ws.write_script(
        &[
            "Rerouted locally; done.",
            "Let me look for the functions.\n{\"tool\": \"grep\", \"arguments\": {\"pattern\": \"fn \"}}",
            "Found them: {{LAST_TOOL_RESULT}}",
            "Now check the build.\n{\"tool\": \"shell\", \"arguments\": {\"command\": \"ls /nonexistent-teton-path\"}}",
            "Checked: {{LAST_TOOL_RESULT}}",
        ]
        .join("\n---\n"),
    );
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    // === The tainted session ===============================================

    let tainted = client.create_session("structured", Some("implement"));

    // Turn 1's request is deliberately too short to name a session by, so the
    // `title` attempt is DEFERRED rather than spent — which is what lets turn 2
    // exercise `title` on an already-tainted session. Without this the duty
    // would fire on turn 1, before the taint, and prove nothing.
    let first = client.prompt(&tainted, "go");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted turn should still complete: {first}"
    );
    client.drain_events(Duration::from_millis(300));
    assert!(
        client.saw_event("privacy_block"),
        "the boundary read must be blocked, which is what taints the session"
    );
    assert_eq!(
        duties.request_count(),
        0,
        "a request too short to name a session by must not buy a `title` call \
         (ADR-11's zero-call case), or the count below starts non-zero"
    );

    // Turn 2: pinned local. `title` claims its one attempt here, and `triage`
    // ranks the grep result — both on the tainted session.
    let second = client.prompt(
        &tainted,
        "Find every function definition in this repository and list them.",
    );
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{second}"
    );
    client.drain_events(Duration::from_millis(300));

    // Turn 3: a failing command, so the `shell` duty fires too.
    let third = client.prompt(&tainted, "Now check whether the build directory exists.");
    assert_eq!(
        third["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{third}"
    );
    client.drain_events(Duration::from_millis(300));

    // --- THE CLAIM, on captured bytes --------------------------------------
    assert_eq!(
        duties.request_count(),
        0,
        "BR-5 VIOLATION: a tainted session's duties reached the provider they are \
         bound to. Captured: {:?}",
        duties
            .requests()
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );

    // --- And they genuinely RAN, on this machine ----------------------------
    // The other half of the claim, and the half a zero count cannot carry: a
    // duty that was never invoked also sends zero bytes. Each marker below is
    // the stand-in engine's own answer, so its presence in the model's visible
    // reply means the local tier served the duty.
    let tainted_reply = agent_message_text(&client);
    assert!(
        tainted_reply.contains(LOCAL_TRIAGE_FRAME),
        "the tainted session's `triage` must have been served on the local tier, \
         not skipped. Reply was: {tainted_reply}"
    );
    assert!(
        tainted_reply.contains(LOCAL_SHELL_INTERPRETATION),
        "the tainted session's `shell` duty must have been served on the local \
         tier. Reply was: {tainted_reply}"
    );
    assert!(
        client.saw_event("session_titled"),
        "the tainted session must still have been named — locally (BR-3/BR-9): {:?}",
        client.events_named("session_titled")
    );

    // === The non-vacuity pair: an untainted session on the same daemon ======

    let clean = client.create_session("structured", Some("implement"));
    let control = client.prompt(
        &clean,
        "Find every function definition in this repository and list them.",
    );
    assert_eq!(
        control["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the control session must complete, or it proves nothing: {control}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        bodies_carrying(&duties, TITLE_OUTPUT_CONTRACT),
        1,
        "an UNTAINTED session on this exact binding must send its `title` duty \
         off the machine — otherwise the zero above is a binding that never \
         worked rather than the taint overriding it"
    );
    assert_eq!(
        bodies_carrying(&duties, TRIAGE_OUTPUT_CONTRACT),
        1,
        "and its `triage` duty too: {:?}",
        duties
            .requests()
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );

    // Both duty prompts belong to the control session, so every byte the duty
    // provider ever saw arrived after the tainted session was done with it.
    assert!(
        duties.request_count() >= 2,
        "the control session's duties really did reach the wire"
    );

    assert_no_boundary_bytes();
}
