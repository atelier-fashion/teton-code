//! Integration test for AC-6, event scoping, and the attach/handshake gates.
//!
//! Two clients attach over a real Unix socket, exchange the handshake, and
//! observe: (1) a session created by one client appears identically in both
//! clients' session lists; (2) a session-scoped event emitted by that creation
//! reaches the creator but **not** the other, unattached client (REQ-568 BR-1);
//! and (3) the daemon and its sessions survive a client disconnecting — a fresh
//! client still *sees* the surviving session in the listing. Further tests cover
//! two clients each prompting a session of their own (AC-1), a connection that
//! never attached anything (AC-2), the attachment gate on the mutating methods
//! (BR-4/AC-4), the seq gaps a filtered stream necessarily has and the fenced
//! response that must complete anyway (AC-6, ADR-A), and the refusal of any
//! method before the handshake.
//!
//! Point (2) inverted at REQ-568: this test used to assert that the second
//! client received the first's `phase_transition` without asking for it, which
//! was the leak written down as a feature. The registry stays shared — sessions
//! outlive their creators and every client can list them — but the *events* are
//! now scoped to the connections that asked for them.
//!
//! ## What REQ-569 inverted in turn
//!
//! REQ-568 left the *grant itself* — `session/attach`, and the `monitor`
//! declaration — available to any handshaked same-UID peer, so scoping was a
//! lock whose key was lying next to it. REQ-569 closes that, and this file is
//! where the closure is asserted at the wire:
//!
//! - `knowing_a_session_id_does_not_let_another_connection_attach` — a peer that
//!   read the id out of `session/list` is still refused `NOT_GRANTED`, while the
//!   creator's own attach is untouched (BR-1/BR-8, AC-5/AC-8).
//! - `a_monitor_declaration_is_refused_without_a_monitor_scope_grant` — replaces
//!   REQ-568's test that a declaration alone bought sight of every session
//!   (BR-2, AC-4).
//! - `a_peers_own_second_connection_cannot_approve_it_a_monitor` — the
//!   regression test for the consent path TASK-108 added and REQ-569's verify
//!   pass removed: it was mintable by one actor holding two connections (F1).
//! - `a_connection_from_the_daemons_own_process_tree_is_refused_attach_and_monitor`
//!   — the ancestry gate, over the real kernel-attested peer pid (BR-4, ADR-A).
//!
//! Because nothing mints a grant until REQ-569 TASK-108's consent flow, the
//! cross-session attaches the older tests performed are refused here rather than
//! served. Where a test needed a second *attached* connection to make some other
//! point, it now has that connection create its own session — attachment by
//! creation is REQ-568's other route into the same set, and the gates below it
//! cannot tell the two apart.
//!
//! ## How an absence is asserted here
//!
//! Every "B did not receive X" claim in this file is decided by *ordering*, not
//! by a timer. A subscription is FIFO, so if a marker envelope published
//! **after** X reaches B, then X either arrived before it or was never
//! delivered at all — and every read goes through [`TestClient::read_line`],
//! which fails the test the moment a forbidden envelope appears. Each negative
//! is bounded by a positive control in the same test: the client that *should*
//! have received the envelope did, and the client that should not still
//! received the daemon-scoped marker, so the test cannot pass by the daemon
//! merely being slow.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_protocol::handshake::{VersionMismatch, VersionSkew};
use teton_protocol::jsonrpc::{error_code, RpcError};
use teton_protocol::{RequestId, SessionId, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
use tetond::harness::permissions::{
    PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
};
use tetond::{server, Daemon};

/// A minimal in-test JSON-RPC client over the daemon socket.
struct TestClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// A session whose envelopes must never reach this client (REQ-568 BR-1).
    ///
    /// Armed once the id is known and then checked on *every* frame this client
    /// reads, so the scoping claim covers the whole conversation rather than the
    /// one drain an assertion happens to look at.
    forbidden_session: Option<String>,
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            forbidden_session: None,
        }
    }

    /// Fail this client's next read that carries an envelope scoped to
    /// `session_id` — the standing form of "B never sees A's session".
    fn forbid_session(&mut self, session_id: &str) {
        self.forbidden_session = Some(session_id.to_owned());
    }

    async fn send(&mut self, id: i64, method: &str, params: Value) {
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut text = serde_json::to_string(&message).unwrap();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_line(&mut self) -> Value {
        self.read_line_raw().await.1
    }

    /// One frame, as the bytes that crossed the socket *and* parsed.
    ///
    /// The raw text is what makes an "the field is absent" claim checkable at
    /// the wire (REQ-569 BR-10): a parsed value can only say the key is missing
    /// from the map, where the text can say the characters never left the
    /// daemon at all.
    async fn read_line_raw(&mut self) -> (String, Value) {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line")
            .unwrap();
        assert!(n > 0, "connection closed unexpectedly");
        let value: Value = serde_json::from_str(&line).unwrap();
        if let Some(forbidden) = self.forbidden_session.as_deref() {
            assert_ne!(
                value["params"].get("session_id").and_then(Value::as_str),
                Some(forbidden),
                "this client is not attached to {forbidden} and must never receive its \
                 envelopes: {value}"
            );
        }
        (line, value)
    }

    /// Reads until the response with a matching id arrives, skipping any event
    /// notifications interleaved on the stream.
    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let value = self.read_line().await;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    /// The response with a matching id, as the raw NDJSON line and parsed.
    async fn read_response_raw(&mut self, id: i64) -> (String, Value) {
        loop {
            let (line, value) = self.read_line_raw().await;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return (line, value);
            }
        }
    }

    /// Reads until an `event` notification with the given event name arrives.
    async fn read_event(&mut self, event_name: &str) -> Value {
        self.read_event_before(event_name, None).await
    }

    /// Reads until the `event` notification named `expect` arrives, failing if
    /// one named `forbidden` reaches this client first.
    ///
    /// This is how an event's *absence* is asserted without a sleep. One
    /// subscription is FIFO, so an envelope published before `expect` either
    /// arrives before it or was never delivered at all — which makes a
    /// filtered-out event a decidable fact rather than a race against a timer.
    async fn read_event_before(&mut self, expect: &str, forbidden: Option<&str>) -> Value {
        loop {
            let value = self.read_line().await;
            if value.get("method").and_then(Value::as_str) != Some("event") {
                continue;
            }
            let name = value["params"]["event"].as_str().unwrap_or_default();
            if Some(name) == forbidden {
                panic!("this client must not have received `{name}`: {value}");
            }
            if name == expect {
                return value;
            }
        }
    }

    /// Send `method` and read frames until its response, returning every event
    /// notification that arrived **before** the response, plus the response.
    ///
    /// Draining to the response is what makes the later negative assertions
    /// exact: the daemon's fence puts every envelope already delivered to this
    /// connection on the wire ahead of the response, so after this call the
    /// client's stream holds nothing published before it.
    async fn call_collecting_events(
        &mut self,
        id: i64,
        method: &str,
        params: Value,
    ) -> (Vec<Value>, Value) {
        self.send(id, method, params).await;
        let mut events = Vec::new();
        loop {
            let frame = self.read_line().await;
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return (events, frame);
            }
            if frame.get("method").and_then(Value::as_str) == Some("event") {
                events.push(frame["params"].clone());
            }
        }
    }

    /// Reads until the event named `expect`, returning everything seen before
    /// it — the drain window a "received only daemon-scoped envelopes" claim is
    /// asserted over.
    async fn collect_events_until(&mut self, expect: &str) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        loop {
            let frame = self.read_line().await;
            if frame.get("method").and_then(Value::as_str) != Some("event") {
                continue;
            }
            let params = frame["params"].clone();
            if params.get("event").and_then(Value::as_str) == Some(expect) {
                return (seen, params);
            }
            seen.push(params);
        }
    }

    async fn handshake(&mut self, id: i64) -> Value {
        self.handshake_declaring(id, false).await
    }

    /// A handshake that may declare `monitor` — the one-time, explicit opt-in
    /// to every session's events (REQ-568 ADR-C).
    async fn handshake_declaring(&mut self, id: i64, monitor: bool) -> Value {
        self.send(
            id,
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": "test-client",
                "client_version": "0.1.0",
                "protocol_min": PROTOCOL_VERSION_MIN,
                "protocol_max": PROTOCOL_VERSION_MAX,
                "monitor": monitor,
            }),
        )
        .await;
        self.read_response(id).await
    }
}

/// A consent window short enough that a test can assert on the *timeout* arm
/// (REQ-569 BR-7) without waiting out the shipped, human-sized one.
///
/// Every refusal on this seam now runs through the consent flow — an ungranted
/// `session/attach` raises a prompt rather than answering `NOT_GRANTED` — so a
/// test whose client does not answer is a test that waits for this window. Two
/// hundred milliseconds is comfortably above the scheduling noise of a loaded
/// runner and comfortably below [`TestClient::read_line_raw`]'s two-second
/// read deadline, which is what keeps the refusal a *consent timeout* rather
/// than a test timeout.
const TEST_CONSENT_WINDOW: Duration = Duration::from_millis(200);

