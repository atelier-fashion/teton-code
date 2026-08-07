//! Event/response ordering: a request's events precede its response (BUG: a
//! turn's trailing streamed text rendered one command late in the CLI).
//!
//! The daemon's per-client outbound channel is fed by two independent
//! producers — request handlers pushing responses, and `forward_events`
//! relaying broadcast events from the client's bus subscription. Before the
//! `EventFence` in `server.rs`, nothing ordered them: an event published
//! *inside* a handler (a prompt turn's `route_decided` and final
//! `session_update`, `session/create`'s `phase_transition`) could be moved to
//! the outbound channel *after* the handler's own response, and a
//! strictly-FIFO client (the CLI's pump reads up to the matching response)
//! then rendered it during the NEXT call — the turn's tail after the next
//! entry prompt, or after the session-end cost summary.
//!
//! These tests pin the fixed ordering over a real Unix socket. The race they
//! guard against is a task-scheduling race (observed live at roughly one run
//! in ten), so each test drives many iterations on a multi-thread runtime:
//! a regression will not fail every iteration, but across the loop it fails
//! with overwhelming probability, while the fence makes every iteration
//! deterministic. Each iteration uses a **fresh session**, so the asserted
//! event is attributable to exactly that iteration's request — a late event
//! from a previous turn cannot satisfy the check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
use tetond::{server, Daemon};

/// Iterations per test. The pre-fence race hit ~1 in 10 turns, so a regression
/// survives all of these with probability well under one in a million while
/// the loop stays fast (each iteration is one round-trip over a local socket).
const TURNS: usize = 150;

/// A minimal in-test JSON-RPC client over the daemon socket (the
/// `multi_client.rs` shape), reading raw frames in wire order.
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

    /// The next frame off the socket, in wire order — the order the assertions
    /// here are about.
    async fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a frame")
            .unwrap();
        assert!(n > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn handshake(&mut self) {
        self.send(
            1,
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": "ordering-test",
                "client_version": "0.1.0",
                "protocol_min": PROTOCOL_VERSION_MIN,
                "protocol_max": PROTOCOL_VERSION_MAX,
            }),
        )
        .await;
        loop {
            let frame = self.read_line().await;
            if frame.get("id").and_then(Value::as_i64) == Some(1) {
                assert!(frame.get("result").is_some(), "handshake failed: {frame}");
                return;
            }
        }
    }

    /// Send `method` and read frames until its response, returning every event
    /// notification that arrived **before** the response, plus the response.
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
}

fn temp_socket(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teton-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Every event a `session/prompt` publishes reaches the client before the
/// turn's own response frame.
///
/// Each turn publishes its `route_decided` (emitted inside `run_prompt_turn`,
/// before the attempt) and then fails — which exercises exactly the racing
/// seam: an event published on the turn task versus the response that same task
/// enqueues moments later. The event must win, every time.
///
/// **The turn needs a real route to decide.** Before REQ-557 this test ran
/// against a bare `Daemon::new()` with no providers at all, and still saw a
/// `route_decided` — because `build_router` synthesized a default from array
/// position and, failing that, from the literal id `"local"`. That fabricated
/// route is BUG-146's root cause #1 and REQ-557 BR-4 deletes it, so a daemon
/// with no providers now correctly emits nothing to order against. The fixture
/// therefore registers a genuine provider over the same `config/set` path the
/// CLI drives; the ordering claim is unchanged.
///
/// The turn is made to fail on an **unresolvable credential** rather than a dead
/// endpoint: `auth_ref` resolution happens before any socket is opened and is
/// classified as settled (never retried), so each of the 150 iterations fails
/// immediately and deterministically — no network, no keychain, no backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turns_events_precede_the_turns_response_on_the_wire() {
    let path = temp_socket("ord-turn");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&path).await;
    client.handshake().await;

    // A provider the router can actually select: a declared `model` (REQ-557
    // BR-1 — without one it is unusable and never enters the provider map) and
    // an `auth_ref` naming an env var that is not set.
    let (_, registered) = client
        .call_collecting_events(
            1000,
            "config/set",
            json!({ "update": {
                "op": "register_provider",
                "id": "ordering",
                "kind": "openai-compatible",
                "endpoint": "http://127.0.0.1:1/v1/chat/completions",
                "model": "deepseek-chat",
                "auth_ref": "env:TETON_ORDERING_TEST_CREDENTIAL_ABSENT",
            }}),
        )
        .await;
    assert_eq!(
        registered["result"]["applied"].as_bool(),
        Some(true),
        "provider registration failed: {registered}"
    );
    // A tier binding, so a structured turn resolves through the routing table
    // rather than through `default_provider` (which no RPC can set — REQ-557
    // adds none, by design). An `implement` turn dispatches on `edit`, which
    // inherits the `build` tier (REQ-558 AC-9: the config op takes no phase).
    let (_, routed) = client
        .call_collecting_events(
            1001,
            "config/set",
            json!({ "update": {
                "op": "set_tier_binding",
                "tier": "build",
                "provider_id": "ordering",
            }}),
        )
        .await;
    assert_eq!(
        routed["result"]["applied"].as_bool(),
        Some(true),
        "tier binding failed: {routed}"
    );

    for turn in 0..TURNS {
        // A fresh session per turn makes the `route_decided` attributable: its
        // `session_id` ties it to this iteration and no other.
        let id = 2 + 2 * turn as i64;
        let (_, created) = client
            .call_collecting_events(
                id,
                "session/create",
                json!({"mode": "structured", "phase": "implement"}),
            )
            .await;
        let sid = created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {created}"))
            .to_owned();

        let (events, response) = client
            .call_collecting_events(
                id + 1,
                "session/prompt",
                json!({
                    "session_id": sid,
                    "prompt": [{ "type": "text", "text": "explain this" }],
                }),
            )
            .await;

        // The turn fails (the credential does not resolve) — the ordering claim
        // is about the error response exactly as much as a success.
        assert!(
            response.get("error").is_some(),
            "expected the unresolvable-credential error response, got: {response}"
        );
        assert!(
            events.iter().any(|e| {
                e.get("event").and_then(Value::as_str) == Some("route_decided")
                    && e.get("session_id").and_then(Value::as_str) == Some(&sid)
            }),
            "turn {turn}: the turn's `route_decided` was not on the wire before the \
             turn's own response — a response overtook an event published before it \
             (events seen first: {events:?})"
        );
    }

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// The synchronous-dispatch path holds the same line: a structured
/// `session/create` publishes its `phase_transition` before its response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_creations_phase_transition_precedes_the_creations_response() {
    let path = temp_socket("ord-create");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&path).await;
    client.handshake().await;

    for turn in 0..TURNS {
        let id = 2 + turn as i64;
        let (events, response) = client
            .call_collecting_events(
                id,
                "session/create",
                json!({"mode": "structured", "phase": "spec"}),
            )
            .await;
        let sid = response["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create failed: {response}"))
            .to_owned();

        assert!(
            events.iter().any(|e| {
                e.get("event").and_then(Value::as_str) == Some("phase_transition")
                    && e.get("session_id").and_then(Value::as_str) == Some(&sid)
            }),
            "creation {turn}: the `phase_transition` was not on the wire before its \
             own response (events seen first: {events:?})"
        );
    }

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}
