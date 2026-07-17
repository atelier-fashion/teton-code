//! The daemon spine: a tokio Unix-domain-socket JSON-RPC server.
//!
//! One [`UnixListener`] accepts connections; each accepted stream is
//! peer-credential checked (see [`crate::auth`]) and then handed to a per-client
//! task. Every client connection is full-duplex: a reader loop parses
//! newline-delimited JSON-RPC requests, and a writer task drains an outbound
//! channel fed by both request responses and broadcast events. A client must
//! complete the [`handshake`] before any other method is accepted. Sessions and
//! the event bus live in the shared [`Daemon`], so they outlive any one client.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use teton_protocol::events::{DaemonClientAttach, Event, PhaseTransition};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Notification, Response, RpcError};
use teton_protocol::methods::{
    RpcMethod, SessionAttachParams, SessionAttachResult, SessionCreateParams, SessionCreateResult,
    SessionListParams, SessionListResult,
};

use crate::auth;
use crate::broadcast::{
    EventBus, Subscription, DEFAULT_CAPACITY, SUBSCRIPTION_LAGGED_CODE, SUBSCRIPTION_LAGGED_METHOD,
};
use crate::sessions::SessionRegistry;

/// Depth of a client's outbound message queue (responses + events).
const OUTBOUND_CAPACITY: usize = 1024;

/// JSON-RPC method name events are delivered under.
const EVENT_METHOD: &str = "event";

/// Shared daemon state: the session registry and the event bus.
///
/// A single `Daemon` is wrapped in an [`Arc`] and shared by every client task,
/// which is what makes sessions outlive the clients that create them.
pub struct Daemon {
    /// Authoritative session registry.
    pub sessions: SessionRegistry,
    /// Event fan-out to subscribed clients.
    pub events: Arc<EventBus>,
}

impl Daemon {
    /// A daemon with no sessions and no subscribers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: SessionRegistry::new(),
            events: Arc::new(EventBus::new()),
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

/// Binds a listener at `path`, replacing any stale socket file and locking the
/// new one down to owner-only (`0600`).
///
/// # Errors
///
/// Returns an OS error if the parent directory, the bind, or the permission
/// change fails.
pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Safe to remove: the caller holds the single-instance lock, so any socket
    // file here is stale (a previous run that did not clean up).
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    auth::secure_socket_permissions(path)?;
    Ok(listener)
}

/// Accepts connections forever, spawning an authorized per-client task for each.
///
/// # Errors
///
/// Returns an OS error only if `accept` itself fails; individual connection
/// failures (including rejected peers) are handled per-connection and do not
/// stop the server.
pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) -> std::io::Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        match auth::check_peer(&stream) {
            Ok(_uid) => {
                let daemon = Arc::clone(&daemon);
                tokio::spawn(handle_client(stream, daemon));
            }
            Err(_err) => {
                // Reject unauthorized peers by dropping the stream. The message
                // is deliberately content-free (conventions: privacy in logs).
                eprintln!("tetond: refused a connection from an unauthorized peer");
            }
        }
    }
}

/// Drives one client connection from handshake to disconnect.
async fn handle_client(stream: UnixStream, daemon: Arc<Daemon>) {
    let (read_half, write_half) = stream.into_split();
    let (out_tx, out_rx) = mpsc::channel::<String>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(write_loop(write_half, out_rx));

    let mut reader = BufReader::new(read_half);
    let mut handshaked = false;
    let mut forwarder: Option<JoinHandle<()>> = None;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF: the client disconnected.
            Ok(_) => {}
            Err(_) => break, // Read error: tear the connection down.
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                let _ = out_tx
                    .send(error_string(
                        Id::Number(0),
                        error_code::PARSE_ERROR,
                        "invalid json",
                    ))
                    .await;
                continue;
            }
        };

        let id = extract_id(&value);
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        if !handshaked {
            // Handshake-before-any-method: everything else is refused until the
            // protocol version has been negotiated.
            if method != HandshakeParams::METHOD {
                let _ = out_tx
                    .send(error_string(
                        id,
                        error_code::INVALID_REQUEST,
                        "handshake required before any other method",
                    ))
                    .await;
                continue;
            }

            // On success, subscribe and start forwarding events. On failure the
            // error response is already queued and the client stays unauthenticated.
            if let Some(sub) = do_handshake(&daemon, id, params, &out_tx) {
                handshaked = true;
                forwarder = Some(tokio::spawn(forward_events(sub, out_tx.clone())));
            }
            continue;
        }

        if let Some(response) = dispatch(&daemon, id, method, params) {
            let _ = out_tx.send(response).await;
        }
    }

    // Teardown: stop forwarding events, then let the writer drain and exit once
    // every outbound sender is gone.
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

