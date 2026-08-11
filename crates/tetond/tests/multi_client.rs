//! Integration test for AC-6, event scoping, and the handshake gate.
//!
//! Two clients attach over a real Unix socket, exchange the handshake, and
//! observe: (1) a session created by one client appears identically in both
//! clients' session lists; (2) a session-scoped event emitted by that creation
//! reaches the creator but **not** the other, unattached client, and does reach
//! it once it attaches (REQ-568 BR-1); and (3) the daemon and its sessions
//! survive a client disconnecting — a fresh client can still attach to the
//! surviving session. Further tests cover two clients each prompting a session
//! of their own (AC-1), a connection that never attached anything
//! (AC-2), the monitor declaration that opts a connection back into everything
//! (BR-1, AC-3, ADR-C), the attachment gate on the mutating methods
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
use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
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
        value
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
    let daemon = Arc::new(Daemon::new());
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

    // BR-1, the other direction: attaching is the grant. Once B has attached,
    // the session's events reach it — a `session/clear` issued by A, so B is
    // purely a receiver and what it sees is delivery, not an echo of its own
    // request.
    b.send(4, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    assert!(b.read_response(4).await.get("result").is_some());

    a.send(5, "session/clear", json!({"session_id": sid.clone()}))
        .await;
    let cleared = b.read_event("context_cleared").await;
    assert_eq!(cleared["params"]["session_id"].as_str().unwrap(), sid);
    assert!(a.read_response(5).await.get("result").is_some());

    // Client A exits. The daemon and its sessions must survive.
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;

    b.send(6, "session/list", json!({})).await;
    let list_b_after = b.read_response(6).await;
    assert_eq!(session_ids(&list_b_after), vec![sid.clone()]);

    // A fresh client can attach to the surviving session.
    let mut d = TestClient::connect(&path).await;
    assert!(d.handshake(1).await.get("result").is_some());
    d.send(2, "session/attach", json!({"session_id": sid}))
        .await;
    let attached = d.read_response(2).await;
    assert_eq!(
        attached["result"]["session"]["session_id"]
            .as_str()
            .unwrap(),
        sid
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
    let daemon = Arc::new(Daemon::new());
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
    let daemon = Arc::new(Daemon::new());
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

/// REQ-568 BR-1 / AC-3: a connection that declared `monitor` at the handshake
/// receives another client's session-scoped events without ever attaching.
///
/// The opt-in half of the filter, at the socket. It is deliberately the *only*
/// way back to the old behaviour: the declaration is a field the client had to
/// send, so a monitor exists because someone asked for one.
#[tokio::test]
async fn a_monitor_declared_at_handshake_receives_another_clients_events() {
    let path = temp_socket("monitor");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut monitor = TestClient::connect(&path).await;
    assert!(monitor
        .handshake_declaring(1, true)
        .await
        .get("result")
        .is_some());

    let mut worker = TestClient::connect(&path).await;
    assert!(worker.handshake(1).await.get("result").is_some());

    worker
        .send(
            2,
            "session/create",
            json!({"mode": "structured", "phase": "spec"}),
        )
        .await;
    let created = worker.read_response(2).await;
    let sid = created["result"]["session_id"].as_str().unwrap().to_owned();

    let event = monitor.read_event("phase_transition").await;
    assert_eq!(event["params"]["session_id"].as_str().unwrap(), sid);

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
    let daemon = Arc::new(Daemon::new());
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

    // Attachment is the grant, and it is the only thing that changed.
    b.send(5, "session/attach", json!({"session_id": sid.clone()}))
        .await;
    assert!(b.read_response(5).await.get("result").is_some());

    b.send(6, "session/clear", json!({"session_id": sid})).await;
    let cleared = b.read_response(6).await;
    assert_eq!(
        cleared["result"]["blocks_dropped"].as_u64(),
        Some(0),
        "after attaching, the clear is served: {cleared}"
    );

    b.send(7, "session/prompt", prompt).await;
    let served = b.read_response(7).await;
    assert_ne!(
        served["error"]["code"].as_i64(),
        Some(error_code::NOT_ATTACHED),
        "after attaching, the prompt must reach the runtime: {served}"
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
    let daemon = Arc::new(Daemon::new());
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

#[tokio::test]
async fn a_method_before_handshake_is_refused() {
    let path = temp_socket("gate");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
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
    let daemon = Arc::new(Daemon::new());
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
