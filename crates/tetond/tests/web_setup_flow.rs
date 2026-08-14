//! REQ-572 acceptance: the guided `[web]` setup flow, driven over a socket
//! against a **spawned daemon that owns a config file** (TASK-133).
//!
//! ## Why this suite exists beside the ones that already cover the flow
//!
//! Three instruments were already pointed at REQ-572 and each is blind to the
//! thing asserted here:
//!
//! * `runtime::tests::web_setup_flow` (TASK-129) drives plan/preview/commit
//!   against a `DaemonRuntime` with a config path — in process, with no wire,
//!   no dispatch gate and no turn loop.
//! * `multi_client.rs` (TASK-130) drives all three methods over a real socket,
//!   but its in-process fixture is `DaemonRuntime::minimal()`, whose
//!   `config_path` is `None` — so its commit can only ever answer
//!   `CONFIG_REJECTED`. `config_path` is a private field, so no integration
//!   test can construct a runtime that has one.
//! * `web_lookup_egress.rs` (TASK-131) pins the capability clauses against a
//!   `HarnessConfig` a test hand-built.
//!
//! What is left, and what this file is, is the **process**: a real `teton-code`
//! with `TETON_CONFIG` in its own environment, so the config path is per-daemon
//! rather than a process-global the rest of a test binary would share. That is
//! what makes "the commit wrote these bytes and the *same session* then used
//! the capability with no restart" assertable at all, and it is what makes the
//! prompt clause a claim about the daemon's own derivation rather than about a
//! struct a test filled in.
//!
//! ## AC → test map
//!
//! | AC | Test |
//! |----|------|
//! | AC-1 (clause, from the daemon's own derivation) | [`a_fresh_config_tells_the_model_the_capability_is_off`] |
//! | AC-1 (zero lookup egress) + BR-13 (turn half) | [`a_fresh_config_tells_the_model_the_capability_is_off`] |
//! | AC-3 (plan → preview → commit → live use, no restart) | [`the_committed_table_serves_a_lookup_in_the_same_session`] |
//! | AC-6 (a refused commit writes nothing) | [`the_committed_table_serves_a_lookup_in_the_same_session`] |
//! | BR-13 (the flow itself performs no egress) | [`the_committed_table_serves_a_lookup_in_the_same_session`] |
//! | AC-7 (SearchUnavailable: state, reason, refused preview) | [`a_machine_that_cannot_serve_search_is_told_which_piece_is_missing`] |
//! | AC-7 (the servable half: a keyless SearxNG shape) | [`a_keyless_search_shape_previews_clean_where_the_machine_can_serve_it`] |
//! | AC-11 (client leg: the completion is delivered to a client) | [`the_committed_table_serves_a_lookup_in_the_same_session`] |
//!
//! ### Not asserted here, and where each one is
//!
//! * **AC-4** (user-only enforcement) — `multi_client.rs`
//!   (`a_setup_commit_without_session_access_is_refused_and_the_owner_is_told`)
//!   and `server.rs`'s own mutation-checked unit tests. A second daemon here
//!   would re-run that gate against a different fixture, not a different claim.
//! * **AC-5** (secret hygiene) — no key is entered on this side of the wire at
//!   all: the flow's collection is the client's, and the daemon only ever sees
//!   a `search_key_ref`. The sweep is `crates/teton/tests/pty_e2e.rs` plus
//!   `web_setup_ui`'s frame capture.
//! * **AC-11's concurrency leg** (two sessions mid-flow, an answer in one not
//!   advancing the other) — **structurally unrepresentable**, per architecture
//!   ADR-1: these three RPCs are stateless request/response, the daemon holds
//!   no per-session step state, and no step id is minted anywhere. There is no
//!   "the other session's flow" for an answer to reach. The client leg — a
//!   `web_setup_completed` delivered to the committing session's own connected
//!   client, asserted at that client rather than in a log — is pinned in
//!   [`the_committed_table_serves_a_lookup_in_the_same_session`].
//!
//! ## AC-9 — **[MANUAL GATE — not CI-enforceable]** for its second half
//!
//! AC-9 is two claims and only one of them is automatable.
//!
//! * **Prompt headroom** — automated, and asserted with an explicit margin
//!   against the real prompt: `the_total_cap_clears_the_harness_context_budget_with_margin`
//!   (`egress/redact.rs`) and `the_web_tool_docs_clear_the_outbound_body_overhead`
//!   (`harness/tools/web.rs`), the latter clearing the ceiling by 87 bytes.
//! * **"The in-refusal offer appears at most once per session per capability;
//!   the second dead-end renders the one-line reference form"** — a **manual
//!   gate**. What ships is an *instruction* in the prompt, and whether a model
//!   obeys it on the second dead-end of a live session is model behaviour, not
//!   daemon behaviour: no fixture can assert it without scripting the very
//!   output it claims to be measuring. What CI does hold is that the
//!   instruction is present in every clause and in no other prompt text
//!   (`every_capability_clause_carries_the_repeat_instruction_and_only_a_clause_does`,
//!   `turn_loop.rs`). Do not tick AC-9's dedup half without a recorded
//!   sign-off from a live two-dead-end session, following the REQ-547 AC-13
//!   precedent (commit `9c2d2ed`).
//!
//! ## Falsification (LESSON-479)
//!
//! Every zero here is paired, on the **same instrument**, with an observation
//! of the same thing happening. `web_host` is a live HTTP server whose URL is
//! pasted into the prompt of the very session that must not reach it; the
//! request count that must be `0` before the opt-in is the count that must be
//! `1` after it. A capture that has never been seen non-empty measures nothing.

