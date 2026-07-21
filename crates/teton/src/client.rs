//! The daemon connection: framing, handshake, autostart, and the event pump.
//!
//! Transport is intentionally thin — a synchronous, newline-delimited JSON-RPC
//! client matching the daemon's framing (`tetond::server`): one background thread
//! reads lines off the [`UnixStream`] and classifies each into an [`Incoming`]
//! (a response, a broadcast event, or a lag notice) on an [`mpsc`] channel; the
//! main thread writes requests and drains the channel. The CLI holds no HTTP
//! client of its own — every remote call is the daemon's job through its single
//! egress choke point (BR-1). All rendering happens through the [`Surface`] and
//! [`Prompter`] seams carried in [`UiContext`], so the pump is testable in the
//! rendering modules with scripted event streams, while this module is the small
//! untested socket shell.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::Value;

use teton_protocol::handshake::{HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{Id, Response, RpcError};
use teton_protocol::methods::{self, RpcMethod};
use teton_protocol::{ClientKind, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};
use crate::session_ui::{self, EventOutcome, SessionState};
use teton_protocol::socket_path::DaemonPaths;

/// Diagnostic client name sent in the handshake.
const CLIENT_NAME: &str = "teton-cli";
/// This build's version, advertised in the handshake.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// JSON-RPC method events are delivered under (matches the daemon).
const EVENT_METHOD: &str = "event";
/// Method a lag eviction is delivered under (matches `tetond::broadcast`).
const LAGGED_METHOD: &str = "subscription/lagged";
/// How many times autostart polls for the socket before giving up.
const POLL_ATTEMPTS: usize = 50;
/// Delay between autostart connection attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The rendering + input context threaded through the event pump.
pub struct UiContext<'a> {
    /// Where rendered output goes.
    pub surface: &'a mut dyn Surface,
    /// Running session state (tool titles, grants, cost meter).
    pub state: &'a mut SessionState,
    /// Interactive input source for permission prompts.
    pub prompter: &'a mut dyn Prompter,
    /// Whether this command owns an interactive session and should answer
    /// permission requests. Non-interactive commands (`doctor`, `cost`, config)
    /// only render them — the daemon broadcasts a permission request to every
    /// attached client, but only the owning interactive session should reply.
    pub answer_permissions: bool,
}

/// One message read off the socket.
enum Incoming {
    /// A response to one of our requests.
    Response(Response<Value>),
    /// A broadcast event (boxed to keep this enum small).
    Event(Box<teton_protocol::events::EventEnvelope>),
    /// The daemon evicted our subscription for lagging.
    Lagged(RpcError),
}

/// A live connection to the daemon.
pub struct Connection {
    writer: UnixStream,
    incoming: Receiver<Incoming>,
    next_id: i64,
}

impl Connection {
    /// Open a connection to the daemon socket and start the reader thread.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the socket cannot be reached (no daemon).
    pub fn connect(socket: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let reader_stream = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("teton-reader".to_owned())
            .spawn(move || reader_loop(reader_stream, &tx))?;
        Ok(Self {
            writer: stream,
            incoming: rx,
            next_id: 1,
        })
    }

    /// Perform the protocol-version handshake. No events precede it (the daemon
    /// subscribes a client only after a successful handshake), so this simply
    /// waits for the matching response.
    ///
    /// # Errors
    ///
    /// Returns an error if the handshake is rejected or the connection drops.
    pub fn handshake(&mut self) -> anyhow::Result<HandshakeResult> {
        let params = HandshakeParams {
            client_kind: ClientKind::Cli,
            client_name: CLIENT_NAME.to_owned(),
            client_version: CLIENT_VERSION.to_owned(),
            protocol_min: PROTOCOL_VERSION_MIN,
            protocol_max: PROTOCOL_VERSION_MAX,
        };
        let id = self.send(params)?;
        loop {
            // No events precede the handshake, so anything but the matching
            // response is ignored.
            if let Incoming::Response(resp) = self.recv()? {
                if resp.id == id {
                    return match resp.error {
                        Some(err) => Err(anyhow::Error::new(err)),
                        None => Ok(serde_json::from_value(resp.result.unwrap_or(Value::Null))?),
                    };
                }
            }
        }
    }

    /// Send a request, pump events until its response arrives, and return either
    /// the typed result or the daemon's [`RpcError`]. Transport/parse failures
    /// surface as the outer `anyhow::Error`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection drops or a payload fails to (de)serialize.
    pub fn call<P: RpcMethod>(
        &mut self,
        params: P,
        ctx: &mut UiContext,
    ) -> anyhow::Result<Result<P::Result, RpcError>> {
        let id = self.send(params)?;
        loop {
            match self.recv()? {
                Incoming::Response(resp) => {
                    match route_response(&id, &resp.id, resp.error.is_some()) {
                        RespRoute::Match => {
                            return Ok(match resp.error {
                                Some(err) => Err(err),
                                None => {
                                    Ok(serde_json::from_value(resp.result.unwrap_or(Value::Null))?)
                                }
                            });
                        }
                        // REQ-544 minor: an uncorrelatable `Id::Null` parse-error
                        // frame belongs to this — the only — in-flight request;
                        // surface it rather than looping forever for a numeric-id
                        // reply the daemon can never send.
                        RespRoute::Surface => {
                            return Ok(Err(resp
                                .error
                                .expect("a surfaced response carries an error")));
                        }
                        RespRoute::Ignore => {} // stray ack (e.g. a permission reply)
                    }
                }
                Incoming::Event(env) => self.dispatch_event(&env, ctx)?,
                Incoming::Lagged(err) => report_lag(&err, ctx.surface),
            }
        }
    }

