//! REQ-565 TASK-087: the supervisor and its server integration, over a real
//! socket but an in-process daemon.
//!
//! The end-to-end claims (a real `teton-code` process exiting on its own) belong
//! to `daemon_lifetime.rs`. What is asserted here is the wiring those e2e tests
//! sit on and cannot isolate: that the count moves at *handshake* rather than at
//! `accept`, that admission and commit cannot interleave, that a committed
//! daemon refuses rather than half-serves, and that a disconnect no longer kills
//! an in-flight turn.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use teton_core::lifetime::{BlockingActivity, LifetimePhase, PolicySource, ShutdownPolicy};
use teton_protocol::jsonrpc::error_code;
use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
use tetond::broadcast::EventBus;
use tetond::lifetime::LifetimeSupervisor;
use tetond::runtime::DaemonRuntime;
use tetond::{server, Daemon};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            let n = timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
                .await
                .expect("timed out waiting for a line")
                .unwrap();
            assert!(n > 0, "connection closed unexpectedly");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    async fn handshake(&mut self, id: i64) -> Value {
        self.send(
            id,
            "handshake",
            json!({
                "client_kind": "cli",
                "client_name": "test-client",
                "client_version": "0.1.0",
                "protocol_min": PROTOCOL_VERSION_MIN,
                "protocol_max": PROTOCOL_VERSION_MAX,
            }),
        )
        .await;
        self.read_response(id).await
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

/// A daemon on a real socket under an explicit policy.
fn spawn_daemon(tag: &str, policy: ShutdownPolicy) -> (PathBuf, Arc<LifetimeSupervisor>) {
    let socket = temp_socket(tag);
    let events = Arc::new(EventBus::new());
    let lifetime = Arc::new(LifetimeSupervisor::new(
        policy,
        PolicySource::Flag,
        Arc::clone(&events),
    ));
    let runtime = Arc::new(DaemonRuntime::minimal());
    let daemon = Arc::new(Daemon::with_lifetime(
        events,
        runtime,
        Arc::clone(&lifetime),
    ));
    let listener = server::bind_listener(&socket).unwrap();
    tokio::spawn(server::serve(listener, daemon));
    (socket, lifetime)
}

/// Poll until `cond` holds, or fail. Cheaper and less flaky than a fixed sleep:
/// the disconnect path runs on a task we do not hold a handle to.
async fn until(label: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

// ---------------------------------------------------------------------------
// D-1: the count moves at handshake, never at accept
// ---------------------------------------------------------------------------

/// The CLI's autostart poll and the e2e harness's readiness probe both open a
/// socket and drop it without handshaking. If those counted, every one of them
/// would arm a shutdown and the daemon would exit under its own liveness check.
#[tokio::test]
async fn a_socket_that_never_handshakes_neither_counts_nor_arms() {
    let (socket, lifetime) = spawn_daemon("no-handshake", ShutdownPolicy::OnLastDisconnect);

    for _ in 0..3 {
        let probe = UnixStream::connect(&socket).await.unwrap();
        drop(probe);
    }
    // Give any (wrong) bookkeeping a chance to happen.
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(lifetime.client_count(), 0, "a bare probe must not count");
    assert!(
        !lifetime.is_committed(),
        "a probe that never handshook must not arm a shutdown"
    );

    // And a real client still works afterwards.
    let mut client = TestClient::connect(&socket).await;
    let response = client.handshake(1).await;
    assert!(response.get("result").is_some(), "{response}");
    assert_eq!(lifetime.client_count(), 1);

    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn the_count_follows_handshakes_in_and_disconnects_out() {
    let (socket, lifetime) = spawn_daemon("counting", ShutdownPolicy::Never);

    let mut first = TestClient::connect(&socket).await;
    first.handshake(1).await;
    until("first client counted", || lifetime.client_count() == 1).await;

    let mut second = TestClient::connect(&socket).await;
    second.handshake(1).await;
    until("second client counted", || lifetime.client_count() == 2).await;

    drop(first);
    until("first client removed", || lifetime.client_count() == 1).await;

    drop(second);
    until("second client removed", || lifetime.client_count() == 0).await;

    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// D-5: admission and commit are one decision
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_daemon_refuses_the_handshake_rather_than_half_serving() {
    let (socket, lifetime) = spawn_daemon("refusal", ShutdownPolicy::OnLastDisconnect);

    // One client in and straight back out: with a 0 s grace and nothing in
    // flight, that commits the shutdown.
    let mut first = TestClient::connect(&socket).await;
    first.handshake(1).await;
    drop(first);
    until("daemon committed", || lifetime.is_committed()).await;

    // The listener may already be gone; if it is, the connect itself fails,
    // which is the other half of the same guarantee (the client then autostarts
    // a successor). If it is still up, the handshake must be refused with the
    // typed, retryable code — never accepted.
    if let Ok(stream) = UnixStream::connect(&socket).await {
        let (read_half, write_half) = stream.into_split();
        let mut late = TestClient {
            reader: BufReader::new(read_half),
            writer: write_half,
        };
        let response = late.handshake(1).await;
        assert!(
            response.get("result").is_none(),
            "a committed daemon must not accept a session it will not serve: {response}"
        );
        assert_eq!(
            response["error"]["code"].as_i64(),
            Some(error_code::DAEMON_SHUTTING_DOWN),
            "the refusal must be the typed retryable code: {response}"
        );
    }

    let _ = std::fs::remove_file(&socket);
}

/// BR-3's first arm. A client that arrives while a shutdown is *pending* (armed
/// or deferred, but not yet committed) cancels it.
#[tokio::test]
async fn a_client_arriving_before_the_commit_cancels_the_shutdown() {
    let (socket, lifetime) = spawn_daemon("cancel", ShutdownPolicy::OnLastDisconnect);

    // Hold the daemon in `Deferred` with a synthetic activity, so the window
    // between "armed" and "committed" is open long enough to race into
    // deterministically rather than with a sleep.
    let work = lifetime.activity(BlockingActivity::ModelDownload);

    let mut first = TestClient::connect(&socket).await;
    first.handshake(1).await;
    until("counted", || lifetime.client_count() == 1).await;
    drop(first);
    until("deferred", || lifetime.phase() == LifetimePhase::Deferred).await;
    assert!(!lifetime.is_committed());

    // A new client cancels it.
    let mut second = TestClient::connect(&socket).await;
    let response = second.handshake(1).await;
    assert!(
        response.get("result").is_some(),
        "a pre-commit arrival must be admitted: {response}"
    );
    assert_eq!(lifetime.phase(), LifetimePhase::Serving);

    // …and the work finishing must not resurrect the cancelled shutdown.
    drop(work);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !lifetime.is_committed(),
        "work finishing while a client is attached must not exit the daemon"
    );

    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// D-3: in-flight work defers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn in_flight_work_defers_the_exit_until_it_finishes() {
    let (socket, lifetime) = spawn_daemon("defer", ShutdownPolicy::OnLastDisconnect);
    let work = lifetime.activity(BlockingActivity::Turn);

    let mut client = TestClient::connect(&socket).await;
    client.handshake(1).await;
    until("counted", || lifetime.client_count() == 1).await;
    drop(client);

    until("deferred", || lifetime.phase() == LifetimePhase::Deferred).await;
    assert!(
        !lifetime.is_committed(),
        "the daemon must not exit while work is in flight"
    );

    drop(work);
    until("committed once idle", || lifetime.is_committed()).await;

    let _ = std::fs::remove_file(&socket);
}

/// A guard released by unwinding still releases. A claim that leaked on panic
/// would hold the model resident forever — the exact harm this REQ removes.
#[tokio::test]
async fn a_guard_dropped_by_a_panicking_task_still_releases() {
    let (socket, lifetime) = spawn_daemon("panic", ShutdownPolicy::OnLastDisconnect);

    let mut client = TestClient::connect(&socket).await;
    client.handshake(1).await;
    until("counted", || lifetime.client_count() == 1).await;

    let supervisor = Arc::clone(&lifetime);
    let panicked = tokio::spawn(async move {
        let _work = supervisor.activity(BlockingActivity::Turn);
        panic!("the turn blew up");
    });
    assert!(panicked.await.is_err(), "the task must have panicked");

    drop(client);
    until("committed despite the panic", || lifetime.is_committed()).await;

    let _ = std::fs::remove_file(&socket);
}

// ---------------------------------------------------------------------------
// The accept loop stops on commit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_never_policy_daemon_outlives_its_last_client() {
    let (socket, lifetime) = spawn_daemon("never", ShutdownPolicy::Never);

    let mut client = TestClient::connect(&socket).await;
    client.handshake(1).await;
    until("counted", || lifetime.client_count() == 1).await;
    drop(client);
    until("uncounted", || lifetime.client_count() == 0).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !lifetime.is_committed(),
        "`never` must survive the last disconnect (AC-8)"
    );

    // Still serving.
    let mut again = TestClient::connect(&socket).await;
    let response = again.handshake(1).await;
    assert!(response.get("result").is_some(), "{response}");

    let _ = std::fs::remove_file(&socket);
}