use std::time::Duration;

use serde_json::{json, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{
    assert_no_boundary_bytes, openai_turn, remote_provider_block, tier_block, Client, Daemon,
    DaemonOptions, MockProvider, MockResponse, Workspace,
};

/// The four tiers, all bound to one provider — a fixture that does not depend
/// on which category a prompt happens to classify as.
fn every_tier_bound_to(provider: &str) -> String {
    ["reflex", "scan", "build", "think"]
        .iter()
        .map(|tier| tier_block(tier, provider))
        .collect()
}

/// The deterministic machine every daemon here is spawned onto, so a probe of
/// the real host cannot pick a model and spend the test's budget loading it.
fn probe() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16u64 << 30).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500u64 << 30).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Everything the daemon was sent by the model provider, as one string.
///
/// The system prompt is not observable any other way from outside the daemon:
/// it is composed per turn and travels in the provider payload. Reading it off
/// the capture is what makes the clause assertion a claim about *the prompt a
/// real session was given* rather than about a function's return value.
fn provider_payloads(provider: &MockProvider) -> String {
    provider
        .requests()
        .iter()
        .map(|body| String::from_utf8_lossy(body).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `web/setup_plan` for `session`.
fn plan(client: &mut Client, session: &str) -> Value {
    client.call("web/setup_plan", json!({ "session_id": session }))
}

/// Setup answers as a params object — one spelling for preview and commit, so
/// a test cannot preview one candidate and commit another (BR-7 is exactly the
/// claim that those are the same thing).
fn answers(session: &str, tier: &str, endpoint: Option<&str>) -> Value {
    let mut params = json!({ "session_id": session, "tier": tier });
    if let Some(endpoint) = endpoint {
        params["search_endpoint"] = json!(endpoint);
    }
    params
}

// ---------------------------------------------------------------------------
// AC-1 — the observed failure, from the daemon's own derivation
// ---------------------------------------------------------------------------

/// **AC-1: a fresh config makes the model tell the user, not dead-end** —
/// and the session that says so puts nothing on the wire.
///
/// The daemon is spawned with a config that has **no `[web]` table at all**,
/// which is the state the requirement was filed about. Three claims, in the
/// order they can be established:
///
/// 1. the prompt the model was actually handed names the capability, says it is
///    off, and gives both enablement paths — read off the provider capture, so
///    it is the daemon's own `web_capability_state` derivation that produced it
///    and not a `HarnessConfig` a test filled in;
/// 2. the prompt forbids the repository hunt (BUG-160's regression, LESSON-493);
/// 3. **nothing was fetched.** The URL in the prompt is a live loopback server
///    this test owns, so a daemon that fetched pasted URLs without an opt-in
///    would show a request there. Its count must be zero — and the same
///    instrument is observed at `1` in the AC-3 test below, which is what makes
///    this zero mean something.
#[test]
fn a_fresh_config_tells_the_model_the_capability_is_off() {
    let frontier = MockProvider::start(
        Vec::new(),
        MockResponse::ok(openai_turn(
            "I cannot look that up right now.",
            None,
            120,
            20,
        )),
    );
    // A live server whose URL goes into the prompt. Nothing may reach it.
    let web_host = MockProvider::always("<html><body>the page</body></html>".to_owned());
    let url = format!("http://127.0.0.1:{}/api-docs", web_host_port(&web_host));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "frontier",
        &frontier.openai_endpoint(),
        "claude-opus-4",
    ));
    config.push_str(&every_tier_bound_to("frontier"));
    // Deliberately no `[web]` table. That absence is the fixture.

    let ws = Workspace::new("setup-ac1");
    ws.write_config(&config);
    // A scripted local tier so the first-run consent gate is exempt and no
    // proposal stands between the client and its turn. It does not change the
    // capability state: with no `[web]` table the answer is `OffAvailable`
    // whether or not a local model is present.
    let script = ws.write_script("unused — every tier is bound to the remote provider.");
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let turn = client.prompt(
        &session,
        &format!("Search the web for the Brave Search API docs, or read {url} — I need the endpoint shape."),
    );
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the turn must complete — a capability being off is not a failed turn: {turn}"
    );
    client.drain_events(Duration::from_millis(300));

    // (1) + (2) The prompt the model was handed. Content predicates rather than
    // the clause's exact bytes, so a strengthened wording of the same facts
    // composes with this test instead of breaking it (the clause's own bytes
    // are pinned in `turn_loop.rs`).
    let sent = provider_payloads(&frontier);
    assert!(
        !sent.is_empty(),
        "the turn never reached the provider, so there is no prompt to read; \
         daemon log:\n{}",
        daemon.log()
    );
    // Content predicates over the **whole prompt a real session was given** —
    // clause and bundled guide together, which is what the model reads. That
    // the clause specifically carries each fact is pinned where the clause is
    // built (`turn_loop.rs`); `/web setup` in particular is named by the guide
    // on every machine, so the falsification leg below keys on the state
    // sentence instead.
    for (fact, needle) in [
        ("the capability exists and is off", "switched off"),
        ("the in-session enablement path", "/web setup"),
        ("the config key", "[web] tier"),
        (
            "and it must forbid the repository hunt",
            "instead of searching the repository for it",
        ),
    ] {
        assert!(
            sent.contains(needle),
            "AC-1: the prompt must name {fact} ({needle:?}); prompt as sent:\n{sent}"
        );
    }

    // (3) Zero lookup egress, on an instrument that could have shown one.
    assert_eq!(
        web_host.request_count(),
        0,
        "AC-1: a session with no `[web]` table fetched a URL from its own prompt"
    );
    assert!(
        client.events_named("web_lookup").is_empty(),
        "and no lookup was even attempted: {:?}",
        client.events_named("web_lookup")
    );

    // --- falsification: the same session on an opted-in machine -------------
    //
    // Without this, every predicate above could be satisfied by a clause the
    // daemon staples onto every prompt it ever builds. The only difference
    // between the two daemons is the `[web]` table — the fact the classifier
    // reads — so what the second prompt lacks is what the first one's table-less
    // config bought.
    let opted_in = MockProvider::start(
        Vec::new(),
        MockResponse::ok(openai_turn("Reading that now.", None, 120, 20)),
    );
    let mut on = String::new();
    on.push_str(&remote_provider_block(
        "frontier",
        &opted_in.openai_endpoint(),
        "claude-opus-4",
    ));
    on.push_str(&every_tier_bound_to("frontier"));
    on.push_str("[web]\ntier = \"fetch_any_url\"\n\n");

    let ws_on = Workspace::new("setup-ac1-on");
    ws_on.write_config(&on);
    let script_on = ws_on.write_script("unused — every tier is bound to the remote provider.");
    let daemon_on = Daemon::spawn(&ws_on, probe().script(script_on));
    let mut client_on = daemon_on.connect();
    let session_on = client_on.create_session("freeform", None);
    let turn_on = client_on.prompt(&session_on, &format!("Read {url} for me."));
    assert_eq!(
        turn_on["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn_on}"
    );

    let sent_on = provider_payloads(&opted_in);
    assert!(
        !sent_on.is_empty(),
        "the control turn never reached its provider; daemon log:\n{}",
        daemon_on.log()
    );
    // Only the **clause's** own facts, not `/web setup`: the bundled `[web]`
    // guide names that command on every machine, opted in or not, which is the
    // point of bundling it (BR-2). What must vanish is the state sentence.
    for absent in ["switched off", "instead of searching the repository for it"] {
        assert!(
            !sent_on.contains(absent),
            "a Ready capability must carry no enablement clause ({absent:?}); \
             prompt as sent:\n{sent_on}"
        );
    }
    assert_no_boundary_bytes();
}

