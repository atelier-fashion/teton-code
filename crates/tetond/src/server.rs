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

use teton_protocol::events::{DaemonClientAttach, Event, ModelLifecycle, PhaseTransition};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Notification, Response, RpcError};
use teton_protocol::methods::{
    ConfigGetParams, ConfigGetResult, ConfigSetParams, ConfigSetResult, CostQueryParams,
    ModelConfirmParams, ModelListParams, ModelSetParams, ModelStatusParams,
    PermissionRespondParams, PermissionRespondResult, PromptBlock, PromptTurnParams, RpcMethod,
    SessionAttachParams, SessionAttachResult, SessionCreateParams, SessionCreateResult,
    SessionListParams, SessionListResult,
};

use crate::auth;
use crate::broadcast::{
    EventBus, Subscription, DEFAULT_CAPACITY, SUBSCRIPTION_LAGGED_CODE, SUBSCRIPTION_LAGGED_METHOD,
};
use crate::runtime::DaemonRuntime;
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
    /// The assembled engine/router/egress/cost/MCP state prompt turns drive.
    pub runtime: Arc<DaemonRuntime>,
}

impl Daemon {
    /// A daemon with no sessions, no subscribers, and a minimal runtime (no local
    /// tier, empty config). Used by the skeleton session-registry tests where no
    /// prompt turns run.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: SessionRegistry::new(),
            events: Arc::new(EventBus::new()),
            runtime: Arc::new(DaemonRuntime::minimal()),
        }
    }

    /// A daemon over an explicit event bus and assembled [`DaemonRuntime`]. This
    /// is the production path ([`crate::main`]) and the acceptance suite's entry
    /// point: the runtime carries the engine, providers, and cost ledger, while
    /// the shared bus is the same one the runtime records cost and privacy events
    /// onto, so those events reach attached clients.
    #[must_use]
    pub fn with_runtime(events: Arc<EventBus>, runtime: Arc<DaemonRuntime>) -> Self {
        Self {
            sessions: SessionRegistry::new(),
            events,
            runtime,
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

/// Binds a listener at `path`, replacing any stale socket file and locking the
/// new one down to owner-only (`0600`). The parent directory is created (or
/// tightened to) `0700` first, so the socket is never briefly reachable by
/// group/other before its own mode lands (REQ-544 L-1).
///
/// # Errors
///
/// Returns an OS error if the parent directory, the bind, or the permission
/// change fails.
pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        auth::secure_socket_dir(parent)?;
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
    // In-flight `session/prompt` executions. A prompt turn is run on its own task
    // so the reader loop stays free to process the `permission/respond` that
    // unblocks the harness permission gate mid-turn (otherwise the loop would
    // deadlock awaiting a reply it cannot read).
    let mut prompt_tasks: Vec<JoinHandle<()>> = Vec::new();
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
                // A frame we cannot even parse has no recoverable id — reply with
                // the spec's `null` id so it never collides with a real request
                // (REQ-544 minor).
                let _ = out_tx
                    .send(error_string(
                        Id::Null,
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

        // `session/prompt` runs on its own task (see `prompt_tasks`); every other
        // method dispatches synchronously and replies immediately.
        if method == PromptTurnParams::METHOD {
            if let Some(handle) = spawn_prompt_turn(&daemon, id, params, &out_tx) {
                // Prune completed turns before tracking a new one so the vector
                // does not grow unbounded across a long-lived connection's turns
                // (REQ-544 minor). Only still-running handles are kept, to be
                // aborted at teardown.
                prompt_tasks.retain(|h| !h.is_finished());
                prompt_tasks.push(handle);
            }
            continue;
        }

        if let Some(response) = dispatch(&daemon, id, method, params) {
            let _ = out_tx.send(response).await;
        }
    }

    // Teardown: stop forwarding events and abandon any in-flight prompt turns,
    // then let the writer drain and exit once every outbound sender is gone.
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    for task in prompt_tasks {
        task.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

/// Spawns a `session/prompt` turn on its own task. The turn streams events over
/// the shared bus while it runs and sends its terminal response (or error) over
/// `out_tx` when it finishes. Returns the task handle so teardown can abandon it,
/// or `None` when the request could not be started (an error response is queued).
fn spawn_prompt_turn(
    daemon: &Arc<Daemon>,
    id: Id,
    params: Value,
    out_tx: &mpsc::Sender<String>,
) -> Option<JoinHandle<()>> {
    let params: PromptTurnParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => {
            let _ = out_tx.try_send(error_string(
                id,
                error_code::INVALID_PARAMS,
                "invalid params",
            ));
            return None;
        }
    };

    let Some(summary) = daemon.sessions.get(&params.session_id) else {
        let _ = out_tx.try_send(error_string(
            id,
            error_code::UNKNOWN_SESSION,
            "unknown session",
        ));
        return None;
    };

    let prompt = flatten_prompt(&params.prompt);
    let runtime = Arc::clone(&daemon.runtime);
    let events = Arc::clone(&daemon.events);
    let out = out_tx.clone();

    Some(tokio::spawn(async move {
        let result = runtime
            .run_prompt_turn(
                &events,
                summary.session_id.clone(),
                summary.mode,
                summary.phase,
                prompt,
            )
            .await;
        let response = match result {
            Ok(res) => ok_string(id, &res),
            Err(err) => error_from(id, err),
        };
        let _ = out.send(response).await;
    }))
}

/// Flatten prompt content blocks into a single prompt string. Text blocks join
/// with newlines; a resource link contributes a bracketed reference.
fn flatten_prompt(blocks: &[PromptBlock]) -> String {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            PromptBlock::Text { text } => parts.push(text.clone()),
            PromptBlock::ResourceLink { uri, name } => {
                let label = name.as_deref().unwrap_or(uri);
                parts.push(format!("[resource: {label} ({uri})]"));
            }
        }
    }
    parts.join("\n")
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

    // Replay the local-model lifecycle (BR-9 / AC-8) to the just-subscribed
    // client so it can observe probe → benchmark → ready (or disabled /
    // stepped-down). Published after the subscribe above, so this client receives
    // it; a machine with no local tier has an empty sequence and emits nothing.
    for lifecycle in daemon.runtime.lifecycle_events() {
        daemon.events.publish(
            None,
            Event::ModelLifecycle(ModelLifecycle::clone(lifecycle)),
        );
    }

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
        PermissionRespondParams::METHOD => Some(handle_permission_respond(daemon, id, params)),
        ModelConfirmParams::METHOD => Some(handle_model_confirm(daemon, id, params)),
        ModelListParams::METHOD => Some(ok_string(id, &daemon.runtime.model_list())),
        ModelSetParams::METHOD => Some(handle_model_set(daemon, id, params)),
        ModelStatusParams::METHOD => Some(ok_string(id, &daemon.runtime.model_status())),
        ConfigGetParams::METHOD => Some(handle_config_get(daemon, id)),
        ConfigSetParams::METHOD => Some(handle_config_set(daemon, id, params)),
        CostQueryParams::METHOD => Some(handle_cost_query(daemon, id)),
        _ => Some(error_string(
            id,
            error_code::METHOD_NOT_FOUND,
            "method not found",
        )),
    }
}

