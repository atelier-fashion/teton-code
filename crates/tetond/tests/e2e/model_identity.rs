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