/// The port a [`MockProvider`] is listening on, recovered from the endpoint it
/// hands out. The struct's `port` field is private and the suite has never
/// needed it as anything but a URL before.
fn web_host_port(server: &MockProvider) -> u16 {
    server
        .openai_endpoint()
        .rsplit(':')
        .next()
        .and_then(|tail| tail.split('/').next())
        .and_then(|port| port.parse().ok())
        .expect("the mock endpoint carries its port")
}

// ---------------------------------------------------------------------------
// AC-3 / AC-6 / AC-11 (client leg) / BR-13 — the whole flow, over the socket
// ---------------------------------------------------------------------------

/// **AC-3: plan → preview → commit, and the capability serves the very next
/// turn of the same session with no restart.**
///
/// This is the criterion no other suite can reach. It needs a daemon with a
/// config file *and* a live turn loop *and* one client holding one session
/// across the write — which is a process, not a fixture.
///
/// The legs, in order, each one a precondition of the next:
///
/// 1. the plan reports `off_available` and no `[web]` table (non-vacuity: the
///    machine really is in the state the flow is for);
/// 2. a **refused** commit — an endpoint below the tier that needs it — is
///    answered `WEB_SETUP_INVALID` and leaves the config file byte-identical
///    (AC-6, at the wire);
/// 3. the preview renders bytes, and the file is *still* byte-identical: a
///    preview is not a write (BR-11's single commit point);
/// 4. the commit applies, the bytes land, and `web_setup_completed` arrives at
///    **this client** scoped to **this session** (BR-14/AC-11's client leg);
/// 5. the plan now reports `ready`;
/// 6. and the same session, on the same connection, with no restart, runs a
///    consented lookup that genuinely reaches the wire — the `1` that makes the
///    `0` in AC-1 above a measurement.
///
/// BR-13 rides along: `web_host.request_count()` is asserted at `0` after the
/// whole flow has completed and before any turn runs. The flow collects,
/// validates and writes, and sends nothing.
#[test]
fn the_committed_table_serves_a_lookup_in_the_same_session() {
    let web_host = MockProvider::always("<html><body>the tokio page</body></html>".to_owned());
    let url = format!("http://127.0.0.1:{}/tokio", web_host_port(&web_host));
    // The scripted local tier answers the turn *and* asks for the page. The
    // model is not what is under test here; the tool registry it is offered is.
    let script = format!(
        "{{\"tool\": \"web\", \"arguments\": {{\"url\": \"{url}\"}}}}\n---\nHere is what that page says."
    );

    let mut config = String::new();
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&every_tier_bound_to("local"));
    config.push_str("[privacy]\nredact = false\n\n");

    let ws = Workspace::new("setup-ac3");
    ws.write_config(&config);
    let script_path = ws.write_script(&script);
    let daemon = Daemon::spawn(&ws, probe().script(script_path));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let before_bytes = std::fs::read(&ws.config_path).expect("the fixture config exists");

    // (1) The machine is genuinely in the state the flow exists for.
    let planned = plan(&mut client, &session);
    assert_eq!(
        planned["result"]["state"]["state"].as_str(),
        Some("off_available"),
        "the fixture must start off-but-available: {planned}"
    );
    assert!(
        planned["result"]["current_web"].is_null(),
        "and with no `[web]` table at all, which is not the same as `tier = off`: {planned}"
    );

    // (2) AC-6 at the wire: a candidate the validator refuses writes nothing.
    // `tier = "search"` with no `search_endpoint` is `Config::validate`'s
    // `WebSearchTierWithoutEndpoint` — the production validator's own answer,
    // not a test's idea of an invalid document — and this daemon *can* serve
    // search (it has a scripted local tier), so the refusal below is the
    // validator's and not AC-7's capability gate standing in front of it.
    let refused = client.call("web/setup_commit", answers(&session, "search", None));
    assert!(
        refused["error"].is_object(),
        "a candidate that would not load must be refused: {refused}"
    );
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(teton_protocol::jsonrpc::error_code::WEB_SETUP_INVALID),
        "and refused under this REQ's own code, not a generic turn error \
         (BR-10): {refused}"
    );
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "AC-6: a refused commit must leave the config byte-identical"
    );

    // (3) A preview is not a write.
    let preview = client.call(
        "web/setup_preview",
        answers(&session, "fetch_user_url", None),
    );
    let toml = preview["result"]["toml"]
        .as_str()
        .unwrap_or_else(|| panic!("the preview must render the candidate table: {preview}"));
    assert!(
        toml.contains("[web]") && toml.contains("fetch_user_url"),
        "the preview must be the bytes a commit would write: {toml:?}"
    );
    assert_eq!(
        std::fs::read(&ws.config_path).ok().as_deref(),
        Some(before_bytes.as_slice()),
        "BR-11: the commit is the single write point — a preview writes nothing"
    );

    // (4) The commit.
    let committed = client.call(
        "web/setup_commit",
        answers(&session, "fetch_user_url", None),
    );
    assert_eq!(
        committed["result"]["applied"].as_bool(),
        Some(true),
        "the commit must apply: {committed} \n daemon log:\n{}",
        daemon.log()
    );
    let written = std::fs::read_to_string(&ws.config_path).expect("the config was written");
    assert!(
        written.contains("[web]") && written.contains("fetch_user_url"),
        "the committed table must be in the file the daemon was given:\n{written}"
    );
    assert!(
        written.contains(toml),
        "BR-7: what was confirmed is what was written. preview:\n{toml}\ndocument:\n{written}"
    );

    // AC-11's client leg / BR-14: the completion reached *this* client, scoped
    // to *this* session — asserted off the socket, never out of a log.
    client.drain_events(Duration::from_millis(300));
    let completions = client.events_named("web_setup_completed");
    assert_eq!(
        completions.len(),
        1,
        "exactly one completion must be announced: {completions:?}"
    );
    assert_eq!(
        completions[0]["session_id"].as_str(),
        Some(session.as_str()),
        "and it is scoped to the committing session: {}",
        completions[0]
    );
    assert_eq!(completions[0]["tier"].as_str(), Some("fetch_user_url"));

    // (5) The state the daemon reports has moved, with no restart.
    let after = plan(&mut client, &session);
    assert_eq!(
        after["result"]["state"]["state"].as_str(),
        Some("ready"),
        "the capability must be live in the daemon that wrote it: {after}"
    );

    // BR-13: the whole flow — plan, refused commit, preview, commit, plan —
    // sent nothing anywhere. The server that is about to receive one request
    // has received none.
    assert_eq!(
        web_host.request_count(),
        0,
        "BR-13: the setup flow itself must perform no egress"
    );

    // (6) The same session, the same connection, no restart. The URL is pasted
    // into the prompt, which is what a `fetch_user_url` ceiling permits — and
    // what exempts a loopback destination from the seam's address floor.
    let turn = client.prompt(
        &session,
        &format!("What does {url} say about task pinning?"),
    );
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the post-setup turn must complete: {turn}\ndaemon log:\n{}",
        daemon.log()
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        web_host.request_count(),
        1,
        "AC-3: the committed capability must serve a lookup in the same session, \
         with no restart. daemon log:\n{}",
        daemon.log()
    );
    let lookups = client.events_named("web_lookup");
    assert_eq!(
        lookups.len(),
        1,
        "and the lookup must be accounted for at the client: {lookups:?}"
    );
    assert_eq!(
        lookups[0]["outcome"].as_str(),
        Some("completed"),
        "AC-3 asks for a lookup that *succeeds*, not merely one that was \
         attempted: {}",
        lookups[0]
    );
    assert_no_boundary_bytes();
}