/// The daemon every test in this file drives: the real one, with a consent
/// window a test can outlast.
fn test_daemon() -> Arc<Daemon> {
    Arc::new(Daemon::new().with_consent_timeout(TEST_CONSENT_WINDOW))
}

/// A daemon whose presence mechanism is satisfiable (REQ-570 AC-2b).
///
/// The granted path cannot be reached otherwise: the real mechanism refuses
/// without a human, and CI has none. See `attest::AcceptingVerifier` for why
/// that double cannot reach a shipped build.
fn test_daemon_with_presence() -> Arc<Daemon> {
    Arc::new(
        Daemon::new()
            .with_consent_timeout(TEST_CONSENT_WINDOW)
            .with_presence_verifier(Box::new(tetond::attest::AcceptingVerifier::default())),
    )
}

/// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
/// can return the same value for two calls within one clock tick.
fn temp_socket(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

/// The events in `events` scoped to `session_id`.
fn events_for<'a>(events: &'a [Value], session_id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("session_id").and_then(Value::as_str) == Some(session_id))
        .collect()
}

/// Create a structured session on `client`, checking on the way that the
/// creator receives its own create-time envelope before the create response.
///
/// That check is a claim about the daemon, not about any client: the creator is
/// attached inside `handle_session_create` *before* the `phase_transition` is
/// published, so the connection that asked for the session is already a legal
/// recipient when the envelope goes out.
async fn create_structured_session(client: &mut TestClient, id: i64) -> String {
    let (events, created) = client
        .call_collecting_events(
            id,
            "session/create",
            json!({"mode": "structured", "phase": "spec"}),
        )
        .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create failed: {created}"))
        .to_owned();
    assert!(
        !events_for(&events, &sid).is_empty(),
        "the creator must receive its own session's create-time event: {events:?}"
    );
    sid
}

/// Register a provider a turn can actually route to, and fail on.
///
/// The same fixture `event_response_ordering.rs` uses: a genuine provider (a
/// declared `model`, so REQ-557 admits it to the provider map) whose `auth_ref`
/// names an env var that is not set. Credential resolution happens before any
/// socket is opened and is classified as settled, so every turn publishes its
/// session-scoped `route_decided` and then fails immediately — no network, no
/// keychain, no model. The scoping tests need *real session-scoped traffic*,
/// not a successful answer.
async fn register_a_provider_every_turn_fails_on(client: &mut TestClient) {
    let (_, registered) = client
        .call_collecting_events(
            900,
            "config/set",
            json!({ "update": {
                "op": "register_provider",
                "id": "scoping",
                "kind": "openai-compatible",
                "endpoint": "http://127.0.0.1:1/v1/chat/completions",
                "model": "deepseek-chat",
                "auth_ref": "env:TETON_SCOPING_TEST_CREDENTIAL_ABSENT",
            }}),
        )
        .await;
    assert_eq!(
        registered["result"]["applied"].as_bool(),
        Some(true),
        "provider registration failed: {registered}"
    );
    let (_, routed) = client
        .call_collecting_events(
            901,
            "config/set",
            json!({ "update": {
                "op": "set_tier_binding",
                "tier": "build",
                "provider_id": "scoping",
            }}),
        )
        .await;
    assert_eq!(
        routed["result"]["applied"].as_bool(),
        Some(true),
        "tier binding failed: {routed}"
    );
}

/// The `session/list` row for `session_id`, failing if the session is not
/// listed at all — REQ-569 reduces the payload, never the listing.
fn row_for<'a>(list_response: &'a Value, session_id: &str) -> &'a Value {
    list_response["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("session/list failed: {list_response}"))
        .iter()
        .find(|row| row["session_id"].as_str() == Some(session_id))
        .unwrap_or_else(|| panic!("{session_id} must still be listed: {list_response}"))
}