/// Performs the handshake, and on success subscribes this client to the bus.
///
/// Returns the new [`Subscription`] on success (so the caller can start the
/// event forwarder), or `None` on failure (an error response has been queued).
fn do_handshake(
    daemon: &Daemon,
    id: Id,
    params: Value,
    out_tx: &mpsc::Sender<String>,
) -> Option<Subscription> {
    let params: HandshakeParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => {
            let _ = out_tx.try_send(error_string(
                id,
                error_code::INVALID_PARAMS,
                "invalid handshake params",
            ));
            return None;
        }
    };

    let version = match handshake::negotiate_from(&params) {
        Ok(version) => version,
        Err(err) => {
            let _ = out_tx.try_send(error_from(id, err.to_rpc_error()));
            return None;
        }
    };

    // Announce the attach to clients already subscribed, *before* subscribing
    // this one, so the newcomer does not receive its own attach event.
    daemon.events.publish(
        None,
        Event::DaemonClientAttach(DaemonClientAttach {
            client_kind: params.client_kind,
            protocol_version: version,
        }),
    );
    let subscription = daemon.events.subscribe(DEFAULT_CAPACITY);

    let result = HandshakeResult {
        protocol_version: version,
        daemon_name: "tetond".to_owned(),
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities: Vec::new(),
    };
    let _ = out_tx.try_send(ok_string(id, &result));
    Some(subscription)
}

/// Dispatches a post-handshake request to its typed handler, returning the
/// serialized response.
fn dispatch(daemon: &Daemon, id: Id, method: &str, params: Value) -> Option<String> {
    match method {
        SessionCreateParams::METHOD => Some(handle_session_create(daemon, id, params)),
        SessionListParams::METHOD => {
            let result = SessionListResult {
                sessions: daemon.sessions.list(),
            };
            Some(ok_string(id, &result))
        }
        SessionAttachParams::METHOD => Some(handle_session_attach(daemon, id, params)),
        _ => Some(error_string(
            id,
            error_code::METHOD_NOT_FOUND,
            "method not found",
        )),
    }
}

fn handle_session_create(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: SessionCreateParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    match daemon.sessions.create(params.mode, params.phase) {
        Ok(summary) => {
            // Broadcast a session-scoped event so subscribed peers learn of the
            // new session. Entering a structured session's first phase is a
            // phase transition from nothing to that phase.
            if let Some(phase) = summary.phase {
                daemon.events.publish(
                    Some(summary.session_id.clone()),
                    Event::PhaseTransition(PhaseTransition {
                        from_phase: None,
                        to_phase: phase,
                        artifacts: Vec::new(),
                    }),
                );
            }
            ok_string(
                id,
                &SessionCreateResult {
                    session_id: summary.session_id,
                },
            )
        }
        Err(message) => error_string(id, error_code::INVALID_PARAMS, message),
    }
}

fn handle_session_attach(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: SessionAttachParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    match daemon.sessions.get(&params.session_id) {
        Some(session) => ok_string(id, &SessionAttachResult { session }),
        None => error_string(id, error_code::UNKNOWN_SESSION, "unknown session"),
    }
}

