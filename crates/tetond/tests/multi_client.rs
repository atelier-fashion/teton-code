//! Integration test for AC-6, event scoping, and the handshake gate.
//!
//! Two clients attach over a real Unix socket, exchange the handshake, and
//! observe: (1) a session created by one client appears identically in both
//! clients' session lists; (2) a session-scoped event emitted by that creation
//! reaches the creator but **not** the other, unattached client, and does reach
//! it once it attaches (REQ-568 BR-1); and (3) the daemon and its sessions
//! survive a client disconnecting — a fresh client can still attach to the
//! surviving session. Further tests cover the monitor declaration that opts a
//! connection back into everything (BR-1, ADR-C) and the refusal of any method
//! before the handshake.
//!
//! Point (2) inverted at REQ-568: this test used to assert that the second
//! client received the first's `phase_transition` without asking for it, which
//! was the leak written down as a feature. The registry stays shared — sessions
//! outlive their creators and every client can list them — but the *events* are
//! now scoped to the connections that asked for them.

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
}

impl TestClient {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        }
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
        serde_json::from_str(&line).unwrap()
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