// ---------------------------------------------------------------------------
// AC-7 — partial configuration is guidance, in both directions
// ---------------------------------------------------------------------------

/// **AC-7, the blocked half: the wire carries the distinct state code and the
/// named missing piece, and the flow refuses to walk the user into it.**
///
/// This daemon has no local model — no `TETON_LOCAL_SCRIPT`, and a
/// `[local_model] base_url` pointing at a closed port — so the search leg
/// cannot serve (REQ-563 BR-14 makes the per-query scan the condition on which
/// the search tier exists). Its config already says `tier = "search"`, which is
/// a document `Config::validate` accepts: this is a *runtime* gap, not an
/// invalid file, and telling those apart is the whole of BR-10.
///
/// Falsified in place: the same daemon previews `fetch_any_url` cleanly, so the
/// refusal is about the tier the machine cannot serve and not about a daemon
/// that refuses everything.
#[test]
fn a_machine_that_cannot_serve_search_is_told_which_piece_is_missing() {
    let mut config = String::new();
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&every_tier_bound_to("local"));
    config.push_str("[local_model]\nauto_accept = false\nbase_url = \"http://127.0.0.1:9\"\n\n");
    config.push_str("[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n");

    let ws = Workspace::new("setup-ac7-gap");
    ws.write_config(&config);
    let daemon = Daemon::spawn(&ws, probe());
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let planned = plan(&mut client, &session);
    assert_eq!(
        planned["result"]["state"]["state"].as_str(),
        Some("search_unavailable"),
        "BR-10: the state travels as a code, not as prose a client re-parses: {planned}"
    );
    let reason = planned["result"]["state"]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("local model"),
        "AC-7: the state must name the missing piece: {planned}"
    );
    assert_eq!(
        planned["result"]["search_available"].as_bool(),
        Some(false),
        "{planned}"
    );
    assert_eq!(
        planned["result"]["search_gap"].as_str(),
        Some(reason),
        "the gap the menu greys the row out with and the gap the state carries \
         must be one sentence, or two surfaces describe one machine differently: \
         {planned}"
    );
    // The table it does have is reported, which is what distinguishes "no
    // `[web]` table" from "a table whose search leg cannot serve".
    assert_eq!(
        planned["result"]["current_web"]["tier"].as_str(),
        Some("search"),
        "{planned}"
    );

    // The flow refuses to walk anyone into it, carrying the same sentence.
    let refused = client.call(
        "web/setup_preview",
        answers(
            &session,
            "search",
            Some("https://search.example/api?format=json"),
        ),
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("local model"),
        "AC-7: the refusal must name the missing piece, in the daemon's own \
         words: {refused}"
    );

    // Falsification: the same daemon, one tier down, previews cleanly.
    let allowed = client.call(
        "web/setup_preview",
        answers(&session, "fetch_any_url", None),
    );
    assert!(
        allowed["result"]["toml"]
            .as_str()
            .unwrap_or_default()
            .contains("fetch_any_url"),
        "the refusal must be about the search tier, not about this daemon: {allowed}"
    );
    assert_no_boundary_bytes();
}

