//! End-to-end verification for REQ-557: the model migration, the ADR-E startup
//! posture, and the BR-8 claim that none of this changed the egress boundary.
//!
//! These spawn the **real** `tetond` binary against a config file on disk,
//! because every claim here is about what happens at *load*: whether the daemon
//! starts at all, what it writes back, and what it tells the user. None of that
//! is observable from a unit test of `Config`.
//!
//! ## Why "the daemon starts" is an assertion and not an assumption
//!
//! `Config::load` validates internally, and `load_config` converts any load
//! error into "Refusing to start rather than fall back to an empty config that
//! would silently drop your privacy boundaries." That refusal is correct and
//! must stay. It is also why a missing `model` had to be a **usability**
//! condition rather than a validity one (ADR-E): as a validation error it would
//! reject every pre-REQ config — every provider `model: None` — and the daemon
//! would refuse to start *before the migration that fixes it could run*. The
//! startup tests below are the regression guard for exactly that, which is why
//! they are pinned separately from the migration test rather than folded into it.

use std::time::Duration;

use crate::harness::{
    assert_no_boundary_bytes, openai_turn, Client, Daemon, DaemonOptions, MockProvider,
    MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// A 16 GiB Apple-Silicon probe. No local script, so the local tier cannot
/// serve — every turn here is about the remote path.
fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// A provider entry **in the pre-REQ-557 shape**: no `model` key, because the
/// field did not exist when the config was written. This is the exact input the
/// migration has to accept.
fn legacy_provider_block(id: &str, kind: &str, endpoint: &str) -> String {
    format!("[[providers]]\nid = \"{id}\"\nkind = \"{kind}\"\nendpoint = \"{endpoint}\"\n\n")
}

fn routing_block(phase: &str, provider: &str) -> String {
    format!("[[routing]]\nphase = \"{phase}\"\nprovider_id = \"{provider}\"\n\n")
}

/// The `model` the config file declares for `id`, or `None` when it declares
/// none. Read from the file on disk, because the migration's whole job is to
/// **write** — an in-memory-only migration would re-run on every start.
fn model_in_config_file(ws: &Workspace, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(&ws.config_path).expect("config file");
    let table = text
        .split("[[providers]]")
        .find(|block| block.contains(&format!("id = \"{id}\"")))
        .unwrap_or_else(|| panic!("provider {id} missing from config:\n{text}"));
    table.lines().find_map(|line| {
        line.strip_prefix("model = ")
            .map(|v| v.trim().trim_matches('"').to_owned())
    })
}

// ===========================================================================
// AC-6 — migration: both legs, and a second start that re-runs nothing.
// ===========================================================================

/// A config in the pre-REQ shape with two providers — one the legacy price
/// lookup can resolve and one it cannot — loads, migrates the resolvable one,
/// reports the other by id, and leaves it unusable. A second start finds nothing
/// to do.
///
/// Both legs are in one test deliberately (AC-6): the interesting claim is that
/// they happen *together*, i.e. one unresolvable provider does not abort the
/// migration of its neighbour or the startup of the daemon.
#[test]
fn migration_resolves_what_it_can_reports_what_it_cannot_and_does_not_re_run() {
    let resolvable = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let unresolvable = MockProvider::always(openai_turn("Done.", None, 10, 5));

    let mut config = String::new();
    // `deepseek` IS in the bundled price table, so the legacy provider-id lookup
    // resolves it to `deepseek-chat` — the model it was implicitly being billed
    // as before this REQ, which is exactly what migration should preserve.
    config.push_str(&legacy_provider_block(
        "deepseek",
        "openai-compatible",
        &resolvable.openai_endpoint(),
    ));
    // `mystery` is in no price table, so the lookup yields nothing. Pre-REQ its
    // model silently became the string "mystery" (`billing_model`'s provider-id
    // fallback). The migration must NOT do that — BR-1: a fallback identifier is
    // not an answer.
    config.push_str(&legacy_provider_block(
        "mystery",
        "openai-compatible",
        &unresolvable.openai_endpoint(),
    ));
    config.push_str(&routing_block("implement", "deepseek"));
    config.push_str(&routing_block("review", "mystery"));

    let ws = Workspace::new("mig");
    ws.write_config(&config);

    // --- First start: the daemon comes up (it must, or nothing below runs) ---
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let log = daemon.log();

    // Leg 1 — resolvable: migrated to a declared model, written back to disk.
    assert_eq!(
        model_in_config_file(&ws, "deepseek").as_deref(),
        Some("deepseek-chat"),
        "the resolvable provider must be migrated to a declared model and \
         PERSISTED; an in-memory-only migration re-runs forever"
    );

    // Leg 2 — unresolvable: still no model, and never defaulted to its own id.
    assert_eq!(
        model_in_config_file(&ws, "mystery"),
        None,
        "an unresolvable provider must keep model: None — the pre-REQ behaviour \
         of falling back to the provider id is the defect BR-1 deletes"
    );
    assert!(
        log.contains("mystery"),
        "the unresolvable provider must be reported BY ID at startup, or it \
         silently stops working after upgrade. log:\n{log}"
    );
    assert!(
        log.contains("--model"),
        "the report must name the remedy, not just the problem. log:\n{log}"
    );

    // The projection agrees with the file: one provider has a model, one does not.
    let mut client = daemon.connect();
    let snapshot = client.config_get();
    let model_of = |id: &str| -> Option<String> {
        snapshot["providers"]
            .as_array()?
            .iter()
            .find(|p| p["id"].as_str() == Some(id))?["model"]
            .as_str()
            .map(str::to_owned)
    };
    assert_eq!(model_of("deepseek").as_deref(), Some("deepseek-chat"));
    assert_eq!(model_of("mystery"), None);

    let after_first_start = std::fs::read_to_string(&ws.config_path).expect("config file");
    drop(client);
    drop(daemon);

    // --- Second start: nothing left to migrate, so nothing happens ---
    let restarted = Daemon::spawn(&ws, probe_16gb());
    let second_log = restarted.log();

    assert_eq!(
        std::fs::read_to_string(&ws.config_path).expect("config file"),
        after_first_start,
        "a second start must not rewrite the config — the migration is keyed on \
         the ABSENCE of a model, so a migrated provider is invisible to it"
    );
    assert!(
        !second_log.contains("migrated"),
        "a second start must not report migrating anything. log:\n{second_log}"
    );
    // The unusable provider IS still reported: that condition has not gone away,
    // and a user who has not fixed it should keep hearing about it.
    assert!(
        second_log.contains("mystery"),
        "the unusable provider is still unusable, so it is still reported. \
         log:\n{second_log}"
    );
}

// ===========================================================================
// AC-3 — the model on the wire is the one the provider DECLARED.
// ===========================================================================

/// A turn's `route_decided` carries the provider's **declared** model, asserted
/// against a provider whose id appears **nowhere** in the price table.
///
/// This is the criterion that proves the value is declared rather than looked
/// up, and the provider id is chosen to make that the only possible explanation:
/// pre-REQ, `billing_model` searched the price table by provider id and fell
/// back to the id itself when it found nothing, so this same config would have
/// announced `model: "no-such-vendor"` — the provider's own id standing in for a
/// model it never called. It now announces `custom-model-v9`, which only the
/// config could have supplied.
///
/// The cost half rides along: the model is genuinely unpriced (it is in no
/// table), so the meter records the call unpriced and the report NAMES it
/// (AC-7b) — rather than pricing it at zero or attributing it to the id.
#[test]
fn route_decided_carries_the_declared_model_not_a_price_table_lookup() {
    let provider = MockProvider::always(openai_turn("Done.", None, 1000, 200));

    let mut config = String::new();
    config.push_str(&format!(
        "[[providers]]\nid = \"no-such-vendor\"\nkind = \"openai-compatible\"\n\
         endpoint = \"{}\"\nmodel = \"custom-model-v9\"\n\n",
        provider.openai_endpoint()
    ));
    config.push_str(&routing_block("implement", "no-such-vendor"));

    let ws = Workspace::new("declared");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let turn = client.prompt(&session, "implement the feature");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the turn should complete: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let decided = client
        .events_named("route_decided")
        .into_iter()
        .find(|e| e["provider_id"].as_str() == Some("no-such-vendor"))
        .expect("a route_decided naming the provider");
    assert_eq!(
        decided["model"].as_str(),
        Some("custom-model-v9"),
        "route_decided must carry the DECLARED model. Pre-REQ this said \
         \"no-such-vendor\" — the provider id standing in for a model, which is \
         the fallback-identifier defect BR-1 deletes: {decided}"
    );

    // The cost side agrees, and names what it could not price (AC-7 / AC-7b).
    let report = client.cost_query();
    assert_eq!(report["unpriced_calls"].as_u64(), Some(1), "{report}");
    let unpriced: Vec<&str> = report["unpriced_models"]
        .as_array()
        .expect("unpriced_models")
        .iter()
        .filter_map(|m| m.as_str())
        .collect();
    assert_eq!(
        unpriced,
        vec!["custom-model-v9"],
        "the report must name the unpriced MODEL, not the provider id: {report}"
    );
    assert_eq!(
        report["total_usd_micros"].as_i64(),
        Some(0),
        "an unpriced call contributes no dollars — and is counted as unpriced \
         rather than as a $0 call (BR-9): {report}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// ADR-E — the startup posture a missing model must NOT have.
// ===========================================================================

/// A config in the pure pre-REQ shape — **every** provider `model: None` —
/// starts the daemon.
///
/// Pinned separately from the migration test because it is the precondition
/// migration depends on, not a consequence of it: if this fails, migration never
/// gets a chance to run and the user is stranded with a daemon that will not
/// boot and no way to fix it from the CLI. This is the test that goes red if the
/// model requirement is ever moved into `Config::validate()`.
#[test]
fn a_pre_req_config_starts_the_daemon_at_all() {
    let provider = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let mut config = String::new();
    config.push_str(&legacy_provider_block(
        "nowhere-in-any-price-table",
        "openai-compatible",
        &provider.openai_endpoint(),
    ));

    let ws = Workspace::new("prereq");
    ws.write_config(&config);

    // `Daemon::spawn` panics with the daemon log if the process exits before
    // binding its socket, so reaching the next line IS the assertion.
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let log = daemon.log();
    assert!(
        !log.contains("Refusing to start"),
        "a config whose providers merely lack a model is incomplete, not \
         corrupt — refusing to start strands the user (ADR-E). log:\n{log}"
    );
    // And it says which provider is unusable, rather than failing silently.
    assert!(log.contains("nowhere-in-any-price-table"), "log:\n{log}");
}

/// With one provider usable and one not, the daemon starts, serves turns on the
/// usable one, and refuses turns routed to the unusable one — naming it.
///
/// "A daemon that refuses to start here is the regression this criterion exists
/// to catch" (TASK-047): BR-7 says the daemon starts *with that provider
/// unusable*, not that the config is invalid.
#[test]
fn one_unusable_provider_does_not_stop_the_others_from_serving() {
    let usable = MockProvider::always(openai_turn("Served by the usable one.", None, 100, 20));
    let broken = MockProvider::always(openai_turn("Should never be reached.", None, 10, 5));

    let mut config = String::new();
    // Declares its model: usable.
    config.push_str(&format!(
        "[[providers]]\nid = \"good\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n\
         model = \"deepseek-chat\"\n\n",
        usable.openai_endpoint()
    ));
    // Declares none and resolves to none: unusable (ADR-E).
    config.push_str(&legacy_provider_block(
        "broken",
        "openai-compatible",
        &broken.openai_endpoint(),
    ));
    config.push_str(&routing_block("implement", "good"));
    config.push_str(&routing_block("review", "broken"));

    let ws = Workspace::new("mixed");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    // The usable provider serves normally — the unusable neighbour is not
    // contagious.
    let ok_session = client.create_session("structured", Some("implement"));
    let ok = client.prompt(&ok_session, "implement the feature");
    assert_eq!(
        ok["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "a usable provider must keep working alongside an unusable one: {ok}"
    );
    assert!(
        usable.request_count() >= 1,
        "the turn must have gone remote"
    );

    // The unusable one is not routable, and the refusal names it and the remedy.
    let bad_session = client.create_session("structured", Some("review"));
    let bad = client.prompt(&bad_session, "review the change");
    assert!(
        bad.get("error").is_some(),
        "a turn routed to an unusable provider must fail, not silently succeed \
         somewhere else: {bad}"
    );
    let message = bad["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("broken"),
        "the refusal must NAME the unusable provider: {bad}"
    );
    assert!(
        message.contains("--model"),
        "the refusal must name the remedy: {bad}"
    );
    assert_eq!(
        broken.request_count(),
        0,
        "not one byte may reach a provider the router considers unusable"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-4 — a missing default is nameable, not a synthesized provider id.
// ===========================================================================

/// With a usable remote provider registered but no `default_provider`, a
/// freeform coding turn fails naming the missing default — it does not route to
/// whichever provider happened to be first in the array.
#[test]
fn no_default_provider_is_reported_not_invented() {
    let provider = MockProvider::always(openai_turn("Should never be reached.", None, 10, 5));
    let mut config = String::new();
    config.push_str(&format!(
        "[[providers]]\nid = \"only-one\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n\
         model = \"deepseek-chat\"\n\n",
        provider.openai_endpoint()
    ));

    let ws = Workspace::new("nodef");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    let turn = client.prompt(&session, "implement the greeting");

    assert!(
        turn.get("error").is_some(),
        "with no default configured the turn must fail rather than pick one: {turn}"
    );
    let message = turn["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("default_provider"),
        "the failure must name the missing default (AC-4): {turn}"
    );
    assert!(
        message.contains("only-one"),
        "and name what it could be set to, so the remedy is actionable: {turn}"
    );
    assert_eq!(
        provider.request_count(),
        0,
        "the sole registered provider must NOT become the default by virtue of \
         being the only one (OQ-3: no implicit default)"
    );

    // BUG-146's actual symptom, pinned directly: an unconfigured install
    // announcing a route to a provider registered nowhere. The error message
    // alone cannot catch this — `unserved_turn_error` classifies from the
    // config, which still correctly says no default is set, so it keeps saying
    // the right thing while the router hands out a fabricated id. Only the
    // emitted route shows it.
    client.drain_events(Duration::from_millis(300));
    let registered = ["only-one"];
    for event in client.events_named("route_decided") {
        let announced = event["provider_id"].as_str().unwrap_or_default();
        assert!(
            registered.contains(&announced),
            "route_decided announced provider {announced:?}, which is registered \
             nowhere. A default nobody configured must not be synthesized — not \
             from array position, and not from the literal \"local\" (BR-4, both \
             halves of the deleted chain): {event}"
        );
    }
}

// ===========================================================================
// BR-8 — the egress boundary is unchanged by this REQ.
// ===========================================================================

/// A `local-only` boundary still blocks, and a tainted session still pins to
/// local, with a `default_provider` configured and a declared model on every
/// provider — the configuration shape REQ-557 introduces.
///
/// BR-8 claims this REQ leaves egress enforcement alone. conventions.md is
/// explicit that such a claim needs mock-transport egress capture rather than
/// code inspection ("the coverage gap and the security gap are the same gap",
/// LESSON-432), so this asserts against what actually reached the wire.
///
/// It complements `privacy_fixes.rs`, which drives the same boundary machinery
/// through a `[[routing]]` policy. This one routes through the **default**,
/// which is the path REQ-557 rewrote.
#[test]
fn a_boundary_still_blocks_when_the_turn_routes_through_the_default() {
    // Turn 1 reads the boundary file via the jailed built-in; the next remote
    // turn carrying that content is blocked before a byte leaves.
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
    config.push_str("default_provider = \"deepseek\"\n\n");
    config.push_str(&format!(
        "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n\
         model = \"deepseek-chat\"\n\n",
        provider.openai_endpoint()
    ));
    config.push_str("[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n\n");

    let ws = Workspace::new("br8");
    ws.write_config(&config);
    // A local tier the blocked turn can reroute onto (REQ-544 M-1).
    let script = ws.write_script("Rerouted locally; done.\n---\nStill local; done.");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    // A **coding** turn, so the freeform heuristic routes it to the default
    // rather than to the local tier. The wording matters: an auxiliary signal
    // ("summarize", "describe", "explain" …) would send it local and the turn
    // would never approach the boundary at all — which is a green test that
    // proves nothing.
    let first = client.prompt(
        &session,
        "Rewrite the production configuration loader to read every key it needs.",
    );
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted-to-local turn should complete cleanly: {first}"
    );
    client.drain_events(Duration::from_millis(300));

    // The boundary fired, naming the provider the default resolved to.
    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one privacy_block, got {blocks:?}"
    );
    assert_eq!(blocks[0]["provider_id"].as_str(), Some("deepseek"));

    // The session is tainted, so the next turn stays local even though the
    // default says otherwise (REQ-544 C-2).
    // Also a coding turn — so the default would take it remote if the taint were
    // not pinning the session local. That is the point of the assertion below.
    let second = client.prompt(&session, "Now apply the same change to the other loader.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the tainted session's next turn should complete on local: {second}"
    );
    client.drain_events(Duration::from_millis(300));
    assert_eq!(
        remote_routes(&client, "deepseek"),
        1,
        "a tainted session must not route to the default remote again (C-2)"
    );

    // The claim itself: nothing from the boundary file reached the wire. Asserted
    // against captured payloads, not inferred from the code path.
    assert_no_boundary_bytes();
    let bodies = provider.requests();
    assert!(
        !bodies
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("sk-live-DO-NOT-LEAK")),
        "the boundary secret must not appear in any payload this provider received"
    );
}

fn remote_routes(client: &Client, provider: &str) -> usize {
    client
        .events_named("route_decided")
        .iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .count()
}

/// REGRESSION (BUG-155): a `default_provider` naming a registered-but-UNUSABLE
/// provider must not send that provider's **id** as the model.
///
/// `Config::validate` accepts this config — BR-6 only rejects a default naming an
/// *unregistered* id — so the daemon starts, correctly, with the provider
/// unusable (ADR-E). But `resolve_freeform`'s coding branch trusts
/// `default_provider` unconditionally, unlike `resolve_structured`, which routes
/// through `health_of` and so cannot select a provider missing from the router
/// map. The resulting `Route` carries `provider_id: Some("mystery")` with
/// `model: None`, and `run_one_attempt` then resolves the model as
/// `route.model.unwrap_or_else(|| provider_cfg.id.clone())` — the provider id
/// standing in for a model, on a real outbound call.
///
/// That is precisely the fallback BR-1 says is "deleted, not relocated".
#[test]
fn an_unusable_default_provider_never_sends_its_id_as_the_model() {
    let provider = MockProvider::always(openai_turn("Done.", None, 10, 5));

    let mut config = String::new();
    config.push_str("default_provider = \"mystery\"\n\n");
    // Registered (so `validate` passes) but declares no model, and the legacy
    // price lookup cannot resolve it either — so it stays unusable.
    config.push_str(&legacy_provider_block(
        "mystery",
        "openai-compatible",
        &provider.openai_endpoint(),
    ));

    let ws = Workspace::new("bug155");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    // A coding turn, so it resolves through the default rather than local.
    let turn = client.prompt(&session, "implement the greeting");

    // Whatever the turn does, the one thing it must NOT do is call out
    // announcing the provider id as a model.
    for body in provider.requests() {
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("\"model\":\"mystery\""),
            "BR-1 VIOLATION: the provider id was sent as the model on the wire. \
             A fallback identifier is not an answer (LESSON-456). Payload:\n{text}"
        );
    }
    // And the turn must not silently succeed against an unusable provider.
    assert!(
        turn.get("error").is_some(),
        "a turn routed to an unusable provider must fail, naming it: {turn}"
    );
}

/// BUG-155: `config/set register_provider` must refuse a remote provider with no
/// model, exactly as `teton provider add` does.
///
/// AC-2's guard lived only in the CLI, but `config/set` is a first-class protocol
/// surface — this suite's own `register_provider` helper drives it — so every
/// non-`teton` ACP client bypassed the check: the registration was persisted,
/// nothing was logged, and the next turn put the provider's id on the wire as its
/// model.
///
/// The rule belongs at *registration* rather than in `Config::validate`, and the
/// distinction is ADR-E's: loading a config that already contains a modelless
/// provider stays permissive so a pre-REQ config can boot far enough to migrate,
/// while registering a new one has no legacy to honour and fails closed.
#[test]
fn registering_a_remote_provider_over_rpc_requires_a_model() {
    let ws = Workspace::new("rpcreq");
    ws.write_config("# nothing registered yet\n");
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let refused = client.call(
        "config/set",
        serde_json::json!({ "update": {
            "op": "register_provider",
            "id": "gpu-box",
            "kind": "openai-compatible",
            "endpoint": "http://127.0.0.1:1/v1/chat/completions",
        }}),
    );
    assert!(
        refused.get("error").is_some(),
        "a modelless remote registration must be refused over RPC too: {refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("model"),
        "the refusal must name what is missing: {refused}"
    );

    // And nothing was persisted.
    let snapshot = client.config_get();
    let ids: Vec<&str> = snapshot["providers"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["id"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !ids.contains(&"gpu-box"),
        "a refused registration must not be persisted: {snapshot}"
    );

    // The same registration WITH a model is accepted.
    let accepted = client.call(
        "config/set",
        serde_json::json!({ "update": {
            "op": "register_provider",
            "id": "gpu-box",
            "kind": "openai-compatible",
            "endpoint": "http://127.0.0.1:1/v1/chat/completions",
            "model": "qwen2.5-coder-7b",
        }}),
    );
    assert_eq!(
        accepted["result"]["applied"].as_bool(),
        Some(true),
        "{accepted}"
    );
}

/// BUG-155: a policy `fallback_id` naming an unusable provider is not failed over
/// to.
///
/// The primary is screened by `evaluate` through `health_of`, which reports an
/// unregistered id as `Unavailable`. The fallback was read straight off the
/// policy with no such screen, so a mid-turn failure of a healthy primary could
/// egress the turn's whole accumulated context to a provider the daemon had
/// announced at startup as unable to serve turns.
#[test]
fn a_failure_does_not_fall_back_to_an_unusable_provider() {
    let flaky = MockProvider::always_bad();
    let unusable = MockProvider::always(openai_turn("Should never be reached.", None, 10, 5));

    let mut config = String::new();
    config.push_str(&format!(
        "[[providers]]\nid = \"flaky\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n\
         model = \"deepseek-chat\"\n\n",
        flaky.openai_endpoint()
    ));
    // Registered, remote, no model, and unresolvable by the legacy lookup.
    config.push_str(&legacy_provider_block(
        "unusable",
        "openai-compatible",
        &unusable.openai_endpoint(),
    ));
    config.push_str(
        "[[routing]]\nphase = \"implement\"\nprovider_id = \"flaky\"\n\
         fallback_id = \"unusable\"\n\n",
    );

    let ws = Workspace::new("fbunus");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("implement"));
    let _ = client.prompt(&session, "implement the feature");

    assert_eq!(
        unusable.request_count(),
        0,
        "not one byte may reach a provider the router considers unusable — not on \
         the primary path, and not by failing over to it"
    );
    for body in unusable.requests() {
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("\"model\":\"unusable\""), "{text}");
    }
}

/// BUG-155: the migration writes down the default the pre-REQ binary computed by
/// position, so an upgrade does not silently change where freeform turns go.
///
/// REQ-557 deleted the positional default but shipped no migration for it, so
/// every pre-REQ config arrived with `default_provider` unset. With a local tier
/// present that is silent rather than loud — the coding turn goes to the local
/// model and the session completes, so a user whose freeform turns went to a
/// remote provider yesterday gets a local answer today with nothing to explain it.
#[test]
fn migration_writes_down_the_default_the_config_was_already_using() {
    let first = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let second = MockProvider::always(openai_turn("Done.", None, 10, 5));

    let mut config = String::new();
    config.push_str(&legacy_provider_block(
        "deepseek",
        "openai-compatible",
        &first.openai_endpoint(),
    ));
    config.push_str(&legacy_provider_block(
        "moonshot",
        "openai-compatible",
        &second.openai_endpoint(),
    ));

    let ws = Workspace::new("defmig");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe_16gb());

    let written = std::fs::read_to_string(&ws.config_path).expect("config file");
    assert!(
        written.contains("default_provider = \"deepseek\""),
        "the pre-REQ positional default (the FIRST remote provider) must be \
         written down explicitly, not silently dropped:\n{written}"
    );
    assert!(
        daemon.log().contains("default_provider"),
        "and the user must be told it happened. log:\n{}",
        daemon.log()
    );

    // It is now an explicit key, so a freeform coding turn routes to it.
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);
    let turn = client.prompt(&session, "implement the greeting");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    assert!(
        first.request_count() >= 1,
        "the turn went to the migrated default"
    );
    assert_eq!(second.request_count(), 0, "and not to the other provider");
}