    /// Render one event and, if it is a permission request, resolve it and send
    /// the reply back (the ack returns later as a stray response and is ignored).
    fn dispatch_event(
        &mut self,
        env: &teton_protocol::events::EventEnvelope,
        ctx: &mut UiContext,
    ) -> anyhow::Result<()> {
        match session_ui::render_event(env, &mut *ctx.surface, &mut *ctx.state) {
            EventOutcome::Rendered => {}
            EventOutcome::Permission(req) if ctx.answer_permissions => {
                let reply = session_ui::resolve_permission(
                    &req,
                    &mut *ctx.surface,
                    &mut *ctx.prompter,
                    &mut ctx.state.grants,
                );
                self.send(reply)?;
            }
            EventOutcome::Permission(req) => {
                // Not our session to answer — surface it and leave it to the
                // interactive client that owns it.
                ctx.surface.line(
                    LineKind::Notice,
                    &format!(
                        "permission requested for tool `{}` in another session",
                        req.tool_name
                    ),
                );
            }
        }
        Ok(())
    }

    /// Serialize and write one request; returns the id assigned to it.
    fn send<P: RpcMethod>(&mut self, params: P) -> anyhow::Result<Id> {
        let id = Id::Number(self.next_id);
        self.next_id += 1;
        let request = methods::request(id.clone(), params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(id)
    }

    /// Block for the next incoming message.
    fn recv(&self) -> anyhow::Result<Incoming> {
        self.incoming
            .recv()
            .map_err(|_| anyhow!("connection to the daemon closed"))
    }
}

/// How a received [`Response`] correlates against the single in-flight request a
/// caller is awaiting (REQ-544 minor).
#[derive(Debug, PartialEq, Eq)]
enum RespRoute {
    /// The correlated reply for the pending id — resolve the call.
    Match,
    /// An uncorrelatable `Id::Null` error frame (a parse error the daemon could
    /// not attribute to an id). Because the synchronous client has exactly one
    /// request in flight, it belongs to that request — surface its error and end
    /// the wait, so the caller does not stall forever awaiting a numeric-id reply
    /// that will never come.
    Surface,
    /// A stray/uncorrelated frame (a different id, or a non-error null id) — skip.
    Ignore,
}

/// Decide how a response frame with id `resp_id` (carrying an error iff
/// `has_error`) routes for a caller awaiting `pending`. Pure so the null-id
/// anti-stall rule is unit-testable without a live socket (REQ-544 minor).
fn route_response(pending: &Id, resp_id: &Id, has_error: bool) -> RespRoute {
    if resp_id == pending {
        RespRoute::Match
    } else if *resp_id == Id::Null && has_error {
        RespRoute::Surface
    } else {
        RespRoute::Ignore
    }
}

/// Render a subscription-lag eviction as a visible error line.
fn report_lag(err: &RpcError, surface: &mut dyn Surface) {
    surface.line(
        LineKind::Error,
        &format!("event stream lagged and was reset: {}", err.message),
    );
}

/// The reader thread: parse newline-delimited frames and classify each.
fn reader_loop(stream: UnixStream, tx: &Sender<Incoming>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF: daemon closed the connection
            Ok(_) => {}
            Err(_) => break, // read error: tear the reader down
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(incoming) = classify(trimmed) {
            if tx.send(incoming).is_err() {
                break; // the Connection was dropped
            }
        }
    }
}

/// Classify one raw JSON-RPC frame into an [`Incoming`], or `None` if it is not
/// something the client acts on.
fn classify(raw: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(raw).ok()?;
    match value.get("method").and_then(Value::as_str) {
        Some(EVENT_METHOD) => {
            let params = value.get("params")?.clone();
            let envelope = serde_json::from_value(params).ok()?;
            Some(Incoming::Event(Box::new(envelope)))
        }
        Some(LAGGED_METHOD) => {
            let params = value.get("params")?.clone();
            let err = serde_json::from_value(params).ok()?;
            Some(Incoming::Lagged(err))
        }
        Some(_) => None, // an unknown notification method; ignore
        None => {
            let resp = serde_json::from_value::<Response<Value>>(value).ok()?;
            Some(Incoming::Response(resp))
        }
    }
}

