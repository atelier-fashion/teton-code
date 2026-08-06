//! End-to-end verification for REQ-558: the privacy override over category
//! routing (AC-6, BR-7), what every `route_decided` carries (AC-8), and the
//! one-resolver rule across the three surfaces that describe a routing state
//! (AC-11, BR-6).
//!
//! These spawn the **real** `tetond` binary against a config file on disk and
//! assert on the bytes a mock provider actually received, because every claim
//! here is about egress or about agreement between surfaces — neither of which
//! is observable from a unit test of the resolver. conventions.md is explicit
//! that a privacy claim needs mock-transport capture rather than code
//! inspection ("the coverage gap and the security gap are the same gap",
//! LESSON-432).
//!
//! ## Why every boundary test here asserts the route as well as the bytes
//!
//! BUG-155's near-miss was a boundary test whose fixture prompt happened to
//! contain an `AUXILIARY_SIGNALS` word, so the turn never approached the
//! boundary and "nothing leaked" was trivially true. That keyword list is
//! deleted, but the general trap survives any routing change: a test that only
//! asserts an absence cannot tell "the guard held" from "the turn was never
//! going there". So each boundary assertion below is paired with a positive
//! one — a `route_decided` naming the remote provider, or a control session on
//! the same daemon and the same binding that genuinely reaches it.

use std::time::Duration;

use crate::harness::{
    assert_no_boundary_bytes, category_block, openai_turn, remote_provider_block, tier_block,
    tier_block_with_fallback, Client, Daemon, DaemonOptions, MockProvider, MockResponse, Workspace,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// A 16 GiB Apple-Silicon probe. Whether a local tier exists is decided by
/// whether the caller attaches a script, not by the machine.
fn probe_16gb() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", "500000000000")
        .env("TETON_PROBE_GPU", "apple-silicon")
}

/// Every `route_decided` this client observed that names `provider`.
fn routes_to<'a>(client: &'a Client, provider: &str) -> Vec<&'a serde_json::Value> {
    client
        .events_named("route_decided")
        .into_iter()
        .filter(|e| e["provider_id"].as_str() == Some(provider))
        .collect()
}

/// The `config/get` routing row for `category` — the value `teton policy show`
/// renders and nothing else.
fn policy_row(client: &mut Client, category: &str) -> serde_json::Value {
    let snapshot = client.config_get();
    snapshot["routing"]
        .as_array()
        .expect("a routing table")
        .iter()
        .find(|row| row["category"].as_str() == Some(category))
        .unwrap_or_else(|| panic!("no '{category}' row in {snapshot}"))
        .clone()
}

// ===========================================================================
// AC-6 / BR-7 — the taint backstop overrides every category binding.
// ===========================================================================

