//! One end-to-end test per REQ-544 acceptance criterion (AC-1..AC-9).
//!
//! Every test spawns the real `tetond` binary and drives it over the socket. No
//! model weights and no live API keys are used: the local tier is a scripted
//! engine, remote providers are localhost mock servers, and hardware is env
//! overridden. Each remote-touching test also asserts the suite-wide BR-1 egress
//! capture stayed clean.

use std::time::Duration;

use serde_json::{json, Value};

use crate::harness::{
    anthropic_turn, assert_no_boundary_bytes, edit_answer_script, mcp_call_script, mcp_stdio_toml,
    openai_turn, remote_provider_block_with_window, write_mcp_stdio_server, Client, Daemon,
    DaemonOptions, MockProvider, MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// Env overriding the hardware probe to a 16 GiB Apple-Silicon machine (a
/// deterministic, above-floor profile that selects a ≤3B model).
fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Env overriding the hardware probe to a below-floor 4 GiB machine.
fn probe_4gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (4 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
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

/// The explicit `default_provider` key (REQ-557 BR-4). It is a top-level key, so
/// it must precede every table/array header in the document.
///
/// There is no implicit default any more — REQ-557 deletes `build_router`'s
/// positional `.find` and its literal-`"local"` tail — so a fixture whose turn
/// resolves through the default rather than a `[[routing]]` policy has to say so.
fn default_provider_key(id: &str) -> String {
    format!("default_provider = \"{id}\"\n\n")
}

/// A `[[tiers]]` row (REQ-558). The tier — not the lifecycle phase — is the
/// configured routing surface, and it is read on **every** turn including
/// freeform (BR-1). A structured turn maps its phase to a category
/// (`spec`/`architect` → `design`, `implement` → `edit`, `review` → `review`,
/// `io` → `digest`), and the category inherits its tier's binding.
fn tier_block(tier: &str, provider: &str, fallback: Option<&str>) -> String {
    let mut s = format!("[[tiers]]\ntier = \"{tier}\"\nprovider_id = \"{provider}\"\n");
    if let Some(fb) = fallback {
        s.push_str(&format!("fallback_id = \"{fb}\"\n"));
    }
    s.push('\n');
    s
}

fn boundary_block(glob: &str, mode: &str) -> String {
    format!("[[boundaries]]\npath_glob = \"{glob}\"\nmode = \"{mode}\"\n\n")
}

fn result_stop_reason(resp: &Value) -> Option<&str> {
    resp["result"]["stop_reason"].as_str()
}

// ===========================================================================
// AC-1 — first-run offline path (local model only, zero egress).
// ===========================================================================

#[test]
fn ac1_first_run_offline_read_edit_verify() {
    let ws = Workspace::new("ac1");
    ws.write_config("# offline: no remote providers\n");
    let script = ws.write_script(&edit_answer_script());
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));

    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    let resp = client.prompt(
        &session,
        "In src/lib.rs change ANSWER from 1 to 2, then verify it.",
    );

    // The offline session completed on the model's end-of-turn.
    assert_eq!(
        result_stop_reason(&resp),
        Some("end_turn"),
        "offline session did not complete cleanly: {resp}"
    );

    // The edit really landed on disk.
    let updated = ws.read_repo_file("src/lib.rs");
    assert!(
        updated.contains("pub const ANSWER: u32 = 2;"),
        "the local model's edit did not land: {updated}"
    );

    client.drain_events(Duration::from_millis(200));

    // Routed to the local tier, and the streaming turn surface fired.
    let routed_local = client
        .events_named("route_decided")
        .iter()
        .any(|e| e["provider_id"].as_str() == Some("local"));
    assert!(
        routed_local,
        "expected a route_decided naming the local tier"
    );
    assert!(
        client.saw_event("session_update"),
        "the turn should have streamed session_update events"
    );

    // Offline path: nothing could egress (no provider, no transport in the loop).
    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-2 — two remote providers registered via config, sessions complete.
// ===========================================================================

#[test]
fn ac2_two_remote_providers_complete_sessions() {
    let anthropic = MockProvider::always(anthropic_turn("All done.", None, 120, 20));
    let deepseek = MockProvider::always(openai_turn("All done.", None, 120, 20));

    let ws = Workspace::new("ac2");
    ws.write_config("# providers registered over the wire\n");
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    // Register both providers via config/set — the same path the CLI's
    // `teton provider add` drives.
    register_provider(
        &mut client,
        "deepseek",
        "openai-compatible",
        &deepseek.openai_endpoint(),
        "deepseek-chat",
    );
    register_provider(
        &mut client,
        "anthropic",
        "anthropic",
        &anthropic.anthropic_endpoint(),
        "claude-opus-4",
    );
    // REQ-558 AC-9: the config surface takes a **tier**, not a phase. An
    // `implement` turn dispatches on `edit`, which inherits `build`; a `spec`
    // turn dispatches on `design`, which inherits `think`. The two structured
    // sessions below still land on the two different providers, but now they get
    // there through the axis the runtime actually reads.
    set_tier(&mut client, "build", "deepseek", None);
    set_tier(&mut client, "think", "anthropic", None);

    // Both appear in the config snapshot (registration is durable + visible).
    let snapshot = client.config_get();
    let ids: Vec<&str> = snapshot["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"deepseek") && ids.contains(&"anthropic"),
        "{ids:?}"
    );

    // REQ-557 TASK-046: the declared model survives daemon persistence and
    // reappears in the `config/get` projection — the round-trip that proves the
    // model is a stored property of the provider, not something re-derived on
    // read.
    let model_of = |id: &str| -> Option<String> {
        snapshot["providers"]
            .as_array()?
            .iter()
            .find(|p| p["id"].as_str() == Some(id))?["model"]
            .as_str()
            .map(str::to_owned)
    };
    assert_eq!(
        model_of("deepseek").as_deref(),
        Some("deepseek-chat"),
        "{snapshot}"
    );
    assert_eq!(
        model_of("anthropic").as_deref(),
        Some("claude-opus-4"),
        "{snapshot}"
    );

    // The OpenAI-compatible provider completes an (implement) session.
    let s1 = client.create_session("structured", Some("implement"));
    let r1 = client.prompt(&s1, "say hello");
    assert_eq!(result_stop_reason(&r1), Some("end_turn"), "{r1}");

    // The Anthropic provider completes a (spec) session.
    let s2 = client.create_session("structured", Some("spec"));
    let r2 = client.prompt(&s2, "say hello");
    assert_eq!(result_stop_reason(&r2), Some("end_turn"), "{r2}");

    // **REQ-558 BR-1, end to end.** The freeform turn resolves through the SAME
    // configured table the structured turns just used: `edit` inherits `build`,
    // which the second `config/set` above bound to `deepseek`. Before this REQ
    // the table was not consulted on a freeform turn at all — the default
    // experience routed on a ten-word substring list — which is the headline
    // defect the REQ exists to close.
    //
    // It is still not an *implicit* default. `deepseek` is here because the user
    // bound the tier over the wire, not because it was first in the array
    // (BUG-146 root cause #1) and not because `default_provider` was invented for
    // them — that absence is pinned in `model_identity::
    // no_default_provider_is_reported_not_invented`, where nothing is bound at
    // all and the turn says so.
    let s3 = client.create_session("freeform", None);
    let r3 = client.prompt(&s3, "implement the greeting");
    assert_eq!(
        result_stop_reason(&r3),
        Some("end_turn"),
        "a freeform turn must resolve through the configured table (BR-1): {r3}"
    );

    client.drain_events(Duration::from_millis(200));
    let routed: Vec<&str> = client
        .events_named("route_decided")
        .iter()
        .filter_map(|e| e["provider_id"].as_str())
        .collect();
    assert!(routed.contains(&"deepseek"), "{routed:?}");
    assert!(routed.contains(&"anthropic"), "{routed:?}");

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-3 — phase-based routing (frontier on spec/architect/review, cheap on
// implement), observable via route_decided.
// ===========================================================================

#[test]
fn ac3_phase_routing_is_observable() {
    let frontier = MockProvider::always(anthropic_turn("Done.", None, 200, 30));
    let cheap = MockProvider::always(openai_turn("Done.", None, 200, 30));

    let mut config = String::new();
    config.push_str(&provider_block(
        "anthropic",
        "anthropic",
        &frontier.anthropic_endpoint(),
        "claude-opus-4",
    ));
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &cheap.openai_endpoint(),
        "deepseek-chat",
    ));
    // spec, architect and review all map to `think`; implement maps to `build`.
    config.push_str(&tier_block("think", "anthropic", None));
    config.push_str(&tier_block("build", "deepseek", None));

    let ws = Workspace::new("ac3");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    for phase in ["spec", "architect", "implement", "review"] {
        let session = client.create_session("structured", Some(phase));
        let resp = client.prompt(&session, "advance the demo requirement");
        assert_eq!(
            result_stop_reason(&resp),
            Some("end_turn"),
            "phase {phase}: {resp}"
        );
    }
    client.drain_events(Duration::from_millis(200));

    // Each phase's route_decided names the configured tier.
    let decided: Vec<(String, String)> = client
        .events_named("route_decided")
        .iter()
        .filter_map(|e| {
            Some((
                e["phase"].as_str()?.to_owned(),
                e["provider_id"].as_str()?.to_owned(),
            ))
        })
        .collect();
    for (phase, expected) in [
        ("spec", "anthropic"),
        ("architect", "anthropic"),
        ("implement", "deepseek"),
        ("review", "anthropic"),
    ] {
        assert!(
            decided
                .iter()
                .any(|(p, prov)| p == phase && prov == expected),
            "phase {phase} should route to {expected}; saw {decided:?}"
        );
    }

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-4 — cost meter: total spend, per-phase attribution, savings vs frontier.
// ===========================================================================

#[test]
fn ac4_cost_meter_reports_totals_phases_and_savings() {
    let frontier = MockProvider::always(anthropic_turn("Done.", None, 1000, 200));
    let cheap = MockProvider::always(openai_turn("Done.", None, 1000, 200));

    let mut config = String::new();
    config.push_str(&provider_block(
        "anthropic",
        "anthropic",
        &frontier.anthropic_endpoint(),
        // The model the arithmetic below prices against; pre-REQ this was
        // derived from the price table by provider id, now it is declared.
        "claude-fable-5",
    ));
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &cheap.openai_endpoint(),
        "deepseek-v4-pro",
    ));
    config.push_str(&tier_block("think", "anthropic", None));
    config.push_str(&tier_block("build", "deepseek", None));

    let ws = Workspace::new("ac4");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    // A frontier spec call and a cheap implement call.
    let spec = client.create_session("structured", Some("spec"));
    client.prompt(&spec, "author the requirement");
    let implement = client.create_session("structured", Some("implement"));
    client.prompt(&implement, "implement the task");

    let report = client.cost_query();

    // EXACT arithmetic (not just direction) for the known scripted token counts
    // against the bundled price table, so a math-corruption bug that still lands
    // positive is caught. Both mocks report 1000 input / 200 output tokens.
    //   spec  → anthropic/claude-fable-5 @ $10/$50 per Mtok:
    //           1000*10 + 200*50          = 10_000 + 10_000 = 20_000 µ$
    //   impl  → deepseek/deepseek-v4-pro  @ $1.32/$3.96 per Mtok (the
    //           time-of-day peak ceiling, per the prices.toml convention):
    //           1000*1.32 + 200*3.96      =   1_320 +    792 =  2_112 µ$
    // Baseline reprices BOTH priced calls' token volume (1000/200 each) at
    // Fable:  2 * 20_000                                     = 40_000 µ$
    //   savings = baseline - actual = 40_000 - 22_112        = 17_888 µ$
    const SPEC_MICROS: i64 = 20_000;
    const IMPLEMENT_MICROS: i64 = 2_112;
    const TOTAL_MICROS: i64 = SPEC_MICROS + IMPLEMENT_MICROS; // 22_112
    const BASELINE_MICROS: i64 = 40_000;
    const SAVINGS_MICROS: i64 = BASELINE_MICROS - TOTAL_MICROS; // 17_888

    assert_eq!(report["total_calls"].as_u64(), Some(2), "{report}");
    assert_eq!(report["priced_calls"].as_u64(), Some(2), "{report}");
    assert_eq!(report["unpriced_calls"].as_u64(), Some(0), "{report}");
    assert_eq!(
        report["total_usd_micros"].as_i64(),
        Some(TOTAL_MICROS),
        "{report}"
    );

    // Per-phase attribution: exact per-phase dollars and token volumes.
    let phase_group = |name: &str| -> Value {
        report["per_phase"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["key"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("per-phase missing {name}: {report}"))
            .clone()
    };
    let spec = phase_group("spec");
    assert_eq!(spec["calls"].as_u64(), Some(1), "{report}");
    assert_eq!(spec["input_tokens"].as_u64(), Some(1000), "{report}");
    assert_eq!(spec["output_tokens"].as_u64(), Some(200), "{report}");
    assert_eq!(spec["usd_micros"].as_i64(), Some(SPEC_MICROS), "{report}");
    let implement = phase_group("implement");
    assert_eq!(implement["calls"].as_u64(), Some(1), "{report}");
    assert_eq!(implement["input_tokens"].as_u64(), Some(1000), "{report}");
    assert_eq!(implement["output_tokens"].as_u64(), Some(200), "{report}");
    assert_eq!(
        implement["usd_micros"].as_i64(),
        Some(IMPLEMENT_MICROS),
        "{report}"
    );

    // Savings vs the all-frontier baseline: exact figures, with its methodology.
    assert_eq!(
        report["baseline_model"].as_str(),
        Some("anthropic/claude-fable-5"),
        "{report}"
    );
    assert_eq!(
        report["baseline_usd_micros"].as_i64(),
        Some(BASELINE_MICROS),
        "{report}"
    );
    assert_eq!(
        report["savings_usd_micros"].as_i64(),
        Some(SAVINGS_MICROS),
        "{report}"
    );
    assert!(
        report["methodology"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("estimate"),
        "savings must be labelled an estimate: {report}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-5 — privacy boundary: a local-only file's content never egresses; a
// deliberate attempt raises privacy_block (egress-capture verified).
// ===========================================================================

#[test]
fn ac5_privacy_boundary_blocks_and_never_leaks() {
    // The provider is scripted to first ask to read the boundary file; the turn
    // that would carry that content is blocked before a byte leaves.
    let provider = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(("c1", "read", r#"{"path":"secrets/prod.env"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Done.", None, 120, 20)),
    );

    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek", None));
    config.push_str(&boundary_block("secrets/**", "local-only"));

    let ws = Workspace::new("ac5");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    // The session touches the boundary file (via the scripted read) and the next
    // remote turn is refused; the prompt turn ends in an error, which is fine —
    // the guarantee is no leak + a visible block.
    let _ = client.prompt(&session, "Summarize the production configuration.");
    client.drain_events(Duration::from_millis(300));

    // A privacy_block fired for the boundary file, naming the provider.
    let blocks = client.events_named("privacy_block");
    assert!(!blocks.is_empty(), "expected a privacy_block event");
    assert!(
        blocks
            .iter()
            .any(|b| b["path"].as_str() == Some("secrets/prod.env")
                && b["provider_id"].as_str() == Some("deepseek")),
        "privacy_block should name the boundary file and provider: {blocks:?}"
    );

    // Egress capture: the boundary file's content never reached the wire.
    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-6 — two clients share one daemon; the daemon survives a client exit.
// ===========================================================================

#[test]
fn ac6_two_clients_share_sessions_daemon_survives_exit() {
    let ws = Workspace::new("ac6");
    ws.write_config("# multi-client\n");
    let daemon = Daemon::spawn(&ws, probe_16gb());

    let mut a = daemon.connect();
    let mut b = daemon.connect();

    // A creates a session; both clients see the same list.
    let sid = a.create_session("structured", Some("spec"));
    assert_eq!(a.session_ids(), vec![sid.clone()]);
    assert_eq!(
        b.session_ids(),
        vec![sid.clone()],
        "clients disagree on sessions"
    );

    // A exits; the daemon and its session survive for B.
    drop(a);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        b.session_ids(),
        vec![sid.clone()],
        "session did not survive client exit"
    );

    // A fresh client still *sees* the surviving session — that listing, above
    // and here, is what AC-6's survival claim rests on.
    //
    // Attaching to it is a separate question, and since REQ-569 the answer is
    // "only by a decision" (BR-1/BR-6): a connection that created nothing and
    // holds no grant has the question put to a user, because knowing an id is
    // not standing (BR-8). This is **AC-3's resume flow** end to end — the last
    // client that held the session is gone, so nothing is attached to it and
    // the prompt is rendered by the client the user just opened. One consent
    // step, and exactly one.
    //
    // `with_auto_consent` is the user saying yes. Every other client in this
    // suite lacks it, which is what keeps the gate real:
    // `multi_client::two_clients_share_sessions_and_daemon_survives_client_exit`
    // drives the identical sequence with nobody answering and the fresh client
    // stays out.
    let mut c = daemon.connect().with_auto_consent();
    assert_eq!(
        c.session_ids(),
        vec![sid.clone()],
        "a fresh client must still see the surviving session"
    );
    let attached = c.call("session/attach", json!({ "session_id": sid.clone() }));
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(sid.as_str()),
        "an approved resume must attach the fresh client: {attached}"
    );
    let prompts = c.events_named("attach_consent_requested");
    assert_eq!(
        prompts.len(),
        1,
        "AC-3: at most one visible consent step, and it is this one: {prompts:?}"
    );
    assert_eq!(prompts[0]["scope"].as_str(), Some("attach"));
    assert_eq!(prompts[0]["session_id"].as_str(), Some(sid.as_str()));
    assert!(
        !c.saw_event("attach_refused"),
        "an approved request must not also be announced as refused"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// BUG-177 — the lifecycle replay a handshake delivers (AC-8 / BR-9) reaches
// the client that attached, and nobody else.
// ===========================================================================

/// The startup lifecycle is *replayed* to a client that just attached so it
/// learns the state of the local tier. It is that client's catch-up, not news:
/// every client already attached has had its own. Until BUG-177 the replay was
/// published on the daemon-wide bus, so every `teton doctor` in another
/// terminal — and every `teton …` a session's own shell tool spawned —
/// re-announced `probe …` / `local model … ready` into every open session.
///
/// The absence is decided by **ordering**, not by a timer (the pattern
/// `multi_client.rs` uses for every "B did not receive X" claim). A's
/// subscription is FIFO, and each attach publishes `daemon_client_attach` to
/// the clients already subscribed *before* the newcomer is subscribed and
/// replayed. So on A's stream B's attach marker precedes anything B's
/// handshake could leak, and C's marker follows it — a leaked replay has
/// exactly one place to land, between the two, and an empty gap is a decided
/// fact. B waiting for its *own* replay before C connects is what closes the
/// gap on the daemon side too: under the bug the leak and B's copy were one
/// publish, so once B has its `ready` the leaked frames were already queued to
/// A ahead of C's marker.
///
/// The positive controls sit in the same test: A and B each receive their own
/// replay ending in `ready` (the AC-8 contract the fix must not break), and A
/// receives both attach markers (the deliberate daemon-wide announcement, which
/// is not this bug).
#[test]
fn bug177_a_replayed_lifecycle_reaches_only_the_client_that_attached() {
    let ws = Workspace::new("bug177");
    ws.write_config("# replay scope\n");
    // A scripted local engine: the tier is up from the start, so every replay
    // is exactly `probed` → `ready` and nothing live is still being published
    // when the second and third clients arrive.
    let script = ws.write_script(&edit_answer_script());
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));

    let is_ready = |e: &Value| e["stage"]["stage"].as_str() == Some("ready");

    let mut a = daemon.connect();
    a.wait_for_event_where("model_lifecycle", is_ready, Duration::from_secs(5))
        .expect("A's own replay must end in `ready` (REQ-544 AC-8)");

    let mut b = daemon.connect();
    b.wait_for_event_where("model_lifecycle", is_ready, Duration::from_secs(5))
        .expect("B's own replay must reach B and end in `ready` (REQ-544 AC-8)");

    let mut c = daemon.connect();
    c.wait_for_event_where("model_lifecycle", is_ready, Duration::from_secs(5))
        .expect("C's own replay must reach C and end in `ready` (REQ-544 AC-8)");

    // A hears both attaches — the second one is the marker that closes the
    // window B's handshake could have leaked into.
    let mut attaches_seen = 0usize;
    a.wait_for_event_where(
        "daemon_client_attach",
        |_| {
            attaches_seen += 1;
            attaches_seen == 2
        },
        Duration::from_secs(5),
    )
    .expect("A must be told about both attaches (daemon-wide by design)");

    let events = a.events();
    let markers: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["event"].as_str() == Some("daemon_client_attach"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        markers.len() >= 2,
        "A must hold both attach markers by now: {events:?}"
    );

    // Everything A has seen since B's attach — its own replay was complete
    // before B ever connected, so a lifecycle event from here on is somebody
    // else's catch-up delivered to the wrong connection.
    let since_first_attach = &events[markers[0]..];
    let leaked: Vec<&Value> = since_first_attach
        .iter()
        .filter(|e| e["event"].as_str() == Some("model_lifecycle"))
        .collect();
    assert!(
        leaked.is_empty(),
        "BUG-177: another client's attach replayed the lifecycle into A's stream: {leaked:?}"
    );

    // And A's own replay was exactly one: one probe, one ready.
    let own: Vec<String> = lifecycle_stages(&a);
    assert_eq!(
        own,
        vec!["probed".to_owned(), "ready".to_owned()],
        "A must have received its own replay exactly once: {own:?}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-7 — a degraded provider triggers provider_degraded and the session
// completes via the fallback.
//
// Extended for REQ-586 AC-15b: the fallback declares a **smaller window** than
// the primary, so the same turn must be re-budgeted and re-fitted before it is
// re-sent — announced, and before the fallback's own `route_decided`.
// ===========================================================================

/// Whitespace words in the AC-15b paste, at 4 bytes per word (`"abc "`).
///
/// 30,000 words / 120,000 bytes sits **inside** the primary's 128k-derived pair
/// (84,650 words / 253,952 bytes) and **outside** the fallback's 32k-derived one
/// (20,650 / 61,952) in *both* currencies — which is the whole of AC-15b's
/// premise. A denser filler (>4 B/word) would bust the primary's byte guard
/// before the turn ever left, and the fixture would be testing the assembly gate
/// rather than the refit (REQ-586 Phase-3 F-19).
const AC15B_PASTE_WORDS: usize = 30_000;

/// A marker planted in the middle of that paste.
///
/// `truncate_middle_with` keeps a head and a tail, so a marker at either end
/// survives a clamp and proves nothing. The middle is the part that goes.
const AC15B_MIDDLE: &str = "MIDDLE-OF-THE-PASTE-MARKER";

/// The paste: a head marker, 30,000 filler words with [`AC15B_MIDDLE`] halfway
/// through, and a tail marker.
fn ac15b_paste() -> String {
    let half = "abc ".repeat(AC15B_PASTE_WORDS / 2);
    format!("HEAD-OF-THE-PASTE-MARKER {half}{AC15B_MIDDLE} {half}TAIL-OF-THE-PASTE-MARKER")
}

#[test]
fn ac7_degraded_provider_falls_back_and_completes() {
    let flaky = MockProvider::always_bad();
    let healthy = MockProvider::always(openai_turn("Recovered and done.", None, 120, 20));

    let mut config = String::new();
    // Two providers declaring the SAME model behind different endpoints — the
    // BR-3 shape this REQ exists to make expressible. Neither id appears in the
    // price table, so both are unpriced; AC-7 is about health fallback, not cost.
    //
    // REQ-586 AC-15b: and they declare **different windows**. Before this REQ
    // every route's budget was equal, so the stale seed was invisible; a turn
    // assembled for a 128k primary and re-sent unchanged to a 32k fallback is
    // the 400 BR-1 exists to prevent.
    config.push_str(&remote_provider_block_with_window(
        "flaky",
        &flaky.openai_endpoint(),
        "deepseek-chat",
        128_000,
    ));
    config.push_str(&remote_provider_block_with_window(
        "healthy",
        &healthy.openai_endpoint(),
        "deepseek-chat",
        32_000,
    ));
    config.push_str(&tier_block("build", "flaky", Some("healthy")));

    let ws = Workspace::new("ac7");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let paste = ac15b_paste();
    let resp = client.prompt(&session, &format!("implement the feature\n\n{paste}"));

    // The session completed rather than failing.
    assert_eq!(
        result_stop_reason(&resp),
        Some("end_turn"),
        "session did not recover via fallback: {resp}"
    );
    client.drain_events(Duration::from_millis(400));

    // provider_degraded named the failing provider and its fallback.
    let degraded = client.events_named("provider_degraded");
    assert!(
        degraded
            .iter()
            .any(|e| e["provider_id"].as_str() == Some("flaky")
                && e["fallback_id"].as_str() == Some("healthy")),
        "expected provider_degraded flaky -> healthy; saw {degraded:?}"
    );

    // --- REQ-586 AC-15b -------------------------------------------------
    //
    // The refit is announced, and it is announced *before* the fallback's own
    // route_decided: choose route, refit, say so, retry. A client that heard the
    // new route's numbers first could not tell which attempt the clamp belonged
    // to.
    let failed = client
        .event_index_from(0, |e| e["event"] == "provider_degraded")
        .expect("the primary was degraded");
    let refit = client
        .event_index_from(failed, |e| {
            e["event"] == "context_pressure" && e["kind"] == "refit_on_reroute"
        })
        .unwrap_or_else(|| {
            panic!(
                "no refit_on_reroute before the fallback attempt: {:?}",
                client.event_names()
            )
        });
    let fallback_route = client
        .event_index_from(failed, |e| {
            e["event"] == "route_decided" && e["provider_id"] == "healthy"
        })
        .expect("the fallback must have announced its own route");
    assert!(
        refit < fallback_route,
        "the refit must precede the route it re-budgeted for: refit at {refit},          fallback route_decided at {fallback_route}"
    );

    // It carries the fallback's pair, and it really cut something: the paste
    // fitted the primary's window and does not fit this one.
    let event = &client.events()[refit];
    assert_eq!(
        event["bound"].as_str(),
        Some("window"),
        "the fallback declares a window, so the bound is that window — not          default_unknown: {event}"
    );
    assert_eq!(
        event["budget_bytes"].as_u64(),
        Some((32_000u64 - 1_024) * 2),
        "the event must carry the pair derived from the *fallback's* window:          {event}"
    );
    assert!(
        event["elided_bytes"].as_u64().unwrap_or(0) > 0
            || event["dropped_blocks"].as_u64().unwrap_or(0) > 0,
        "a refit that cut nothing would leave AC-15b vacuous — the paste is          sized to fit one window and not the other: {event}"
    );

    // …and the bytes agree with the event. The primary was sent the whole
    // paste; the fallback was sent one with its middle taken out. That is the
    // "no 400" claim read off the wire rather than off an event.
    let sent_to_flaky = String::from_utf8_lossy(&flaky.requests()[0]).into_owned();
    let sent_to_healthy = String::from_utf8_lossy(&healthy.requests()[0]).into_owned();
    assert!(
        sent_to_flaky.contains(AC15B_MIDDLE),
        "the primary's 128k window had room for the whole paste"
    );
    assert!(
        !sent_to_healthy.contains(AC15B_MIDDLE),
        "the fallback was re-sent a context assembled for a window four times          its own — the over-window 400 BR-1 exists to prevent"
    );
    assert!(
        sent_to_healthy.contains("HEAD-OF-THE-PASTE-MARKER"),
        "…but it was re-fitted, not emptied: the clamp keeps a head and a tail"
    );
    assert!(
        sent_to_healthy.len() < sent_to_flaky.len(),
        "the fallback's request must be the smaller one: {} against {}",
        sent_to_healthy.len(),
        sent_to_flaky.len()
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-8 — hardware probe: a 16GB machine selects a <=3B model, benchmarks it,
// and steps down on a forced-slow benchmark; a <8GB machine disables the local
// tier and runs remote-only.
// ===========================================================================

#[test]
fn ac8_probe_selects_benchmarks_and_steps_down() {
    // Scenario A: 16 GiB machine, forced-slow benchmark -> step-down.
    {
        let ws = Workspace::new("ac8a");
        ws.write_config("# probe scenario A\n");
        let script = ws.write_script(&edit_answer_script());
        let daemon = Daemon::spawn(
            &ws,
            probe_16gb()
                .script(script)
                .env("TETON_PROBE_FORCE_SLOW_BENCH", "1"),
        );
        let mut client = daemon.connect();
        client.drain_events(Duration::from_millis(400));

        let stages: Vec<String> = lifecycle_stages(&client);
        assert!(
            stages.contains(&"probed".to_owned()),
            "no probe stage: {stages:?}"
        );
        assert!(
            stages.contains(&"benchmark".to_owned()),
            "no benchmark: {stages:?}"
        );
        assert!(
            stages.contains(&"stepped_down".to_owned()),
            "no step-down: {stages:?}"
        );

        // The probe selected a <=3B model (above the floor) before stepping down.
        let probed = client
            .events_named("model_lifecycle")
            .into_iter()
            .find(|e| e["stage"]["stage"].as_str() == Some("probed"))
            .expect("a probed event");
        assert_eq!(probed["stage"]["above_floor"].as_bool(), Some(true));
        assert_eq!(probed["model_id"].as_str(), Some("qwen2.5-coder-3b"));
    }

    // Scenario B: <8 GiB machine -> local tier disabled, remote-only sessions.
    {
        let provider = MockProvider::always(openai_turn("Remote-only done.", None, 100, 10));
        let mut config = String::new();
        // The freeform session below has no phase policy to match, so it resolves
        // through the default. REQ-557 BR-4 removed the implicit positional
        // default, so this fixture states it.
        config.push_str(&default_provider_key("deepseek"));
        config.push_str(&provider_block(
            "deepseek",
            "openai-compatible",
            &provider.openai_endpoint(),
            "deepseek-chat",
        ));
        let ws = Workspace::new("ac8b");
        ws.write_config(&config);
        let daemon = Daemon::spawn(&ws, probe_4gb());
        let mut client = daemon.connect();
        client.drain_events(Duration::from_millis(300));

        let stages = lifecycle_stages(&client);
        assert!(
            stages.contains(&"disabled".to_owned()),
            "below-floor machine should disable the local tier: {stages:?}"
        );

        // A freeform session still completes, remote-only.
        let session = client.create_session("freeform", None);
        let resp = client.prompt(&session, "implement the greeting remotely");
        assert_eq!(
            result_stop_reason(&resp),
            Some("end_turn"),
            "remote-only session did not complete: {resp}"
        );
    }

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-9 — an MCP server's tools appear in a session and execute under the
// standard permission prompts (ADR-003).
// ===========================================================================

#[test]
fn ac9_mcp_tools_appear_and_run_under_permission() {
    let ws = Workspace::new("ac9");
    let mcp_server = write_mcp_stdio_server(&ws.root);
    // AC-9: the MCP server is registered in the MAIN config TOML (`[[mcp_server]]`)
    // — the single source of truth, read from `TETON_CONFIG` with no side file.
    ws.write_config(&mcp_stdio_toml(&mcp_server));
    let script = ws.write_script(&mcp_call_script());

    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let resp = client.prompt(&session, "look something up in the knowledge base");

    // The session completed, having run the MCP tool.
    assert_eq!(
        result_stop_reason(&resp),
        Some("end_turn"),
        "MCP session did not complete: {resp}"
    );
    client.drain_events(Duration::from_millis(200));

    // (1) The MCP tool declared in the main TOML surfaced under the standard
    // permission model (asked, then run) — proving the `[[mcp_server]]` table is
    // registered and its tool is available.
    let prompted_for_mcp = client
        .events_named("permission_request")
        .iter()
        .any(|e| e["tool_name"].as_str() == Some("mcp__demo__echo"));
    assert!(
        prompted_for_mcp,
        "the MCP tool declared in the main TOML config should appear and be gated"
    );

    // (2) EXECUTION, not just offered+gated: the MCP tool's actual RESULT must
    // reach the model context and the final response. The scripted final reply
    // quotes the tool output via {{LAST_TOOL_RESULT}}, so this sentinel appears in
    // the streamed answer ONLY if the result was plumbed back into context. A
    // tool-result-plumbing regression (discarding the result) erases it and fails
    // here — the gap the old offered+gated-only assertion could not catch.
    let answer = agent_message_text(&client);
    assert!(
        answer.contains("echoed from the demo MCP server"),
        "the MCP tool's result must reach the model context / final response; \
         streamed answer was: {answer:?}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// Optional live smoke test (real provider). Ignored unless built with
// `--features live` AND `TETON_LIVE_OPENAI_ENDPOINT` (+ a key on file) is set.
// Never runs in CI: the mocked matrix above is the required gate.
// ===========================================================================

#[test]
#[cfg_attr(
    not(feature = "live"),
    ignore = "live smoke test: run with `--features live` and TETON_LIVE_OPENAI_ENDPOINT set"
)]
fn live_smoke_real_provider_completes_a_session() {
    let Ok(endpoint) = std::env::var("TETON_LIVE_OPENAI_ENDPOINT") else {
        eprintln!("TETON_LIVE_OPENAI_ENDPOINT unset; nothing to smoke-test");
        return;
    };

    let ws = Workspace::new("live");
    let mut config = String::new();
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &endpoint,
        "deepseek-chat",
    ));
    config.push_str(&tier_block("build", "deepseek", None));
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let resp = client.prompt(&session, "Reply with the single word: ok.");
    assert_eq!(
        result_stop_reason(&resp),
        Some("end_turn"),
        "live provider session did not complete: {resp}"
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn register_provider(client: &mut Client, id: &str, kind: &str, endpoint: &str, model: &str) {
    let resp = client.call(
        "config/set",
        json!({ "update": {
            "op": "register_provider",
            "id": id,
            "kind": kind,
            "endpoint": endpoint,
            "model": model,
        }}),
    );
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "register {id}: {resp}"
    );
}

fn set_tier(client: &mut Client, tier: &str, provider: &str, fallback: Option<&str>) {
    let mut update = json!({
        "op": "set_tier_binding",
        "tier": tier,
        "provider_id": provider,
    });
    if let Some(fb) = fallback {
        update["fallback_id"] = json!(fb);
    }
    let resp = client.call("config/set", json!({ "update": update }));
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "binding the {tier} tier: {resp}"
    );
}

/// The `stage` names of every observed `model_lifecycle` event.
fn lifecycle_stages(client: &Client) -> Vec<String> {
    client
        .events_named("model_lifecycle")
        .iter()
        .filter_map(|e| e["stage"]["stage"].as_str().map(str::to_owned))
        .collect()
}

/// The concatenation of every streamed assistant-message chunk this session — the
/// model's visible answer text, used to prove a tool result reached the final
/// response (AC-9 execution).
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
