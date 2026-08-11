//! The daemon spine: a tokio Unix-domain-socket JSON-RPC server.
//!
//! One [`UnixListener`] accepts connections; each accepted stream is
//! peer-credential checked (see [`crate::auth`]) and then handed to a per-client
//! task. Every client connection is full-duplex: a reader loop parses
//! newline-delimited JSON-RPC requests, and a writer task drains an outbound
//! channel fed by both request responses and broadcast events. A client must
//! complete the [`handshake`] before any other method is accepted. Sessions and
//! the event bus live in the shared [`Daemon`], so they outlive any one client.
//!
//! ## Event/response ordering
//!
//! Responses and events reach the outbound channel from *different* producers —
//! request handlers push responses directly, while [`forward_events`] relays
//! broadcast events from this client's bus subscription. Left unordered, a
//! turn's final streamed `session_update` could be queued *after* the turn's
//! own response, and a strictly-FIFO client (the CLI's pump) would then render
//! it one command late — after the next prompt, or after the session-end cost
//! summary. [`EventFence`] closes that race: before a post-handshake response
//! is enqueued, the sender waits until every event the bus had already
//! delivered to this client has been moved to the outbound channel. Events a
//! handler publishes therefore always precede its response on the wire.
//!
//! ## Event scoping
//!
//! A connection receives a *session-scoped* event only for a session it is
//! attached to — one it created, or one it named in `session/attach` — or if it
//! declared `monitor` at handshake; *daemon-scoped* events (`session_id: None`)
//! reach every handshaked connection (REQ-568 BR-1/BR-2). The decision is taken
//! in [`forward_events`] against this connection's [`ConnState`], so the bus
//! stays connection-agnostic. A skipped envelope still advances the
//! [`EventFence`] watermark exactly like a delivered one, or a response would
//! wait forever on an event this connection was never going to receive.
//!
//! The same attachment set gates the *mutating* methods: `session/prompt` and
//! `session/clear` against a session this connection never attached are refused
//! with `NOT_ATTACHED` (REQ-568 BR-4). The two gates sit at the two seams every
//! client crosses — [`forward_events`] for reads, [`handle_session_clear`] and
//! [`spawn_prompt_turn`] for writes — never in a client, and never in the
//! reader loop above them, so the direct-RPC tests exercise the real gate
//! (LESSON-484). Attachment is the single grant: `monitor` buys sight of a
//! session, never the right to drive it.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use teton_protocol::events::{
    DaemonClientAttach, Event, PhaseTransition, EVENT_METHOD, SUBSCRIPTION_LAGGED_METHOD,
};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Notification, Response, RpcError};
use teton_protocol::methods::{
    ConfigGetParams, ConfigGetResult, ConfigSetParams, ConfigSetResult, CostQueryParams,
    ModelConfirmParams, ModelListParams, ModelSetParams, ModelStatusParams,
    PermissionRespondParams, PermissionRespondResult, PromptBlock, PromptTurnParams, RpcMethod,
    SessionAttachParams, SessionAttachResult, SessionClearParams, SessionCreateParams,
    SessionCreateResult, SessionListParams, SessionListResult, WebOverrideParams, WebRefreshParams,
};
use teton_protocol::SessionId;

use teton_core::lifetime::{BlockingActivity, PolicySource, ShutdownPolicy};

use crate::auth;
use crate::broadcast::{EventBus, Subscription, DEFAULT_CAPACITY};
use crate::lifetime::LifetimeSupervisor;
use crate::runtime::DaemonRuntime;
use crate::sessions::SessionRegistry;

/// Depth of a client's outbound message queue (responses + events).
const OUTBOUND_CAPACITY: usize = 1024;

/// Largest inbound frame the reader will buffer, in bytes (REQ-568 BR-6, ADR-D).
///
/// Measured, not guessed: the only method that carries bulk is `session/prompt`
/// with pasted text, and observed prompts sit well under 100 KiB; every other
/// method is sub-KiB. 4 MiB is ~40× headroom over the largest legitimate frame
/// while keeping a connection's read buffer bounded by something an
/// unauthenticated peer cannot grow.
const MAX_FRAME: u64 = 4 * 1024 * 1024;

/// How long a disconnecting client's in-flight prompt turns are given to finish
/// before they are abandoned (REQ-565 BR-2).
///
/// Generous, because the thing being protected is the turn's cost row and the
/// work already paid for; a local turn on a large model can legitimately run for
/// minutes. It is an upper bound on pathology, not a normal-path timeout.
const TURN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

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
    /// The exit-with-the-last-client decision (REQ-565). Every handshake asks it
    /// for admission and every prompt turn holds one of its activity guards, so
    /// it is shared state like the registry and the bus.
    pub lifetime: Arc<LifetimeSupervisor>,
}

impl Daemon {
    /// A daemon with no sessions, no subscribers, and a minimal runtime (no local
    /// tier, empty config). Used by the skeleton session-registry tests where no
    /// prompt turns run.
    #[must_use]
    pub fn new() -> Self {
        let events = Arc::new(EventBus::new());
        // `Never`: a bare `Daemon::new()` is a fixture, and a fixture that could
        // decide to exit would make unrelated tests race a shutdown they never
        // asked for. The production path states its policy explicitly.
        let lifetime = Arc::new(LifetimeSupervisor::new(
            ShutdownPolicy::Never,
            PolicySource::Default,
            Arc::clone(&events),
        ));
        Self {
            sessions: SessionRegistry::new(),
            events,
            runtime: Arc::new(DaemonRuntime::minimal()),
            lifetime,
        }
    }

    /// A daemon over an explicit event bus and assembled [`DaemonRuntime`]. This
    /// is the production path ([`crate::main`]) and the acceptance suite's entry
    /// point: the runtime carries the engine, providers, and cost ledger, while
    /// the shared bus is the same one the runtime records cost and privacy events
    /// onto, so those events reach attached clients.
    #[must_use]
    pub fn with_runtime(events: Arc<EventBus>, runtime: Arc<DaemonRuntime>) -> Self {
        let lifetime = Arc::new(LifetimeSupervisor::new(
            ShutdownPolicy::Never,
            PolicySource::Default,
            Arc::clone(&events),
        ));
        Self::with_lifetime(events, runtime, lifetime)
    }

