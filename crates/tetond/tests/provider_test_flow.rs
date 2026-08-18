//! REQ-581 acceptance: `provider/test` over the socket, against a **spawned
//! daemon** and a mock vendor that can be told what to answer (TASK-167).
//!
//! ## Why this suite exists beside the ones that already cover the method
//!
//! Three instruments are already pointed at REQ-581, and each is blind to what
//! is asserted here:
//!
//! * `runtime::tests::provider_test` drives [`DaemonRuntime::provider_test`]
//!   in-process against a scripted `Transport`. It owns the mapping table and
//!   the health movement, and it never builds an HTTP request, never resolves
//!   an `auth_ref` out of a real process environment, and never crosses a wire.
//! * `server.rs`'s own unit tests drive the dispatch gates against a bare
//!   `Daemon::new()` — no config, so no provider to reach and no vendor to
//!   answer.
//! * `teton`'s `provider_test_ui` suite renders the report against a fake RPC.
//!
//! What is left, and what this file is, is the **whole chain on the real
//! binary**: a config file the daemon loaded, a credential the daemon resolved
//! from its own environment, the shipped OpenAI-compatible adapter, the real
//! HTTP transport, the real egress choke point and the real ledger, in front of
//! a server that records what it was sent and answers with a status the test
//! chose. Every claim below is read off the socket or off that server's request
//! count — never out of the daemon's log.
//!
//! ## AC → test map
//!
//! | AC / BR | Test |
//! |---------|------|
//! | AC-1 (reached: latency, tokens, recorded cost; one probe row in `teton cost`), AC-5 (health, and the next turn routes there), BR-1, BR-4, BR-5 | [`a_reached_test_reports_what_came_back_bills_one_probe_and_leaves_the_provider_routable`] |
//! | AC-2 (401 names the credential *reference*, never its value; no ledger row), AC-3 (404 / 429 / 5xx / closed port are four distinct typed outcomes), BR-3 | [`every_way_the_call_can_fail_is_its_own_typed_outcome_and_bills_nothing`] |
//! | verify F1 (a 2xx/3xx that completed nothing is `not_a_completion`, never a green `reached`) | [`an_endpoint_that_answers_without_completing_is_its_own_outcome_not_reached`] |
//! | AC-6 (a connection that did not attach the session is refused and nothing is dialed) | [`a_connection_that_did_not_attach_the_session_is_refused_before_anything_is_dialed`] |
//! | AC-7 (a `kind = "local"` provider is refused with the tier's state, not tested), BR-8 | [`a_local_provider_is_refused_with_its_own_state_and_nothing_is_dialed`] |
//!
//! ### Not asserted here, and where each one is
//!
//! * **AC-4** (the `[y/N]` preview, and the piped `--yes` degradation) — a claim
//!   about the *client* binary's stdin and its confirm seam, which this side of
//!   the wire cannot observe. It is `provider_test_ui`'s own suite.
//! * **AC-8a/AC-8b** (the guide sentence and the surface nudge) — the contract
//!   test and `session_ui`'s render seam.
//!
//! ## Every outcome is asserted on the **typed tag**, never on prose
//!
//! LESSON-456: the daemon classifies once and a surface branches on the
//! variant. A test that told 404 from 429 by grepping a sentence would pass on
//! an implementation that had collapsed both into one variant with two
//! wordings, which is exactly the bug the typed enum exists to prevent. The one
//! place a `reason` is read is AC-2's, and there it is the *subject* of the
//! claim: the sentence names the credential reference and not the key.
//!
//! ## The planted key is distinctive on purpose (LESSON-519)
//!
//! [`PROBE_KEY_VALUE`] is a string that exists nowhere else in this repository,
//! so "the key was never printed" is a search that could genuinely fail. And it
//! is non-vacuous in the other direction too: a key that had not resolved would
//! have been [`error_code::CONFIG_REJECTED`] with **no** request made, so the
//! `refused` outcome and the mock's request count together prove the daemon
//! really did resolve this value and send it.
//!
//! ## Falsification (LESSON-479)
//!
//! Both refusals are paired, on the **same daemon and the same mock**, with an
//! observation of a call that does land: the foreign-connection test hands the
//! same provider to the session's own connection afterwards, and the local-kind
//! test tests the remote provider beside it. A refusal whose "nothing was
//! dialed" could never have been anything else measures nothing.

use std::net::TcpListener;
use std::time::Duration;