/// Forwards broadcast events from `sub` to the client's outbound channel until
/// the subscription ends. If the bus evicted it for lagging, emits a final
/// [`SUBSCRIPTION_LAGGED_METHOD`] notice before stopping.
async fn forward_events(mut sub: Subscription, out_tx: mpsc::Sender<String>) {
    loop {
        match sub.recv().await {
            Some(envelope) => {
                let note = Notification::new(EVENT_METHOD, envelope);
                if let Ok(text) = serde_json::to_string(&note) {
                    if out_tx.send(text).await.is_err() {
                        break; // client's writer is gone
                    }
                }
            }
            None => {
                if sub.is_lagged() {
                    let err = RpcError::new(
                        SUBSCRIPTION_LAGGED_CODE,
                        "subscription evicted: the client fell too far behind the event stream",
                    );
                    let note = Notification::new(SUBSCRIPTION_LAGGED_METHOD, err);
                    if let Ok(text) = serde_json::to_string(&note) {
                        let _ = out_tx.try_send(text);
                    }
                }
                break;
            }
        }
    }
}

/// Writes newline-delimited outbound messages to the socket until the channel
/// closes or the socket errors.
async fn write_loop(mut write_half: OwnedWriteHalf, mut out_rx: mpsc::Receiver<String>) {
    while let Some(mut message) = out_rx.recv().await {
        message.push('\n');
        if write_half.write_all(message.as_bytes()).await.is_err() {
            break;
        }
        if write_half.flush().await.is_err() {
            break;
        }
    }
}

/// Extracts the JSON-RPC id from a raw request, defaulting to `0` when absent
/// or malformed (skeleton clients always send a concrete id).
fn extract_id(value: &Value) -> Id {
    match value.get("id") {
        Some(Value::Number(n)) => n.as_i64().map_or(Id::Number(0), Id::Number),
        Some(Value::String(s)) => Id::Str(s.clone()),
        _ => Id::Number(0),
    }
}

/// Serializes a success response.
fn ok_string<R: Serialize>(id: Id, result: &R) -> String {
    let value = serde_json::to_value(result).unwrap_or(Value::Null);
    serde_json::to_string(&Response::success(id, value)).unwrap_or_default()
}

/// Serializes an error response from a code and message.
fn error_string(id: Id, code: i64, message: &str) -> String {
    serde_json::to_string(&Response::<Value>::failure(
        id,
        RpcError::new(code, message),
    ))
    .unwrap_or_default()
}

/// Serializes an error response from an existing [`RpcError`].
fn error_from(id: Id, error: RpcError) -> String {
    serde_json::to_string(&Response::<Value>::failure(id, error)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_socket(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "teton-{tag}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn bind_listener_creates_the_socket_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_socket("perm");
        let _listener = bind_listener(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn bind_listener_replaces_a_stale_socket_file() {
        let path = temp_socket("stale");
        std::fs::write(&path, b"stale").unwrap();
        // Should succeed by removing the stale file rather than erroring.
        let _listener = bind_listener(&path).unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_id_reads_numbers_and_strings() {
        assert_eq!(extract_id(&serde_json::json!({"id": 7})), Id::Number(7));
        assert_eq!(
            extract_id(&serde_json::json!({"id": "abc"})),
            Id::Str("abc".to_owned())
        );
        assert_eq!(extract_id(&serde_json::json!({})), Id::Number(0));
    }

    #[test]
    fn dispatch_rejects_unknown_methods() {
        let daemon = Daemon::new();
        let response = dispatch(&daemon, Id::Number(1), "does/not-exist", Value::Null).unwrap();
        assert!(response.contains("-32601")); // METHOD_NOT_FOUND
    }

    #[test]
    fn dispatch_lists_created_sessions() {
        let daemon = Daemon::new();
        let created = handle_session_create(
            &daemon,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("session_id"));

        let listed = dispatch(
            &daemon,
            Id::Number(2),
            SessionListParams::METHOD,
            Value::Null,
        )
        .unwrap();
        assert!(listed.contains("sess-0"));
    }
}