/// Connect to the daemon, autostarting `tetond` if the socket is absent.
///
/// # Errors
///
/// Returns an error if the daemon cannot be reached even after autostart, or if
/// the handshake is rejected.
pub fn ensure_connected(
    paths: &DaemonPaths,
    surface: &mut dyn Surface,
) -> anyhow::Result<Connection> {
    if let Ok(mut conn) = Connection::connect(&paths.socket) {
        conn.handshake()?;
        return Ok(conn);
    }

    surface.line(LineKind::Info, "no daemon reachable — starting tetond…");
    spawn_daemon()?;

    for _ in 0..POLL_ATTEMPTS {
        thread::sleep(POLL_INTERVAL);
        if let Ok(mut conn) = Connection::connect(&paths.socket) {
            conn.handshake()?;
            surface.line(LineKind::Info, "daemon started.");
            return Ok(conn);
        }
    }
    bail!("could not reach the daemon after autostart; try running `tetond` manually")
}

/// Spawn a detached `tetond` process. It takes the single-instance lock itself,
/// so a redundant spawn is harmless (the extra process exits cleanly).
fn spawn_daemon() -> anyhow::Result<()> {
    let binary = daemon_binary_path();
    Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("failed to start daemon `{}`: {e}", binary.display()))?;
    Ok(())
}

/// Locate the `tetond` binary: next to this executable if present, else on PATH.
fn daemon_binary_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_daemon_binary(exe_dir.as_deref())
}

/// Pure resolver: prefer `tetond` beside `exe_dir`, else the bare name for PATH.
fn resolve_daemon_binary(exe_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = exe_dir {
        let candidate = dir.join("tetond");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("tetond")
}

#[cfg(test)]
mod tests {
    use super::*;
    use teton_protocol::jsonrpc::error_code;

    #[test]
    fn classify_reads_a_success_response() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"session_id":"s1"}}"#;
        match classify(raw) {
            Some(Incoming::Response(resp)) => {
                assert_eq!(resp.id, Id::Number(1));
                assert!(resp.error.is_none());
            }
            _ => panic!("expected a response"),
        }
    }

    #[test]
    fn classify_reads_an_error_response() {
        let raw = format!(
            r#"{{"jsonrpc":"2.0","id":2,"error":{{"code":{},"message":"method not found"}}}}"#,
            error_code::METHOD_NOT_FOUND
        );
        match classify(&raw) {
            Some(Incoming::Response(resp)) => {
                assert_eq!(resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
            }
            _ => panic!("expected an error response"),
        }
    }

    #[test]
    fn classify_reads_an_event_notification() {
        let raw = r#"{"jsonrpc":"2.0","method":"event","params":{
            "session_id":"s1","seq":3,"event":"route_decided",
            "provider_id":"anthropic","reason":"spec routes to the frontier tier"}}"#;
        match classify(raw) {
            Some(Incoming::Event(env)) => {
                assert_eq!(env.event_name(), "route_decided");
                assert_eq!(env.seq, 3);
            }
            _ => panic!("expected an event"),
        }
    }

    #[test]
    fn classify_reads_a_lag_notice() {
        let raw = r#"{"jsonrpc":"2.0","method":"subscription/lagged","params":{
            "code":-32004,"message":"subscription evicted"}}"#;
        match classify(raw) {
            Some(Incoming::Lagged(err)) => assert_eq!(err.code, -32004),
            _ => panic!("expected a lag notice"),
        }
    }

    #[test]
    fn classify_ignores_unknown_notifications_and_junk() {
        assert!(classify(r#"{"jsonrpc":"2.0","method":"mystery","params":{}}"#).is_none());
        assert!(classify("not json at all").is_none());
    }

    #[test]
    fn a_null_id_error_frame_is_surfaced_so_a_caller_never_stalls() {
        // REQ-544 minor: the daemon answers an unparseable request with an
        // `Id::Null` error frame. A synchronous caller awaiting its numeric id must
        // NOT loop forever — the null-id error belongs to the single in-flight
        // request and is surfaced to end the wait.
        let pending = Id::Number(7);
        // The correlated reply matches regardless of whether it carries an error.
        assert_eq!(
            route_response(&pending, &Id::Number(7), false),
            RespRoute::Match
        );
        assert_eq!(
            route_response(&pending, &Id::Number(7), true),
            RespRoute::Match
        );
        // A null-id ERROR frame ends the wait (the anti-stall path).
        assert_eq!(
            route_response(&pending, &Id::Null, true),
            RespRoute::Surface
        );
        // A null-id WITHOUT an error is not actionable (never issued in practice) —
        // ignore rather than surface a non-existent error.
        assert_eq!(
            route_response(&pending, &Id::Null, false),
            RespRoute::Ignore
        );
        // A stray ack for a different numeric id is ignored (e.g. a permission
        // reply), matching the prior behavior.
        assert_eq!(
            route_response(&pending, &Id::Number(99), false),
            RespRoute::Ignore
        );
    }

    #[test]
    fn resolve_daemon_binary_prefers_a_sibling_then_falls_back_to_path() {
        // Empty/absent dir → bare name for PATH lookup.
        assert_eq!(resolve_daemon_binary(None), PathBuf::from("tetond"));

        let dir = std::env::temp_dir().join(format!("teton-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_daemon_binary(Some(&dir)), PathBuf::from("tetond"));

        // A sibling `tetond` file is preferred.
        let sibling = dir.join("tetond");
        std::fs::write(&sibling, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_daemon_binary(Some(&dir)), sibling);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