use serde_json::{json, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{
    openai_turn, tier_block, Client, Daemon, DaemonOptions, MockProvider, MockResponse, Workspace,
};

const INVALID_PARAMS: i64 = teton_protocol::jsonrpc::error_code::INVALID_PARAMS;
const NOT_ATTACHED: i64 = teton_protocol::jsonrpc::error_code::NOT_ATTACHED;

/// How long an event is waited for once the response that caused it is already
/// in hand. The publish happens *before* the return, so this is slack for the
/// reader thread rather than a real wait.
const EVENT_WINDOW: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// The credential, planted so that "never printed" is a falsifiable claim
// ---------------------------------------------------------------------------

/// The environment variable every fixture provider's `auth_ref` names.
const PROBE_KEY_ENV: &str = "TETON_REQ581_PROBE_KEY";

/// The reference the config carries — the string a `refused` reason is required
/// to name (AC-2), and the only half of the credential that may ever be said out
/// loud.
const PROBE_AUTH_REF: &str = "env:TETON_REQ581_PROBE_KEY";

/// The credential **value** the daemon resolves that reference to.
///
/// Deliberately unlike any other literal in this repository so the
/// "never printed" assertions below are a search that could fail. It is planted
/// only in the spawned daemon's own environment — never in the config file, so
/// a leak found in the response, an event or the log came from the code path
/// under test rather than from the fixture being echoed back.
const PROBE_KEY_VALUE: &str = "sk-PROBETESTKEY-9f4c2ae17b3d5e60";

/// A model the bundled price table knows, so `reached` can report a **recorded**
/// cost rather than the unpriced `None` (BR-5). Which model does not matter to
/// the mock — it answers whatever it is asked — only that the ledger can price
/// it, and that the same figure comes back two ways.
const PRICED_MODEL: &str = "deepseek-v4-pro";

/// The usage the mock reports. Distinctive numbers, so a token assertion cannot
/// pass against a zero, a default, or the other field.
const PROBE_INPUT_TOKENS: u64 = 1200;
const PROBE_OUTPUT_TOKENS: u64 = 24;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The deterministic machine every daemon here is spawned onto — plus the
/// credential its providers' `auth_ref` resolves against.
///
/// The hardware override is [`provider_setup_flow`]'s: a probe of the *real*
/// host could pick a local model and spend the test's budget loading it.
fn probe_machine() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500u64 << 30).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
        .env(PROBE_KEY_ENV, PROBE_KEY_VALUE)
}

/// The scripted local engine every daemon here gets.
///
/// No test in this suite wants a *local* turn — the one turn that runs
/// (`a_reached_test_…`) is the assertion that it went **remote**. The script is
/// here for what it buys at startup: a scripted local tier downloads nothing,
/// so the first-run consent gate is exempt and no proposal stands between a
/// client and the session it just opened.
const NO_LOCAL_TURNS: &str = "unused — no test in this suite wants a local turn.";

/// A `[[providers]]` row for a remote provider whose credential is
/// [`PROBE_AUTH_REF`].
///
/// The harness's own `remote_provider_block` declares no `auth_ref`, and this
/// suite's whole subject is a call that carries a credential: AC-2 is a claim
/// about the sentence a *refusal* composes from the reference, which a provider
/// with no reference cannot make.
fn provider_block(id: &str, endpoint: &str, model: &str) -> String {
    format!(
        "[[providers]]\nid = \"{id}\"\nkind = \"openai-compatible\"\nendpoint = \"{endpoint}\"\n\
         model = \"{model}\"\nauth_ref = \"{PROBE_AUTH_REF}\"\n\n"
    )
}

/// A TCP port with nothing listening on it: bound to learn a free number, then
/// released. The honest stand-in for "the vendor cannot be reached" — a refused
/// connection, not a slow one. (`consent_matrix.rs`'s pattern.)
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to find a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// An OpenAI-compatible endpoint on a port nothing answers.
fn dark_endpoint() -> String {
    format!("http://127.0.0.1:{}/v1/chat/completions", closed_port())
}

/// `provider/test` for `session`.
fn test_provider(client: &mut Client, session: &str, provider_id: &str) -> Value {
    client.call(
        "provider/test",
        json!({ "session_id": session, "provider_id": provider_id }),
    )
}

/// The typed outcome tag of a `provider/test` answer, or a failure that shows
/// the whole response.
///
/// Every case below branches on **this** and never on a sentence (BR-3).
fn outcome_tag(response: &Value) -> &str {
    response["result"]["outcome"]["outcome"]
        .as_str()
        .unwrap_or_else(|| panic!("`provider/test` must answer a typed outcome: {response}"))
}

/// The error code a refused `provider/test` answered with.
fn error_code_of(response: &Value) -> i64 {
    response["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected a refusal, got: {response}"))
}

/// The error message a refused `provider/test` answered with.
fn error_message_of(response: &Value) -> &str {
    response["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal must carry a message: {response}"))
}