    /// A daemon over an explicit lifetime supervisor — the production path
    /// (`crate::main`), which resolves the policy from flags, environment, and
    /// config before the daemon exists (REQ-565 BR-7).
    #[must_use]
    pub fn with_lifetime(
        events: Arc<EventBus>,
        runtime: Arc<DaemonRuntime>,
        lifetime: Arc<LifetimeSupervisor>,
    ) -> Self {
        Self {
            sessions: SessionRegistry::new(),
            events,
            runtime,
            lifetime,
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
        // REQ-565: the accept loop is no longer infinite. It races `accept`
        // against the lifetime supervisor's shutdown signal, so a daemon whose
        // last client left stops listening and returns to `main` for the ordered
        // teardown (BR-8) instead of blocking here forever.
        let accepted = tokio::select! {
            // Biased so a pending shutdown wins a tie: once committed, the
            // daemon must not pick up one more connection it would immediately
            // refuse.
            biased;
            () = daemon.lifetime.wait_for_shutdown() => return Ok(()),
            accepted = listener.accept() => accepted,
        };
        let (stream, _addr) = accepted?;
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

/// The per-client event/response ordering fence (see the module docs).
///
/// `delivered` counts events the bus has queued into this client's
/// subscription; `forwarded` reports how many of them [`forward_events`] has
/// moved to the outbound channel. [`Self::sync`] holds a response until the
/// forwarder catches up to everything delivered so far, which is what keeps a
/// response from overtaking an event that was published before it.
#[derive(Clone)]
struct EventFence {
    delivered: Arc<AtomicU64>,
    forwarded: watch::Receiver<u64>,
}

impl EventFence {
    /// Wait until every event already delivered to this client's subscription
    /// has been handed to the outbound writer.
    ///
    /// This cannot wait for an event the client will never get: the target is
    /// the *delivered* count (not the bus-wide publish count), and if the
    /// forwarder ends first — teardown, or a lag eviction — the watch closes
    /// and the wait ends. It can only stall while the outbound channel is
    /// full, i.e. while the response it is ordering could not be enqueued
    /// anyway.
    async fn sync(mut self) {
        let target = self.delivered.load(Ordering::SeqCst);
        while *self.forwarded.borrow_and_update() < target {
            if self.forwarded.changed().await.is_err() {
                break; // the forwarder is gone; nothing left to order against
            }
        }
    }
}

/// One connection's view of the daemon's sessions (REQ-568 BR-1/BR-2).
///
/// `attached` starts empty and grows two ways: `session/create` attaches the
/// creator to what it just made, and `session/attach` attaches on success.
/// `session/clear` does **not** remove — attachment is connection-lifetime,
/// where a cleared transcript is content-lifetime; a client that cleared its
/// session is still watching it.
///
/// `monitor` is fixed at handshake and never changes (ADR-C). Immutability is
/// what keeps the forwarder holding one shared set and a plain `bool` rather
/// than two shared mutables; a client that wants to stop monitoring reconnects.
///
/// Cloning shares the set, which is the point: the dispatch path mutates it
/// while the forwarder task reads it, and they must see one set, not two.
#[derive(Clone)]
struct ConnState {
    attached: Arc<RwLock<HashSet<SessionId>>>,
    monitor: bool,
}

impl ConnState {
    /// A connection attached to nothing, monitoring or not as declared.
    fn new(monitor: bool) -> Self {
        Self {
            attached: Arc::new(RwLock::new(HashSet::new())),
            monitor,
        }
    }

    /// Grant this connection sight of `session_id`'s events. Idempotent.
    fn attach(&self, session_id: SessionId) {
        self.attached
            .write()
            .expect("connection attachment lock poisoned")
            .insert(session_id);
    }

    /// Whether an envelope scoped to `session_id` may be delivered here.
    ///
    /// Deliberately synchronous: the read guard cannot outlive this call, so it
    /// is structurally impossible for the forwarder to hold the lock across the
    /// `out_tx.send(...).await` that follows.
    fn may_receive(&self, session_id: Option<&SessionId>) -> bool {
        let attached = self
            .attached
            .read()
            .expect("connection attachment lock poisoned");
        should_forward(session_id, &attached, self.monitor)
    }

    /// Whether this connection may *drive* `session_id` — the gate on
    /// `session/prompt` and `session/clear` (REQ-568 BR-4).
    ///
    /// Membership only, and deliberately not [`may_receive`](Self::may_receive):
    /// `monitor` grants receipt of every session's events and nothing else (the
    /// spec's Permissions table lists it against "receive", never against the
    /// mutating methods). Reading the write gate off the delivery policy would
    /// make one declaration mean two things and silently promote every observer
    /// into a driver of every session it can see.
    fn may_drive(&self, session_id: &SessionId) -> bool {
        self.attached
            .read()
            .expect("connection attachment lock poisoned")
            .contains(session_id)
    }
}

/// The refusal every unattached mutating call gets (REQ-568 BR-4).
///
/// One sentence, shared by `session/prompt` and `session/clear` so the two
/// refusals are indistinguishable to the caller. Content-free by design: it
/// carries no session id, no prompt text and no path (conventions), and it says
/// nothing about whether the named session exists — the whole point of
/// answering before the registry is consulted (ADR-B). It names the remedy,
/// because `session/attach` is the one thing that changes the answer.
const NOT_ATTACHED_MESSAGE: &str = "not attached to this session; attach to it first";

/// The delivery policy: may an envelope scoped to `env_session` reach a
/// connection attached to `attached` that declared `monitor`? (REQ-568 BR-1/BR-2)
///
/// Pure, so the policy is table-tested directly instead of being inferred from
/// the forwarder's behaviour — mechanism is gated, policy is checkable.
///
/// Each arm names the real condition: daemon-scoped, monitor, attached. None of
/// them is a proxy — an emptiness check standing in for "an old client that
/// never learned to attach" would hand exactly the connection that attached to
/// nothing everything there is to see (LESSON-443).
fn should_forward(
    env_session: Option<&SessionId>,
    attached: &HashSet<SessionId>,
    monitor: bool,
) -> bool {
    match env_session {
        // Daemon-scoped: model lifecycle, install progress, daemon lifetime.
        // It belongs to no session, so no session can gate it (BR-2).
        None => true,
        Some(session_id) => monitor || attached.contains(session_id),
    }
}

/// Drives one client connection from handshake to disconnect.
async fn handle_client(stream: UnixStream, daemon: Arc<Daemon>) {
    let (read_half, write_half) = stream.into_split();
    let (out_tx, out_rx) = mpsc::channel::<String>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(write_loop(write_half, out_rx));

    let mut reader = BufReader::new(read_half);
    let mut handshaked = false;
    // REQ-565 BR-1: the client's claim on the daemon's life, taken when the
    // handshake completes — never at `accept`. A bare `UnixStream::connect` that
    // never handshakes (the CLI's own autostart poll, the e2e harness's
    // readiness probe) must not pin the daemon, and must not arm a shutdown when
    // it drops.
    let mut client_guard: Option<crate::lifetime::ClientGuard> = None;
    let mut forwarder: Option<JoinHandle<()>> = None;
    let mut fence: Option<EventFence> = None;
    // This connection's attached sessions and monitor declaration (REQ-568).
    // Installed by the handshake, alongside the fence and the forwarder, and
    // `None` until then — an unhandshaked connection has no sessions to see.
    let mut conn: Option<ConnState> = None;
    // In-flight `session/prompt` executions. A prompt turn is run on its own task
    // so the reader loop stays free to process the `permission/respond` that
    // unblocks the harness permission gate mid-turn (otherwise the loop would
    // deadlock awaiting a reply it cannot read).
    let mut prompt_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut line = String::new();

    loop {
        line.clear();
        // BR-6/AC-5: the read is capped by construction. A fresh `take` every
        // iteration is load-bearing — `MAX_FRAME` is a per-*frame* budget, and a
        // `Take` hoisted out of the loop would spend it once across the whole
        // connection lifetime, refusing the second legal frame of a long-lived
        // client.
        let read = (&mut reader).take(MAX_FRAME).read_line(&mut line).await;
        let n = match read {
            Ok(0) => break, // EOF: the client disconnected.
            Ok(n) => n,
            Err(_) => break, // Read error: tear the connection down.
        };

        // This is not a length check standing in for the cap — by here the
        // buffer is already bounded, which is the property AC-5 asks for. It
        // only classifies *why* the read stopped: `read_line` returns at a
        // newline or when the budget runs out, so a full budget with no
        // terminator means the frame was still going.
        //
        // The neighbouring case is a legitimate final frame that ends at EOF
        // without a newline; that one stops short of the budget, is processed
        // normally below, and the next iteration's `Ok(0)` ends the loop. A
        // frame that fills the budget exactly *and* ends at EOF is
        // indistinguishable from a truncated oversized one without reading the
        // byte after it — the byte we are refusing to buy — so it classifies as
        // oversized.
        if n as u64 == MAX_FRAME && !line.ends_with('\n') {
            // ADR-D: refuse, then close — no resync to the next newline. The
            // send is best-effort (`try_send`) because the outbound channel may
            // be full and this connection is ending either way; the teardown
            // below stops the forwarder and drains the writer, so a queued
            // refusal still reaches the wire before the socket closes.
            let _ = out_tx.try_send(error_string(
                Id::Null,
                error_code::INVALID_PARAMS,
                "frame exceeds maximum length",
            ));
            break;
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
            if let Some((sub, guard, state)) = do_handshake(&daemon, id, params, &out_tx) {
                handshaked = true;
                client_guard = Some(guard);
                let (forwarded_tx, forwarded_rx) = watch::channel(0u64);
                fence = Some(EventFence {
                    delivered: sub.delivered_counter(),
                    forwarded: forwarded_rx,
                });
                // The forwarder's clone shares the attached set with the one
                // the dispatch path below mutates (REQ-568 BR-1).
                forwarder = Some(tokio::spawn(forward_events(
                    sub,
                    out_tx.clone(),
                    forwarded_tx,
                    state.clone(),
                )));
                conn = Some(state);
            }
            continue;
        }

        // Past the handshake gate `conn` is always set — it is installed with
        // `handshaked` above. Stated as a pattern rather than an unwrap.
        //
        // Bound *before* the prompt branch: the attachment gate (REQ-568 BR-4)
        // applies to `session/prompt` too, and that method never reaches
        // `dispatch`.
        let Some(conn) = conn.as_ref() else { continue };

        // `session/prompt` runs on its own task (see `prompt_tasks`); every other
        // method dispatches synchronously and replies immediately.
        if method == PromptTurnParams::METHOD {
            if let Some(handle) =
                spawn_prompt_turn(&daemon, conn, id, params, &out_tx, fence.clone())
            {
                // Prune completed turns before tracking a new one so the vector
                // does not grow unbounded across a long-lived connection's turns
                // (REQ-544 minor). Only still-running handles are kept, to be
                // aborted at teardown.
                prompt_tasks.retain(|h| !h.is_finished());
                prompt_tasks.push(handle);
            }
            continue;
        }

        if let Some(response) = dispatch(&daemon, conn, id, method, params) {
            // Any event the handler just published (e.g. `session/create`'s
            // phase transition) must be on the outbound channel before its
            // response — see the module docs on ordering.
            if let Some(fence) = fence.clone() {
                fence.sync().await;
            }
            let _ = out_tx.send(response).await;
        }
    }

    // Teardown. Stop forwarding events — nobody is listening — but do NOT
    // abandon in-flight prompt turns.
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }

    // REQ-565 BR-2/AC-3, and the order here is the whole mechanism:
    //
    //   1. drop the client guard   → the count falls to zero and a shutdown arms
    //   2. await the prompt tasks  → each holds an ActivityGuard(Turn), so the
    //                                armed shutdown *defers* rather than commits
    //   3. the last turn finishes  → its guard drops → the shutdown commits
    //
    // Awaiting before dropping the guard would never arm, and the deferral the
    // event vocabulary must show would never happen.
    //
    // Until this REQ these turns were `abort()`ed, which killed the turn at
    // whatever await point it had reached — so it never reached its
    // `record_call` and its cost row was simply lost. A client closing its
    // terminal mid-turn is exactly AC-3's scenario, and "the ledger row for that
    // turn is intact" was false. The turn's *output* still goes nowhere (the
    // writer half is gone by now); the durable row is the point.
    drop(client_guard);
    for task in prompt_tasks {
        if task.is_finished() {
            continue;
        }
        // Bounded: a wedged turn must not hold the daemon open forever, which
        // would reinstate the standing-resident-model harm by another route.
        //
        // The abort on timeout is load-bearing, not tidiness. Dropping a
        // `JoinHandle` *detaches* the task rather than cancelling it, so a bare
        // `timeout` would leave the turn running, its `ActivityGuard` held, and
        // the daemon deferring forever — a bound that bounds nothing.
        let abort = task.abort_handle();
        if tokio::time::timeout(TURN_DRAIN_TIMEOUT, task)
            .await
            .is_err()
        {
            abort.abort();
            eprintln!(
                "tetond: a prompt turn did not finish within {}s of its client \
                 disconnecting; abandoning it (its cost row may be missing)",
                TURN_DRAIN_TIMEOUT.as_secs()
            );
        }
    }
    drop(out_tx);
    let _ = writer.await;
}

/// Spawns a `session/prompt` turn on its own task. The turn streams events over
/// the shared bus while it runs and sends its terminal response (or error) over
/// `out_tx` when it finishes — after `fence` confirms every event the turn
/// published (its final streamed text included) has reached the outbound
/// channel, so the response cannot overtake them on the wire. Returns the task
/// handle so teardown can abandon it, or `None` when the request could not be
/// started (an error response is queued).
///
/// `conn` is the issuing connection: a prompt against a session it never
/// attached is refused here (REQ-568 BR-4). This function is the *only* gate on
/// that path — `session/prompt` bypasses [`dispatch`] entirely — which is why
/// the check lives beside the spawn rather than in the reader loop that calls
/// it (ADR-B, LESSON-484).
fn spawn_prompt_turn(
    daemon: &Arc<Daemon>,
    conn: &ConnState,
    id: Id,
    params: Value,
    out_tx: &mpsc::Sender<String>,
    fence: Option<EventFence>,
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

    // REQ-568 BR-4, and its position is the requirement: ahead of the registry
    // lookup below, ahead of the lifetime claim, ahead of the spawn. A refused
    // prompt starts no task and touches no runtime state — so a connection that
    // guessed a session id learns nothing from the answer, not even whether the
    // id was real. `UNKNOWN_SESSION` for an id this connection is not attached
    // to would be an existence oracle sitting behind a refusal (ADR-B).
    if !conn.may_drive(&params.session_id) {
        let _ = out_tx.try_send(error_string(
            id,
            error_code::NOT_ATTACHED,
            NOT_ATTACHED_MESSAGE,
        ));
        return None;
    }

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
    // The turn carries the registry, not just the summary read out of it: the
    // `title` duty (REQ-561 TASK-062) has to *write back* the name it derives
    // and take the once-per-session claim that keeps it from re-deriving one.
    // The summary above is a snapshot, so it cannot serve either purpose.
    let daemon = Arc::clone(daemon);
    let out = out_tx.clone();

    // REQ-565 BR-2: the turn pins the daemon for its whole execution. Taken here
    // rather than inside the task so the claim exists before `spawn` returns —
    // a client that disconnects in the gap between the two would otherwise see
    // an idle daemon and commit to exiting while this turn was still starting.
    //
    // Moved into the task, so it is released by `Drop` on every exit path the
    // turn can take: normal completion, an error, a panic, or the teardown
    // abort. A claim released only on the happy path would wedge the daemon
    // alive on the unhappy ones.
    let turn_guard = daemon.lifetime.activity(BlockingActivity::Turn);

    Some(tokio::spawn(async move {
        let _turn_guard = turn_guard;
        let result = runtime
            .run_prompt_turn(
                &events,
                &daemon.sessions,
                summary.session_id.clone(),
                summary.mode,
                summary.phase,
                summary.cwd.clone(),
                prompt,
            )
            .await;
        let response = match result {
            Ok(res) => ok_string(id, &res),
            Err(err) => error_from(id, err),
        };
        // The turn has finished publishing; hold its response until the
        // forwarder has moved everything it streamed onto the outbound
        // channel, or a FIFO client renders the turn's tail one command late.
        if let Some(fence) = fence {
            fence.sync().await;
        }
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

/// Performs the handshake, and on success subscribes this client to the bus and
/// counts it against the daemon's lifetime.
///
/// Returns the new [`Subscription`], the client's lifetime claim, and the
/// connection's fresh [`ConnState`] on success (so the caller can start the
/// event forwarder, hold the claim for the connection's life, and carry the
/// attachment set), or `None` on failure — an error response has been queued,
/// and no claim was taken.
///
/// The `ConnState` is minted here rather than by the caller because this is the
/// only place the `monitor` declaration exists: it arrives in the handshake
/// frame and is fixed for the connection (ADR-C).
fn do_handshake(
    daemon: &Daemon,
    id: Id,
    params: Value,
    out_tx: &mpsc::Sender<String>,
) -> Option<(Subscription, crate::lifetime::ClientGuard, ConnState)> {
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

    // REQ-565 BR-3, second arm. Admission is the last gate and it is atomic with
    // the daemon's decision to exit: either this client is counted in (which
    // *cancels* a pending shutdown — the first arm) or the daemon has already
    // committed and refuses. There is deliberately no third outcome, and no
    // window in which a committed daemon accepts a session it will not serve.
    //
    // Placed after negotiation so a version-incompatible client still gets its
    // version diagnosis rather than a shutdown notice that would send it off to
    // restart a daemon whose version was never the problem.
    let Some(client_guard) = daemon.lifetime.admit() else {
        let _ = out_tx.try_send(error_string(
            id,
            error_code::DAEMON_SHUTTING_DOWN,
            "the daemon is shutting down; start a new one and retry",
        ));
        return None;
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
        daemon_name: "teton-code".to_owned(),
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities: Vec::new(),
    };
    let _ = out_tx.try_send(ok_string(id, &result));

    // Replay the local-model lifecycle (REQ-544 BR-9 / AC-8) to the just-subscribed
    // client, so it learns the state of the local tier on this machine: probed,
    // then awaiting a decision / disabled / ready. Published after the subscribe
    // above, so this client receives it; a machine with no local tier has an
    // empty sequence and emits nothing.
    //
    // Because it is replayed on *every* attach, every stage in it must be true of
    // the machine right now — see `runtime::startup_lifecycle`. A replayed
    // `download` or `ready` that described nothing would be repeated to every
    // client that ever connects, which is how a decorative sequence becomes a
    // daemon-wide lie.
    for lifecycle in daemon.runtime.lifecycle_events() {
        daemon
            .events
            .publish(None, Event::ModelLifecycle(lifecycle));
    }

    // REQ-568 BR-5: a monitor is announced, never inferred from traffic.
    // Logged at the one moment the declaration is made, and only for a
    // handshake that succeeded — a refused client never became a monitor.
    if params.monitor {
        eprintln!("{}", monitor_declaration_line(&params));
    }

    Some((subscription, client_guard, ConnState::new(params.monitor)))
}

/// The daemon-log sentence announcing a monitor declaration (REQ-568 BR-5).
///
/// A function rather than a bare `eprintln!` so the observability rule is
/// assertable: "the daemon says so" is a claim a test can check, where a format
/// string buried in the handshake can only be read.
///
/// Both client-supplied strings go through `{:?}`. They arrive from a peer that
/// is merely same-UID, and `Debug` escapes the newline a client would otherwise
/// embed in its name to forge a second log line under the daemon's own prefix.
fn monitor_declaration_line(params: &HandshakeParams) -> String {
    format!(
        "tetond: {:?} client {:?} declared monitor at handshake: \
         it receives every session's events",
        params.client_kind, params.client_name
    )
}

/// Dispatches a post-handshake request to its typed handler, returning the
/// serialized response.
///
/// `conn` is the issuing connection's state: the session handlers grow its
/// attachment set (REQ-568 BR-1).
fn dispatch(
    daemon: &Daemon,
    conn: &ConnState,
    id: Id,
    method: &str,
    params: Value,
) -> Option<String> {
    match method {
        SessionCreateParams::METHOD => Some(handle_session_create(daemon, conn, id, params)),
        SessionListParams::METHOD => {
            let result = SessionListResult {
                sessions: daemon.sessions.list(),
            };
            Some(ok_string(id, &result))
        }
        SessionAttachParams::METHOD => Some(handle_session_attach(daemon, conn, id, params)),
        SessionClearParams::METHOD => Some(handle_session_clear(daemon, conn, id, params)),
        PermissionRespondParams::METHOD => Some(handle_permission_respond(daemon, id, params)),
        ModelConfirmParams::METHOD => Some(handle_model_confirm(daemon, id, params)),
        ModelListParams::METHOD => Some(ok_string(id, &daemon.runtime.model_list())),
        ModelSetParams::METHOD => Some(handle_model_set(daemon, id, params)),
        ModelStatusParams::METHOD => Some(ok_string(id, &daemon.runtime.model_status())),
        ConfigGetParams::METHOD => Some(handle_config_get(daemon, id)),
        ConfigSetParams::METHOD => Some(handle_config_set(daemon, id, params)),
        CostQueryParams::METHOD => Some(handle_cost_query(daemon, id)),
        WebOverrideParams::METHOD => Some(handle_web_override(daemon, id, params)),
        WebRefreshParams::METHOD => Some(handle_web_refresh(daemon, id, params)),
        _ => Some(error_string(
            id,
            error_code::METHOD_NOT_FOUND,
            "method not found",
        )),
    }
}

/// Lift a session's web taint restriction (`web/override`, REQ-563 AC-12).
///
/// **This function is the entire path to that flag.** The setter behind
/// [`DaemonRuntime::web_override`] is private to the runtime module, and tool
/// dispatch holds a `ToolContext` rather than a `DaemonRuntime` — so a model
/// that emits a tool call named `web/override` reaches the tool registry, finds
/// no such tool, and is told so. The requirement's "the override is rejected
/// when issued by the model" is a fact about which channel this code hangs off,
/// not a check that could be omitted.
fn handle_web_override(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: WebOverrideParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    ok_string(id, &daemon.runtime.web_override(&params, &daemon.events))
}

/// Evict a cached document so the next lookup re-fetches (`web/refresh`,
/// REQ-563 AC-10).
///
/// The same channel argument as [`handle_web_override`] applies: this is a
/// client RPC, and tool dispatch cannot reach one. It differs in being
/// fallible — a cached file that will not unlink is the one outcome that would
/// otherwise leave the user's next lookup silently reading the copy they asked
/// to drop, so it comes back as an error rather than as `absent`.
fn handle_web_refresh(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: WebRefreshParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    match daemon.runtime.web_refresh(&params) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
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
            // The selection is already recorded by `set_model` above; the install
            // runs as a spawned task, gated by two guards:
            //
            // * `try_current().is_ok()` — `tokio::spawn` panics with no runtime.
            //   Production dispatch always runs inside the daemon's runtime, so
            //   this is only ever false in the synchronous dispatch unit tests
            //   that call `handle_model_set` directly with no runtime; the guard
            //   exists solely so those tests do not panic. Because the selection
            //   is already persisted, skipping the spawn loses nothing a test
            //   relies on.
            // * M-2 in-flight guard — only spawn when an install for this entry is
            //   not already running, so repeated `model/set` calls cannot pile up
            //   unbounded install tasks.
            if tokio::runtime::Handle::try_current().is_ok()
                && !daemon.runtime.consent().install_in_flight(&params.name)
            {
                let runtime = Arc::clone(&daemon.runtime);
                // REQ-565 BR-2: the install pins the daemon for its whole
                // duration — download, verify, load, benchmark (ADR-006 holds
                // one claim across all four). This is the 17 GB case the rule
                // names explicitly: a user who kicks off an install and then
                // closes the terminal must not have it killed mid-flight.
                let install_guard = daemon.lifetime.activity(BlockingActivity::ModelDownload);
                tokio::spawn(async move {
                    let _install_guard = install_guard;
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

/// Create a session (`session/create`), attaching the creating connection to it
/// (REQ-568 BR-1).
fn handle_session_create(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SessionCreateParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    // BUG-147: the cwd becomes this session's tool jail. Refuse a relative or
    // nonexistent one up front — jailing tools to a directory that isn't there
    // reproduces the every-tool-fails session this validates against.
    if let Some(cwd) = &params.cwd {
        if !cwd.is_absolute() {
            return error_string(
                id,
                error_code::INVALID_PARAMS,
                "cwd must be an absolute path",
            );
        }
        if !cwd.is_dir() {
            return error_string(
                id,
                error_code::INVALID_PARAMS,
                "cwd does not exist or is not a directory",
            );
        }
    }

    match daemon
        .sessions
        .create(params.mode, params.phase, params.cwd)
    {
        Ok(summary) => {
            // REQ-568 BR-1: the creator sees what it just made. Before the
            // publish below, not after — the envelope is queued into this
            // connection's subscription the moment it is published, and the
            // forwarder consults the attachment set when it drains, so an
            // attach that landed second would race the session's own first
            // event out of its creator's stream.
            conn.attach(summary.session_id.clone());

            // Broadcast a session-scoped event so attached peers learn of the
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

/// Attach a connection to an existing session (`session/attach`), which is what
/// grants it that session's events from here on (REQ-568 BR-1).
fn handle_session_attach(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SessionAttachParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    match daemon.sessions.get(&params.session_id) {
        Some(session) => {
            // Only a successful attach grants sight: a name the registry does
            // not know falls through to the error below with the set untouched,
            // so a client cannot pre-attach to a session id it guessed and
            // collect its events when someone later creates it.
            conn.attach(session.session_id.clone());
            ok_string(id, &SessionAttachResult { session })
        }
        None => error_string(id, error_code::UNKNOWN_SESSION, "unknown session"),
    }
}

/// Empty a session's retained conversation (`session/clear`, REQ-567 BR-8).
///
/// Dispatched from the synchronous path, so the reader loop's fence puts the
/// `context_cleared` event on the wire ahead of this response (see the module
/// docs on ordering) — a client therefore learns *that* the conversation was
/// cleared before it is told how much went, never the reverse.
///
/// The unknown-session and busy-session answers both come from the runtime's
/// single claim, so this handler decides nothing about *those*:
/// [`handle_session_attach`]'s `UNKNOWN_SESSION` and a concurrent prompt's
/// `SESSION_BUSY` reach a client through one classifier rather than two
/// agreeing ones.
///
/// It decides exactly one thing: whether this connection may drive the session
/// at all (REQ-568 BR-4). That answer cannot come off the runtime, because it
/// is a fact about the connection rather than about the session — and it is
/// given *before* the runtime is consulted, so an unattached caller cannot read
/// a session's existence out of which refusal it got (ADR-B).
fn handle_session_clear(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SessionClearParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    match daemon
        .runtime
        .clear_session(&params, &daemon.sessions, &daemon.events)
    {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Forwards broadcast events from `sub` to the client's outbound channel until
/// the subscription ends, reporting its progress on `forwarded` (the
/// [`EventFence`] watermark: how many delivered events have reached the
/// outbound channel). Dropping the sender on exit is load-bearing — it is what
/// releases any response still waiting on the fence. If the bus evicted the
/// subscription for lagging, emits a final [`SUBSCRIPTION_LAGGED_METHOD`]
/// notice before stopping.
///
/// This is the forwarding seam REQ-568 BR-3 names: every client, present and
/// future, crosses it, so the session filter is applied here against `conn`
/// rather than left to any client's rendering.
async fn forward_events(
    mut sub: Subscription,
    out_tx: mpsc::Sender<String>,
    forwarded: watch::Sender<u64>,
    conn: ConnState,
) {
    let mut count: u64 = 0;
    loop {
        match sub.recv().await {
            Some(envelope) => {
                // BR-1/BR-2: a session-scoped envelope goes out only to a
                // connection attached to that session, or to a monitor.
                if conn.may_receive(envelope.session_id.as_ref()) {
                    let note = Notification::new(EVENT_METHOD, envelope);
                    if let Ok(text) = serde_json::to_string(&note) {
                        if out_tx.send(text).await.is_err() {
                            break; // client's writer is gone
                        }
                    }
                }
                // Counted even when the envelope was filtered out or failed to
                // serialize: either way it will never be sent, so nothing
                // should wait on it. A skipped envelope that did not advance
                // the watermark would hang the next fenced response on an event
                // this connection was never going to receive (BR-7).
                count += 1;
                let _ = forwarded.send(count);
            }
            None => {
                if sub.is_lagged() {
                    let err = RpcError::new(
                        error_code::SUBSCRIPTION_LAGGED,
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

    /// The counter, not the timestamp, guarantees uniqueness: `SystemTime::now()`
    /// can return the same value for two calls within one clock tick.
    fn temp_socket(tag: &str) -> std::path::PathBuf {
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

    /// A connection that has attached to nothing and declared nothing — the
    /// state every direct-dispatch test starts from.
    fn unattached() -> ConnState {
        ConnState::new(false)
    }

    #[test]
    fn dispatch_rejects_unknown_methods() {
        let daemon = Daemon::new();
        let response = dispatch(
            &daemon,
            &unattached(),
            Id::Number(1),
            "does/not-exist",
            Value::Null,
        )
        .unwrap();
        assert!(response.contains("-32601")); // METHOD_NOT_FOUND
    }

    #[test]
    fn dispatch_lists_created_sessions() {
        let daemon = Daemon::new();
        let created = handle_session_create(
            &daemon,
            &unattached(),
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("session_id"));

        let listed = dispatch(
            &daemon,
            &unattached(),
            Id::Number(2),
            SessionListParams::METHOD,
            Value::Null,
        )
        .unwrap();
        assert!(listed.contains("sess-0"));
    }

    /// REQ-568 BR-1: the two ways a connection comes to see a session, and the
    /// one way it does not.
    ///
    /// Creating attaches the creator — checked *through* the handler rather
    /// than by calling `attach` directly, because "the creator is attached" is
    /// a property of `session/create`, not of the set. Attaching a session the
    /// registry knows grants sight; attaching a name it does not know grants
    /// nothing, so a client cannot stake a claim on a guessed id and collect
    /// the events of whoever creates it later.
    #[test]
    fn create_attaches_the_creator_and_only_a_real_attach_grants_sight() {
        let daemon = Daemon::new();
        let creator = unattached();
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("sess-0"), "{created}");

        let session = SessionId::from("sess-0");
        assert!(
            creator.may_receive(Some(&session)),
            "the creator must see the session it just made"
        );

        let onlooker = unattached();
        assert!(
            !onlooker.may_receive(Some(&session)),
            "a connection that did nothing must not see another's session"
        );

        let ghost = handle_session_attach(
            &daemon,
            &onlooker,
            Id::Number(2),
            serde_json::json!({"session_id": "sess-ghost"}),
        );
        assert!(
            ghost.contains(&error_code::UNKNOWN_SESSION.to_string()),
            "{ghost}"
        );
        assert!(
            !onlooker.may_receive(Some(&SessionId::from("sess-ghost"))),
            "a refused attach must not leave the name in the set"
        );

        let attached = handle_session_attach(
            &daemon,
            &onlooker,
            Id::Number(3),
            serde_json::json!({"session_id": "sess-0"}),
        );
        assert!(attached.contains("sess-0"), "{attached}");
        assert!(
            onlooker.may_receive(Some(&session)),
            "attaching is the grant — after it the session's events are visible"
        );
    }

    /// REQ-568 BR-1/BR-2, all six cells of the delivery policy: the envelope's
    /// scope (daemon or session) crossed with the connection's standing
    /// (attached, monitor, neither).
    #[test]
    fn should_forward_gates_session_scope_and_never_daemon_scope() {
        let mine = SessionId::from("sess-mine");
        let theirs = SessionId::from("sess-theirs");
        let attached: HashSet<SessionId> = [mine.clone()].into_iter().collect();
        let nothing: HashSet<SessionId> = HashSet::new();

        // (case, envelope scope, attached set, monitor, delivered?)
        for (case, scope, set, monitor, expected) in [
            ("daemon-scoped, attached", None, &attached, false, true),
            ("daemon-scoped, monitor", None, &nothing, true, true),
            ("daemon-scoped, neither", None, &nothing, false, true),
            (
                "scoped, attached to it",
                Some(&mine),
                &attached,
                false,
                true,
            ),
            ("scoped, monitor", Some(&theirs), &nothing, true, true),
            // The "neither" cell twice over: attached to *a* session but not
            // this one, and attached to none at all. A filter that read
            // emptiness as "a client too old to have attached" would pass the
            // second — so the empty set is asserted to be refused rather than
            // assumed to be.
            (
                "scoped, attached elsewhere",
                Some(&theirs),
                &attached,
                false,
                false,
            ),
            (
                "scoped, attached to nothing",
                Some(&theirs),
                &nothing,
                false,
                false,
            ),
        ] {
            assert_eq!(
                should_forward(scope, set, monitor),
                expected,
                "{case}: expected delivered={expected}"
            );
        }
    }

    /// REQ-568 BR-5: a monitor is announced in the daemon log, so its existence
    /// is observable rather than inferred from who is receiving what.
    ///
    /// The second half is the reason the line is built by a function: the
    /// client names itself, over a socket whose only gate is the uid, and a
    /// name carrying a newline would otherwise let it write a second log line
    /// of its own under the daemon's prefix.
    #[test]
    fn a_monitor_declaration_is_announced_and_cannot_forge_a_log_line() {
        use teton_protocol::handshake::HandshakeParams;
        use teton_protocol::{ClientKind, PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN};

        let params = HandshakeParams {
            client_kind: ClientKind::Cli,
            client_name: "teton-cli".to_owned(),
            client_version: "0.1.0".to_owned(),
            protocol_min: PROTOCOL_VERSION_MIN,
            protocol_max: PROTOCOL_VERSION_MAX,
            monitor: true,
        };
        let line = monitor_declaration_line(&params);
        assert!(line.contains("monitor"), "{line}");
        assert!(line.contains("teton-cli"), "{line}");
        assert!(line.contains("Cli"), "{line}");

        let forged = monitor_declaration_line(&HandshakeParams {
            client_name: "innocent\ntetond: listening on /tmp/other.sock".to_owned(),
            ..params
        });
        assert!(
            !forged.contains('\n'),
            "a client-supplied name must not break the line: {forged}"
        );
    }

    /// REQ-567 BR-8 / D-2 and REQ-568 BR-4: `session/clear` is a dispatchable
    /// method, and it has three answers, one per state the caller can be in.
    ///
    /// The connection here reaches the attached state the way a real one does —
    /// through `session/create`, which attaches the creator — rather than by
    /// poking the set, so what is asserted is the gate a client actually meets.
    ///
    /// The third state, *attached to an id the registry does not know*, is
    /// unreachable through the handlers: `session/attach` only grants ids the
    /// registry has, `session/create` grants what it just made, the registry
    /// has no removal, and an attachment set cannot outlive the daemon holding
    /// that registry. It is constructed directly below because the property it
    /// pins is real anyway — the gate is a check placed *in front of* the
    /// runtime's classifier, not a replacement for it, so an attached id still
    /// gets the runtime's `UNKNOWN_SESSION`. A gate that had swallowed that
    /// answer would pass every other assertion here.
    #[test]
    fn dispatch_routes_session_clear_and_tells_attached_from_unattached() {
        let daemon = Daemon::new();
        let conn = unattached();
        let created = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("sess-0"), "{created}");

        // Attached and live: clears, idempotently, and says how much went.
        let cleared = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": "sess-0"}),
        )
        .unwrap();
        assert!(
            !cleared.contains("-32601"),
            "the method must be routed, not rejected as unknown: {cleared}"
        );
        assert!(
            cleared.contains("\"blocks_dropped\":0"),
            "a session that has said nothing clears to zero, and says so: {cleared}"
        );

        // Not attached: refused before the runtime, and refused *identically*
        // for a session that exists and one that does not — the pair is the
        // assertion, since two different codes here would be the existence
        // oracle ADR-B refuses to build.
        let stranger = unattached();
        for target in ["sess-0", "sess-ghost"] {
            let refused = dispatch(
                &daemon,
                &stranger,
                Id::Number(3),
                SessionClearParams::METHOD,
                serde_json::json!({"session_id": target}),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::NOT_ATTACHED.to_string()),
                "clearing `{target}` unattached must be refused: {refused}"
            );
            assert!(
                !refused.contains("blocks_dropped"),
                "a refused clear must not report a count: {refused}"
            );
        }

        // Attached to a name the registry never had: the runtime still
        // classifies it, and the gate did not take that answer away.
        conn.attach(SessionId::from("sess-ghost"));
        let ghost = dispatch(
            &daemon,
            &conn,
            Id::Number(4),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": "sess-ghost"}),
        )
        .unwrap();
        assert!(
            ghost.contains(&error_code::UNKNOWN_SESSION.to_string()),
            "an unknown session must not clear cheerfully: {ghost}"
        );
    }

    /// REQ-568 BR-4: an unattached `session/prompt` is refused before any turn
    /// work starts.
    ///
    /// `spawn_prompt_turn` returning `None` *is* the "no task spawned" claim —
    /// the handle it would otherwise return is the task — and it is checked
    /// against a session that genuinely exists, so the refusal cannot be the
    /// pre-existing `UNKNOWN_SESSION` arm wearing a new code. The attached
    /// counterpart is asserted by transition rather than by outcome: this
    /// daemon has no provider to route to, so the turn it spawns will fail, but
    /// spawning at all means the gate opened.
    #[tokio::test]
    async fn an_unattached_prompt_is_refused_without_spawning_a_turn() {
        let daemon = Arc::new(Daemon::new());
        let creator = unattached();
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("sess-0"), "{created}");

        let prompt = serde_json::json!({
            "session_id": "sess-0",
            "prompt": [{"type": "text", "text": "what is in this session?"}],
        });
        let (tx, mut rx) = mpsc::channel::<String>(4);

        let stranger = unattached();
        let handle =
            spawn_prompt_turn(&daemon, &stranger, Id::Number(2), prompt.clone(), &tx, None);
        assert!(
            handle.is_none(),
            "a refused prompt must not spawn a turn task"
        );
        let refused = rx.try_recv().expect("a refusal is queued for the client");
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "{refused}"
        );
        assert!(
            !refused.contains(&error_code::UNKNOWN_SESSION.to_string()),
            "the session exists — this must not be answered as unknown: {refused}"
        );

        // The creator is attached, so the same call is accepted and run.
        let accepted = spawn_prompt_turn(&daemon, &creator, Id::Number(3), prompt, &tx, None);
        let accepted = accepted.expect("an attached prompt must start its turn");
        accepted.await.unwrap();
        let response = rx.try_recv().expect("the turn answered");
        assert!(
            !response.contains(&error_code::NOT_ATTACHED.to_string()),
            "an attached prompt must never be refused as unattached: {response}"
        );
    }

    /// REQ-568 BR-4 boundary: `monitor` is a receive-side declaration, so it
    /// grants sight of every session and the right to drive none of them.
    ///
    /// The two halves are asserted together on one connection, because the bug
    /// this guards is precisely reading the write gate off the read policy: a
    /// `may_drive` implemented as `may_receive` passes every other test in this
    /// file while handing a passive observer the ability to clear and prompt
    /// every session on the machine.
    #[test]
    fn a_monitor_may_watch_every_session_and_drive_none() {
        let daemon = Daemon::new();
        let owner = unattached();
        let created = handle_session_create(
            &daemon,
            &owner,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("sess-0"), "{created}");

        let monitor = ConnState::new(true);
        assert!(
            monitor.may_receive(Some(&SessionId::from("sess-0"))),
            "a monitor sees every session's events"
        );

        let refused = dispatch(
            &daemon,
            &monitor,
            Id::Number(2),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": "sess-0"}),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "watching a session is not driving it: {refused}"
        );
    }

    /// REQ-563 AC-10: `web/refresh` is a dispatchable method, and it answers
    /// with an outcome rather than the URL it was handed (BR-7 — the daemon
    /// never echoes an outgoing destination back).
    #[test]
    fn dispatch_answers_web_refresh_with_an_outcome_and_no_url() {
        let daemon = Daemon::new();
        let response = dispatch(
            &daemon,
            &unattached(),
            Id::Number(1),
            WebRefreshParams::METHOD,
            serde_json::json!({"url": "https://docs.rs/serde"}),
        )
        .unwrap();

        assert!(
            !response.contains("-32601"),
            "the method must be routed, not rejected as unknown: {response}"
        );
        // Nothing was cached in this daemon's (temp) data dir, so `absent` is
        // the honest answer — and it is an answer, not an error.
        assert!(response.contains("absent"), "{response}");
        assert!(
            !response.contains("docs.rs"),
            "the refreshed URL must not travel back: {response}"
        );
    }

    /// Params that are not a `web/refresh` request are an invalid-params error,
    /// not a panic and not a silent no-op.
    #[test]
    fn dispatch_rejects_a_malformed_web_refresh() {
        let daemon = Daemon::new();
        let response = dispatch(
            &daemon,
            &unattached(),
            Id::Number(1),
            WebRefreshParams::METHOD,
            serde_json::json!({"not_a_url": 3}),
        )
        .unwrap();
        assert!(response.contains("-32602"), "{response}"); // INVALID_PARAMS
    }

    /// The two web controls are **client** RPCs, and that is what makes them
    /// user-only (AC-12). This pins the half that is checkable here: they are in
    /// the dispatch table. The other half — that no tool of these names exists —
    /// is pinned beside the tool registry.
    #[test]
    fn both_web_controls_are_client_methods() {
        let daemon = Daemon::new();
        for method in [WebOverrideParams::METHOD, WebRefreshParams::METHOD] {
            let response =
                dispatch(&daemon, &unattached(), Id::Number(1), method, Value::Null).unwrap();
            assert!(
                !response.contains("-32601"),
                "`{method}` must be a routed client method: {response}"
            );
        }
    }
}
