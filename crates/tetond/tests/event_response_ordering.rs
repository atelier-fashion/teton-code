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

/// REQ-568 ADR-A regression net: the ordering invariant is unchanged when a
/// second client, attached to a *different* session, is connected for the whole
/// racing run.
///
/// The filter added at the forwarding seam means the bystander's forwarder now
/// *skips* almost every envelope the bus hands it. Two things could go wrong
/// with that, and both would show up here:
///
/// 1. **A stall.** A skipped envelope that failed to advance the bystander's
///    forwarded watermark, or a forwarder that stopped draining its
///    subscription, would back the connection up. The prompting client's turns
///    share the same bus, so a wedged peer is a wedged run — the loop would
///    time out rather than fail an assertion.
/// 2. **A leak under load.** The bystander drains its stream at the end and
///    must find nothing scoped to any of the prompting client's sessions.
///
/// Iteration count: the existing [`TURNS`], unchanged. The bystander is passive
/// — one connection, one session, two requests — so it adds no turns and no
/// measurable runtime; the race being sampled is the same one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turns_ordering_holds_while_another_client_holds_a_different_session() {
    let path = temp_socket("ord-scoped");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&path).await;
    client.handshake().await;

    // The same provider fixture as the test above: a real route, an
    // unresolvable credential, an immediate settled failure per turn.
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

    // The bystander: its own session, and then silence for the whole run. It
    // reads nothing while the turns race, so a forwarder that wrongly delivered
    // the prompting client's stream would also be filling this connection's
    // outbound channel — the shape a stall would take.
    let mut bystander = TestClient::connect(&path).await;
    bystander.handshake().await;
    let (_, bystander_session) = bystander
        .call_collecting_events(
            2000,
            "session/create",
            json!({"mode": "structured", "phase": "spec"}),
        )
        .await;
    let bystander_sid = bystander_session["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session/create failed: {bystander_session}"))
        .to_owned();

    for turn in 0..TURNS {
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
             turn's own response while a filtered peer was connected \
             (events seen first: {events:?})"
        );
    }

    // The bystander is still live and still scoped: its fenced response comes
    // back, and everything it drains on the way belongs to its own session or
    // to no session at all.
    let (seen, listed) = bystander
        .call_collecting_events(2001, "session/list", json!({}))
        .await;
    assert!(
        listed.get("result").is_some(),
        "the filtered peer's fenced response never completed: {listed}"
    );
    let foreign: Vec<&Value> = seen
        .iter()
        .filter(|e| {
            e.get("session_id")
                .and_then(Value::as_str)
                .is_some_and(|s| s != bystander_sid)
        })
        .collect();
    assert!(
        foreign.is_empty(),
        "the filtered peer received another session's envelopes: {foreign:?}"
    );

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

// ---------------------------------------------------------------------------
// REQ-623 BR-13 / ADR-3 — a child's events never enter the parent's sequence
// ---------------------------------------------------------------------------

#[path = "e2e/harness.rs"]
mod harness;

/// Iterations of the child-ordering run. Children interleave by scheduler
/// accident (LESSON-591); a parent sequence that let one child event through
/// would differ between runs, and across these it would be caught.
const CHILD_TURNS: usize = 25;

/// Whether `event` is child-scoped: it names a child at its top level
/// (`session_update`, `permission_request`, `context_pressure`, every
/// `agent_child_*`) or in its ledger record (`cost_recorded`).
fn child_of(event: &Value) -> Option<&str> {
    event
        .get("child_id")
        .and_then(Value::as_str)
        .or_else(|| event["record"].get("child_id").and_then(Value::as_str))
}

/// An event's name, with a `session_update`'s kind — the granularity the
/// golden sequences below are written at.
fn shape(event: &Value) -> String {
    let name = event["event"].as_str().unwrap_or_default();
    match event["update"]["kind"].as_str() {
        Some(kind) if name == "session_update" => format!("{name}:{kind}"),
        _ => name.to_owned(),
    }
}