/// A session tainted by boundary content stays on the local tier for every
/// subsequent turn, with `think` bound to a remote provider and a `design`
/// turn — and **zero** payloads carrying boundary content reach the wire,
/// asserted by capture.
///
/// The non-vacuity leg is the first half of the test, not an afterthought: the
/// *pre-taint* turn on this exact binding produces a `route_decided` naming
/// `frontier`, so the post-taint absence of one is the pin holding rather than
/// a turn that was never going remote. Without that pairing a future change
/// that quietly routes `design` local would leave this test green and
/// meaningless — which is precisely the shape BUG-155 found.
///
/// The category is `design` by construction rather than by classification: a
/// structured session at the `architect` phase maps to `Category::Design`
/// through `category_for_phase`, with no model call and no prompt text
/// involved (ADR-C). That keeps the fixture's routing independent of what the
/// stand-in classifier happens to answer.
#[test]
fn a_tainted_session_stays_local_and_the_pre_taint_turn_proves_it_would_not_have() {
    // Turn 1 reads the boundary file through the jailed built-in; the next
    // remote call of that same turn carries the content and is blocked before a
    // byte leaves.
    let frontier = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Reading the production config.",
            Some(("c1", "read", r#"{"path":"secrets/prod.env"}"#)),
            120,
            20,
        ))],
        MockResponse::ok(openai_turn("Should never be reached.", None, 10, 5)),
    );

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "frontier",
        &frontier.openai_endpoint(),
        "claude-opus-4",
    ));
    // `think` is the tier `design` inherits, and it is bound to a REMOTE
    // provider. This is the binding the taint has to override.
    config.push_str(&tier_block("think", "frontier"));
    config.push_str("[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n\n");

    let ws = Workspace::new("ac6");
    ws.write_config(&config);
    // A local tier for the blocked turn to reroute onto (REQ-544 M-1) and for
    // every later turn of the tainted session to run on.
    let script = ws.write_script("Rerouted locally; done.\n---\nStill local; done.");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("structured", Some("architect"));

    // --- Turn 1: genuinely remote, and it says so ---------------------------
    let first = client.prompt(&session, "Decompose the configuration loader into modules.");
    assert_eq!(
        first["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the rerouted-to-local turn should still complete cleanly: {first}"
    );
    client.drain_events(Duration::from_millis(300));

    // NON-VACUITY. The turn was routed to the remote provider by the `design`
    // category through its `think` binding — so the boundary assertion below is
    // about a guard holding, not about a turn that never approached the wire.
    let remote_routes = routes_to(&client, "frontier");
    assert_eq!(
        remote_routes.len(),
        1,
        "the pre-taint turn must genuinely route to the remote provider, or the \
         rest of this test proves nothing: {:?}",
        client.events_named("route_decided")
    );
    assert_eq!(
        remote_routes[0]["category"].as_str(),
        Some("design"),
        "{}",
        remote_routes[0]
    );
    assert_eq!(remote_routes[0]["tier"].as_str(), Some("think"));
    assert!(
        frontier.request_count() >= 1,
        "and it must have actually called the provider"
    );

    // The boundary fired at the choke point, naming the provider it would have
    // reached.
    let blocks = client.events_named("privacy_block");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one privacy_block, got {blocks:?}"
    );
    assert_eq!(blocks[0]["provider_id"].as_str(), Some("frontier"));

    // --- Turn 2: the same binding, the same category, pinned local ----------
    let second = client.prompt(
        &session,
        "Now decompose the neighbouring loader the same way.",
    );
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the tainted session's next turn should complete on the local tier: {second}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        routes_to(&client, "frontier").len(),
        1,
        "a tainted session must not route to the remote `think` binding again \
         (BR-7); route_decided events: {:?}",
        client.events_named("route_decided")
    );

    // AC-8's ONE documented exception, asserted rather than dodged. The taint
    // pin resolves no category on purpose — it is a privacy guarantee
    // overriding every binding, not a category decision — so its `route_decided`
    // carries no `category` and no `tier`. Minting one would be the second
    // computation ADR-D forbids. Recorded in docs/manual-verification.md too.
    let pinned = routes_to(&client, "local");
    assert!(
        !pinned.is_empty(),
        "the pinned turn must still announce its route: {:?}",
        client.events_named("route_decided")
    );
    for event in &pinned {
        assert!(
            event.get("category").is_none(),
            "the taint pin consults no binding, so it reports no category — a \
             synthesized one would be a second answer to a question the pin \
             deliberately did not ask (ADR-D): {event}"
        );
        assert!(event.get("tier").is_none(), "{event}");
        assert!(
            !event["reason"].as_str().unwrap_or_default().is_empty(),
            "but the reason is still non-empty and still names the pin: {event}"
        );
    }

    // --- The claim itself, asserted against captured bytes ------------------
    assert_no_boundary_bytes();
    for body in frontier.requests() {
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("sk-live-DO-NOT-LEAK"),
            "BR-7 VIOLATION: boundary content reached the remote provider:\n{text}"
        );
    }
}

// ===========================================================================
// AC-8 — what every route_decided carries.
// ===========================================================================

