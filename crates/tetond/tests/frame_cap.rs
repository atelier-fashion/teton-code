//! Integration test for REQ-568 BR-6/AC-5: the inbound frame cap.
//!
//! A client pushes a frame larger than `MAX_FRAME` at the daemon over a real
//! Unix socket and observes ADR-D's contract: an `INVALID_PARAMS` refusal with a
//! `null` id (the frame was never parsed, so there is no id to answer), then the
//! connection closes — no resync to the next newline. The daemon itself is
//! unharmed: a fresh connection handshakes and creates a session immediately
//! afterwards, and a large-but-legal frame still round-trips, which is what the
//! cap's headroom exists for.
//!
//! The memory bound the cap actually asserts is structural, not observable from
//! here: the reader takes its bytes through a per-frame `take(MAX_FRAME)`, so it
//! is incapable of buffering more. This file covers the behaviour that bound
//! produces.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use teton_protocol::jsonrpc::error_code;
use teton_protocol::{PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};
use tetond::{server, Daemon};

/// ADR-D's cap, restated here because it is private to the daemon.
///
/// Restating it is safe in the direction that matters: a frame of this size + 1
/// is oversized for any cap this small or smaller, so the test can only start
/// failing if the cap *grows* — which is the change that should have to argue
/// for itself.
const MAX_FRAME: usize = 4 * 1024 * 1024;

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

    /// Reads until the response with a matching id arrives, skipping any event
    /// notifications interleaved on the stream.
    async fn read_response(&mut self, id: i64) -> Value {
        loop {
            let value = read_line(&mut self.reader)
                .await
                .expect("connection closed unexpectedly");
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

/// Reads one line from the daemon, or `None` when the daemon closed the
/// connection.
///
/// A free function rather than a method because the oversized-frame test has to
/// read and write at the same time, and so borrows the two halves separately.
/// The timeout is generous: the write side of that test is 4 MiB.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<Value> {
    let mut line = String::new();
    let n = timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for a line")
        .unwrap();
    if n == 0 {
        return None; // EOF: the daemon closed the connection.
    }
    Some(serde_json::from_str(&line).unwrap())
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

/// One newline-terminated frame of `MAX_FRAME + 1` bytes.
///
/// The payload is *valid* JSON on purpose. An oversized blob of garbage would
/// also be refused, but with `PARSE_ERROR` from the parse arm — the test would
/// then pass without the cap existing at all. Valid JSON makes `INVALID_PARAMS`
/// attributable to the size and nothing else.
fn oversized_frame() -> String {
    let prefix = r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{"pad":""#;
    let suffix = r#""}}"#;
    let pad = "a".repeat(MAX_FRAME + 1 - prefix.len() - suffix.len());
    let frame = format!("{prefix}{pad}{suffix}");
    assert_eq!(frame.len(), MAX_FRAME + 1);
    assert!(
        serde_json::from_str::<Value>(&frame).is_ok(),
        "the oversized frame must be valid JSON, or the refusal proves nothing"
    );
    format!("{frame}\n")
}

#[tokio::test]
async fn an_oversized_frame_is_refused_and_closed_and_the_daemon_keeps_serving() {
    let path = temp_socket("frame-cap");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    // Deliberately no handshake first: the cap has to hold for a peer that has
    // negotiated nothing, which is the connection that would otherwise get to
    // size the daemon's read buffer before proving anything about itself.
    let TestClient {
        mut reader,
        mut writer,
    } = TestClient::connect(&path).await;
    let frame = oversized_frame();

    // Written and read concurrently, because the daemon stops reading at
    // MAX_FRAME and then closes: the tail of the frame either sits against a
    // full socket buffer or fails with a broken pipe. Both are the refusal
    // working, so the write's outcome is deliberately dropped — the assertion
    // is on the read side.
    let (_wrote, refusal) = tokio::join!(
        async {
            let _ = writer.write_all(frame.as_bytes()).await;
            let _ = writer.flush().await;
        },
        read_line(&mut reader),
    );

    let refusal = refusal.expect("the daemon must answer before it closes");
    assert!(
        refusal["id"].is_null(),
        "an unparsed frame has no id to answer, got: {refusal}"
    );
    assert_eq!(
        refusal["error"]["code"].as_i64().unwrap(),
        error_code::INVALID_PARAMS,
        "expected the size refusal, got: {refusal}"
    );

    // ADR-D: refuse, then close — the daemon does not resync to the next
    // newline, so the remainder of the frame is never seen as a request.
    assert!(
        read_line(&mut reader).await.is_none(),
        "the connection must close after the refusal"
    );

    // AC-5's recovery half: the refusal cost the connection, not the daemon.
    let mut fresh = TestClient::connect(&path).await;
    assert!(fresh.handshake(1).await.get("result").is_some());
    fresh
        .send(
            2,
            "session/create",
            json!({"mode": "structured", "phase": "spec"}),
        )
        .await;
    let created = fresh.read_response(2).await;
    assert!(
        created["result"]["session_id"].as_str().is_some(),
        "a fresh connection must serve normally, got: {created}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// One newline-free frame of `MAX_FRAME + 1` bytes whose `MAX_FRAME`-th byte
/// splits a two-byte UTF-8 character.
///
/// A single leading ASCII byte offsets the `é` (0xC3 0xA9) run by one, so the
/// even `MAX_FRAME` cut lands on a lead byte (0xC3) rather than aligning with a
/// whole character — the daemon's `take(MAX_FRAME)` then hands `read_line` a
/// buffer that ends mid-sequence, and `read_line` fails with `InvalidData`
/// instead of returning an oversized-but-valid line.
fn boundary_split_frame() -> Vec<u8> {
    let mut frame = String::from("x");
    frame.push_str(&"é".repeat(MAX_FRAME / 2));
    // "é" is two bytes: 1 + 2 * (MAX_FRAME / 2) == MAX_FRAME + 1.
    assert_eq!(frame.len(), MAX_FRAME + 1);
    frame.into_bytes()
}

/// REQ-568 BR-6: an oversized frame whose cap-cut splits a multi-byte UTF-8
/// sequence (or is otherwise not valid UTF-8) is *refused* with `INVALID_PARAMS`,
/// not silently dropped by closing the connection.
///
/// This is the arm the reader reaches when `read_line` returns
/// `ErrorKind::InvalidData` rather than `Ok(MAX_FRAME)`: the newline-terminated
/// ASCII oversized case (the sibling test) stops at the length check, but a
/// boundary-split frame never gets there. Reverting the error arm to
/// `Err(_) => break` drops it with no answer and fails this test.
#[tokio::test]
async fn a_boundary_split_oversized_frame_is_refused_not_dropped() {
    let path = temp_socket("frame-split");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let TestClient {
        mut reader,
        mut writer,
    } = TestClient::connect(&path).await;
    let frame = boundary_split_frame();

    // As in the oversized test: the daemon stops reading at MAX_FRAME and
    // closes, so the frame's tail sits against a full socket buffer or breaks
    // the pipe. Both are the refusal working; the assertion is on the read side.
    let (_wrote, refusal) = tokio::join!(
        async {
            let _ = writer.write_all(&frame).await;
            let _ = writer.flush().await;
        },
        read_line(&mut reader),
    );

    let refusal = refusal.expect("the daemon must answer before it closes");
    assert!(
        refusal["id"].is_null(),
        "a split frame was never parsed, so it has no id to answer: {refusal}"
    );
    assert_eq!(
        refusal["error"]["code"].as_i64().unwrap(),
        error_code::INVALID_PARAMS,
        "a boundary-split frame must be refused, not dropped: {refusal}"
    );
    assert!(
        read_line(&mut reader).await.is_none(),
        "the connection must close after the refusal"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// A frame whose JSON content plus its trailing newline totals *exactly*
/// `MAX_FRAME` bytes: the newline is the `MAX_FRAME`-th byte, so it is read
/// within budget and the frame dispatches normally.
///
/// content == MAX_FRAME - 1, then '\n' — the inside of ADR-D's boundary, one
/// byte the other side of the no-newline refusal below.
fn exact_cap_frame_with_newline() -> String {
    let prefix = r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{"pad":""#;
    let suffix = r#""}}"#;
    let pad = "a".repeat(MAX_FRAME - 1 - prefix.len() - suffix.len());
    let frame = format!("{prefix}{pad}{suffix}");
    assert_eq!(frame.len(), MAX_FRAME - 1);
    format!("{frame}\n")
}

/// REQ-568 ADR-D boundary: a frame whose content and its terminating newline
/// sum to exactly `MAX_FRAME` round-trips — it is dispatched, not refused.
///
/// Paired with [`a_frame_of_exactly_the_cap_without_a_newline_is_refused`], this
/// pins the reader's `n == MAX_FRAME && !ends_with('\n')` classifier: here the
/// newline lands *within* budget, so the `&&` short-circuits to "not oversized".
/// The `&&`→`||` mutant would instead refuse this frame (null id, INVALID_PARAMS)
/// and fail the id/code assertions below.
#[tokio::test]
async fn a_frame_that_is_exactly_the_cap_including_its_newline_round_trips() {
    let path = temp_socket("fr-ok");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let TestClient {
        mut reader,
        mut writer,
    } = TestClient::connect(&path).await;
    // No handshake first: `session/list` before the handshake is refused with
    // INVALID_REQUEST — still a *dispatched* answer carrying the frame's own id,
    // which only a fully-read frame produces (a size refusal is null id +
    // INVALID_PARAMS). That contrast is the whole point of the test.
    let frame = exact_cap_frame_with_newline();
    let (_wrote, response) = tokio::join!(
        async {
            let _ = writer.write_all(frame.as_bytes()).await;
            let _ = writer.flush().await;
        },
        read_line(&mut reader),
    );

    let response = response.expect("an exactly-cap frame must be answered");
    assert_eq!(
        response["id"].as_i64(),
        Some(1),
        "a dispatched frame is answered by its own id, not the null of a refusal: {response}"
    );
    assert_eq!(
        response["error"]["code"].as_i64().unwrap(),
        error_code::INVALID_REQUEST,
        "an exactly-cap frame must dispatch (here: handshake-required), not be \
         size-refused: {response}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

/// A frame of *exactly* `MAX_FRAME` bytes with no trailing newline.
fn exact_cap_frame_no_newline() -> Vec<u8> {
    let prefix = r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{"pad":""#;
    let suffix = r#""}}"#;
    let pad = "a".repeat(MAX_FRAME - prefix.len() - suffix.len());
    let frame = format!("{prefix}{pad}{suffix}");
    assert_eq!(frame.len(), MAX_FRAME);
    frame.into_bytes()
}

/// REQ-568 ADR-D: a full-budget frame that never presents a terminator is
/// refused. The budget fills before any newline, so `read_line` returns a
/// MAX_FRAME-length line that does not end in `\n` — the documented "oversized"
/// classification, even though the frame is not strictly over the cap.
#[tokio::test]
async fn a_frame_of_exactly_the_cap_without_a_newline_is_refused() {
    let path = temp_socket("fr-nonl");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let TestClient {
        mut reader,
        mut writer,
    } = TestClient::connect(&path).await;
    let frame = exact_cap_frame_no_newline();
    let (_wrote, refusal) = tokio::join!(
        async {
            let _ = writer.write_all(&frame).await;
            let _ = writer.flush().await;
        },
        read_line(&mut reader),
    );

    let refusal = refusal.expect("the daemon must answer before it closes");
    assert!(
        refusal["id"].is_null(),
        "a size-refused frame is never parsed, so it has no id: {refusal}"
    );
    assert_eq!(
        refusal["error"]["code"].as_i64().unwrap(),
        error_code::INVALID_PARAMS,
        "a full-budget frame with no terminator is classified oversized: {refusal}"
    );
    assert!(
        read_line(&mut reader).await.is_none(),
        "the connection must close after the refusal"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_large_but_legal_frame_round_trips() {
    let path = temp_socket("frame-legal");
    let listener = server::bind_listener(&path).unwrap();
    let daemon = Arc::new(Daemon::new());
    let server_task = tokio::spawn(server::serve(listener, daemon));

    let mut client = TestClient::connect(&path).await;
    assert!(client.handshake(1).await.get("result").is_some());

    // 64 KiB: above anything the protocol carries today and far below the cap.
    // That band is what ADR-D's headroom is for, so a frame in it must be
    // ordinary traffic, not a near miss.
    let session_id = "s".repeat(64 * 1024);
    client
        .send(2, "session/attach", json!({"session_id": session_id}))
        .await;
    let response = client.read_response(2).await;

    // Read whole and dispatched: the answer is a *handler* verdict, which only
    // a fully-parsed frame can produce — not the reader's size refusal.
    //
    // `NOT_GRANTED` rather than `UNKNOWN_SESSION` since REQ-569: this
    // connection created no session and holds no grant, so `session/attach`
    // answers before it consults the registry (BR-1), and it answers the same
    // way for a 64 KiB nonsense id as for a real one — which is the point of
    // that ordering. Either code proves the same thing here: the frame was
    // parsed and routed rather than refused by size.
    assert_eq!(
        response["error"]["code"].as_i64().unwrap(),
        error_code::NOT_GRANTED,
        "a legal 64 KiB frame must reach its handler, got: {response}"
    );

    server_task.abort();
    let _ = std::fs::remove_file(&path);
}