/// BUG-155: a config that is already post-REQ never has a `default_provider`
/// invented for it. The migration is a one-shot bridge for configs that predate
/// the key, not a standing rule — OQ-3's "no implicit default" holds for anyone
/// who was never migrated.
#[test]
fn a_post_req_config_is_never_given_a_default_it_did_not_ask_for() {
    let provider = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let mut config = String::new();
    // Every provider already declares a model: nothing to migrate.
    config.push_str(&format!(
        "[[providers]]\nid = \"deepseek\"\nkind = \"openai-compatible\"\nendpoint = \"{}\"\n\
         model = \"deepseek-chat\"\n\n",
        provider.openai_endpoint()
    ));

    let ws = Workspace::new("nodefmig");
    ws.write_config(&config);
    // Bound (not dropped) so the daemon stays up while the file is inspected.
    let _daemon = Daemon::spawn(&ws, probe_16gb());

    let written = std::fs::read_to_string(&ws.config_path).expect("config file");
    assert!(
        !written.contains("default_provider"),
        "a fully-migrated config must keep its deliberate absence of a default:\n{written}"
    );
}

/// BUG-155: a migration that cannot be saved leaves the user's existing config
/// **byte-for-byte intact**, and the daemon still starts.
///
/// This is the property the atomic write exists for. The old code called
/// `std::fs::write` straight at the target, which truncates on open — so a
/// failure part-way through left a truncated file. That is not fail-closed:
/// every `Config` field is `#[serde(default)]`, and `providers` serializes
/// before `boundaries`, so a truncated config is very likely to be valid TOML
/// carrying the user's remote providers and NONE of their `local-only`
/// boundaries. The daemon would start, report nothing, and route remotely with
/// boundary enforcement silently gone.
///
/// A read-only config *directory* is the cleanest way to make the save fail
/// while leaving the file itself readable: the atomic path cannot create its
/// sibling temp file and gives up without touching the original, whereas
/// writing directly to the target would have truncated a file it then could not
/// refill.
#[test]
fn a_migration_that_cannot_be_saved_leaves_the_config_untouched() {
    let provider = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let ws = Workspace::new("roconf");

    // A pre-REQ config (migratable) that also declares a privacy boundary — the
    // thing a truncating write would drop.
    let dir = ws.root.join("cfgdir");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    let original = format!(
        "{}[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n",
        legacy_provider_block("deepseek", "openai-compatible", &provider.openai_endpoint())
    );
    std::fs::write(&config_path, &original).unwrap();

    // Read-only directory: no new file can be created inside it.
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&dir, perms).unwrap();

    let daemon = Daemon::spawn(
        &ws,
        probe_16gb().env("TETON_CONFIG", config_path.to_string_lossy().to_string()),
    );

    // The daemon came up (reaching this line is the assertion) and said it could
    // not save.
    let log = daemon.log();
    assert!(
        log.contains("could not be saved"),
        "a failed migration save must be reported, not swallowed. log:\n{log}"
    );

    // The point: the config on disk is exactly what the user wrote.
    let after = std::fs::read_to_string(&config_path).expect("config still readable");
    assert_eq!(
        after, original,
        "a migration that could not be saved must leave the config byte-for-byte \
         intact — a truncating write would drop the privacy boundary below the \
         providers and the daemon would start with it silently gone"
    );
    assert!(
        after.contains("[[boundaries]]"),
        "the boundary in particular must survive:\n{after}"
    );

    // Restore permissions so the temp workspace can be cleaned up.
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&dir, perms).unwrap();
}