/// Deliver a client's `permission/respond` to the waiting harness gate. Always
/// acknowledges (idempotent): a late or duplicate reply for a prompt that already
/// resolved simply finds no waiter.
fn handle_permission_respond(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: PermissionRespondParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    daemon
        .runtime
        .pending()
        .resolve(&params.request_id, params.outcome);
    ok_string(id, &PermissionRespondResult {})
}

/// Deliver a client's `model/confirm` to the waiting consent flow (REQ-547 BR-1).
///
/// The counterpart to [`handle_permission_respond`], and deliberately the same
/// shape: the daemon broadcast a proposal carrying a `request_id`, and the
/// deciding client answers by that id while this reader loop stays free to keep
/// reading. That is what makes the round-trip deadlock-free — the consent flow
/// awaits on its own task, never on this one.
///
/// Unlike a permission answer, a model choice can be *wrong* in a way the client
/// can fix (an unknown catalog name, an above-RAM-floor pick with no second
/// confirmation, BR-3). Those come back as `INVALID_PARAMS` with the proposal
/// still open, rather than silently consuming the user's one chance to answer.
fn handle_model_confirm(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: ModelConfirmParams = match serde_json::from_value(params) {
        Ok(params) => params,
        // A closed enum by design (TASK-001): an `outcome` this build does not
        // understand is an error, never a silent fallback to "accept".
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    match daemon.runtime.confirm_model(params) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Change the selected model after first run (`model/set`, AC-9).
///
/// Records and announces the decision synchronously so the client gets an
/// immediate answer, then installs the newly chosen weights on its own task —
/// a multi-gigabyte download must not hold the reader loop.
fn handle_model_set(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: ModelSetParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    match daemon
        .runtime
        .set_model(&params.name, params.confirmed_above_ram_floor)
    {
        Ok(result) => {
            if tokio::runtime::Handle::try_current().is_ok() {
                let runtime = Arc::clone(&daemon.runtime);
                tokio::spawn(async move {
                    runtime.install_selected_model().await;
                });
            }
            ok_string(id, &result)
        }
        Err(err) => error_from(id, err),
    }
}

/// Serve the current configuration snapshot (`config/get`).
fn handle_config_get(daemon: &Daemon, id: Id) -> String {
    ok_string(
        id,
        &ConfigGetResult {
            snapshot: daemon.runtime.config_snapshot(),
        },
    )
}

/// Apply a configuration mutation (`config/set`), rejecting it on validation
/// failure (e.g. a raw key in `auth_ref`, BR-7).
fn handle_config_set(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: ConfigSetParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    match daemon.runtime.apply_config_update(params.update) {
        Ok(()) => ok_string(id, &ConfigSetResult { applied: true }),
        Err(err) => error_from(id, err),
    }
}

/// Serve the authoritative cost report from the ledger (`cost/query`, BR-2).
fn handle_cost_query(daemon: &Daemon, id: Id) -> String {
    match daemon.runtime.cost_report() {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
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

/// Extracts the JSON-RPC id from a raw request, falling back to the spec's
/// `null` id when it is absent or malformed (REQ-544 minor).
///
/// A `null` fallback — rather than a `0` sentinel — means two malformed requests
/// cannot produce colliding response ids (and neither can collide with a real
/// pending request id `0`).
fn extract_id(value: &Value) -> Id {
    match value.get("id") {
        Some(Value::Number(n)) => n.as_i64().map_or(Id::Null, Id::Number),
        Some(Value::String(s)) => Id::Str(s.clone()),
        _ => Id::Null,
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
        // REQ-544 minor: an absent or malformed id maps to the spec's `null` id,
        // never a `0` sentinel that two bad requests would share.
        assert_eq!(extract_id(&serde_json::json!({})), Id::Null);
        assert_eq!(
            extract_id(&serde_json::json!({"id": {"nested": true}})),
            Id::Null
        );
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