/// Across one scripted session per origin, `route_decided` carries a category,
/// a tier, a provider, and a non-empty reason — for a **harness-known**
/// category (`digest`, tagged at its call site and never derived from text) and
/// for an **intent-classified** one (`edit`, assigned by the `route`
/// classifier).
///
/// The two origins are the interesting axis: BR-2 says a harness-known category
/// must never come from a keyword match, and the way to show the event surface
/// treats them identically is to route one of each through it and compare the
/// shape of what comes out.
#[test]
fn route_decided_names_a_category_a_tier_a_provider_and_a_reason_for_both_origins() {
    let provider = MockProvider::always(openai_turn("Done.", None, 100, 20));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "frontier",
        &provider.openai_endpoint(),
        "claude-opus-4",
    ));
    // Both tiers a turn below can land on, bound to the same provider — so the
    // test is about what the event says, not about which provider won.
    config.push_str(&tier_block("scan", "frontier"));
    config.push_str(&tier_block("build", "frontier"));

    let ws = Workspace::new("ac8");
    ws.write_config(&config);
    // A local tier so the `route` classifier can run rather than bypass: the
    // intent-classified leg needs a classification to have happened.
    let script = ws.write_script("Local, done.");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    // --- harness-known: `io` maps to `digest` with no model call and no text --
    let io = client.create_session("structured", Some("io"));
    let turn = client.prompt(&io, "Summarize the diff for the changelog.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let digest = client
        .events_named("route_decided")
        .into_iter()
        .find(|e| e["category"].as_str() == Some("digest"))
        .unwrap_or_else(|| {
            panic!(
                "no route_decided for the `digest` category: {:?}",
                client.events_named("route_decided")
            )
        })
        .clone();
    assert_eq!(digest["tier"].as_str(), Some("scan"), "{digest}");
    assert_eq!(digest["provider_id"].as_str(), Some("frontier"), "{digest}");
    assert!(
        !digest["reason"].as_str().unwrap_or_default().is_empty(),
        "{digest}"
    );

    // --- intent-classified: a freeform turn asks the `route` classifier ------
    let freeform = client.create_session("freeform", None);
    let turn = client.prompt(&freeform, "Add a retry to the upload helper.");
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    let edit = client
        .events_named("route_decided")
        .into_iter()
        .find(|e| e["category"].as_str() == Some("edit"))
        .unwrap_or_else(|| {
            panic!(
                "no route_decided for an intent-classified category: {:?}",
                client.events_named("route_decided")
            )
        })
        .clone();
    assert_eq!(edit["tier"].as_str(), Some("build"), "{edit}");
    assert_eq!(edit["provider_id"].as_str(), Some("frontier"), "{edit}");
    let reason = edit["reason"].as_str().unwrap_or_default();
    assert!(!reason.is_empty(), "{edit}");
    assert!(
        reason.contains("'edit'"),
        "the reason names the category that fired (BR-5): {edit}"
    );

    // And the general claim, over every event either turn produced: no route
    // announces a provider without also saying what it was routing.
    for event in client.events_named("route_decided") {
        assert!(
            event["provider_id"].as_str().is_some_and(|p| !p.is_empty()),
            "{event}"
        );
        assert!(
            !event["reason"].as_str().unwrap_or_default().is_empty(),
            "{event}"
        );
        assert_eq!(
            event.get("category").is_some(),
            event.get("tier").is_some(),
            "category and tier are both projected off one resolution, so they \
             are present together or not at all (ADR-D): {event}"
        );
    }

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-11 / BR-6 — one resolver, three surfaces, no drift.
// ===========================================================================

/// For one deliberately-unset binding, `route_decided`, `teton policy show`
/// (the `config/get` projection it renders) and the turn-failure sentence agree
/// on the provider, the category, the tier, and the reason.
///
/// `think` is left unbound with no `default_provider` to inherit and no local
/// tier to fall back to, so `design` cannot be routed. The three surfaces then
/// have to say the same thing about it:
///
/// - **`policy show`** carries the resolver's row verbatim — category `design`,
///   tier `think`, no provider, and the resolver's sentence.
/// - **the turn-failure sentence** for a turn dispatched on that category is
///   that same sentence, byte for byte.
/// - **`route_decided`** announces nothing, because the resolution selected no
///   provider and the event's `provider_id` is required. That is agreement, not
///   an exemption: the surface that reports *where a turn went* has nothing to
///   report when nothing was selected, and a `route_decided` naming a provider
///   here would be a fabricated id (BR-8, BUG-146).
///
/// A second call site computing its own answer makes this red — which is the
/// entire point of the criterion, because BUG-155 shipped four instances of
/// exactly that defect in this subsystem one REQ earlier.
#[test]
fn one_resolver_answers_policy_show_and_the_turn_failure_alike() {
    let bound = MockProvider::always(openai_turn("Done.", None, 10, 5));
    let unreachable = MockProvider::always(openai_turn("Should never be reached.", None, 10, 5));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "bound",
        &bound.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&remote_provider_block(
        "unreachable",
        &unreachable.openai_endpoint(),
        "claude-opus-4",
    ));
    // `build` is bound; `think` deliberately is not, and no `default_provider`
    // is set for it to inherit. Nothing may be borrowed from the bound tier —
    // an unresolvable category names itself rather than quietly taking a
    // neighbour's provider (BR-8).
    config.push_str(&tier_block("build", "bound"));

    let ws = Workspace::new("ac11");
    ws.write_config(&config);
    // No script: no local tier, so the inherited local fill cannot serve either.
    let daemon = Daemon::spawn(&ws, probe_16gb());
    let mut client = daemon.connect();

    // Surface 1 — the table a human reads.
    let row = policy_row(&mut client, "design");
    assert_eq!(row["tier"].as_str(), Some("think"), "{row}");
    assert!(
        row.get("provider_id").is_none(),
        "an unset binding resolves to no provider, and none is synthesized: {row}"
    );
    let resolver_sentence = row["reason"]
        .as_str()
        .expect("the row carries the resolver's sentence")
        .to_owned();
    assert!(
        resolver_sentence.contains("design"),
        "an unresolvable category names itself (BR-8): {resolver_sentence}"
    );

    // Surface 2 — the sentence the user gets when a turn dispatches on it.
    let session = client.create_session("structured", Some("architect"));
    let turn = client.prompt(&session, "Decompose the loader into modules.");
    let message = turn["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("an unresolvable category must fail the turn: {turn}"))
        .to_owned();
    assert!(
        message.contains(&resolver_sentence),
        "the turn-failure sentence must carry the resolver's own answer, byte \
         for byte, rather than composing a second one (BR-6, AC-11).\n\
         resolver: {resolver_sentence}\n\
         turn:     {message}"
    );

    // Surface 3 — the event stream announces no route, because none was chosen.
    client.drain_events(Duration::from_millis(300));
    for event in client.events_named("route_decided") {
        assert_ne!(
            event["category"].as_str(),
            Some("design"),
            "no provider was selected for `design`, so nothing may announce one \
             for it: {event}"
        );
    }
    assert_eq!(
        unreachable.request_count(),
        0,
        "an unresolvable category must not borrow a registered provider"
    );

    // The same three surfaces on a binding that IS set agree too, which is what
    // makes the assertions above about *agreement* rather than about everything
    // being empty.
    let bound_row = policy_row(&mut client, "edit");
    let bound_session = client.create_session("structured", Some("implement"));
    let served = client.prompt(&bound_session, "Implement the retry helper.");
    assert_eq!(
        served["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{served}"
    );
    client.drain_events(Duration::from_millis(300));
    let decided = client
        .events_named("route_decided")
        .into_iter()
        .find(|e| e["category"].as_str() == Some("edit"))
        .unwrap_or_else(|| panic!("no route_decided for `edit`"))
        .clone();
    assert_eq!(
        decided["tier"], bound_row["tier"],
        "{decided} vs {bound_row}"
    );
    assert_eq!(
        decided["provider_id"], bound_row["provider_id"],
        "{decided} vs {bound_row}"
    );
    assert_eq!(
        decided["reason"], bound_row["reason"],
        "the event and the table are two renderings of one resolution, so the \
         sentence is identical: {decided} vs {bound_row}"
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// BUG-156 — a taint-pinned session cannot fail over to a remote provider.
// ===========================================================================

/// A session pinned local by the taint backstop is served **on this machine**,
/// and never reaches the tier's configured `fallback_id` — asserted by captured
/// bytes, against a fallback the very same daemon and the very same binding
/// reach on an untainted session.
///
/// ## Why the fixture looks like this
///
/// This is the config that used to make the pin dispatch over HTTP, and it is a
/// natural one to write: a user runs their own llama.cpp server, registers it
/// `openai-compatible` under the id `local`, and binds `think` to it with a
/// frontier fallback. Before the locality fix, `local_provider_id` fell back to
/// the literal id `local` whenever no `kind = "local"` provider was declared, so
/// the taint pin resolved to *that* id, `run_one_attempt` read
/// `ProviderKind::OpenaiCompatible`, and a tainted session's turn went out over
/// the network while its own route sentence said it was pinned local.
///
/// The pin now asserts locality rather than naming it: the canonical id belongs
/// to the engine-backed tier only while nothing else answers to it, so here it
/// resolves to **no provider at all**. Dispatch then has no `[[providers]]`
/// entry to dial and serves the turn on the real on-device engine — which is
/// what "pinned local" was always supposed to mean. There is no remaining path
/// by which a pinned route can be handed to an HTTP transport, which is why the
/// mid-turn failover leg BUG-156 patched is now unreachable from a pin by
/// construction rather than by assertion. `Router::fallback_for` still carries
/// its own unit-level pin.
///
/// ## The non-vacuity legs
///
/// Two, because the interesting claim is a *negative*:
///
/// - a control session on the same daemon, the same tier binding and the same
///   failing primary **does** fail over to `backup` and completes there, so the
///   tainted session's zero bytes are the pin holding rather than an
///   unreachable fallback;
/// - the tainted turn genuinely completes, on the local engine's own scripted
///   answer, so the zero bytes are not simply a turn that died early.
#[test]
fn a_tainted_session_is_served_on_device_and_never_reaches_the_tiers_fallback() {
    // The provider the taint pin names. Its one scripted reply issues a `shell`
    // call, whose result is unknown-provenance and taints the session; every
    // call after that fails with a fallback-class error.
    let pinned = MockProvider::start(
        vec![MockResponse::ok(openai_turn(
            "Checking the build first.",
            Some(("c1", "shell", r#"{"command":"echo probing"}"#)),
            80,
            10,
        ))],
        MockResponse::bad_request(),
    );
    let backup = MockProvider::always(openai_turn("Served by the fallback.", None, 50, 10));

    let mut config = String::new();
    // Registered under the id the taint pin resolves to, but remote in kind.
    config.push_str(&remote_provider_block(
        "local",
        &pinned.openai_endpoint(),
        "qwen2.5-coder-7b",
    ));
    config.push_str(&remote_provider_block(
        "backup",
        &backup.openai_endpoint(),
        "claude-opus-4",
    ));
    // BUG-156's triggering shape in this REQ's vocabulary: the tier's primary
    // IS the local tier and its fallback is remote. "Try local, fall back to a
    // frontier model if it struggles" is a sensible thing to write, and is the
    // shape the local-first pitch invites.
    config.push_str(&tier_block_with_fallback("think", "local", "backup"));
    // A boundary must exist for the taint backstop to arm at all
    // (`context_is_sensitive` returns false with none configured) — which is
    // itself worth knowing: there is no state in which a session is tainted by
    // unknown provenance and egress would have let that same content through.
    config.push_str("[[boundaries]]\npath_glob = \"secrets/**\"\nmode = \"local-only\"\n\n");

    let ws = Workspace::new("bug156");
    ws.write_config(&config);
    // A live local tier — and it is now load-bearing rather than incidental.
    // The engine is what actually serves every pinned turn below: the pin
    // resolves to no provider id (the canonical name is taken by a remote
    // entry), so dispatch has nothing to dial and falls to the engine. Its
    // scripted answer is what the tainted session gets back.
    let script = ws.write_script("Local, done.");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    // --- Turn 1: routes through the `think` binding and taints the session ---
    let tainted = client.create_session("structured", Some("architect"));
    let first = client.prompt(&tainted, "Decompose the loader into modules.");
    client.drain_events(Duration::from_millis(300));
    // The follow-up call carrying the `shell` result is fail-closed at egress
    // (REQ-544 C-1: unknown provenance is blocked exactly like a boundary hit),
    // and the reroute lands on the pin. What matters here is the side effect:
    // the session is now tainted.
    assert!(
        client.saw_event("privacy_block"),
        "the unknown-provenance follow-up must be blocked, which is what taints \
         the session: {first}"
    );
    let pre_taint = routes_to(&client, "local");
    assert_eq!(
        pre_taint.len(),
        1,
        "exactly ONE route names the squatted id — the pre-taint one, through \
         the `think` binding. The reroute onto the pin must NOT announce it: \
         that id belongs to a remote provider, and a route that names it is a \
         route that dispatches over HTTP: {:?}",
        client.events_named("route_decided")
    );
    assert_eq!(
        pre_taint[0]["category"].as_str(),
        Some("design"),
        "the pre-taint route went through the category chain: {}",
        pre_taint[0]
    );

    // --- Control: an UNTAINTED session on the same binding does fail over ----
    // The non-vacuity leg. The primary now answers 400 for every call, so a
    // clean session exercises the failover path and lands on `backup`.
    let clean = client.create_session("structured", Some("architect"));
    let control = client.prompt(&clean, "Decompose the other loader into modules.");
    assert_eq!(
        control["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "an untainted session must reach the fallback, or the assertion below \
         proves nothing: {control}"
    );
    assert_eq!(
        backup.request_count(),
        1,
        "the fallback is genuinely reachable on this daemon and this binding"
    );
    client.drain_events(Duration::from_millis(300));

    // --- Turn 2 of the tainted session: served on device, nothing egresses ---
    let before = backup.request_count();
    let before_pinned = pinned.request_count();
    let second = client.prompt(&tainted, "Now decompose the neighbouring loader.");
    assert_eq!(
        second["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "the pinned turn is served by the on-device engine: {second}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        backup.request_count(),
        before,
        "BR-7 VIOLATION: a tainted session's turn reached the tier's remote \
         fallback. The taint pin carries no resolution by construction, so it \
         has no fallback to hand out — the failover path must not manufacture \
         one from the configured table (BUG-156)."
    );
    assert_eq!(
        pinned.request_count(),
        before_pinned,
        "BR-7 VIOLATION: a tainted session's turn went out over HTTP to the \
         provider squatting the local tier's id. The pin asserts locality, not \
         a name — an id a remote `[[providers]]` entry answers to is not the \
         engine-backed tier, whatever it is called."
    );
    assert_eq!(
        routes_to(&client, "backup").len(),
        1,
        "only the control session ever announced a route to the fallback: {:?}",
        client.events_named("route_decided")
    );

    assert_no_boundary_bytes();
}

// ===========================================================================
// AC-1 — the direct regression the REQ exists for.
// ===========================================================================

/// A per-category override is what the runtime reads, on a freeform turn.
///
/// The pre-REQ freeform path consulted no configured table at all: it picked
/// between two providers with a ten-word substring list, so a `[[categories]]`
/// row could not have changed where this turn went. Binding `edit` to a
/// provider the tier does **not** name is the cheapest way to show the table is
/// genuinely read — an inert configuration surface would send the turn to the
/// `build` tier's provider instead, and the assertion is by captured bytes on
/// both providers rather than by the event alone.
#[test]
fn a_freeform_turn_reads_the_category_override_not_a_keyword_list() {
    let tier_provider = MockProvider::always(openai_turn("Tier answered.", None, 10, 5));
    let override_provider = MockProvider::always(openai_turn("Override answered.", None, 10, 5));

    let mut config = String::new();
    config.push_str(&remote_provider_block(
        "tier-bound",
        &tier_provider.openai_endpoint(),
        "deepseek-chat",
    ));
    config.push_str(&remote_provider_block(
        "override-bound",
        &override_provider.openai_endpoint(),
        "claude-opus-4",
    ));
    config.push_str(&tier_block("build", "tier-bound"));
    config.push_str(&category_block("edit", "override-bound"));

    let ws = Workspace::new("ac1");
    ws.write_config(&config);
    let script = ws.write_script("Local, done.");
    let daemon = Daemon::spawn(&ws, probe_16gb().script(script));
    let mut client = daemon.connect();

    let session = client.create_session("freeform", None);
    // Deliberately worded with `explain`, one of the ten deleted
    // `AUXILIARY_SIGNALS` words. Pre-REQ that alone sent the turn to the 3B
    // local model with the configured table unread; now the word carries no
    // routing meaning at all.
    let turn = client.prompt(
        &session,
        "Explain the tradeoffs between these two architectures, then apply one.",
    );
    assert_eq!(
        turn["result"]["stop_reason"].as_str(),
        Some("end_turn"),
        "{turn}"
    );
    client.drain_events(Duration::from_millis(300));

    assert_eq!(
        override_provider.request_count(),
        1,
        "the per-category override is what the freeform turn read: {:?}",
        client.events_named("route_decided")
    );
    assert_eq!(
        tier_provider.request_count(),
        0,
        "the tier binding is overridden for this category, so its provider sees \
         nothing"
    );
    let decided = routes_to(&client, "override-bound");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(
        decided[0]["category"].as_str(),
        Some("edit"),
        "{}",
        decided[0]
    );

    assert_no_boundary_bytes();
}
