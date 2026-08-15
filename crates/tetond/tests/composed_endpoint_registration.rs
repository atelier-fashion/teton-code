//! REQ-578 BR-8 / AC-1 / AC-3: **a composed endpoint is a registration the real
//! `config/set` accepts and persists, byte for byte.**
//!
//! The composition rule is unit-tabled in `teton_core::endpoint_composition` and
//! pinned against the vendor catalog by `endpoint_composition_bridge.rs`. Both
//! of those are claims about a *string*. What neither can show is that the
//! string survives the trip a registration actually makes: the daemon's
//! `config/set` handler, `Config::validate`, the document writer, and the
//! read-back through the production loader. BR-8 asks for at least one
//! end-to-end registration that executes the composed result through that path,
//! and this is it.
//!
//! ## Why the CLI is not the vehicle
//!
//! `teton provider add` is where composition happens, and its e2e tests
//! (`crates/teton/tests/cli_e2e.rs`, the REQ-578 section) drive the real binary
//! through the real argv — but they stop one step short of `config/set`, because
//! the step in between writes a credential to the **real OS keychain** and no
//! test may do that (the rule is stated in that file and in `pty_e2e.rs`). So
//! the flow is proven in two halves that meet at one value: the CLI half proves
//! the composed URL is what the flow settles on and echoes, and this file proves
//! that same URL is a registration the daemon accepts and stores unchanged. The
//! value itself is spelled as a literal in both, so the halves cannot drift
//! apart without one of them going red.
//!
//! ## The two registrations
//!
//! | Claim | Payload |
//! |---|---|
//! | AC-1: a pasted base URL persists as the request URL | `compose_endpoint(OpenaiCompatible, "https://api.moonshot.ai/v1")` |
//! | AC-3: the Anthropic default persists as written | `ANTHROPIC_DEFAULT_ENDPOINT` |
//!
//! Both assert on the **persisted document** and on the **live snapshot**, not
//! on the RPC's `applied` flag alone: "the daemon said yes" and "the URL on disk
//! is the one that will be POSTed" are different facts, and only the second is
//! what a user's first turn depends on (LESSON-519's inspect-don't-infer rule).

use serde_json::{json, Value};

#[path = "e2e/harness.rs"]
mod harness;

use harness::{tier_block, Daemon, DaemonOptions, Workspace};
use teton_core::entities::ProviderKind;
use teton_core::{compose_endpoint, ANTHROPIC_DEFAULT_ENDPOINT};

const GIB: u64 = 1024 * 1024 * 1024;

/// The base URL Moonshot's own quickstart hands an OpenAI-compatible SDK — the
/// paste this REQ exists for (BUG-170).
const MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";

/// What AC-1 says must end up in the config file. Spelled out rather than
/// derived from [`compose_endpoint`]: a test whose expectation is computed by
/// the code under test asserts only that the code agrees with itself.
const MOONSHOT_REQUEST_URL: &str = "https://api.moonshot.ai/v1/chat/completions";

/// A deterministic machine, so the first-run probe cannot pick a model and start
/// spending; plus the REQ-575/576 presence seam set to **accept**.
///
/// The presence env is belt-and-braces rather than load-bearing: on the default
/// build `config/set`'s BR-10(b) attestation degrades to allow, so these
/// registrations land either way. Setting it means the same test still passes
/// under the `--all-features` gated leg, where the verifier is real.
fn options() -> DaemonOptions {
    DaemonOptions::default()
        .env("TETON_PROBE_RAM_BYTES", (16 * GIB).to_string())
        .env("TETON_PROBE_DISK_BYTES", (500 * GIB).to_string())
        .env("TETON_PROBE_GPU", "apple-silicon")
        .env("TETON_TEST_SEAMS", "1")
        .env("TETON_PRESENCE_ACCEPT", "1")
}

/// A minimal valid config: one local provider bound to every tier, and
/// deliberately none of the providers these tests register — so a successful
/// `config/set` is visible as a change to the document.
fn base_config() -> String {
    let mut c = String::new();
    c.push_str("[[providers]]\nid = \"local\"\nkind = \"local\"\n\n");
    for tier in ["reflex", "scan", "build", "think"] {
        c.push_str(&tier_block(tier, "local"));
    }
    c
}