/// Consecutive repeats collapsed to one entry — the REQ-598 fixture's rule, so
/// a chunk count is never pinned.
fn collapsed(shapes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in shapes {
        if out.last() != Some(&s) {
            out.push(s);
        }
    }
    out
}

/// The parent turn's golden sequence for one prompt whose single `agent` call
/// dispatches two children, **child-scoped events excluded** (ADR-3).
///
/// Written out by hand, entry by entry, and checked against what the turn
/// does — not regenerated from a run (LESSON-569): the parent decides its
/// route, streams its dispatch call (`tool_call` for `agent`), the tool
/// announces the call, every child runs entirely inside the gap that follows,
/// the call's end is announced, the `agent` tool call completes, and the
/// parent streams its closing reply. (This in-process daemon installs no cost
/// ledger, so no `cost_recorded` is published by parent or child; the
/// spawned-daemon suites see those, and `agent_dispatch.rs` reads them.)
const PARENT_GOLDEN: [&str; 6] = [
    "route_decided",
    "session_update:tool_call",
    "agent_call_started",
    "agent_call_finished",
    "session_update:tool_call_update",
    "session_update:agent_message_chunk",
];

/// `left`'s own sequence: started, its `read` started and finished, its answer
/// streamed, finished.
const LEFT_GOLDEN: [&str; 5] = [
    "agent_child_started",
    "session_update:tool_call",
    "session_update:tool_call_update",
    "session_update:agent_message_chunk",
    "agent_child_finished",
];

/// `right`'s own sequence: started, its answer streamed, finished.
const RIGHT_GOLDEN: [&str; 3] = [
    "agent_child_started",
    "session_update:agent_message_chunk",
    "agent_child_finished",
];