/// Every `provider_tested` event this client observed, as a fresh vector.
fn tested_events(client: &Client) -> Vec<Value> {
    client
        .events_named("provider_tested")
        .into_iter()
        .cloned()
        .collect()
}

/// Assert that neither the response, nor any event this client saw, nor
/// anything the daemon wrote to stderr, contains the planted credential
/// (AC-2's "never the key value", LESSON-519).
///
/// The log is included deliberately: a value that reached a `tracing` field
/// would be on a user's disk, which is the same leak as one on the wire.
fn assert_key_never_said(response: &Value, client: &Client, daemon: &Daemon, where_: &str) {
    let response = response.to_string();
    assert!(
        !response.contains(PROBE_KEY_VALUE),
        "{where_}: the credential VALUE reached the RPC answer: {response}"
    );
    for event in client.events() {
        let rendered = event.to_string();
        assert!(
            !rendered.contains(PROBE_KEY_VALUE),
            "{where_}: the credential VALUE reached an event payload: {rendered}"
        );
    }
    let log = daemon.log();
    assert!(
        !log.contains(PROBE_KEY_VALUE),
        "{where_}: the credential VALUE reached the daemon's own stderr:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// (a) AC-1 / AC-5 — the call lands, is billed as one probe, and moves routing
// ---------------------------------------------------------------------------

/// **AC-1 and AC-5, over the socket.** One consented call reaches the vendor;
/// the answer carries latency, the token counts the vendor billed and the cost
/// the ledger actually recorded; `teton cost` counts it as exactly one probe;
/// the provider is left `healthy`; and the very next `edit`-class turn of the
/// same session routes to it.
///
/// The legs, in order, each a precondition of the next:
///
/// 1. the mock received exactly **one** request, and it is the minimal shape
///    BR-1 promises: the configured model, one user message, no tools;
/// 2. the outcome is the typed `reached`, with the mock's own usage — not a
///    zero, not a default;
/// 3. exactly one `provider_tested`, on this connection, scoped to **this**
///    session, carrying the same tag and the same health;
/// 4. the ledger holds exactly one call, counted as one **probe**, in one row
///    for this provider, and the `usd_micros` the report and the ledger give
///    are the same figure read two ways (they are priced by one call to one
///    table, and a test that let them disagree would be checking neither);
/// 5. AC-5's end-to-end half: a `session/prompt` on an `implement` phase — an
///    `edit`-class turn — publishes a `route_decided` naming this provider, so
///    the health this test left behind is the health the router read.
#[test]
fn a_reached_test_reports_what_came_back_bills_one_probe_and_leaves_the_provider_routable() {
    // One body for every request: the probe gets it, and so does the turn in
    // leg (5) — which is what lets a single mock serve both.
    let vendor = MockProvider::always(openai_turn(
        "OK",
        None,
        PROBE_INPUT_TOKENS,
        PROBE_OUTPUT_TOKENS,
    ));

    let mut config = provider_block("probe", &vendor.openai_endpoint(), PRICED_MODEL);
    // `implement` maps to the `edit` category, which inherits the `build` tier.
    config.push_str(&tier_block("build", "probe"));

    let ws = Workspace::new("provider-test-reached");
    ws.write_config(&config);
    let script = ws.write_script(NO_LOCAL_TURNS);
    let daemon = Daemon::spawn(&ws, probe_machine().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("structured", Some("implement"));

    assert_eq!(
        vendor.request_count(),
        0,
        "nothing may have been dialed before the test asked for it"
    );

    let response = test_provider(&mut client, &session, "probe");

    // (1) One request, and the minimal shape BR-1 promises.
    assert_eq!(
        vendor.request_count(),
        1,
        "BR-1: a connection test is exactly one call — not zero (a shortcut) and \
         not several (a retry): {:?}",
        vendor.requests()
    );
    let sent: Value = serde_json::from_slice(&vendor.requests()[0])
        .expect("the probe posts a JSON body the adapter built");
    assert_eq!(
        sent["model"].as_str(),
        Some(PRICED_MODEL),
        "the probe asks for the model the config declares: {sent}"
    );
    assert_eq!(
        sent["messages"].as_array().map(Vec::len),
        Some(1),
        "BR-1: one fixed user message, no system prompt and no conversation: {sent}"
    );
    assert_eq!(sent["messages"][0]["role"].as_str(), Some("user"), "{sent}");
    assert!(
        sent.get("tools")
            .is_none_or(|t| t.as_array() == Some(&vec![])),
        "BR-1: a probe offers no tools: {sent}"
    );

    // (2) The typed outcome, with the vendor's own usage.
    assert_eq!(
        outcome_tag(&response),
        "reached",
        "daemon log:\n{}",
        daemon.log()
    );
    let outcome = &response["result"]["outcome"];
    assert!(
        outcome["latency_ms"].is_u64(),
        "a reached test reports what the user waited: {response}"
    );
    assert_eq!(
        outcome["input_tokens"].as_u64(),
        Some(PROBE_INPUT_TOKENS),
        "the tokens are the vendor's, read off the completed stream: {response}"
    );
    assert_eq!(
        outcome["output_tokens"].as_u64(),
        Some(PROBE_OUTPUT_TOKENS),
        "{response}"
    );
    let reported_micros = outcome["usd_micros"].as_i64().unwrap_or_else(|| {
        panic!("`{PRICED_MODEL}` is priced, so a reached test reports what it cost: {response}")
    });
    assert!(
        reported_micros > 0,
        "a priced call costs something: {response}"
    );
    assert_eq!(response["result"]["provider_id"].as_str(), Some("probe"));
    assert_eq!(response["result"]["model"].as_str(), Some(PRICED_MODEL));
    assert_eq!(
        response["result"]["dial_host"].as_str().map(|h| h
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned()),
        Some("127.0.0.1".to_owned()),
        "the report names the host the request was actually dialed at: {response}"
    );
    assert_eq!(
        response["result"]["health_after"].as_str(),
        Some("healthy"),
        "BR-4: a served call clears any downgrade: {response}"
    );

    // (3) Exactly one announcement, scoped to the session that asked.
    client.drain_events(EVENT_WINDOW);
    let announced = tested_events(&client);
    assert_eq!(
        announced.len(),
        1,
        "one test announces exactly one `provider_tested`: {announced:?}"
    );
    let event = &announced[0];
    assert_eq!(
        event["session_id"].as_str(),
        Some(session.as_str()),
        "the announcement belongs to the session that asked: {event}"
    );
    assert_eq!(event["provider_id"].as_str(), Some("probe"), "{event}");
    assert_eq!(
        event["outcome"]["outcome"].as_str(),
        Some("reached"),
        "the event carries the same typed outcome the answer did: {event}"
    );
    assert_eq!(event["health_after"].as_str(), Some("healthy"), "{event}");

    // (4) One call in the ledger, counted as one probe, in one row for `probe`.
    let report = client.cost_query();
    assert_eq!(
        report["total_calls"].as_u64(),
        Some(1),
        "BR-5: the probe IS a model call and the ledger holds it: {report}"
    );
    assert_eq!(
        report["probe_calls"].as_u64(),
        Some(1),
        "BR-5: and it is counted apart, so `teton cost` can say `1 probe`: {report}"
    );
    let per_provider = report["per_provider"]
        .as_array()
        .unwrap_or_else(|| panic!("the report rolls up per provider: {report}"));
    assert_eq!(
        per_provider.len(),
        1,
        "one call reaches one provider: {report}"
    );
    let row = &per_provider[0];
    assert_eq!(row["key"].as_str(), Some("probe"), "{report}");
    assert_eq!(row["calls"].as_u64(), Some(1), "{report}");
    assert_eq!(
        row["input_tokens"].as_u64(),
        Some(PROBE_INPUT_TOKENS),
        "{report}"
    );
    assert_eq!(
        row["output_tokens"].as_u64(),
        Some(PROBE_OUTPUT_TOKENS),
        "{report}"
    );
    assert_eq!(
        row["usd_micros"].as_i64(),
        Some(reported_micros),
        "the figure the report gave the user and the figure the ledger kept are \
         one number priced once: {report}"
    );

    // (5) AC-5's end-to-end half: the next `edit`-class turn routes here.
    let turn = client.prompt(&session, "Change the constant in src/lib.rs to 2.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the follow-up turn must complete, or its route proves nothing: {turn}\n\
         daemon log:\n{}",
        daemon.log()
    );
    client.drain_events(EVENT_WINDOW);
    let routed: Vec<&str> = client
        .events_named("route_decided")
        .iter()
        .filter_map(|e| e["provider_id"].as_str())
        .collect();
    assert!(
        routed.contains(&"probe"),
        "AC-5: the turn after a `reached` test must route to the provider the \
         test left healthy; routes seen: {routed:?}"
    );
}

// ---------------------------------------------------------------------------
// (b)–(f) AC-2 / AC-3 — five ways to fail, five typed outcomes, no ledger row
// ---------------------------------------------------------------------------

/// **AC-2 and AC-3, over the socket, on one daemon.** A 401, a 404, a 429, a
/// 503 and a closed port are five *distinct typed outcomes* — not five wordings
/// of one — and none of the five writes a ledger row.
///
/// One daemon and five providers rather than five daemons, because the claim is
/// about the classifier telling them apart: five separate fixtures could each
/// pass while the daemon answered the same variant to all of them, and the
/// distinctness is the criterion.
///
/// The 401 leg carries AC-2 in full: the reason names the credential
/// **reference** the request authenticated with, never its value, and — the
/// half that is easy to lose — the call really did carry a resolved credential,
/// which the mock's request count is the evidence for. A reference that had not
/// resolved would have been `CONFIG_REJECTED` with nothing sent at all.
#[test]
fn every_way_the_call_can_fail_is_its_own_typed_outcome_and_bills_nothing() {
    // The 401's body **echoes the planted key back** (LESSON-519). A vendor
    // error body routinely quotes the credential it rejected, and ADR-3's whole
    // claim is that the daemon composes its own sentence from its own facts
    // rather than forwarding that prose. Against a mock whose body never
    // contained the key, `assert_key_never_said` below would be asserting about
    // a string that was never on this side of the wire — true, and about
    // nothing.
    let refuser = MockProvider::start(
        Vec::new(),
        MockResponse::status_with_body(
            401,
            format!("{{\"error\":\"bad key: {PROBE_KEY_VALUE}\"}}"),
        ),
    );
    let unknown = MockProvider::always_status(404);
    let limiter = MockProvider::always_status(429);
    let failer = MockProvider::always_status(503);
    // No mock at all: the dark provider's endpoint is a port nothing answers.

    /// The model the 404 leg's reason has to name — distinctive, so "the report
    /// says which string needs fixing" is falsifiable.
    const MISSING_MODEL: &str = "no-such-model-v9";

    let mut config = String::new();
    config.push_str(&provider_block(
        "refuser",
        &refuser.openai_endpoint(),
        PRICED_MODEL,
    ));
    config.push_str(&provider_block(
        "unknown",
        &unknown.openai_endpoint(),
        MISSING_MODEL,
    ));
    config.push_str(&provider_block(
        "limiter",
        &limiter.openai_endpoint(),
        PRICED_MODEL,
    ));
    config.push_str(&provider_block(
        "failer",
        &failer.openai_endpoint(),
        PRICED_MODEL,
    ));
    config.push_str(&provider_block("dark", &dark_endpoint(), PRICED_MODEL));

    let ws = Workspace::new("provider-test-failures");
    ws.write_config(&config);
    let script = ws.write_script(NO_LOCAL_TURNS);
    let daemon = Daemon::spawn(&ws, probe_machine().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let before = client.cost_query();
    assert_eq!(
        before["total_calls"].as_u64(),
        Some(0),
        "the ledger must start empty, or 'no row was written' is unfalsifiable: {before}"
    );

    // --- (b) 401: refused, naming the reference and not the key (AC-2) ------
    let refused = test_provider(&mut client, &session, "refuser");
    assert_eq!(
        outcome_tag(&refused),
        "refused",
        "daemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        refused["result"]["outcome"]["status"].as_u64(),
        Some(401),
        "the refusal carries the status the vendor answered with: {refused}"
    );
    assert!(
        refused["result"]["health_after"].as_str().is_some(),
        "every outcome says where it left routing health: {refused}"
    );
    let reason = refused["result"]["outcome"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal carries the daemon's sentence: {refused}"));
    assert!(
        reason.contains(PROBE_AUTH_REF),
        "AC-2: the reason must name the credential REFERENCE the request \
         authenticated with, which is the actionable half of a 401: {reason:?}"
    );
    assert_eq!(
        refuser.request_count(),
        1,
        "the credential really resolved and really was sent — an unresolvable \
         reference is `CONFIG_REJECTED` with nothing dialed, which would make \
         the assertion above vacuous"
    );

    // --- (c) 404: unknown_model, naming the model the config declares ------
    let unknown_model = test_provider(&mut client, &session, "unknown");
    assert_eq!(
        outcome_tag(&unknown_model),
        "unknown_model",
        "AC-3: a 404 is its own outcome — the endpoint answered, so the missing \
         thing is the model. daemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        unknown_model["result"]["outcome"]["status"].as_u64(),
        Some(404),
        "{unknown_model}"
    );
    let unknown_reason = unknown_model["result"]["outcome"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("{unknown_model}"));
    assert!(
        unknown_reason.contains(MISSING_MODEL),
        "AC-3: the report names the model string that needs fixing: {unknown_reason:?}"
    );
    assert_eq!(unknown.request_count(), 1);

    // --- (d) 429: rate_limited --------------------------------------------
    let limited = test_provider(&mut client, &session, "limiter");
    assert_eq!(
        outcome_tag(&limited),
        "rate_limited",
        "AC-3: a 429 is not a refusal — the vendor is up and holding the call \
         off. daemon log:\n{}",
        daemon.log()
    );
    assert!(
        limited["result"]["outcome"]["retry_after_secs"].is_null(),
        "ADR-2 / OQ-5: `Retry-After` is deferred, not carried — the transport \
         surfaces exactly one named header and a probe does not earn a second: \
         {limited}"
    );
    assert_eq!(limiter.request_count(), 1);

    // --- (e) 503: server_error --------------------------------------------
    let server_error = test_provider(&mut client, &session, "failer");
    assert_eq!(
        outcome_tag(&server_error),
        "server_error",
        "AC-3: a 5xx is its own outcome — the vendor answered and is failing, \
         and the configuration is not the suspect. daemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        server_error["result"]["outcome"]["status"].as_u64(),
        Some(503),
        "{server_error}"
    );
    assert_eq!(failer.request_count(), 1);

    // --- (f) closed port: unreachable -------------------------------------
    let unreachable = test_provider(&mut client, &session, "dark");
    assert_eq!(
        outcome_tag(&unreachable),
        "unreachable",
        "AC-3: nothing answered at all. daemon log:\n{}",
        daemon.log()
    );
    assert!(
        unreachable["result"]["outcome"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("127.0.0.1")),
        "the sentence names the host that did not answer: {unreachable}"
    );

    // --- The five are five, not one wearing five sentences ----------------
    let tags: Vec<&str> = vec![
        outcome_tag(&refused),
        outcome_tag(&unknown_model),
        outcome_tag(&limited),
        outcome_tag(&server_error),
        outcome_tag(&unreachable),
    ];
    let mut distinct = tags.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        tags.len(),
        "AC-3: each failure must be a DISTINCT typed outcome, not distinguishing \
         prose on one variant: {tags:?}"
    );

    // --- AC-2's other half: five failed calls, and no ledger row ----------
    let after = client.cost_query();
    assert_eq!(
        after["total_calls"].as_u64(),
        Some(0),
        "AC-2: a call the vendor would not serve bills nothing: {after}"
    );
    assert_eq!(after["probe_calls"].as_u64(), Some(0), "{after}");
    assert_eq!(
        after["per_provider"].as_array().map(Vec::len),
        Some(0),
        "no provider earned a row from a failed test: {after}"
    );
    assert_eq!(
        after["total_usd_micros"].as_i64(),
        Some(0),
        "nothing was priced: {after}"
    );

    // --- And the key was never said, anywhere -----------------------------
    client.drain_events(EVENT_WINDOW);
    assert_eq!(
        tested_events(&client).len(),
        5,
        "every outcome is announced, including the ones that failed (LESSON-505)"
    );
    for response in [
        &refused,
        &unknown_model,
        &limited,
        &server_error,
        &unreachable,
    ] {
        assert_key_never_said(response, &client, &daemon, "a failed connection test");
    }
}

// ---------------------------------------------------------------------------
// (f2) verify F1 — an endpoint that answers without completing is not a success
// ---------------------------------------------------------------------------

/// **Verify F1, over the socket.** A `200` that is not a completion stream, and
/// a `301` that is not followed, are both `not_a_completion` — never `reached`,
/// and never the `unreachable` a host that answered nothing at all earns.
///
/// This is the connection test's worst possible failure and the reason it is
/// worth a leg of its own: a *green* answer for an endpoint no turn can use.
/// The shipped adapters raise a `ProviderError` only at status >= 400, and the
/// SSE reader synthesizes a terminal completion even for a body that carried no
/// `data:` lines — so an endpoint pasted without its `/v1/chat/completions`
/// path (which is what both of these mocks stand in for: a `/models`-shaped JSON
/// answer, and a redirect the daemon deliberately does not follow) used to come
/// back as `reached { 0 in / 0 out }` and leave the provider stamped healthy.
///
/// Both legs are asserted on the typed tag, and the pair is asserted *identical*
/// on purpose: these are two routes to one fact — something is listening, and it
/// is not a chat endpoint — so they earn the same variant rather than two. That
/// variant is its own, `not_a_completion`, and not the `unreachable` the closed
/// port earns in
/// [`every_way_the_call_can_fail_is_its_own_typed_outcome_and_bills_nothing`]:
/// "the address is wrong" and "the path is wrong" are different next moves for
/// the reader, so they are different values rather than one value with two
/// sentences (BR-3, LESSON-456).
///
/// The ledger assertion is the deliberate, honest half. Each of these answers
/// carries a status < 400 whose body the daemon polled, which is the cost
/// meter's whole definition of a billable call, so each writes a zero-token
/// probe row. The rows are not suppressed: a request really was made, and a
/// vendor really may charge for one — a ledger that hid it would be lying about
/// egress that happened. The row and the outcome disagree about *usefulness*,
/// not about facts.
#[test]
fn an_endpoint_that_answers_without_completing_is_its_own_outcome_not_reached() {
    let not_a_stream = MockProvider::start(
        Vec::new(),
        MockResponse::status_with_body(200, r#"{"object":"list","data":[]}"#),
    );
    let redirector = MockProvider::always_status(301);

    let mut config = provider_block("notastream", &not_a_stream.openai_endpoint(), PRICED_MODEL);
    config.push_str(&provider_block(
        "redirector",
        &redirector.openai_endpoint(),
        PRICED_MODEL,
    ));

    let ws = Workspace::new("provider-test-noncompletion");
    ws.write_config(&config);
    let script = ws.write_script(NO_LOCAL_TURNS);
    let daemon = Daemon::spawn(&ws, probe_machine().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let answered = test_provider(&mut client, &session, "notastream");
    assert_eq!(
        outcome_tag(&answered),
        "not_a_completion",
        "a 200 carrying ordinary JSON completed nothing — reporting it as \
         `reached` would hand the user a green light for an endpoint no turn can \
         use, and reporting it as `unreachable` would send them to check an \
         address that resolved. daemon log:\n{}",
        daemon.log()
    );
    assert!(
        answered["result"]["outcome"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("127.0.0.1")),
        "the sentence names the endpoint that answered wrongly: {answered}"
    );
    assert_eq!(
        not_a_stream.request_count(),
        1,
        "non-vacuity: the call really was made, so this is a verdict about an \
         answer rather than about a dial that never happened"
    );

    let redirected = test_provider(&mut client, &session, "redirector");
    assert_eq!(
        outcome_tag(&redirected),
        "not_a_completion",
        "redirects are deliberately not followed (a credential header must not \
         be carried to a host the vendor chose), so a 301 asks the chat endpoint \
         nothing at all. daemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        outcome_tag(&redirected),
        outcome_tag(&answered),
        "two routes to one fact — something is listening and it is not a chat \
         endpoint — earn one variant, not two"
    );
    assert_eq!(redirector.request_count(), 1);

    // Neither provider may be left looking healthy *because* of this call: the
    // outcome carries a health reading, and it must not be a clearance earned by
    // an answer that served nothing.
    for response in [&answered, &redirected] {
        assert!(
            response["result"]["health_after"].as_str().is_some(),
            "every outcome says where it left routing health: {response}"
        );
    }

    // The rows: written, counted, and zero — see the doc comment.
    let report = client.cost_query();
    assert_eq!(
        report["probe_calls"].as_u64(),
        Some(2),
        "a status < 400 whose body was polled is a billable call, and the ledger \
         records it rather than pretending the request never left: {report}"
    );
    let per_provider = report["per_provider"]
        .as_array()
        .unwrap_or_else(|| panic!("the report rolls up per provider: {report}"));
    assert_eq!(per_provider.len(), 2, "{report}");
    for row in per_provider {
        assert_eq!(row["calls"].as_u64(), Some(1), "{report}");
        assert_eq!(
            (row["input_tokens"].as_u64(), row["output_tokens"].as_u64()),
            (Some(0), Some(0)),
            "and it records what it actually saw — no tokens — rather than a \
             completion it never received: {report}"
        );
    }

    client.drain_events(EVENT_WINDOW);
    assert_eq!(
        tested_events(&client).len(),
        2,
        "both outcomes are announced (LESSON-505): {:?}",
        tested_events(&client)
    );
}

// ---------------------------------------------------------------------------
// (g) AC-6 — a foreign connection is refused, and nothing is dialed
// ---------------------------------------------------------------------------

/// **AC-6.** A second connection that never attached the session is refused
/// `NOT_ATTACHED` in the response, and the vendor sees nothing.
///
/// The gate is a *spending* boundary, not only an access one (ADR-5): what it
/// buys is that a same-UID peer, or a `teton provider test … --yes` the model
/// spawned through the `shell` tool, cannot make the user's provider bill them.
/// So the assertion that matters is the mock's request count, not the code.
///
/// Falsification: the same provider, on the same daemon and the same mock, is
/// then tested by the connection that **did** open the session — and the count
/// moves. Without that leg a daemon with a broken endpoint would pass this test.
///
/// The refusal is also *silent* — no `provider_tested` on either connection.
/// A probe is read-shaped and attacker-paced, and a refusal that published
/// would hand an unauthorized caller a way to fill a stranger's transcript at
/// whatever rate it liked (LESSON-513; only commits announce their refusals).
#[test]
fn a_connection_that_did_not_attach_the_session_is_refused_before_anything_is_dialed() {
    let vendor = MockProvider::always(openai_turn(
        "OK",
        None,
        PROBE_INPUT_TOKENS,
        PROBE_OUTPUT_TOKENS,
    ));

    let ws = Workspace::new("provider-test-foreign");
    ws.write_config(&provider_block(
        "probe",
        &vendor.openai_endpoint(),
        PRICED_MODEL,
    ));
    let script = ws.write_script(NO_LOCAL_TURNS);
    let daemon = Daemon::spawn(&ws, probe_machine().script(script));

    let mut owner = daemon.connect();
    let session = owner.create_session("freeform", None);

    let mut foreign = daemon.connect();
    let refused = test_provider(&mut foreign, &session, "probe");

    assert_eq!(
        error_code_of(&refused),
        NOT_ATTACHED,
        "AC-6: a connection that may not drive the session may not spend on it: \
         {refused}"
    );
    assert_eq!(
        vendor.request_count(),
        0,
        "AC-6: the refusal lands before the runtime is touched — nothing was \
         dialed and nothing was billed. Captured: {:?}",
        vendor.requests()
    );
    foreign.drain_events(EVENT_WINDOW);
    owner.drain_events(EVENT_WINDOW);
    assert!(
        tested_events(&foreign).is_empty() && tested_events(&owner).is_empty(),
        "a probe's refusal announces nothing (LESSON-513): {:?} / {:?}",
        tested_events(&foreign),
        tested_events(&owner)
    );

    // Falsification: the session's own connection reaches the same provider.
    let allowed = test_provider(&mut owner, &session, "probe");
    assert_eq!(
        outcome_tag(&allowed),
        "reached",
        "the fixture must be one where a permitted call DOES land, or the zero \
         above measures a broken endpoint: {allowed}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(
        vendor.request_count(),
        1,
        "and the count moves when the gate opens"
    );
}

// ---------------------------------------------------------------------------
// (h) AC-7 / BR-8 — a local provider is refused with its own state
// ---------------------------------------------------------------------------

/// **AC-7 / BR-8.** A `kind = "local"` provider has no host to dial, so it is
/// refused `INVALID_PARAMS` with the local tier's own state — and no request is
/// made, on that provider or any other.
///
/// Falsification, on the same daemon: the remote provider registered beside it
/// is tested afterwards and reaches. Otherwise "no request was made" would hold
/// equally on a daemon that could not make one at all.
#[test]
fn a_local_provider_is_refused_with_its_own_state_and_nothing_is_dialed() {
    let vendor = MockProvider::always(openai_turn(
        "OK",
        None,
        PROBE_INPUT_TOKENS,
        PROBE_OUTPUT_TOKENS,
    ));

    let mut config = String::from("[[providers]]\nid = \"onlocal\"\nkind = \"local\"\n\n");
    config.push_str(&provider_block(
        "probe",
        &vendor.openai_endpoint(),
        PRICED_MODEL,
    ));

    let ws = Workspace::new("provider-test-local");
    ws.write_config(&config);
    let script = ws.write_script(NO_LOCAL_TURNS);
    let daemon = Daemon::spawn(&ws, probe_machine().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let refused = test_provider(&mut client, &session, "onlocal");
    assert_eq!(
        error_code_of(&refused),
        INVALID_PARAMS,
        "BR-8: a local provider is refused rather than tested: {refused}"
    );
    let message = error_message_of(&refused);
    assert!(
        message.contains("onlocal"),
        "the refusal names the provider that was asked for: {message:?}"
    );
    assert!(
        message.contains("teton doctor"),
        "AC-7: the refusal hands the user the surface that DOES answer 'does the \
         local tier work': {message:?}"
    );
    assert_eq!(
        vendor.request_count(),
        0,
        "a refusal that never dials must not dial anything else either: {:?}",
        vendor.requests()
    );
    client.drain_events(EVENT_WINDOW);
    assert!(
        tested_events(&client).is_empty(),
        "a refusal that made no call announces nothing: {:?}",
        tested_events(&client)
    );

    // Falsification: the remote provider on the same daemon does reach.
    let reached = test_provider(&mut client, &session, "probe");
    assert_eq!(
        outcome_tag(&reached),
        "reached",
        "the fixture must be one where a testable provider IS testable: \
         {reached}\ndaemon log:\n{}",
        daemon.log()
    );
    assert_eq!(vendor.request_count(), 1);
}