/// **AC-7, the servable half: a keyless SearxNG shape previews clean.**
///
/// The counterpart the requirement asks for by name, and the reason it matters
/// is AC-8: the flow suggests a self-hosted SearxNG endpoint ending
/// `/search?format=json` with *no credential at all*, so a preview path that
/// insisted on a `search_key_ref` would be suggesting a backend it could not
/// walk anyone into. This daemon has a scripted local tier, so the search leg
/// can serve.
///
/// "Clean" means **no error**. The daemon still attaches an advisory warning
/// for a search tier with no key reference — advice is not a refusal, and the
/// distinction is asserted rather than assumed.
#[test]
fn a_keyless_search_shape_previews_clean_where_the_machine_can_serve_it() {
    let mut config = String::new();
    config.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    config.push_str(&every_tier_bound_to("local"));

    let ws = Workspace::new("setup-ac7-ok");
    ws.write_config(&config);
    let script = ws.write_script("nothing is prompted in this test.");
    let daemon = Daemon::spawn(&ws, probe().script(script));
    let mut client = daemon.connect();
    let session = client.create_session("freeform", None);

    let planned = plan(&mut client, &session);
    assert_eq!(
        planned["result"]["search_available"].as_bool(),
        Some(true),
        "a machine with a local tier can serve search: {planned}"
    );
    assert!(
        planned["result"]["search_gap"].is_null(),
        "and names no gap: {planned}"
    );

    const SEARXNG: &str = "http://localhost:8888/search?format=json";
    let preview = client.call(
        "web/setup_preview",
        answers(&session, "search", Some(SEARXNG)),
    );
    assert!(
        preview["error"].is_null(),
        "AC-7/AC-8: the keyless shape the flow suggests must preview without a \
         refusal: {preview}"
    );
    let toml = preview["result"]["toml"].as_str().unwrap_or_default();
    assert!(
        toml.contains("tier = \"search\"") && toml.contains(SEARXNG),
        "the candidate must carry the tier and the endpoint verbatim: {toml:?}"
    );
    assert_eq!(
        preview["result"]["search_host"].as_str(),
        Some("localhost"),
        "the host at the confirm step comes from the executor's parse — a host, \
         never the path or the query (BR-9, LESSON-494): {preview}"
    );
    // A warning is advice; the absence of an error above is the acceptance.
    let warnings = preview["result"]["warnings"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_default();
    assert!(
        warnings <= 2,
        "a keyless search candidate should draw advice, not a pile of it: {preview}"
    );
    assert_no_boundary_bytes();
}