/// A `config/set` `RegisterProvider` carrying `endpoint`.
///
/// The `auth_ref` is an env reference to a variable that is deliberately never
/// set: it is a recognized shape (so `Config::validate` accepts it) that resolves
/// to nothing (so no credential, and no keychain, is anywhere near this test).
fn register(id: &str, kind: &str, endpoint: &str, model: &str) -> Value {
    json!({ "update": {
        "op": "register_provider",
        "id": id,
        "kind": kind,
        "endpoint": endpoint,
        "model": model,
        "auth_ref": "env:TETON_REQ578_TEST_CREDENTIAL_ABSENT",
    }})
}

/// The endpoint the live daemon holds for `id`, read out of a `config/get`
/// snapshot.
fn snapshot_endpoint(snapshot: &Value, id: &str) -> Option<String> {
    snapshot["result"]["snapshot"]["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == json!(id))?["endpoint"]
        .as_str()
        .map(str::to_owned)
}

/// **AC-1 + AC-3: what composition produced is what the daemon stores.**
///
/// Two registrations through one daemon, each the output of the registration
/// seam's rule: the composed form of a pasted vendor base URL, and the Anthropic
/// default that BR-3 writes explicitly when no `--endpoint` is given. Each is
/// checked in three places — the RPC applied, the document on disk carries the
/// full request URL (and *not* the base form), and the running config hands the
/// same string back.
#[test]
fn a_composed_base_url_registers_and_persists_as_the_request_url() {
    // The rule's own output, checked against the literal AC-1 names before it
    // is put on the wire. A composition that regressed would fail here rather
    // than register something plausible and wrong.
    let composed = compose_endpoint(ProviderKind::OpenaiCompatible, Some(MOONSHOT_BASE_URL));
    assert_eq!(
        composed.stored.as_deref(),
        Some(MOONSHOT_REQUEST_URL),
        "AC-1: `{MOONSHOT_BASE_URL}` must compose to `{MOONSHOT_REQUEST_URL}` before there is \
         anything worth registering"
    );
    let composed = composed.stored.expect("a class (b) input always composes");

    let ws = Workspace::new("composed-endpoint-registration");
    ws.write_config(&base_config());
    let daemon = Daemon::spawn(&ws, options());
    let mut client = daemon.connect();

    // --- AC-1: the composed OpenAI-compatible request URL --------------------
    let applied = client.call(
        "config/set",
        register("kimi", "openai-compatible", &composed, "kimi-k3"),
    );
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "AC-1: a composed endpoint must be a registration `config/set` accepts — the whole \
         point of composing at the seam is that nothing downstream changes shape: \
         {applied}\ndaemon log:\n{}",
        daemon.log()
    );

    // --- AC-3: the Anthropic default, written explicitly --------------------
    let applied = client.call(
        "config/set",
        register(
            "claude",
            "anthropic",
            ANTHROPIC_DEFAULT_ENDPOINT,
            "claude-opus-5",
        ),
    );
    assert_eq!(
        applied["result"]["applied"].as_bool(),
        Some(true),
        "AC-3: the default endpoint BR-3 writes into config must itself be a valid \
         registration: {applied}\ndaemon log:\n{}",
        daemon.log()
    );

    // --- The document on disk -----------------------------------------------
    let document = std::fs::read_to_string(&ws.config_path).expect("the config was written");
    assert!(
        document.contains(MOONSHOT_REQUEST_URL),
        "AC-1: the persisted endpoint must be the absolute request URL; config.toml:\n{document}"
    );
    assert!(
        !document.contains(&format!("endpoint = \"{MOONSHOT_BASE_URL}\"")),
        "AC-1: the base URL must not survive as the stored endpoint — Teton POSTs what is \
         stored, verbatim, so a base URL here is a 404 on the first turn; \
         config.toml:\n{document}"
    );
    assert!(
        document.contains(ANTHROPIC_DEFAULT_ENDPOINT),
        "AC-3: the Anthropic default is written into the document, not applied at call time \
         — the stored config stays the declared identity; config.toml:\n{document}"
    );

    // --- The running config -------------------------------------------------
    let snapshot = client.call("config/get", json!({}));
    assert_eq!(
        snapshot_endpoint(&snapshot, "kimi").as_deref(),
        Some(MOONSHOT_REQUEST_URL),
        "AC-1: the live snapshot must hand back the composed URL byte-identically — this is \
         the value the adapter POSTs: {snapshot}"
    );
    assert_eq!(
        snapshot_endpoint(&snapshot, "claude").as_deref(),
        Some(ANTHROPIC_DEFAULT_ENDPOINT),
        "AC-3: and the Anthropic default, unchanged: {snapshot}"
    );
}