/// **REQ-623 BR-13 / ADR-3 (LESSON-591): the parent's golden sequence excludes
/// every child-scoped event; each child's own order is asserted separately; and
/// no order across children is pinned.**
///
/// Over the real socket, `CHILD_TURNS` fresh sessions each run one prompt whose
/// `agent` call dispatches `left` (a `read`, then an answer) and `right` (an
/// answer), every event read off the wire before the prompt's response. For
/// every run:
///
/// - with child-scoped events filtered out, the parent's sequence is
///   [`PARENT_GOLDEN`] exactly — the same every run, however the children
///   interleaved;
/// - filtered by `child_id`, each child's sequence is its own golden;
/// - every child event lies strictly between `agent_call_started` and
///   `agent_call_finished` — a parent-anchored fact, not a cross-child one.
///
/// Benign path: the parent sequence keeps `agent_call_started` and
/// `agent_call_finished`, which are the parent's news and name no child.
///
/// # Mutation (run 2026-10-07, reverted)
///
/// - **`SessionEvents::for_child` stamps nothing** (returns the parent's
///   emitter unchanged): red here — the children's updates enter the parent's
///   sequence — among 5 across this binary, `agent_dispatch` and
///   `provenance_egress`.
/// - **Every result block pins**: red here as well — the parent's closing
///   call is blocked, and this in-process daemon has no local tier to answer
///   it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_events_excluded_from_parent_golden() {
    use harness::{openai_turn, Matcher, MockProvider, MockResponse};

    let reply = |text: &str| MockResponse::ok(openai_turn(text, None, 100, 10));
    let mut table = Vec::new();
    for _ in 0..CHILD_TURNS {
        table.push((
            Matcher::body_contains("\"content\":\"GOLDEN-LEFT"),
            MockResponse::ok(openai_turn(
                "",
                Some(("mock-call", "read", r#"{"path":"left.txt"}"#)),
                100,
                10,
            )),
        ));
        table.push((
            Matcher::body_contains("\"content\":\"GOLDEN-LEFT"),
            reply("left read its file"),
        ));
        table.push((
            Matcher::body_contains("\"content\":\"GOLDEN-RIGHT"),
            reply("right answered"),
        ));
        table.push((
            Matcher::body_contains("GOLDEN-PARENT"),
            MockResponse::ok(openai_turn(
                "",
                Some((
                    "mock-call",
                    "agent",
                    r#"{"tasks":[{"task":"GOLDEN-LEFT read left.txt","name":"left"},{"task":"GOLDEN-RIGHT answer","name":"right"}]}"#,
                )),
                100,
                10,
            )),
        ));
        table.push((
            Matcher::body_contains("GOLDEN-PARENT"),
            reply("Both are back."),
        ));
    }
    let provider = MockProvider::start_matching(table, reply("UNMATCHED-REQUEST"));

    let root = std::env::temp_dir().join(format!(
        "teton-golden-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("left.txt"), "left contents\n").unwrap();

    let path = temp_socket("ord-child");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));
    let mut client = TestClient::connect(&path).await;
    client.handshake().await;
    for (id, update) in [
        (
            1000,
            json!({ "op": "register_provider", "id": "golden", "kind": "openai-compatible",
                    "endpoint": provider.openai_endpoint(), "model": "deepseek-v4-flash" }),
        ),
        (
            1001,
            json!({ "op": "set_tier_binding", "tier": "build", "provider_id": "golden" }),
        ),
    ] {
        let (_, applied) = client
            .call_collecting_events(id, "config/set", json!({ "update": update }))
            .await;
        assert_eq!(
            applied["result"]["applied"].as_bool(),
            Some(true),
            "{applied}"
        );
    }

    for turn in 0..CHILD_TURNS {
        let id = 2 + 2 * turn as i64;
        let (_, created) = client
            .call_collecting_events(
                id,
                "session/create",
                json!({ "mode": "structured", "phase": "implement", "cwd": root }),
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
                    "prompt": [{ "type": "text", "text": "GOLDEN-PARENT dispatch two" }],
                }),
            )
            .await;
        assert_eq!(
            response["result"]["stop_reason"], "end_turn",
            "turn {turn}: {response}"
        );
        let mine: Vec<&Value> = events
            .iter()
            .filter(|e| e.get("session_id").and_then(Value::as_str) == Some(sid.as_str()))
            .collect();

        let parent = collapsed(
            mine.iter()
                .filter(|e| child_of(e).is_none())
                .map(|e| shape(e)),
        );
        assert_eq!(
            parent, PARENT_GOLDEN,
            "turn {turn}: the parent's sequence, child events excluded"
        );

        let started = mine
            .iter()
            .position(|e| e["event"] == "agent_call_started")
            .expect("the call started");
        let finished = mine
            .iter()
            .position(|e| e["event"] == "agent_call_finished")
            .expect("the call finished");
        let ids: Vec<(String, String)> = mine
            .iter()
            .filter(|e| e["event"] == "agent_child_started")
            .map(|e| {
                (
                    e["name"].as_str().unwrap_or_default().to_owned(),
                    e["child_id"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        assert_eq!(ids.len(), 2, "turn {turn}: two children started");
        for (name, child_id) in &ids {
            let own: Vec<(usize, String)> = mine
                .iter()
                .enumerate()
                .filter(|(_, e)| child_of(e) == Some(child_id.as_str()))
                .map(|(i, e)| (i, shape(e)))
                .collect();
            assert!(
                own.iter().all(|(i, _)| *i > started && *i < finished),
                "turn {turn}: every event of {name} lies inside its call"
            );
            let golden: &[&str] = match name.as_str() {
                "left" => &LEFT_GOLDEN,
                "right" => &RIGHT_GOLDEN,
                other => panic!("an unexpected child {other}"),
            };
            assert_eq!(
                collapsed(own.into_iter().map(|(_, s)| s)),
                golden,
                "turn {turn}: {name}'s own order"
            );
        }
    }

    server_task.abort();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&root);
    harness::assert_no_boundary_bytes();
}