fn session_ids(list_response: &Value) -> Vec<String> {
    list_response["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn two_clients_share_sessions_and_daemon_survives_client_exit() {
    let path = temp_socket("mc");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    // Both clients attach and handshake.
    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());

    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    // Client A creates a structured session.
    a.send(
        2,
        "session/create",
        json!({"mode": "structured", "phase": "spec"}),
    )
    .await;

    // BR-8: A's own flow is unchanged — the creator is attached to what it made
    // and receives its first event, ahead of the response (the daemon's fence).
    // Read in that order because `read_response` would otherwise skip past it.
    let event = a.read_event("phase_transition").await;
    let sid = event["params"]["session_id"].as_str().unwrap().to_owned();
    assert_eq!(event["params"]["to_phase"].as_str().unwrap(), "spec");
    let created = a.read_response(2).await;
    assert_eq!(created["result"]["session_id"].as_str().unwrap(), sid);

    // BR-1: B never created or attached that session, so the envelope was not
    // delivered to it. Asserted by ordering, not by a timeout: a third client's
    // handshake publishes a daemon-scoped `daemon_client_attach` *after* the
    // phase transition, and one subscription is FIFO — so B reaching the attach
    // without passing the phase transition means the phase transition was
    // filtered rather than merely slow.
    let mut c = TestClient::connect(&path).await;
    assert!(c.handshake(1).await.get("result").is_some());
    let daemon_scoped = b
        .read_event_before("daemon_client_attach", Some("phase_transition"))
        .await;
    // BR-2: and that daemon-scoped envelope belongs to no session, which is
    // exactly why every handshaked connection still gets it.
    assert!(
        daemon_scoped["params"].get("session_id").is_none(),
        "a daemon-scoped envelope carries no session: {daemon_scoped}"
    );

    // AC-6: the session *registry* is still shared — scoping events changed
    // nothing about which sessions a client can see exist.
    a.send(3, "session/list", json!({})).await;
    let list_a = a.read_response(3).await;
    b.send(3, "session/list", json!({})).await;
    let list_b = b.read_response(3).await;
    assert_eq!(session_ids(&list_a), vec![sid.clone()]);
    assert_eq!(session_ids(&list_b), session_ids(&list_a));

    // REQ-569 BR-1/BR-6, the change this REQ is: attaching is no longer
    // something a connection can help itself to. B knows the id — it just read
    // it out of `session/list` — and that buys it a *question put to A*, not an
    // attachment (BR-8: ids are names, grants are credentials). A is attached
    // and never answers, so the window closes and B is refused.
    b.send(4, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    let refused = b.read_response(4).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "a connection that created nothing must not attach on its own say-so: {refused}"
    );

    // And the refusal is real rather than cosmetic: A clears the session and
    // the envelope does not reach B. Bounded by ordering, not a timer — B's
    // forbidden-session arm fails any frame scoped to `sid`, and the
    // daemon-scoped marker published afterwards is what proves B's stream was
    // live through the window.
    b.forbid_session(&sid);
    a.send(5, "session/clear", json!({"session_id": sid.clone()}))
        .await;
    assert!(a.read_response(5).await.get("result").is_some());

    // Client A exits. The daemon and its sessions must survive.
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;

    b.send(6, "session/list", json!({})).await;
    let list_b_after = b.read_response(6).await;
    assert_eq!(session_ids(&list_b_after), vec![sid.clone()]);

    // A fresh client is refused the same way — the survival of the session is
    // read off the listing above, not off an attach anyone could have.
    //
    // This is the resume flow, and since TASK-108 it is open *through an
    // explicit consent step and nothing else* (BR-6). A is gone, so nothing is
    // attached to the session and the prompt is rendered by D itself (the
    // second arm) — which D here never answers, so the window closes and the
    // fresh client stays out. `ac_matrix::ac6_two_clients_share_sessions_daemon_
    // survives_exit` drives the answering half of the same flow.
    let mut d = TestClient::connect(&path).await;
    assert!(d.handshake(1).await.get("result").is_some());
    d.send(2, "session/attach", json!({"session_id": sid}))
        .await;
    let refused_fresh = d.read_response(2).await;
    assert_eq!(
        refused_fresh["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "a fresh client is let in by a decision or not at all: {refused_fresh}"
    );

    // The marker that closes B's window: D's handshake published a
    // daemon-scoped `daemon_client_attach` *after* A's `context_cleared`, and
    // one subscription is FIFO — so B reaching it having read no `sid`-scoped
    // frame means the clear was filtered, not merely late.
    let marker = b.read_event("daemon_client_attach").await;
    assert!(
        marker["params"].get("session_id").is_none(),
        "the marker is the daemon-scoped envelope BR-2 keeps broadcasting: {marker}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 AC-1: two clients, two sessions, a full prompt turn on each — and
/// neither client sees a single envelope of the other's session.
///
/// The spec's headline criterion, asserted on raw NDJSON at the socket rather
/// than through any client's rendering (BR-3): the filter under test is the
/// daemon's, so the observation point is the wire.
///
/// Both halves of AC-1 are here. The **negative** — B receives none of A's
/// envelopes and vice versa — is armed with [`TestClient::forbid_session`], so
/// it holds over every frame either client reads, not just the ones an
/// assertion inspects. The **positive controls** that bound it, in the same
/// test and the same window:
///
/// 1. each client *did* receive its own session's create-time and turn events,
///    so the envelopes exist and delivery works;
/// 2. both clients receive the daemon-scoped `daemon_client_attach` published
///    by a third client's handshake — which is also the ordering fence for the
///    negative. That attach is published *after* every session envelope in this
///    test, and one subscription is FIFO, so reaching it without having passed
///    the other session's envelopes means they were filtered, not merely late.
///
/// The turn fails on an unresolvable credential (see
/// [`register_a_provider_every_turn_fails_on`]); a turn that *routes* is what
/// produces the session-scoped `route_decided`, and the failure keeps the test
/// hermetic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_prompting_their_own_sessions_see_only_their_own_envelopes() {
    let path = temp_socket("scope-two");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    register_a_provider_every_turn_fails_on(&mut a).await;

    // A's own session. The creator is attached *before* the creation event is
    // published, so the session-scoped `phase_transition` reaches the connection
    // that made it — a delivery the CLI cannot observe (its own `session_id` is
    // still unset at that instant), which is exactly why it is pinned here.
    let (created_a_events, created_a) = a
        .call_collecting_events(
            10,
            "session/create",
            json!({"mode": "structured", "phase": "implement"}),
        )
        .await;
    let sid_a = created_a["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create failed: {created_a}"))
        .to_owned();
    assert!(
        !events_for(&created_a_events, &sid_a).is_empty(),
        "the creating connection must receive its session's create-time event \
         ahead of the create response: {created_a_events:?}"
    );

    // B's own session — and from here B must never see anything of A's.
    b.forbid_session(&sid_a);
    let (created_b_events, created_b) = b
        .call_collecting_events(
            10,
            "session/create",
            json!({"mode": "structured", "phase": "implement"}),
        )
        .await;
    let sid_b = created_b["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create failed: {created_b}"))
        .to_owned();
    assert_ne!(sid_a, sid_b, "the two clients must hold distinct sessions");
    assert!(
        !events_for(&created_b_events, &sid_b).is_empty(),
        "the creating connection must receive its session's create-time event \
         ahead of the create response: {created_b_events:?}"
    );
    a.forbid_session(&sid_b);

    // A full turn on each session, A first.
    let prompt = |sid: &str| {
        json!({
            "session_id": sid,
            "prompt": [{ "type": "text", "text": "explain this" }],
        })
    };

    let (turn_a_events, turn_a) = a
        .call_collecting_events(11, "session/prompt", prompt(&sid_a))
        .await;
    assert!(
        turn_a.get("error").is_some(),
        "expected the unresolvable-credential error response: {turn_a}"
    );
    assert!(
        !events_for(&turn_a_events, &sid_a).is_empty(),
        "A must receive its own turn's session-scoped events: {turn_a_events:?}"
    );

    let (turn_b_events, turn_b) = b
        .call_collecting_events(11, "session/prompt", prompt(&sid_b))
        .await;
    assert!(
        turn_b.get("error").is_some(),
        "expected the unresolvable-credential error response: {turn_b}"
    );
    assert!(
        !events_for(&turn_b_events, &sid_b).is_empty(),
        "B must receive its own turn's session-scoped events: {turn_b_events:?}"
    );

    // The marker. A third client's handshake publishes a daemon-scoped attach
    // *after* every session envelope above; both A and B must reach it, and
    // neither may pass one of the other's envelopes on the way (the guard).
    let mut c = TestClient::connect(&path).await;
    assert!(c.handshake(1).await.get("result").is_some());

    for (name, client) in [("A", &mut a), ("B", &mut b)] {
        let marker = client.read_event("daemon_client_attach").await;
        assert!(
            marker["params"].get("session_id").is_none(),
            "{name}'s marker must be the daemon-scoped envelope every handshaked \
             connection still receives (BR-2): {marker}"
        );
    }

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 AC-2: a handshaked connection that never created or attached a
/// session receives only daemon-scoped envelopes.
///
/// The observer handshakes *last* of the three working clients, so its stream
/// begins clean: it never sees the others' attach announcements (a newcomer is
/// announced before it subscribes, so nobody hears their own attach), and the
/// only `daemon_client_attach` it can reach is the marker client's at the end.
/// Everything between is a window in which two other sessions transition phase
/// and clear their transcripts — and the observer's copy of that window must be
/// empty of session scope.
///
/// The positive controls: the two working clients each receive their own
/// session's events inside that same window (so the envelopes were published
/// and delivery worked), and the observer does receive the daemon-scoped marker
/// that ends it (so its stream was live the whole time).
#[tokio::test]
async fn a_client_that_never_attached_receives_only_daemon_scoped_envelopes() {
    let path = temp_socket("scope-none");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    let mut observer = TestClient::connect(&path).await;
    assert!(observer.handshake(1).await.get("result").is_some());

    let sid_a = create_structured_session(&mut a, 2).await;
    let sid_b = create_structured_session(&mut b, 2).await;

    // More session-scoped traffic in the window, on a path that needs no model.
    for (sid, client) in [(&sid_a, &mut a), (&sid_b, &mut b)] {
        let (events, cleared) = client
            .call_collecting_events(3, "session/clear", json!({"session_id": sid}))
            .await;
        assert!(cleared.get("result").is_some(), "clear failed: {cleared}");
        assert!(
            events_for(&events, sid)
                .iter()
                .any(|e| e["event"] == "context_cleared"),
            "the attached client must receive its own `context_cleared`: {events:?}"
        );
    }

    // Close the window with the daemon-scoped marker.
    let mut marker_client = TestClient::connect(&path).await;
    assert!(marker_client.handshake(1).await.get("result").is_some());

    let (seen, marker) = observer.collect_events_until("daemon_client_attach").await;
    let scoped: Vec<&Value> = seen
        .iter()
        .filter(|e| e.get("session_id").is_some())
        .collect();
    assert!(
        scoped.is_empty(),
        "a connection attached to nothing received session-scoped envelopes: {scoped:?}"
    );
    assert!(
        marker.get("session_id").is_none(),
        "the marker is the daemon-scoped envelope BR-2 keeps broadcasting: {marker}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-2: `monitor` is grant-gated with its own scope, so declaring it
/// is refused — and the **handshake itself** fails.
///
/// This inverts REQ-568's `a_monitor_declared_at_handshake_receives_another_
/// clients_events`, which pinned the old behaviour: a declaration was enough,
/// and any same-UID peer could ask for sight of every session on the machine.
/// REQ-568 shipped that knowingly ("a monitor exists because someone asked for
/// one"); REQ-569 is the REQ that says asking is not standing.
///
/// The load-bearing half is the *second* assertion. A daemon that answered the
/// handshake with an error but went on treating the connection as handshaked —
/// or that quietly set `monitor: false` and handed back a success — would pass
/// a test that only looked at the error code, while leaving the client
/// believing something untrue about what it can see. So the test asks the
/// connection to do something only a handshaked connection may do, and requires
/// it to be told the handshake never happened.
///
/// The refusal is **terminal**: since REQ-569's verify pass there is no consent
/// path to `monitor` at all, so `NOT_GRANTED` is the whole answer rather than
/// the "nobody was available to ask" arm of one (F1). The arm that did exist was
/// mintable by one peer holding two connections —
/// `a_peers_own_second_connection_cannot_approve_it_a_monitor` is the regression
/// test for that, and it is this test's other half: this one says an unattached
/// daemon refuses, that one says an *attached* daemon refuses too, which is the
/// case the removed path served.
#[tokio::test]
async fn a_monitor_declaration_is_refused_without_a_monitor_scope_grant() {
    let path = temp_socket("monitor");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut monitor = TestClient::connect(&path).await;
    let refused = monitor.handshake_declaring(1, true).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::NOT_GRANTED),
        "a monitor declaration without a monitor-scope grant must be refused: {refused}"
    );

    // The handshake *failed*: not a success with `monitor` silently downgraded,
    // and not a refusal the daemon then ignored. The connection is still
    // pre-handshake, which is the only state that produces this answer.
    monitor.send(2, "session/list", json!({})).await;
    let before_handshake = monitor.read_response(2).await;
    assert_eq!(
        before_handshake["error"]["code"].as_i64(),
        Some(error_code::INVALID_REQUEST),
        "a refused monitor must not be left holding a working connection: {before_handshake}"
    );

    // The positive control, in the same test: the identical handshake without
    // the declaration succeeds. Without it, a daemon that refused every
    // handshake would pass everything above.
    let mut plain = TestClient::connect(&path).await;
    assert!(
        plain.handshake(1).await.get("result").is_some(),
        "only the declaration is refused — an ordinary client still connects"
    );
    plain
        .send(
            2,
            "session/create",
            json!({"mode": "structured", "phase": "spec"}),
        )
        .await;
    assert!(plain.read_response(2).await["result"]["session_id"]
        .as_str()
        .is_some());

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// **REQ-569 verify, F3.** A requester that goes away mid-consent leaves the
/// daemon holding nothing.
///
/// `handle_client`'s teardown aborts the in-flight `session/attach` tasks, and
/// aborting a task **drops its future at the await point** — so none of
/// `await_decision`'s three endings runs, and before this fix the waiter stayed
/// in the daemon-wide registry for the life of the daemon. A `connect → attach →
/// disconnect` loop grew it without bound, and since F4 every leaked entry also
/// consumed one of the requester's three consent slots for a request that could
/// never be answered.
///
/// The daemon here runs the **shipped** thirty-second consent window rather than
/// this file's shortened one, and that is the whole design of the test: with a
/// 200 ms window a leak and a fix look identical after 200 ms, and the assertion
/// would pass against the bug. Under the real window the timeout arm cannot
/// possibly have run by the time the poll below gives up, so anything that
/// empties the registry is the teardown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requester_that_disconnects_mid_consent_leaves_no_waiter_behind() {
    let path = temp_socket("consent-leak");
    let listener = server::bind_listener(&path).unwrap();
    // The shipped window, deliberately: see the doc comment.
    let daemon = Arc::new(Daemon::new());
    let held = Arc::clone(&daemon);
    let server_task = tokio::spawn(server::serve(listener, daemon));

    // The owner keeps the session attached, so the requester's attach takes the
    // peer arm and waits on a decision the owner never makes.
    let mut owner = TestClient::connect(&path).await;
    assert!(owner.handshake(1).await.get("result").is_some());
    owner
        .send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = owner.read_response(2).await["result"]["session_id"]
        .as_str()
        .expect("the owner's session is created")
        .to_owned();

    {
        let mut requester = TestClient::connect(&path).await;
        assert!(requester.handshake(1).await.get("result").is_some());
        requester
            .send(2, "session/attach", json!({"session_id": sid.clone()}))
            .await;
        // Wait until the prompt is genuinely outstanding — reading it off the
        // owner's stream rather than sleeping, so the drop below lands on a
        // *live* consent rather than on a race the test never entered.
        let prompt = owner.read_event("attach_consent_requested").await;
        assert_eq!(prompt["params"]["session_id"].as_str(), Some(sid.as_str()));
        assert_eq!(held.consents.pending_count(), 1);
        // ...and the socket closes here, with the decision still outstanding.
    }

    // Teardown is another task's work, so poll rather than assume — bounded
    // well under the thirty-second window whose expiry would otherwise be a
    // second explanation for an empty registry.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while held.consents.pending_count() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        held.consents.pending_count(),
        0,
        "a consent whose requester disconnected must not sit in the registry \
         until the daemon exits"
    );
    assert!(
        held.grants.is_empty(),
        "and it must certainly not have minted anything: {} grants",
        held.grants.len()
    );

    // The surface that rendered the prompt is told the request is over, so a
    // security dialog is not left on screen asking about a connection that no
    // longer exists.
    let retired = owner.read_event("attach_refused").await;
    assert_eq!(
        retired["params"]["reason"].as_str(),
        Some("requester_gone"),
        "the prompt must be retired, and named as the peer leaving rather than \
         as the user being slow: {retired}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// **REQ-569 verify, F1 — the regression test for a working attack.**
///
/// This test used to be
/// `a_monitor_consent_granted_by_an_attached_client_produces_a_working_monitor`,
/// and it passed. It was also, exactly as written, the exploit: one actor
/// holding two connections could mint itself a daemon-wide monitor with no
/// human anywhere in the loop.
///
/// The sequence is the one below. `session/create` is ungated by design, so
/// connection A creates a throwaway session — which makes A *attached*, which
/// makes A a registered consent surface, which makes A the only candidate the
/// monitor routing rule ("any attached peer other than the requester") had to
/// pick from. Connection B then declares `monitor`, the prompt is routed to A,
/// and A answers its own request. Nothing in the daemon noticed: the two
/// connections have different `ConnectionId`s, so `self_approved_by` was false
/// and even the self-approval log line stayed silent.
///
/// No approver predicate over these primitives fixed it — the daemon cannot tell
/// an attacker's second connection from a user's real client, and a peer-pid
/// check only forces a fork — so REQ-569 **removed** the consent path entirely,
/// leaving `monitor` with no socket-reachable minter.
///
/// **REQ-570 AC-2: the path is back, and the attack must still fail.** The
/// missing piece was never a better predicate over connection ids; it was a way
/// to reach the *machine's human*. A granting answer now requires a presence
/// attestation the daemon itself verified, so the attacker's second connection
/// has to produce a person at the keyboard.
///
/// That changes what this test asserts, and it is a stronger claim than before:
///
/// 1. A **is** asked now — that is AC-2b's capability, and the prompt reaching A
///    is the thing that used to be the whole vulnerability.
/// 2. A answering `granted` **mints nothing**, because no human is behind it.
///    This is the load-bearing line: before REQ-570 this exact exchange handed B
///    sight of every session on the machine.
/// 3. B's handshake still fails, and B is not left holding a working connection
///    with `monitor` silently downgraded.
///
/// Asserting the *absence* of a grant rather than the presence of an error is
/// deliberate: a daemon that refused loudly and granted anyway would pass an
/// error-code check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_own_second_connection_cannot_approve_it_a_monitor() {
    let path = temp_socket("mon-attack");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let held = Arc::clone(&daemon);
    let server_task = tokio::spawn(server::serve(listener, daemon));

    // Connection A: the attacker's first half. It creates a throwaway session,
    // which is all it takes to become an attached, registered surface.
    let mut conn_a = TestClient::connect(&path).await;
    assert!(conn_a.handshake(1).await.get("result").is_some());
    conn_a
        .send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = conn_a.read_response(2).await["result"]["session_id"]
        .as_str()
        .expect("the throwaway session is created")
        .to_owned();

    // Connection B: the attacker's second half, asking for sight of every
    // session on the machine.
    let mut conn_b = TestClient::connect(&path).await;
    let handshake_b = tokio::spawn(async move {
        let refused = conn_b.handshake_declaring(1, true).await;
        (conn_b, refused)
    });

    // (1) A is asked. Since REQ-570 the monitor path exists again, so the
    // prompt the attack depends on really does arrive — the fixture is not
    // passing because nothing happened.
    let prompt = conn_a.read_event("attach_consent_requested").await;
    assert_eq!(
        prompt["params"]["scope"].as_str(),
        Some("monitor"),
        "the monitor request must reach the attached peer — this is the exchange \
         that used to be the vulnerability: {prompt}"
    );

    // (2) **The load-bearing line.** A answers `granted` — the attacker
    // approving its own request through its other connection, exactly the F1
    // attack — and it mints nothing, because no human was verified.
    let request_id = prompt["params"]["request_id"]
        .as_str()
        .expect("the prompt carries its request id")
        .to_owned();
    conn_a
        .send(
            3,
            "attach/consent",
            json!({"request_id": request_id, "outcome": {"outcome": "granted"}}),
        )
        .await;
    let answer = conn_a.read_response(3).await;
    assert!(
        answer["error"].is_object(),
        "an approval with no verified human must be refused, not accepted: {answer}"
    );
    assert!(
        held.grants.is_empty(),
        "AC-2: the two-connection monitor attack must mint nothing — {} grants",
        held.grants.len()
    );

    // (3) And B's handshake fails rather than leaving it a working connection
    // with `monitor` quietly downgraded.
    let (mut conn_b, refused) = handshake_b.await.expect("the handshake task completes");
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::NOT_GRANTED),
        "a monitor declaration nobody could attest must be refused: {refused}"
    );
    conn_b.send(2, "session/list", json!({})).await;
    assert_eq!(
        conn_b.read_response(2).await["error"]["code"].as_i64(),
        Some(error_code::INVALID_REQUEST),
        "a refused monitor must not be left holding a working connection"
    );
    assert!(
        held.grants.is_empty(),
        "and still nothing minted: {} grants",
        held.grants.len()
    );
    let _ = sid;

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-6: the reader loop keeps serving the connection whose consent is
/// pending.
///
/// The flow is a round trip through the *same* socket — a client asks to
/// attach, and the answer may have to come back on that very connection (BR-6's
/// second arm, where the requester renders its own prompt). A daemon that
/// awaited the decision on the reader loop could never read the frame that
/// ends the wait: the flow would deadlock on itself, and the only symptom
/// would be every ungranted attach timing out, which looks exactly like the
/// fail-closed behaviour working.
///
/// So the assertion is *ordering*, not liveness: a request issued after the
/// attach is answered **before** it. A blocked loop cannot produce that, and no
/// amount of waiting makes a blocked loop produce it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reader_loop_keeps_serving_while_a_consent_is_pending() {
    let path = temp_socket("live");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut owner = TestClient::connect(&path).await;
    assert!(owner.handshake(1).await.get("result").is_some());
    owner
        .send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = owner.read_response(2).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut newcomer = TestClient::connect(&path).await;
    assert!(newcomer.handshake(1).await.get("result").is_some());
    newcomer
        .send(2, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    // Issued while the attach is still awaiting a decision.
    newcomer.send(3, "session/list", json!({})).await;

    let listed = newcomer.read_response(3).await;
    assert!(
        listed.get("result").is_some(),
        "a pending consent must not stop this connection being served: {listed}"
    );

    // The attach then resolves on its own timetable — after the request that
    // overtook it.
    let refused = newcomer.read_response(2).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "{refused}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-1/BR-8 at the raw RPC surface (AC-5/AC-8): knowing a session id
/// does not enable attaching to it.
///
/// B learns the id exactly as any same-UID peer would — by asking
/// `session/list`, which REQ-569 deliberately keeps open — and is still
/// refused. That is the BR-8 claim in one test: ids are names, grants are
/// credentials.
///
/// Three things are pinned beyond the refusal itself:
///
/// 1. **The creator is unaffected.** A's own attach to what it made still
///    succeeds, so the gate discriminates on standing rather than just closing
///    the method.
/// 2. **No existence oracle.** A session that exists and a name that never did
///    draw the *same* code, so B cannot confirm a guessed id by which refusal
///    it drew (the reason the grant check precedes `sessions.get`).
/// 3. **The refusal is not `NOT_ATTACHED`.** That code names `session/attach`
///    as the remedy, and here `session/attach` is the thing being refused — a
///    client folding the two together would loop.
#[tokio::test]
async fn knowing_a_session_id_does_not_let_another_connection_attach() {
    let path = temp_socket("grant-attach");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    a.send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = a.read_response(2).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The creator's own attach: unchanged by REQ-569.
    a.send(3, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    let mine = a.read_response(3).await;
    assert_eq!(
        mine["result"]["session"]["session_id"].as_str(),
        Some(sid.as_str()),
        "the creator must still attach to what it created: {mine}"
    );

    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());
    b.send(2, "session/list", json!({})).await;
    assert_eq!(
        session_ids(&b.read_response(2).await),
        vec![sid.clone()],
        "the listing stays open — that is what makes this test mean something"
    );

    for (n, (case, target)) in [
        ("a session that exists", sid.clone()),
        ("a name no session ever had", "sess-imaginary".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 3 + i64::try_from(n).unwrap();
        b.send(id, "session/attach", json!({"session_id": target}))
            .await;
        let refused = b.read_response(id).await;
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(error_code::CONSENT_TIMEOUT),
            "{case}: an unapproved attach must be refused: {refused}"
        );
    }

    // And the refusal really did leave B with nothing, rather than with an
    // attachment it can use: the mutating gate is the observable form of "no
    // grant was minted" at the wire (BR-7).
    b.send(9, "session/clear", json!({"session_id": sid.clone()}))
        .await;
    assert_eq!(
        b.read_response(9).await["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "a timed-out consent must not have attached B to anything"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-4 / ADR-A at the raw RPC surface: a connection whose process
/// descends from the daemon's own is refused attach and monitor outright, with
/// no consent path and no session lookup.
///
/// The ancestry here is **real**, not injected: the daemon is told its own
/// process is this test process (`DaemonProcess::Own`), and the clients connect
/// from that same process, so the kernel-attested peer pid genuinely resolves to
/// a member of the daemon's process tree — the walk in `tetond::peer` runs for
/// real over `getsockopt(LOCAL_PEERPID)` / `SO_PEERCRED`. It is the in-process
/// analogue of the tool child AC-1 names; TASK-109 drives the genuine spawned
/// descendant end to end.
///
/// `ATTACH_FORBIDDEN` rather than `NOT_GRANTED` is the assertion that pins the
/// *ordering*. A descendant holds no grant either, so a daemon that ran the
/// grant check first would refuse it too — and look correct — right up until
/// TASK-108 puts a consent request in the `NOT_GRANTED` branch, at which point
/// the daemon's own children would start being offered consent prompts.
#[tokio::test]
async fn a_connection_from_the_daemons_own_process_tree_is_refused_attach_and_monitor() {
    let path = temp_socket("ancestry");
    let listener = server::bind_listener(&path).unwrap();
    let me = i32::try_from(std::process::id()).unwrap();
    let daemon = Arc::new(
        Daemon::new()
            .with_daemon_process(server::DaemonProcess::Own(me))
            .with_consent_timeout(TEST_CONSENT_WINDOW),
    );
    // A real session id to aim at, seeded straight into the registry.
    //
    // This used to be created over the socket by a descendant client, on the
    // premise — stated in this test's original comment — that "`session/create`
    // is not gated". REQ-570 BR-10(a) closed exactly that: a daemon descendant
    // may no longer create and drive its own session on the user's provider
    // credits (BUG-162's `session/create` row). Every client in this test is a
    // descendant, because the daemon's "own process" is the test process, so
    // there is no longer any socket path to a session id here.
    //
    // What this test asserts is unchanged and is about `session/attach`: a
    // descendant is refused, and refused *identically* whether the target
    // exists or not.
    let sid = daemon
        .sessions
        .create(teton_protocol::SessionMode::Freeform, None, None)
        .expect("the registry accepts a freeform session")
        .session_id
        .to_string();

    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut owner = TestClient::connect(&path).await;
    assert!(
        owner.handshake(1).await.get("result").is_some(),
        "a descendant that declares no monitor still handshakes"
    );

    let mut child = TestClient::connect(&path).await;
    assert!(child.handshake(1).await.get("result").is_some());

    for (n, (case, target)) in [
        ("a session that exists", sid.clone()),
        ("a name no session ever had", "sess-imaginary".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 2 + i64::try_from(n).unwrap();
        child
            .send(id, "session/attach", json!({"session_id": target}))
            .await;
        let refused = child.read_response(id).await;
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(error_code::ATTACH_FORBIDDEN),
            "{case}: a daemon descendant must be forbidden, not merely ungranted: {refused}"
        );
    }

    // The monitor declaration, refused at the handshake and for the ancestry
    // reason rather than the grant one.
    let mut watcher = TestClient::connect(&path).await;
    let refused = watcher.handshake_declaring(1, true).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::ATTACH_FORBIDDEN),
        "a daemon descendant must not be able to declare monitor: {refused}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 BR-4 / AC-4: the mutating methods are refused for a session the
/// connection never attached, and the same calls go through once it has.
///
/// Driven over the raw socket rather than through the CLI, which is the whole
/// point: a gate a client enforces is a rendering choice, and BUG-155 is what
/// happens when the test surface is not the enforcement surface (LESSON-484).
/// Client B here is an arbitrary same-UID peer holding a session id it did not
/// create — `session/list` hands it one to anyone who asks — so this is the
/// exact shape of the finding.
///
/// **The post-attach prompt assertion is a code transition, not a success.**
/// This daemon has no provider configured and no local tier answered, so B's
/// second `session/prompt` reaches the runtime, runs, and comes back
/// `UNKNOWN_PROVIDER` (-32002): "nothing can serve this turn". That is the
/// evidence the gate opened — `NOT_ATTACHED` is issued *before* the runtime is
/// consulted, as is the only other pre-runtime refusal (`UNKNOWN_SESSION`), so
/// an answer that is neither of those was produced by the turn itself. Pinning
/// -32002 exactly would pin the fixture's lack of a provider rather than the
/// gate, so the two pre-runtime codes are excluded instead.
/// `session/clear` needs no model at all, so its post-attach half is asserted
/// as an outright success.
#[tokio::test]
async fn mutating_methods_are_refused_until_the_connection_attaches() {
    let path = temp_socket("gate-attach");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    a.send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let created = a.read_response(2).await;
    let sid = created["result"]["session_id"].as_str().unwrap().to_owned();

    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    // B learns the id the way any same-UID peer would: by asking.
    b.send(2, "session/list", json!({})).await;
    assert_eq!(session_ids(&b.read_response(2).await), vec![sid.clone()]);

    let prompt = json!({
        "session_id": sid.clone(),
        "prompt": [{"type": "text", "text": "what has this session been told?"}],
    });

    b.send(3, "session/prompt", prompt.clone()).await;
    let refused_prompt = b.read_response(3).await;
    assert_eq!(
        refused_prompt["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "an unattached prompt must be refused: {refused_prompt}"
    );

    b.send(4, "session/clear", json!({"session_id": sid.clone()}))
        .await;
    let refused_clear = b.read_response(4).await;
    assert_eq!(
        refused_clear["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "an unattached clear must be refused: {refused_clear}"
    );

    // REQ-569 BR-1: B cannot attach to A's session at all any more — knowing
    // the id is not standing (asserted on its own in
    // `knowing_a_session_id_does_not_let_another_connection_attach`). So the
    // *served* half of this gate is shown on a session B is legitimately
    // attached to: one it created, which attaches its creator (REQ-568 BR-1).
    // The gate under test is `may_drive`, and what it reads is the attachment
    // set — which route put the session in that set is not its business.
    b.send(5, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    let refused_attach = b.read_response(5).await;
    assert_eq!(
        refused_attach["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "B holds no grant for A's session and nobody approved one: {refused_attach}"
    );

    b.send(6, "session/create", json!({"mode": "freeform"}))
        .await;
    let own = b.read_response(6).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(own, sid, "B's own session must be a different one");

    b.send(7, "session/clear", json!({"session_id": own.clone()}))
        .await;
    let cleared = b.read_response(7).await;
    assert_eq!(
        cleared["result"]["blocks_dropped"].as_u64(),
        Some(0),
        "attached to its own session, the clear is served: {cleared}"
    );

    let own_prompt = json!({
        "session_id": own,
        "prompt": [{"type": "text", "text": "what has this session been told?"}],
    });
    b.send(8, "session/prompt", own_prompt).await;
    let served = b.read_response(8).await;
    assert_ne!(
        served["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "attached to its own session, the prompt must reach the runtime: {served}"
    );
    // Neither of the two refusals `spawn_prompt_turn` can issue *before* the
    // runtime. Ruling both out is what makes the transition mean "it was
    // served" rather than "it was refused for some other reason": the turn ran
    // and failed on routing, which is this daemon's honest answer.
    assert_ne!(
        served["error"]["code"].as_i64(),
        Some(error_code::UNKNOWN_SESSION),
        "the session exists and B is attached to it: {served}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 BR-4 / F1: `web/override` is a mutating method — it lifts a session's
/// web taint — so it is gated on attachment exactly like `session/prompt` and
/// `session/clear`.
///
/// Same shape as the gate test above, over the raw socket: an unattached
/// same-UID peer that learned the id from `session/list` is refused
/// `NOT_ATTACHED` *before* the runtime is touched; once it attaches the same
/// call reaches the runtime, whose answer is no longer `NOT_ATTACHED` (the code
/// transition is the assertion — the runtime's own outcome is out of scope).
/// Inverting the `may_drive` gate in `handle_web_override` lets the unattached
/// call through and fails the first assertion.
#[tokio::test]
async fn web_override_is_refused_until_the_connection_attaches() {
    let path = temp_socket("gate-wov");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    a.send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let created = a.read_response(2).await;
    let sid = created["result"]["session_id"].as_str().unwrap().to_owned();

    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    // B learns the id the way any same-UID peer would: by asking.
    b.send(2, "session/list", json!({})).await;
    assert_eq!(session_ids(&b.read_response(2).await), vec![sid.clone()]);

    // Unattached: refused before the runtime is consulted.
    b.send(3, "web/override", json!({"session_id": sid.clone()}))
        .await;
    let refused = b.read_response(3).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "an unattached web/override must be refused: {refused}"
    );

    // REQ-569 BR-1: B cannot attach to A's session, so the served half runs on
    // a session B created — which attaches its creator (REQ-568 BR-1). The gate
    // under test is `may_drive`, which reads the attachment set and does not
    // care which route filled it.
    b.send(4, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    let refused_attach = b.read_response(4).await;
    assert_eq!(
        refused_attach["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "B holds no grant for A's session and nobody approved one: {refused_attach}"
    );

    b.send(5, "session/create", json!({"mode": "freeform"}))
        .await;
    let own = b.read_response(5).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(own, sid, "B's own session must be a different one");

    b.send(6, "web/override", json!({"session_id": own})).await;
    let served = b.read_response(6).await;
    assert_ne!(
        served["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "attached to its own session, web/override must reach the runtime: {served}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-9 / AC-9 at the raw RPC surface: an unattached connection cannot
/// answer another session's permission prompt, and the refusal does not consume
/// the prompt.
///
/// Answering a `permission_request` decides whether a session's tool call runs,
/// so it is driving that session and is gated exactly like `session/prompt`,
/// `session/clear` and `web/override`. What makes it worth its own test is the
/// second half: a gate that *refused* by resolving the waiter unfavourably would
/// pass an assertion on the error code alone while letting any same-UID peer
/// deny every tool call on the machine — the user's prompt would vanish off
/// their screen answered by someone else. So the prompt is asserted to be **still
/// outstanding**, still owned by its session, and then answered by the client
/// that legitimately holds it.
///
/// The prompt is raised through the daemon's own gate over its own bus and
/// pending registry (the wiring `runtime.rs` builds per session), because a
/// prompt that arrives from a real turn needs a provider that emits a tool call
/// — which would pin the fixture, not the gate. Everything the gate reads is
/// genuine: a real waiter, its real recorded owner, and the real registry the
/// handler consults. The *answer* travels the whole way — client socket, reader
/// loop, `dispatch`, handler — which is the surface AC-9 names (BUG-155).
///
/// **The monitor half of AC-9 is not here, and cannot be yet.** Declaring
/// `monitor` at the handshake now requires a monitor-scope grant that nothing
/// mints until TASK-108 (asserted in
/// `a_monitor_declaration_is_refused_without_a_monitor_scope_grant`), so no
/// monitor connection is constructible over a real socket on this branch. It is
/// covered one layer down, at `dispatch`, by
/// `server::tests::a_monitor_may_see_a_permission_prompt_and_may_not_answer_it`
/// — which is where the gate actually lives, and which additionally asserts the
/// thing this test cannot: that the refused connection *did* receive the prompt
/// it was refused permission to answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unattached_connection_cannot_answer_another_sessions_permission_prompt() {
    let path = temp_socket("gate-perm");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    // Kept alive beside the server so the test can raise a prompt in the
    // daemon's own registry and read back whether it is still pending.
    let held = Arc::clone(&daemon);
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    a.send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = a.read_response(2).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());
    // B never attached, so no envelope of A's session may reach it — including
    // the `permission_request` it is about to try to answer.
    b.forbid_session(&sid);

    // B learns the id the way any same-UID peer would: by asking.
    b.send(2, "session/list", json!({})).await;
    assert_eq!(session_ids(&b.read_response(2).await), vec![sid.clone()]);

    // A real prompt in A's session, awaiting a real answer.
    let gate = PermissionGate::new(
        SessionId::from(sid.clone()),
        PermissionConfig::with_default(PermissionPolicy::Ask),
        Arc::clone(&held.events),
        Arc::clone(held.runtime.pending()),
    );
    let blocked_tool_call = tokio::spawn(async move { gate.authorize("shell", None).await });

    // A is attached by creation, so the prompt reaches it at the wire — and the
    // `request_id` a client answers with is the one it read off that event.
    let prompt = a.read_event("permission_request").await;
    assert_eq!(prompt["params"]["session_id"].as_str(), Some(sid.as_str()));
    let request_id = prompt["params"]["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the prompt carried no request_id: {prompt}"))
        .to_owned();

    let answer = json!({
        "request_id": request_id.clone(),
        "outcome": {"outcome": "selected", "option_id": "allow_once"},
    });

    // B answers a prompt that is not its to answer.
    b.send(3, "permission/respond", answer.clone()).await;
    let refused = b.read_response(3).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "an unattached connection must not answer another session's prompt: {refused}"
    );

    // The prompt survived the refusal, still owned by the session that raised
    // it. Read off the daemon rather than inferred from a timeout, so "still
    // pending" is a fact rather than the absence of an observation.
    assert_eq!(
        held.runtime.pending().pending_count(),
        1,
        "a refusal must leave the prompt standing for its rightful answerer"
    );
    assert_eq!(
        held.runtime
            .pending()
            .owner_of(&RequestId::from(request_id)),
        Some(SessionId::from(sid.clone())),
        "and it must still belong to the session that raised it"
    );

    // The rightful answerer's own call is untouched, and releases the tool.
    a.send(4, "permission/respond", answer).await;
    let accepted = a.read_response(4).await;
    assert!(
        accepted.get("result").is_some(),
        "the attached client's own answer must be served: {accepted}"
    );
    assert_eq!(
        timeout(Duration::from_secs(2), blocked_tool_call)
            .await
            .expect("the answered tool call must not still be waiting")
            .unwrap(),
        PermissionDecision::Allowed,
        "the tool call must receive the answer the user actually gave"
    );
    assert_eq!(held.runtime.pending().pending_count(), 0);

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 BR-1 / F7: attachment *composes*. One connection that creates two
/// sessions is attached to both and receives both their session-scoped
/// envelopes, while a third session it never touched stays invisible.
///
/// This pins that `ConnState.attached` is a set, not a single `Option` a second
/// attach would overwrite: were it the latter, the first session's envelopes
/// would vanish the moment the second was created. The positive is the two
/// create-time events the one connection receives; the negative — C's traffic —
/// is armed with [`TestClient::forbid_session`] and bounded by a daemon-scoped
/// marker, so a filtered envelope is a decidable fact rather than a race.
#[tokio::test]
async fn one_connection_attached_to_two_sessions_receives_both_and_only_those() {
    let path = temp_socket("compose");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut multi = TestClient::connect(&path).await;
    assert!(multi.handshake(1).await.get("result").is_some());
    let mut other = TestClient::connect(&path).await;
    assert!(other.handshake(1).await.get("result").is_some());

    // `multi` creates two sessions — auto-attached to each. The helper asserts
    // it received each session's own create-time envelope, so both A's and B's
    // scoped events reach the one connection: the set holds two, not the last.
    let sid_a = create_structured_session(&mut multi, 2).await;
    let sid_b = create_structured_session(&mut multi, 3).await;
    assert_ne!(sid_a, sid_b, "the two sessions must be distinct");

    // A third session `multi` never created or attached — invisible from here.
    let sid_c = create_structured_session(&mut other, 2).await;
    multi.forbid_session(&sid_c);

    // Fresh scoped traffic on all three: `multi` must receive A's and B's own
    // `context_cleared`, and its forbid guard trips if C's ever arrives.
    for (sid, id) in [(&sid_a, 4i64), (&sid_b, 5)] {
        let (events, cleared) = multi
            .call_collecting_events(id, "session/clear", json!({"session_id": sid}))
            .await;
        assert!(cleared.get("result").is_some(), "clear failed: {cleared}");
        assert!(
            events_for(&events, sid)
                .iter()
                .any(|e| e["event"] == "context_cleared"),
            "the connection attached to `{sid}` must receive its clear: {events:?}"
        );
    }
    let (_, cleared_c) = other
        .call_collecting_events(3, "session/clear", json!({"session_id": sid_c}))
        .await;
    assert!(
        cleared_c.get("result").is_some(),
        "clear failed: {cleared_c}"
    );

    // A daemon-scoped marker bounds the negative: `multi` reaches it without C's
    // envelope tripping the forbid guard, so C was filtered, not merely late.
    let mut marker = TestClient::connect(&path).await;
    assert!(marker.handshake(1).await.get("result").is_some());
    let seen = multi.read_event("daemon_client_attach").await;
    assert!(
        seen["params"].get("session_id").is_none(),
        "the marker is the daemon-scoped envelope every connection still receives: {seen}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-568 AC-6 and ADR-A's consequence: a filtered connection observes
/// monotonic but **non-contiguous** `seq`, and a response gated on the event
/// fence still completes.
///
/// `seq` is assigned bus-side at publish, so the numbers a connection sees are
/// the global publish order with the envelopes it was not entitled to removed.
/// That is a gap, and it is correct. This test pins the two halves of that:
///
/// - **Monotonic, gapped, never contiguous.** The clears alternate B, A, B, A…
///   so a `seq` B never receives sits between every pair B does. The assertion
///   is strict increase plus *at least one* gap; asserting contiguity anywhere
///   would be asserting that the leak is back.
/// - **No hang (AC-6/BR-7).** The last publish before B's `session/list` is A's
///   envelope — one B's forwarder skips. If a skipped envelope failed to
///   advance the forwarded watermark, `EventFence::sync` would wait forever for
///   an event B is never going to receive, and this read would time out instead
///   of returning a session list.
#[tokio::test]
async fn a_filtered_client_sees_gapped_seqs_and_its_fenced_response_still_completes() {
    let path = temp_socket("scope-seq");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());

    let sid_a = create_structured_session(&mut a, 2).await;
    b.forbid_session(&sid_a);
    let sid_b = create_structured_session(&mut b, 2).await;

    // Interleave the two sessions' traffic, B's turn first each round so an
    // envelope B cannot receive always falls between two it can.
    let mut seqs: Vec<u64> = Vec::new();
    for round in 0..3i64 {
        let id = 10 + round;
        let (events, cleared) = b
            .call_collecting_events(id, "session/clear", json!({"session_id": sid_b}))
            .await;
        assert!(cleared.get("result").is_some(), "clear failed: {cleared}");
        let mine = events_for(&events, &sid_b);
        let seq = mine
            .iter()
            .find(|e| e["event"] == "context_cleared")
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or_else(|| {
                panic!("round {round}: B's own `context_cleared` never arrived: {events:?}")
            });
        seqs.push(seq);

        let (_, cleared_a) = a
            .call_collecting_events(id, "session/clear", json!({"session_id": sid_a}))
            .await;
        assert!(
            cleared_a.get("result").is_some(),
            "clear failed: {cleared_a}"
        );
    }

    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "a connection's observed `seq` must strictly increase: {seqs:?}"
    );
    assert!(
        seqs.windows(2).any(|w| w[1] - w[0] > 1),
        "a filtered connection must observe at least one gap — a contiguous run \
         would mean it received the envelopes published between its own: {seqs:?}"
    );

    // The fence, immediately after an envelope B will never receive.
    b.send(20, "session/list", json!({})).await;
    let listed = b.read_response(20).await;
    assert_eq!(
        session_ids(&listed).len(),
        2,
        "the fenced response must complete rather than wait on a filtered \
         envelope: {listed}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// REQ-569 BR-10 / AC-10: `session/list` tells an unattached connection that a
/// session exists, and nothing about what it is.
///
/// Asserted on raw NDJSON at the socket, for the same reason REQ-568's scoping
/// tests are: the reduction is the daemon's, so a client's rendering is the
/// wrong observation point (LESSON-484 — a gate a client enforces is a
/// rendering choice). Two claims, and they are different claims:
///
/// 1. the `title` and `cwd` **keys are absent** from the JSON object, not
///    present and empty. An empty string is a value a client would have to
///    learn to tell apart from a session that genuinely has no title, and it is
///    also the shape a "redact by blanking" implementation produces — so the
///    key set is asserted, not the field's contents;
/// 2. neither string appears **anywhere in the frame**, which is what rules out
///    the leak arriving by some other field.
///
/// The positive controls that bound the negative sit in the same test and the
/// same window: the row is still listed (BR-10 is about the payload, and a
/// listing that dropped rows would be a different, worse answer), and three
/// connections that *may* see the session — the creator, which `session/create`
/// auto-attached (REQ-568); a peer that attached; and a monitor — each get it
/// whole. Without them, an implementation that omitted `title`/`cwd` from
/// everyone would pass.
#[tokio::test]
async fn session_list_omits_title_and_cwd_from_unattached_connections() {
    let path = temp_socket("list-reduced");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, Arc::clone(&daemon)));

    // Boundary content, both halves of it: a title is model-generated from the
    // user's prompt text, and `cwd` is an absolute path naming a repo on this
    // machine. Distinctive enough that either string appearing in a frame can
    // only have come from this session's summary.
    const TITLE: &str = "rewrite the payroll importer's retry loop";
    let jail = std::env::temp_dir().join(format!("teton-req569-jail-{}", std::process::id()));
    std::fs::create_dir_all(&jail).unwrap();
    let jail_text = jail.to_str().unwrap().to_owned();

    let mut a = TestClient::connect(&path).await;
    assert!(a.handshake(1).await.get("result").is_some());
    let (_, created) = a
        .call_collecting_events(
            2,
            "session/create",
            json!({"mode": "structured", "phase": "spec", "cwd": jail}),
        )
        .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create failed: {created}"))
        .to_owned();
    // The title is set on the registry directly: a real one is written by the
    // title duty at turn time, which needs a model, and what is under test here
    // is the payload rather than the naming.
    assert!(
        daemon
            .sessions
            .set_title(&teton_protocol::SessionId::from(sid.clone()), TITLE),
        "the fixture must actually have a title to redact"
    );

    // B is any same-UID peer: handshaked, attached to nothing.
    let mut b = TestClient::connect(&path).await;
    assert!(b.handshake(1).await.get("result").is_some());
    b.send(2, "session/list", json!({})).await;
    let (raw, listed) = b.read_response_raw(2).await;
    let row = row_for(&listed, &sid);

    // What a listing is for survives intact.
    assert_eq!(row["mode"].as_str(), Some("structured"), "{raw}");
    assert_eq!(row["phase"].as_str(), Some("spec"), "{raw}");

    // Claim 1: the keys are gone, not blanked.
    let fields = row.as_object().unwrap();
    assert!(
        !fields.contains_key("title"),
        "an unattached connection must be sent no `title` key at all: {raw}"
    );
    assert!(
        !fields.contains_key("cwd"),
        "an unattached connection must be sent no `cwd` key at all: {raw}"
    );
    // Claim 2: and the content is nowhere else in the frame either.
    assert!(
        !raw.contains(TITLE),
        "the user's words must not cross this socket: {raw}"
    );
    assert!(
        !raw.contains(&jail_text),
        "the session's path must not cross this socket: {raw}"
    );

    /// The whole summary, which is what every connection that may see the
    /// session gets.
    fn assert_whole(who: &str, raw: &str, listed: &Value, sid: &str, jail_text: &str) {
        let row = row_for(listed, sid);
        assert_eq!(
            row["title"].as_str(),
            Some(TITLE),
            "{who} may see this session and must get its title: {raw}"
        );
        assert_eq!(
            row["cwd"].as_str(),
            Some(jail_text),
            "{who} may see this session and must get its cwd: {raw}"
        );
    }

    // B stays reduced for as long as it holds no grant, and REQ-569 means it
    // cannot lift that on its own: the `session/attach` that used to be the
    // whole story now costs a decision it cannot make for itself (BR-1/BR-6).
    // The two gates compose the way they should — the reduction is not a
    // second, weaker copy of the attach rule.
    b.send(3, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    let refused = b.read_response(3).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(error_code::CONSENT_TIMEOUT),
        "B cannot promote itself out of the reduced view: {refused}"
    );
    b.send(4, "session/list", json!({})).await;
    let (raw_still, listed_still) = b.read_response_raw(4).await;
    let row_still = row_for(&listed_still, &sid);
    assert!(
        !row_still.as_object().unwrap().contains_key("title")
            && !raw_still.contains(TITLE)
            && !raw_still.contains(&jail_text),
        "a refused attach must leave the view reduced: {raw_still}"
    );

    // The positive control, and it is what keeps this test from passing for a
    // daemon that reduced everyone: the creator, which never attached
    // explicitly — `session/create` did it (REQ-568 BR-1) — sees the session
    // whole. A reduction that keyed on "attached explicitly" would hide a
    // client's own session from it.
    a.send(3, "session/list", json!({})).await;
    let (raw_a, listed_a) = a.read_response_raw(3).await;
    assert_whole("the creator", &raw_a, &listed_a, &sid, &jail_text);

    server_task.abort();
    let _ = std::fs::remove_dir(&jail);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_method_before_handshake_is_refused() {
    let path = temp_socket("gate");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut a = TestClient::connect(&path).await;
    // No handshake — a session/list must be rejected.
    a.send(1, "session/list", json!({})).await;
    let response = a.read_response(1).await;

    assert!(
        response.get("error").is_some(),
        "expected an error before handshake, got: {response}"
    );
    // -32600 == INVALID_REQUEST.
    assert_eq!(response["error"]["code"].as_i64().unwrap(), -32600);

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// A client speaking the pre-REQ-558 protocol is refused at the handshake, and
/// stays refused for every method after it.
///
/// This is the released-pairing bug from the daemon's side. `ConfigSnapshot`
/// changed shape in REQ-558 while the version stayed at 1, so the two builds
/// negotiated happily and then failed inside `config/get` with a bare
/// `missing field \`category\`` — a serde string, out of `Connection::call`,
/// with nothing in it a user could act on. The version bump moves that failure
/// here, where it is a typed code carrying the four bounds a client turns into
/// "restart the daemon".
///
/// The second half matters as much as the first: a refused client must not be
/// left half-attached, able to call `config/get` anyway and hit the original
/// serde error by another road.
#[tokio::test]
async fn a_client_from_the_previous_protocol_is_refused_at_the_handshake() {
    let path = temp_socket("skew");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon();
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut old = TestClient::connect(&path).await;
    old.send(
        1,
        "handshake",
        json!({
            "client_kind": "cli",
            "client_name": "teton-cli",
            "client_version": "0.1.10",
            "protocol_min": 1,
            "protocol_max": 1,
        }),
    )
    .await;
    let response = old.read_response(1).await;

    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("a v1 client must be refused, got: {response}"));
    assert_eq!(
        error["code"].as_i64().unwrap(),
        error_code::UNSUPPORTED_PROTOCOL_VERSION
    );

    // The bounds ride in `data` — that payload is what lets the rejected client
    // say which half is stale instead of restating the daemon's sentence.
    //
    // The skew reads `ClientIsOlder` because the daemon under test is always
    // this build: staged from here, the old half is the client. The released
    // pairing is the mirror image — an old *daemon* rejecting a new CLI — and
    // its arithmetic is identical, which is why the direction is pinned in
    // `handshake`'s unit tests and the sentence in the CLI's, where the old
    // daemon's range can actually be stood up.
    let rpc: RpcError = serde_json::from_value(error.clone()).unwrap();
    let mismatch = VersionMismatch::from_rpc_error(&rpc).expect("bounds are on the wire");
    assert_eq!(mismatch.skew(), Some(VersionSkew::ClientIsOlder));
    assert_eq!(mismatch.client_max, teton_protocol::ProtocolVersion(1));
    assert_eq!(mismatch.daemon_min, PROTOCOL_VERSION_MIN);
    assert_eq!(mismatch.daemon_max, PROTOCOL_VERSION_MAX);

    // Still unauthenticated: the config call that used to blow up on a serde
    // error never reaches the snapshot at all.
    old.send(2, "config/get", json!({})).await;
    let after = old.read_response(2).await;
    assert_eq!(
        after["error"]["code"].as_i64().unwrap(),
        -32600,
        "a refused client must stay refused, got: {after}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// **REQ-570 AC-2b: `monitor` is mintable again.**
///
/// The positive direction, and it exists because a capability that is only ever
/// observed being *refused* is indistinguishable from the dead code Gap 3
/// describes. REQ-569 removed the monitor consent path, which left `monitor` —
/// a shipped REQ-568 feature — permanently unreachable; the sibling test above
/// proves the attack still fails, and on its own that would be equally true of a
/// daemon where the whole capability had been deleted.
///
/// So: a connection presenting a valid attestation, answering a monitor-scope
/// request it did **not** raise, mints the grant, and the requester's handshake
/// then succeeds and it can actually monitor.
///
/// The difference from the attack next door is exactly one thing — a human was
/// verified — which is the claim REQ-570 exists to make.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_monitor_grant_is_minted_when_a_human_approves_a_request_it_did_not_raise() {
    let path = temp_socket("mon-mint");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = test_daemon_with_presence();
    let held = Arc::clone(&daemon);
    let server_task = tokio::spawn(server::serve(listener, daemon));

    // The user's real client, holding a session — the surface the request is
    // routed to.
    let mut owner = TestClient::connect(&path).await;
    assert!(owner.handshake(1).await.get("result").is_some());
    owner
        .send(2, "session/create", json!({"mode": "freeform"}))
        .await;
    let sid = owner.read_response(2).await["result"]["session_id"]
        .as_str()
        .expect("the session is created")
        .to_owned();

    // A second, separate client asking to watch everything.
    let mut watcher = TestClient::connect(&path).await;
    let handshake = tokio::spawn(async move {
        let result = watcher.handshake_declaring(1, true).await;
        (watcher, result)
    });

    let prompt = owner.read_event("attach_consent_requested").await;
    assert_eq!(prompt["params"]["scope"].as_str(), Some("monitor"));
    let request_id = prompt["params"]["request_id"]
        .as_str()
        .expect("the prompt carries its request id")
        .to_owned();

    owner
        .send(
            3,
            "attach/consent",
            json!({"request_id": request_id, "outcome": {"outcome": "granted"}}),
        )
        .await;
    let answered = owner.read_response(3).await;
    assert_eq!(
        answered["result"]["resolved"].as_bool(),
        Some(true),
        "an attested approval must actually decide the request: {answered}"
    );

    let (mut watcher, result) = handshake.await.expect("the handshake task completes");
    assert!(
        result.get("result").is_some(),
        "an attested monitor grant must let the handshake succeed: {result}"
    );
    assert_eq!(
        held.grants.len(),
        1,
        "exactly one grant, at monitor scope, for the connection that asked"
    );

    // And it can genuinely monitor: a session it never attached to is visible.
    watcher.send(2, "session/list", json!({})).await;
    let listed = watcher.read_response(2).await;
    let sessions = listed["result"]["sessions"]
        .as_array()
        .expect("session/list answers a monitor");
    assert!(
        sessions.iter().any(|row| row["session_id"] == sid.as_str()),
        "a monitor that was granted must actually see the session: {listed}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}
