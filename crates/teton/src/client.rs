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

use teton_protocol::events::{EVENT_METHOD, SUBSCRIPTION_LAGGED_METHOD};
use teton_protocol::handshake::{HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{Id, Response, RpcError};
use teton_protocol::methods::{self, RpcMethod};
use teton_protocol::{ClientKind, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

use crate::firstrun;
use crate::model_ui;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};
use crate::session_ui::{self, EventOutcome, SessionState};
use teton_protocol::socket_path::DaemonPaths;

/// Diagnostic client name sent in the handshake.
const CLIENT_NAME: &str = "teton-cli";
/// This build's version, advertised in the handshake.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
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
    /// Whether this command should answer a local-model proposal (REQ-547 BR-1).
    /// Same rule as `answer_permissions`, and for the same reason: the proposal
    /// is broadcast to every attached client, but only an interactive session
    /// asks the user. Other commands render it and leave it alone.
    pub answer_model_proposals: bool,
    /// Explicit opt-in auto-accept (BR-5 / AC-5): answer a proposal with `accept`
    /// and read no user input at all. Off unless `--yes` was passed.
    pub auto_accept_model: bool,
    /// Whether this process's **stdin** is a terminal — the one world-fact the
    /// `/model set` gate turns on (REQ-555 spec Permissions).
    ///
    /// It rides here rather than being read at handler depth because the slash
    /// handlers touch the world only through this context and the [`Surface`]
    /// seam — that is what lets a ratatui front-end inherit the commands by
    /// implementing the same seams (REQ-555 BR-9). A handler calling
    /// `std::io::stdin()` itself would be a second, invisible seam, and would
    /// leave one session holding two notions of "interactive" — this one and
    /// the stdout-derived flag that governs the banner and the entry frame.
    ///
    /// Stdin, not stdout, and read once at the edge: the stdout check asks
    /// whether *output* is a terminal, which says nothing about who produced the
    /// line that reached a handler.
    pub typed_input: bool,
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

/// What one non-blocking drain of the event channel produced (REQ-556 BR-1).
///
/// Deliberately just a count. Lifecycle stages are folded into
/// [`SessionState::loading`] by `render_event`, which every event passes
/// through — returning them here as well would be a second path to the same
/// fact, and the two would drift the first time one of them was updated.
#[derive(Debug, Default)]
pub struct Drained {
    /// How many things actually rendered. Zero means the caller's screen is
    /// untouched, so an open entry frame is still intact.
    pub rendered: usize,
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
                            // `route_response` only surfaces a frame it saw an
                            // error on, so `error` is `Some` here — but this runs
                            // on the production event pump, so a malformed frame
                            // must fail the call, not panic the CLI.
                            let err = resp.error.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "daemon returned an uncorrelatable error frame with no \
                                     error payload"
                                )
                            })?;
                            return Ok(Err(err));
                        }
                        RespRoute::Ignore => {} // stray ack (e.g. a permission reply)
                    }
                }
                Incoming::Event(env) => self.dispatch_event(&env, ctx)?,
                Incoming::Lagged(err) => report_lag(&err, ctx.surface),
            }
        }
    }

    /// Render every event already queued, without blocking (REQ-556 BR-1).
    ///
    /// The interactive entry loop calls this between polls of stdin, which is
    /// what makes a lifecycle event reach the user at the time it arrives
    /// rather than at the next turn. Before this existed, the only thing that
    /// drained the channel was [`Self::call`]'s pump, so events queued silently
    /// while the loop sat in `read_line` — the daemon knew the tier was ready
    /// and the session had no way to say so.
    ///
    /// Rendering goes through the same [`Self::dispatch_event`] the turn pump
    /// uses, so there is exactly one renderer for any given event (REQ-556
    /// BR-10) and permission/proposal answering behaves identically whether an
    /// event arrives mid-turn or while idle.
    ///
    /// Returns the `model_lifecycle` payloads seen, in order, so an idle caller
    /// can fold them into its indicator without re-inspecting the envelopes.
    ///
    /// `on_first` runs **once**, immediately before the first thing renders,
    /// and only if something is going to render. An interactive caller uses it
    /// to tear down its open entry frame, which would otherwise be overwritten
    /// by the notice. Nothing queued ⇒ `on_first` never runs ⇒ no teardown and
    /// no redraw, so an idle session does not flicker once per poll interval.
    ///
    /// # Errors
    ///
    /// Propagates a send failure from answering a permission or proposal. A
    /// disconnected channel is **not** an error here — it is reported as "no
    /// more events", and the caller discovers the drop on its next `call`.
    pub fn drain_events(
        &mut self,
        ctx: &mut UiContext,
        mut on_first: impl FnMut(),
    ) -> anyhow::Result<Drained> {
        let mut drained = Drained::default();
        loop {
            let incoming = match self.incoming.try_recv() {
                Ok(incoming) => incoming,
                // Empty: nothing queued right now. Disconnected: the daemon is
                // gone, which the next `call` reports properly — draining is not
                // the place to fail a session.
                Err(_) => return Ok(drained),
            };
            match incoming {
                Incoming::Event(env) => {
                    if drained.rendered == 0 {
                        on_first();
                    }
                    drained.rendered += 1;
                    self.dispatch_event(&env, ctx)?;
                }
                Incoming::Lagged(err) => {
                    if drained.rendered == 0 {
                        on_first();
                    }
                    drained.rendered += 1;
                    report_lag(&err, ctx.surface);
                }
                // A stray ack for a permission or proposal reply we already
                // sent fire-and-forget. `call` ignores these too. Renders
                // nothing, so it must not trigger `on_first`.
                Incoming::Response(_) => {}
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
            EventOutcome::ModelProposal(proposal) if ctx.answer_model_proposals => {
                if ctx.state.claim_model_proposal(&proposal.request_id) {
                    if let Some(reply) = model_ui::resolve_proposal(
                        &proposal,
                        ctx.auto_accept_model,
                        &mut *ctx.surface,
                        &mut *ctx.prompter,
                    ) {
                        // Fire-and-forget, exactly like a permission answer: the
                        // ack returns later as a stray response and is ignored.
                        // Awaiting it here would re-enter the event pump from
                        // inside an event dispatch.
                        self.send(reply)?;
                    }
                }
            }
            EventOutcome::ModelProposal(proposal) => {
                // Not our prompt to answer, but very much worth seeing: this is
                // why the local tier is unavailable (BR-1/BR-2).
                firstrun::render_proposal(&proposal, &mut *ctx.surface);
                ctx.surface.line(
                    LineKind::Notice,
                    "answer this prompt from an interactive `teton` session.",
                );
            }
        }
        Ok(())
    }

    /// Find and answer a proposal that was raised before this client attached.
    ///
    /// The daemon broadcasts `model_selection_proposed` exactly once, never
    /// replays it, and runs the consent flow on a task spawned *beside* the
    /// server (D-3) — so it can publish the proposal before the socket accepts
    /// anyone. A client that waited only for the event would wait forever.
    /// `model/status` is therefore the delivery path, not a fallback: it carries
    /// the entire proposal, so what is rendered here is what the event would have
    /// rendered, named pick and all (BR-2).
    ///
    /// Failures are deliberately quiet — an older daemon without the methods, or
    /// a status call that errors, must not stop a session from starting; the
    /// local tier simply stays gated (BR-1).
    ///
    /// # Errors
    ///
    /// Returns an error only if the connection drops.
    pub fn answer_outstanding_model_proposal(&mut self, ctx: &mut UiContext) -> anyhow::Result<()> {
        if !ctx.answer_model_proposals {
            return Ok(());
        }
        let Ok(status) = self.call(methods::ModelStatusParams::default(), ctx)? else {
            return Ok(());
        };
        let Some(proposal) = status.pending_proposal else {
            return Ok(());
        };
        // The live event may have arrived first (the event pump inside the
        // `model/status` call above could even have delivered it). A proposal is
        // prompted exactly once, and the shared `request_id` is what says so.
        if !ctx.state.claim_model_proposal(&proposal.request_id) {
            return Ok(());
        }
        let reply = model_ui::resolve_outstanding(
            &proposal,
            ctx.auto_accept_model,
            &mut *ctx.surface,
            &mut *ctx.prompter,
        );
        if let Some(reply) = reply {
            // Not inside an event dispatch here, so the answer is sent as a real
            // call: a refusal (an unknown name, or a missing second confirmation)
            // leaves the proposal open and deserves to be shown, not swallowed.
            match self.call(reply, ctx)? {
                Err(err) => ctx.surface.line(
                    LineKind::Error,
                    &format!("the daemon refused the model choice: {}", err.message),
                ),
                // E-8: the daemon accepted the call but found no proposal waiting
                // on that id — it was already answered, or a `teton model set`
                // superseded and cancelled it. Reporting that as success would
                // tell the user their answer decided something when a different
                // decision is on record.
                Ok(result) if !result.delivered => ctx.surface.line(
                    LineKind::Notice,
                    "that proposal was no longer open, so this answer decided nothing — \
                     the decision on record was made elsewhere (`teton model status` shows it).",
                ),
                Ok(_) => {}
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
        Some(SUBSCRIPTION_LAGGED_METHOD) => {
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

/// Connect to the daemon, autostarting `teton-code` if the socket is absent.
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

    surface.line(LineKind::Info, "no daemon reachable — starting teton-code…");
    spawn_daemon(&paths.log)?;

    if let Some(conn) = poll_for_daemon(paths)? {
        surface.line(LineKind::Info, "daemon started.");
        return Ok(conn);
    }
    // H-1 (E-4): the daemon we just spawned had no terminal, so whatever it said
    // on the way down went to its log and nowhere else. The commonest cause by
    // far is a config it refused to load — and that refusal is worthless if the
    // user only ever sees "could not reach the daemon". Quote it.
    match tail_daemon_log(&paths.log) {
        Some(tail) => bail!(
            "could not reach the daemon after autostart. The daemon reported:\n{tail}\n\
             (full log: {})",
            paths.log.display()
        ),
        None => bail!(
            "could not reach the daemon after autostart, and it left no diagnostic at {}; \
             try running `teton-code` manually to see why.",
            paths.log.display()
        ),
    }
}

/// The interactive-session variant of [`ensure_connected`]: before falling
/// back to a direct (unmanaged) spawn, give a brew-installed `teton` the
/// chance to register the launchd service instead — the consent-first absorb
/// of `brew services start teton` (see [`crate::service`]). Every gate that
/// makes the offer inapplicable (non-macOS, piped stdin, dev build, recorded
/// decline) falls straight through to the plain path, byte-identical.
pub fn ensure_connected_session(
    paths: &DaemonPaths,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> anyhow::Result<Connection> {
    if let Ok(mut conn) = Connection::connect(&paths.socket) {
        conn.handshake()?;
        return Ok(conn);
    }
    if crate::service::offer_registration(paths, surface, prompter) {
        if let Some(conn) = poll_for_daemon(paths)? {
            surface.line(
                LineKind::Info,
                "daemon registered with launchd and started — it will survive reboots.",
            );
            return Ok(conn);
        }
        // launchd accepted the service but no socket appeared in time. The
        // direct path below spawns its own daemon (a second instance exits 0
        // with "already running", so a slow service start races harmlessly)
        // and owns the log-quoting diagnostics for a daemon that dies on boot.
        surface.line(
            LineKind::Notice,
            "the service was registered but the daemon has not answered yet — trying a direct \
             start.",
        );
    }
    ensure_connected(paths, surface)
}

/// Poll the socket until a daemon answers the handshake, or give up after
/// [`POLL_ATTEMPTS`]. `Ok(None)` is "nobody came up", not an error — the
/// caller owns the diagnostics for its own start path.
fn poll_for_daemon(paths: &DaemonPaths) -> anyhow::Result<Option<Connection>> {
    for _ in 0..POLL_ATTEMPTS {
        thread::sleep(POLL_INTERVAL);
        if let Ok(mut conn) = Connection::connect(&paths.socket) {
            conn.handshake()?;
            return Ok(Some(conn));
        }
    }
    Ok(None)
}

/// How many bytes of the daemon log to quote back on an autostart failure.
const LOG_TAIL_BYTES: u64 = 4096;

/// The last few lines of the daemon's captured stderr, when it wrote any.
///
/// Bounded: a log that has been appended to across many runs must not be pasted
/// into a terminal in full, and the cause of *this* failure is at the end.
fn tail_daemon_log(log: &Path) -> Option<String> {
    let text = read_tail(log, LOG_TAIL_BYTES)?;
    let tail = text
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    (!tail.trim().is_empty()).then_some(tail)
}

/// Read at most the last `limit` bytes of `path` as lossy UTF-8.
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > limit {
        file.seek(SeekFrom::Start(len - limit)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Spawn a detached `teton-code` daemon process. It takes the single-instance lock itself,
/// so a redundant spawn is harmless (the extra process exits cleanly).
///
/// Its stderr goes to `log` rather than `/dev/null` (E-4). A daemon started this
/// way has no terminal, so discarding stderr discarded every reason it could give
/// for failing to come up — including the config refusal H-1 added, which every
/// existing user carrying REQ-544's hard-deprecated `pinned_local_model` key hits
/// on their first start. Appending (not truncating) keeps the previous run's
/// explanation if this one dies before writing its own.
fn spawn_daemon(log: &Path) -> anyhow::Result<()> {
    let binary = daemon_binary_path();
    Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(daemon_log_sink(log))
        .spawn()
        .map_err(|e| anyhow!("failed to start daemon `{}`: {e}", binary.display()))?;
    Ok(())
}

/// Size past which the daemon log is restarted rather than appended to.
///
/// Appending keeps the previous run's explanation when this one dies before
/// writing its own; a cap keeps that from becoming an unbounded file in the
/// user's state directory. Only the tail is ever read, so nothing of value is
/// lost by starting over.
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// The stderr sink for a spawned daemon: the log file, or `/dev/null` if it
/// cannot be opened (a daemon that cannot log is still a daemon worth starting).
fn daemon_log_sink(log: &Path) -> Stdio {
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let oversized = std::fs::metadata(log).is_ok_and(|meta| meta.len() > LOG_MAX_BYTES);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(oversized)
        .truncate(oversized)
        .open(log)
        .map_or_else(|_| Stdio::null(), Stdio::from)
}

/// Locate the `teton-code` daemon binary: next to this executable if present, else on PATH.
fn daemon_binary_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_daemon_binary(exe_dir.as_deref())
}

/// Pure resolver: prefer `teton-code` beside `exe_dir`, else the bare name for PATH.
fn resolve_daemon_binary(exe_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = exe_dir {
        let candidate = dir.join("teton-code");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("teton-code")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;
    use std::os::unix::net::UnixListener;
    use teton_protocol::events::ModelLifecycleStage;
    use teton_protocol::jsonrpc::error_code;

    use crate::render::RecordingSurface;

    /// A `Connection` whose channel the test feeds directly. `UnixStream::pair`
    /// gives the writer half a real, connected socket without a daemon, so
    /// `drain_events` is exercised with no server, no terminal, and no thread.
    fn test_connection() -> (Connection, mpsc::Sender<Incoming>, UnixStream) {
        let (writer, peer) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = mpsc::channel();
        (
            Connection {
                writer,
                incoming: rx,
                next_id: 1,
            },
            tx,
            peer,
        )
    }

    fn lifecycle_envelope(model: &str, stage: ModelLifecycleStage) -> Incoming {
        use teton_protocol::events::{Event, EventEnvelope, ModelLifecycle};
        Incoming::Event(Box::new(EventEnvelope {
            session_id: None,
            seq: 1,
            event: Event::ModelLifecycle(ModelLifecycle {
                model_id: model.to_owned(),
                stage,
            }),
        }))
    }

    /// REQ-556 BR-1. Before `drain_events`, the only thing that emptied this
    /// channel was `call`'s pump, so a lifecycle event that arrived while the
    /// entry loop sat in `read_line` stayed queued until the next turn — the
    /// daemon knew the tier was ready and the session had no way to say so.
    ///
    /// Exercised with no socket server and no terminal, which is the point:
    /// BR-2 hides the interactive path from every piped e2e, so this behaviour
    /// needs a route that does not involve a tty.
    #[test]
    fn draining_renders_queued_events_and_reports_their_lifecycle_stages() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Verifying {
                total_bytes: 18_600_000_000,
            },
        ))
        .expect("queue");
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Ready,
        ))
        .expect("queue");

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
        };

        let mut teardowns = 0;
        let drained = conn
            .drain_events(&mut ctx, || teardowns += 1)
            .expect("drain");

        assert_eq!(drained.rendered, 2, "both queued events rendered");
        // The caller's frame is torn down once, not once per event — otherwise
        // a burst would erase and redraw for every line in it.
        assert_eq!(teardowns, 1, "on_first runs exactly once per drain");
        assert!(
            surface.any_line_contains(LineKind::Notice, "ready"),
            "the ready line reached the surface: {:?}",
            surface.calls
        );
        // The indicator was folded on the way through, in arrival order, so the
        // terminal `Ready` wins and the session stops animating (REQ-556 BR-6).
        // Asserting through session state rather than a returned vector is the
        // point: this is the path a real session takes.
        assert!(
            state.loading.frame(0).is_none(),
            "a drained Ready must leave the indicator hidden"
        );
    }

    /// The fold happens in `render_event`, so an event drained while idle and
    /// an event drained by a turn's pump reach the indicator identically. This
    /// pins the idle half; the mid-turn half rides the same `dispatch_event`.
    #[test]
    fn draining_a_mid_load_stage_leaves_the_indicator_visible() {
        let (mut conn, tx, _peer) = test_connection();
        tx.send(lifecycle_envelope(
            "qwen3-coder-30b-a3b",
            ModelLifecycleStage::Verifying {
                total_bytes: 18_600_000_000,
            },
        ))
        .expect("queue");

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
        };
        conn.drain_events(&mut ctx, || {}).expect("drain");
        assert!(
            state.loading.frame(0).is_some(),
            "a verifying tier is work in progress and must draw"
        );
    }

    /// An idle session must not flicker: with nothing queued, the caller's
    /// entry frame is never torn down, so there is nothing to redraw.
    #[test]
    fn draining_an_empty_channel_touches_nothing() {
        let (mut conn, _tx, _peer) = test_connection();
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
        };

        let mut teardowns = 0;
        let drained = conn
            .drain_events(&mut ctx, || teardowns += 1)
            .expect("drain");

        assert_eq!(drained.rendered, 0);
        assert_eq!(teardowns, 0, "no teardown when nothing renders");
        assert!(
            surface.calls.is_empty(),
            "nothing rendered: {:?}",
            surface.calls
        );
    }

    /// A dropped daemon is reported by the next `call`, not by a drain — a
    /// disconnected channel here must read as "nothing more queued" rather than
    /// failing a session that is otherwise fine.
    #[test]
    fn draining_a_disconnected_channel_is_not_an_error() {
        let (mut conn, tx, _peer) = test_connection();
        drop(tx);
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&[]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: true,
            answer_model_proposals: true,
            auto_accept_model: false,
            typed_input: true,
        };
        let drained = conn.drain_events(&mut ctx, || {}).expect("not an error");
        assert_eq!(drained.rendered, 0);
    }

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
        let raw = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription/lagged","params":{{
            "code":{},"message":"subscription evicted"}}}}"#,
            error_code::SUBSCRIPTION_LAGGED
        );
        match classify(&raw) {
            Some(Incoming::Lagged(err)) => {
                assert_eq!(err.code, error_code::SUBSCRIPTION_LAGGED);
            }
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

    /// The stub daemon's deadlock backstop: how long it will block on any single
    /// socket operation before giving up (E-10).
    ///
    /// Both sides of this test block: the client waits for a response only the
    /// stub can send, and the stub waits for a line only the client can send. Any
    /// bug that breaks that lock-step wedges both threads — and because the
    /// client's reader thread holds a dup of the socket, dropping the connection
    /// does not give the stub an EOF either. Untimed, `join()` would then hang
    /// until CI killed the whole job, hiding a real failure behind a job timeout.
    /// Long enough that a loaded runner never trips it; short enough to be a test
    /// failure rather than an outage.
    const STUB_IO_TIMEOUT: Duration = Duration::from_secs(20);

    /// How long the stub waits for *more* traffic after the client has answered.
    ///
    /// The exchange is over at that point, so the backstop above would otherwise
    /// be paid in full on every green run. The window still has to be wide enough
    /// to catch the failure this test is about: a client that prompts twice sends
    /// its second `model/confirm` microseconds after the first, so a duplicate
    /// cannot slip out after the stub stops listening.
    const STUB_IDLE_GRACE: Duration = Duration::from_millis(500);

    /// A stub daemon that hands the *same* proposal to a client twice — once as
    /// the broadcast event, once on the `model/status` it answers next — and
    /// records every request the client sent back.
    ///
    /// Deliberately over a real `UnixStream` with the real framing: the de-dup
    /// this proves lives in the seam between the event pump and the status call,
    /// and a hand-fed `SessionState` would not exercise that seam at all.
    fn serve_a_doubly_delivered_proposal(socket: PathBuf) -> thread::JoinHandle<Vec<Value>> {
        let listener = UnixListener::bind(&socket).expect("bind the stub daemon socket");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a client connects");
            stream
                .set_read_timeout(Some(STUB_IO_TIMEOUT))
                .expect("the stub socket accepts a read timeout");
            stream
                .set_write_timeout(Some(STUB_IO_TIMEOUT))
                .expect("the stub socket accepts a write timeout");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = BufWriter::new(stream);
            let proposal = serde_json::json!({
                "request_id": "model-0",
                "probe": {
                    "total_ram_bytes": 17_179_869_184u64,
                    "free_disk_bytes": 536_870_912_000u64,
                    "gpu_class": "apple_silicon",
                    "chosen_band": "small",
                    "reason": "16 GiB of RAM puts this machine in the small band",
                },
                "proposed": {
                    "entry": {
                        "name": "qwen2.5-coder-3b",
                        "band": "small",
                        "size_bytes": 2_147_483_648u64,
                        "ram_floor_bytes": 5_368_709_120u64,
                        "provenance": {
                            "repo": "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF",
                            "host": "huggingface.co",
                            "revision": "f74adce",
                        },
                    },
                    "required_disk_bytes": 3_221_225_472u64,
                },
                "alternatives": [],
            });
            let mut seen = Vec::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let request: Value = match serde_json::from_str(line.trim()) {
                    Ok(value) => value,
                    Err(_) => break,
                };
                line.clear();
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                seen.push(request);
                if method == methods::ModelStatusParams::METHOD {
                    // The event first — the client is mid-call when it lands, so
                    // it is dispatched by the pump before the status reply.
                    let event = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": EVENT_METHOD,
                        "params": {
                            "seq": 1,
                            "event": "model_selection_proposed",
                            "request_id": proposal["request_id"],
                            "probe": proposal["probe"],
                            "proposed": proposal["proposed"],
                            "alternatives": proposal["alternatives"],
                        },
                    });
                    writeln!(writer, "{event}").unwrap();
                    writer.flush().unwrap();
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "pending_proposal": proposal },
                    });
                    writeln!(writer, "{response}").unwrap();
                } else {
                    let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}});
                    writeln!(writer, "{response}").unwrap();
                }
                writer.flush().unwrap();
                if method == methods::ModelConfirmParams::METHOD {
                    // The client has answered, so the conversation is over and
                    // the long backstop has nothing left to protect. Drop to the
                    // grace window: a duplicate answer would already be on its
                    // way, and a green run should not pay 20 seconds to prove one
                    // never came. (A socket option applies to the socket, so this
                    // reaches the reader's dup of it too.)
                    let _ = writer.get_ref().set_read_timeout(Some(STUB_IDLE_GRACE));
                }
            }
            seen
        })
    }

    /// REQ-547: seeing a proposal twice must ask a human once.
    ///
    /// Both delivery paths are live now — the daemon broadcasts the proposal
    /// *and* serves it from `model/status` — so the client meets it twice
    /// whenever it attaches in time to catch the event. Prompting twice would be
    /// a worse failure than the missed event this fixed: the second prompt would
    /// be answered into a waiter that no longer exists.
    #[test]
    fn a_proposal_seen_as_an_event_and_on_model_status_prompts_exactly_once() {
        let socket = std::env::temp_dir().join(format!("tcl{:x}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let server = serve_a_doubly_delivered_proposal(socket.clone());

        let mut conn = Connection::connect(&socket).expect("connect to the stub daemon");
        let mut surface = crate::render::RecordingSurface::new();
        let mut state = SessionState::new();
        let mut prompter = crate::prompt::ScriptedPrompter::new(&["y"]);
        let mut ctx = UiContext {
            surface: &mut surface,
            state: &mut state,
            prompter: &mut prompter,
            answer_permissions: false,
            answer_model_proposals: true,
            auto_accept_model: false,
            // No command runs under this context; the honest value for a test
            // process is the same check the real edge makes.
            typed_input: std::io::IsTerminal::is_terminal(&std::io::stdin()),
        };
        conn.answer_outstanding_model_proposal(&mut ctx)
            .expect("the round-trip completes");
        drop(conn);

        let requests = server.join().expect("the stub daemon thread");
        let _ = std::fs::remove_file(&socket);

        assert_eq!(
            prompter.asked, 1,
            "the user must be asked exactly once, however many times the \
             proposal was delivered"
        );
        let confirms = requests
            .iter()
            .filter(|r| r["method"].as_str() == Some(methods::ModelConfirmParams::METHOD))
            .count();
        assert_eq!(confirms, 1, "exactly one answer reaches the daemon");
        // And the one prompt named the pick, its size, and its RAM floor (BR-2).
        let text = surface.lines_of(crate::render::LineKind::Info).join("\n");
        assert_eq!(
            text.matches("proposed: qwen2.5-coder-3b").count(),
            1,
            "the proposal is rendered once, by name: {text}"
        );
        assert!(text.contains("2.0 GiB download"), "{text}");
        assert!(text.contains("needs 5.0 GiB RAM"), "{text}");
        // Proof that both sightings really happened and the *event* was the one
        // that prompted: the late-attach path prints its own notice, and it is
        // absent because `claim_model_proposal` had already taken this id.
        let notices = surface.lines_of(crate::render::LineKind::Notice).join("\n");
        assert!(
            !notices.contains("before this client attached"),
            "the status sighting must have been suppressed, not re-prompted: {notices}"
        );
    }

    #[test]
    fn resolve_daemon_binary_prefers_a_sibling_then_falls_back_to_path() {
        // Empty/absent dir → bare name for PATH lookup.
        assert_eq!(resolve_daemon_binary(None), PathBuf::from("teton-code"));

        let dir = std::env::temp_dir().join(format!("teton-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            resolve_daemon_binary(Some(&dir)),
            PathBuf::from("teton-code")
        );

        // A sibling `teton-code` file is preferred.
        let sibling = dir.join("teton-code");
        std::fs::write(&sibling, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_daemon_binary(Some(&dir)), sibling);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
