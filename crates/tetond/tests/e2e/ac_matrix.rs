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
    openai_turn, write_mcp_stdio_server, Client, Daemon, DaemonOptions, MockProvider, MockResponse,
    Workspace,
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

fn provider_block(id: &str, kind: &str, endpoint: &str) -> String {
    format!("[[providers]]\nid = \"{id}\"\nkind = \"{kind}\"\nendpoint = \"{endpoint}\"\n\n")
}

fn routing_block(phase: &str, provider: &str, fallback: Option<&str>) -> String {
    let mut s = format!("[[routing]]\nphase = \"{phase}\"\nprovider_id = \"{provider}\"\n");
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
    );
    register_provider(
        &mut client,
        "anthropic",
        "anthropic",
        &anthropic.anthropic_endpoint(),
    );
    set_routing(&mut client, "implement", "deepseek", None);
    set_routing(&mut client, "spec", "anthropic", None);

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

    // The OpenAI-compatible provider completes an (implement) session.
    let s1 = client.create_session("structured", Some("implement"));
    let r1 = client.prompt(&s1, "say hello");
    assert_eq!(result_stop_reason(&r1), Some("end_turn"), "{r1}");

    // The Anthropic provider completes a (spec) session.
    let s2 = client.create_session("structured", Some("spec"));
    let r2 = client.prompt(&s2, "say hello");
    assert_eq!(result_stop_reason(&r2), Some("end_turn"), "{r2}");

    // And a freeform session completes routed to a remote (the default remote).
    let s3 = client.create_session("freeform", None);
    let r3 = client.prompt(&s3, "implement the greeting");
    assert_eq!(result_stop_reason(&r3), Some("end_turn"), "{r3}");

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
    ));
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &cheap.openai_endpoint(),
    ));
    config.push_str(&routing_block("spec", "anthropic", None));
    config.push_str(&routing_block("architect", "anthropic", None));
    config.push_str(&routing_block("implement", "deepseek", None));
    config.push_str(&routing_block("review", "anthropic", None));

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
    ));
    config.push_str(&provider_block(
        "deepseek",
        "openai-compatible",
        &cheap.openai_endpoint(),
    ));
    config.push_str(&routing_block("spec", "anthropic", None));
    config.push_str(&routing_block("implement", "deepseek", None));

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
    //   spec  → anthropic/claude-opus-4  @ $15/$75 per Mtok:
    //           1000*15 + 200*75          = 15_000 + 15_000 = 30_000 µ$
    //   impl  → deepseek/deepseek-chat    @ $0.27/$1.10 per Mtok:
    //           1000*0.27 + 200*1.10      =     270 +    220 =    490 µ$
    // Baseline reprices BOTH priced calls' token volume (1000/200 each) at Opus:
    //           2 * 30_000                                    = 60_000 µ$
    //   savings = baseline - actual = 60_000 - 30_490         = 29_510 µ$
    const SPEC_MICROS: i64 = 30_000;
    const IMPLEMENT_MICROS: i64 = 490;
    const TOTAL_MICROS: i64 = SPEC_MICROS + IMPLEMENT_MICROS; // 30_490
    const BASELINE_MICROS: i64 = 60_000;
    const SAVINGS_MICROS: i64 = BASELINE_MICROS - TOTAL_MICROS; // 29_510

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
        Some("anthropic/claude-opus-4"),
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
    ));
    config.push_str(&routing_block("implement", "deepseek", None));
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

    // A fresh client can still attach to the surviving session.
    let mut c = daemon.connect();
    let attached = c.call("session/attach", json!({ "session_id": sid }));
    assert_eq!(
        attached["result"]["session"]["session_id"].as_str(),
        Some(sid.as_str()),
        "{attached}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-7 — a degraded provider triggers provider_degraded and the session
// completes via the fallback.
// ===========================================================================

#[test]
fn ac7_degraded_provider_falls_back_and_completes() {
    let flaky = MockProvider::always_bad();
    let healthy = MockProvider::always(openai_turn("Recovered and done.", None, 120, 20));

    let mut config = String::new();
    config.push_str(&provider_block(
        "flaky",
        "openai-compatible",
        &flaky.openai_endpoint(),
    ));
    config.push_str(&provider_block(
        "healthy",
        "openai-compatible",
        &healthy.openai_endpoint(),
    ));
    config.push_str(&routing_block("implement", "flaky", Some("healthy")));

    let ws = Workspace::new("ac7");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let resp = client.prompt(&session, "implement the feature");

    // The session completed rather than failing.
    assert_eq!(
        result_stop_reason(&resp),
        Some("end_turn"),
        "session did not recover via fallback: {resp}"
    );
    client.drain_events(Duration::from_millis(200));

    // provider_degraded named the failing provider and its fallback.
    let degraded = client.events_named("provider_degraded");
    assert!(
        degraded
            .iter()
            .any(|e| e["provider_id"].as_str() == Some("flaky")
                && e["fallback_id"].as_str() == Some("healthy")),
        "expected provider_degraded flaky -> healthy; saw {degraded:?}"
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
        config.push_str(&provider_block(
            "deepseek",
            "openai-compatible",
            &provider.openai_endpoint(),
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
    config.push_str(&provider_block("deepseek", "openai-compatible", &endpoint));
    config.push_str(&routing_block("implement", "deepseek", None));
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

fn register_provider(client: &mut Client, id: &str, kind: &str, endpoint: &str) {
    let resp = client.call(
        "config/set",
        json!({ "update": {
            "op": "register_provider",
            "id": id,
            "kind": kind,
            "endpoint": endpoint,
        }}),
    );
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "register {id}: {resp}"
    );
}

fn set_routing(client: &mut Client, phase: &str, provider: &str, fallback: Option<&str>) {
    let mut update = json!({
        "op": "set_routing_rule",
        "phase": phase,
        "provider_id": provider,
    });
    if let Some(fb) = fallback {
        update["fallback_id"] = json!(fb);
    }
    let resp = client.call("config/set", json!({ "update": update }));
    assert_eq!(
        resp["result"]["applied"].as_bool(),
        Some(true),
        "routing {phase}: {resp}"
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
