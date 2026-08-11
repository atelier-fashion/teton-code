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
//! The same attachment set gates the *mutating* methods: `session/prompt`,
//! `session/clear`, `web/override` and `permission/respond` against a session
//! this connection never attached are refused with `NOT_ATTACHED` (REQ-568 BR-4,
//! REQ-569 BR-9). The write gates sit at the seams every client crosses —
//! [`forward_events`] for reads, and [`handle_session_clear`],
//! [`spawn_prompt_turn`], [`handle_web_override`],
//! [`handle_permission_respond`] for writes — never in a client, and never in
//! the reader loop above them, so the direct-RPC tests exercise the real gate
//! (LESSON-484).
//!
//! `permission/respond` joined that list at REQ-569 (BR-9, ADR-F). It used to be
//! ungated, which meant a monitor — which by design *sees* every session's
//! `permission_request` — could answer one and so authorize another session's
//! tool call. It is gated on [`ConnState::may_drive`] like its neighbours, and
//! the refusal deliberately leaves the prompt pending for its rightful
//! answerer. Delivery is unchanged: `monitor` still buys sight of every
//! session's prompts, and now buys the right to answer none of them.
//!
//! ## Who may attach at all (REQ-569)
//!
//! REQ-568 left `session/attach` itself open to any handshaked same-UID
//! connection, so attachment was a grant anyone could help themselves to.
//! REQ-569 puts two gates in front of it, applied in this order and both
//! *before* the session registry is consulted:
//!
//! 1. **Ancestry (BR-4, ADR-A).** A connection whose process descends from this
//!    daemon's own process tree — a tool child, an MCP server subprocess, or any
//!    future daemon-spawned process that links the client crate — may never
//!    attach and never declare `monitor`. `ATTACH_FORBIDDEN`, no consent path,
//!    ever. It keys on kernel-attested process ancestry ([`crate::peer`]) rather
//!    than on what such a child happens to do today, which is what makes it
//!    survive the arrival of a child that does something new (LESSON-443).
//! 2. **Grant (BR-1/BR-2).** Everyone else may attach only to a session they
//!    created or hold an attach-scope grant for ([`crate::grants`]);
//!    `monitor` needs its own monitor-scope grant.
//! 3. **Consent (BR-6, [`crate::consent`]).** A connection with no standing is
//!    not simply refused: the question is put to a user, and their answer is
//!    the only thing in this daemon that mints a grant. Approved →
//!    exactly one grant, at exactly the scope asked for. Declined, unanswered,
//!    or asked with nobody to ask → `CONSENT_DENIED` / `CONSENT_TIMEOUT` /
//!    `NOT_GRANTED`, and nothing minted (BR-7).
//!
//! Gate (1) and every outcome of (3) precede `daemon.sessions.get`, and all of
//! them answer identically for a session that exists and one that does not, so
//! none becomes an existence oracle for a connection that guessed an id (BR-8:
//! ids are names, grants are credentials). The prompt is raised for a
//! nonexistent id too, for that reason.
//!
//! `session/attach` therefore runs on its own task, like `session/prompt`: it
//! awaits a decision that may arrive as an `attach/consent` on this very
//! connection, and a reader loop that awaited it inline could not read the
//! answer that would end the wait.

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
    AttachConsentRequested, AttachRefused, AttachRefusedReason, ConsentScope, DaemonClientAttach,
    Event, EventEnvelope, PhaseTransition, SessionGrantMinted, EVENT_METHOD,
    SUBSCRIPTION_LAGGED_METHOD,
};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Notification, Response, RpcError};
use teton_protocol::methods::{
    AttachConsentOutcome, AttachConsentParams, AttachConsentResult, ConfigGetParams,
    ConfigGetResult, ConfigSetParams, ConfigSetResult, CostQueryParams, ModelConfirmParams,
    ModelListParams, ModelSetParams, ModelStatusParams, PermissionRespondParams,
    PermissionRespondResult, PromptBlock, PromptTurnParams, RpcMethod, SessionAttachParams,
    SessionAttachResult, SessionClearParams, SessionCreateParams, SessionCreateResult,
    SessionListParams, SessionListResult, SessionSummary, WebOverrideParams, WebRefreshParams,
};
use teton_protocol::{RequestId, SessionId};

use teton_core::lifetime::{BlockingActivity, PolicySource, ShutdownPolicy};

use crate::auth::{self, PeerIdentity};
use crate::broadcast::{EventBus, Subscription, DEFAULT_CAPACITY};
use crate::consent::{
    ConsentOutcome, ConsentRoute, ConsentSurfaces, PendingConsents, CONSENT_TIMEOUT,
};
use crate::grants::{ConnectionId, Grant, GrantRegistry};
use crate::lifetime::LifetimeSupervisor;
use crate::peer::{is_descendant_of, Ancestry, KernelParentOf, MAX_ANCESTRY_DEPTH};
use crate::runtime::DaemonRuntime;
use crate::sessions::{self, SessionCreateError, SessionRegistry};

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

/// Capacity above which the reader releases its line buffer between frames
/// rather than retaining it. `MAX_FRAME` is a per-*frame* budget, not a standing
/// reservation: one near-4 MiB legal frame would otherwise pin that much for the
/// connection's whole life through the buffer's retained capacity (ADR-D).
const LINE_RETAIN_CAP: usize = 64 * 1024;

/// How long a disconnecting client's in-flight prompt turns are given to finish
/// before they are abandoned (REQ-565 BR-2).
///
/// Generous, because the thing being protected is the turn's cost row and the
/// work already paid for; a local turn on a large model can legitimately run for
/// minutes. It is an upper bound on pathology, not a normal-path timeout.
const TURN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Whose process tree the attach/monitor gate excludes (REQ-569 BR-4, ADR-A).
///
/// The gate's question is "did this connection come out of the daemon's own
/// process tree?", and answering it needs to know which process *is* the
/// daemon. That is a property of how this `Daemon` was assembled, not a
/// constant, because the daemon is not always its own process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProcess {
    /// The daemon runs as its own process, whose pid this is. Every tool child
    /// and MCP subprocess it spawns is a descendant of that pid, and those are
    /// exactly the connections BR-4 excludes. **This is the production answer**,
    /// set from `std::process::id()` by [`Daemon::with_lifetime`].
    Own(i32),
    /// The daemon is embedded in a host process it does not own — an in-process
    /// harness, where the daemon task, its clients, and the test all share one
    /// pid.
    ///
    /// This is not the gate switched off; it is the honest answer to the gate's
    /// question. An embedded daemon has spawned no children of its own, so no
    /// connection is a descendant of it, and [`Ancestry::NotDescendant`] is
    /// true rather than merely convenient. Stating it as
    /// [`Own`](Self::Own)`(that shared pid)` would be the dishonest option: the
    /// client *is* the host process, so every one of them would classify as the
    /// daemon itself and the harness would test a daemon nobody can talk to.
    ///
    /// The production binary cannot reach this arm — `main` builds its daemon
    /// through [`Daemon::with_lifetime`], which sets
    /// [`Own`](Self::Own) unconditionally, and a fixture has to say
    /// [`Daemon::with_daemon_process`] out loud to change it. Pinned by
    /// `the_production_constructors_own_their_process`.
    Embedded,
}

impl DaemonProcess {
    /// Classifies a peer against this daemon's process tree.
    ///
    /// The walk is done **once per connection**, at the handshake, and the
    /// verdict is then carried on the connection: one kernel read rather than
    /// one per call, and — the reason that matters — a value that cannot drift
    /// mid-connection as pids are reused underneath a long-lived client.
    fn ancestry_of(self, peer_pid: Option<i32>) -> Ancestry {
        match self {
            // No process tree of our own, so nothing can have come out of it.
            Self::Embedded => Ancestry::NotDescendant,
            // A platform that reports no peer pid (the BSD arm of
            // `auth::peer_identity`) leaves the question unanswerable rather
            // than answered favourably — `None` is "we cannot tell", and the
            // caller's policy turns that into a refusal.
            Self::Own(_) if peer_pid.is_none() => Ancestry::Indeterminate,
            Self::Own(root) => is_descendant_of(
                peer_pid.unwrap_or_default(),
                root,
                &KernelParentOf,
                MAX_ANCESTRY_DEPTH,
            ),
        }
    }
}

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
    /// Who may attach to which session (REQ-569 BR-1/BR-2, ADR-C). Shared like
    /// the registry and the bus, and in-memory only — nothing here is ever
    /// persisted.
    pub grants: GrantRegistry,
    /// The consent requests in flight, and the ids that resolve them
    /// (REQ-569 BR-6/BR-7, ADR-E). The only thing that mints an entry in
    /// [`Self::grants`].
    pub consents: PendingConsents,
    /// Every live connection a consent prompt can be rendered at (BR-6's
    /// routing question, which no single connection can answer for itself).
    pub surfaces: ConsentSurfaces,
    /// The process whose descendants may never attach or monitor (BR-4, ADR-A).
    pub process: DaemonProcess,
    /// How long a consent request waits before it defaults closed (BR-7).
    ///
    /// A field rather than the [`CONSENT_TIMEOUT`] constant read at the use
    /// site, for the reason [`Self::process`] is a field: a test that has to
    /// observe the *timeout* arm should not have to wait out a window sized for
    /// a human. Production never sets it — [`Daemon::with_lifetime`] and
    /// [`Daemon::new`] both take the constant — and a fixture has to say
    /// [`Daemon::with_consent_timeout`] out loud to change it.
    pub consent_timeout: std::time::Duration,
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
            grants: GrantRegistry::new(),
            consents: PendingConsents::new(),
            surfaces: ConsentSurfaces::new(),
            consent_timeout: CONSENT_TIMEOUT,
            // `Embedded`, for the same reason `ShutdownPolicy::Never` is above:
            // a bare `Daemon::new()` is a fixture, and a fixture is run *inside*
            // the process that also holds its clients. See
            // [`DaemonProcess::Embedded`] — this is the honest classification of
            // that topology, not a relaxation of the gate. The production path
            // states its own process explicitly.
            process: DaemonProcess::Embedded,
        }
    }

    /// Replaces the process this daemon excludes the descendants of (BR-4).
    ///
    /// For fixtures that need to state a topology the constructor cannot infer:
    /// an in-process test that wants the ancestry gate to *bite* declares
    /// `Own(std::process::id())`, which makes its own in-process clients genuine
    /// kernel-attested descendants and exercises the real walk rather than a
    /// stubbed verdict.
    #[must_use]
    pub fn with_daemon_process(mut self, process: DaemonProcess) -> Self {
        self.process = process;
        self
    }

    /// Replaces the window a consent request waits before defaulting closed
    /// (BR-7).
    ///
    /// For fixtures that assert on the *timeout* arm. [`CONSENT_TIMEOUT`] is
    /// sized for a human noticing a prompt; a test that waited it out would
    /// spend half a minute proving a branch it can prove in milliseconds, and a
    /// suite that slow is a suite people stop running.
    #[must_use]
    pub fn with_consent_timeout(mut self, window: std::time::Duration) -> Self {
        self.consent_timeout = window;
        self
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
            grants: GrantRegistry::new(),
            consents: PendingConsents::new(),
            surfaces: ConsentSurfaces::new(),
            // The production answer, and taken here rather than passed in so
            // `main` cannot ship a daemon that forgot to state it: this daemon
            // is its own process, and the children it spawns are what BR-4
            // excludes (ADR-A).
            process: DaemonProcess::Own(
                i32::try_from(std::process::id()).expect("a pid fits in i32"),
            ),
            consent_timeout: CONSENT_TIMEOUT,
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
            Ok(peer) => {
                // The identity travels with the connection rather than being
                // re-read later: the credentials are a property of the socket as
                // the kernel recorded them at `connect(2)`, so reading them once
                // here is reading them at the only moment they are attested
                // (REQ-569 ADR-A). What the *pid* costs is decided at the
                // handshake, in `do_handshake`.
                let daemon = Arc::clone(&daemon);
                tokio::spawn(handle_client(stream, daemon, peer));
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

/// One connection's view of the daemon's sessions (REQ-568 BR-1/BR-2,
/// REQ-569 BR-1/BR-4).
///
/// `attached` starts empty and grows two ways: `session/create` attaches the
/// creator to what it just made, and `session/attach` attaches on success.
/// `session/clear` does **not** remove — attachment is connection-lifetime,
/// where a cleared transcript is content-lifetime; a client that cleared its
/// session is still watching it.
///
/// `created` is the subset of those this connection actually *made*, and it is
/// tracked separately rather than read off `attached` because the two answer
/// different questions: `attached` is "may this connection see and drive the
/// session", `created` is "is this connection the session's origin", which is
/// the standing REQ-569 grants attach on. Folding them together would make a
/// grant-based attach retroactively confer creator standing — a proxy for the
/// real condition, which is the shape LESSON-443 warns about.
///
/// `monitor` and `ancestry` are both fixed at handshake and never change.
/// Immutability is what keeps the forwarder holding one shared set and plain
/// values rather than more shared mutables; for `ancestry` it is also the
/// security property — a verdict computed once from the pid the kernel attested
/// at `connect(2)` cannot drift mid-connection.
///
/// Cloning shares the sets, which is the point: the dispatch path mutates them
/// while the forwarder task reads them, and they must see one set, not two.
#[derive(Clone)]
struct ConnState {
    /// This connection's grant subject (REQ-569 ADR-D), minted at handshake.
    id: ConnectionId,
    /// Whether this connection's process came out of the daemon's own process
    /// tree (BR-4). Computed once, at the handshake.
    ancestry: Ancestry,
    /// What a consent prompt calls this connection: the kind and name it gave
    /// at the handshake, bounded and stripped of control characters
    /// ([`requester_descriptor`]). Fixed at the handshake like `monitor` and
    /// `ancestry`, because that is the only moment the strings exist — and
    /// because a descriptor that could change mid-connection would describe one
    /// requester in the prompt and another in the log.
    requester: String,
    attached: Arc<RwLock<HashSet<SessionId>>>,
    created: Arc<RwLock<HashSet<SessionId>>>,
    monitor: bool,
}

impl ConnState {
    /// A connection attached to nothing, monitoring or not as declared.
    fn new(id: ConnectionId, ancestry: Ancestry, monitor: bool, requester: String) -> Self {
        Self {
            id,
            ancestry,
            requester,
            attached: Arc::new(RwLock::new(HashSet::new())),
            created: Arc::new(RwLock::new(HashSet::new())),
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

    /// Record that this connection created `session_id`, and attach it.
    ///
    /// Both, in one call, because both are true of a creator and splitting them
    /// at the call site is how one of them gets forgotten.
    fn record_created(&self, session_id: SessionId) {
        self.created
            .write()
            .expect("connection creation lock poisoned")
            .insert(session_id.clone());
        self.attach(session_id);
    }

    /// A snapshot of the sessions this connection created.
    ///
    /// Cloned rather than lent out under the lock so no caller can hold the
    /// guard across an `await` or take the grant-registry lock while holding it.
    fn created(&self) -> HashSet<SessionId> {
        self.created
            .read()
            .expect("connection creation lock poisoned")
            .clone()
    }

    /// A snapshot of the sessions this connection is attached to.
    ///
    /// Cloned rather than lent out under the lock, for [`Self::created`]'s
    /// reason: the consent rule reads it and then takes other locks, and a
    /// guard held across that is a deadlock waiting for the right interleaving.
    fn attached(&self) -> HashSet<SessionId> {
        self.attached
            .read()
            .expect("connection attachment lock poisoned")
            .clone()
    }

    /// Whether this connection is even *eligible* to hold session access —
    /// the REQ-569 BR-4 ancestry gate, before any grant question is asked.
    ///
    /// [`Ancestry::Indeterminate`] is refused here alongside
    /// [`Ancestry::Descendant`], and that is the deliberate policy TASK-103 left
    /// to this caller: "I could not tell whether this process came out of my own
    /// tree" must cost the same as "it did". Treating it as
    /// [`Ancestry::NotDescendant`] would make every lookup failure — a vanished
    /// pid, a platform with no peer-pid option, a chain that cycled — into a way
    /// through the gate, and a guard whose failure mode is *open* is a guard an
    /// attacker only has to break rather than beat. The two are still told apart
    /// in the daemon log ([`ancestry_refusal_line`]), because an operator
    /// debugging a refusal needs to know which one happened even though the
    /// connection is told the same thing either way.
    fn may_hold_session_access(&self) -> bool {
        matches!(self.ancestry, Ancestry::NotDescendant)
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
    /// `session/prompt`, `session/clear`, `web/override` and
    /// `permission/respond` (REQ-568 BR-4, REQ-569 BR-9).
    ///
    /// Membership only, and deliberately not [`may_receive`](Self::may_receive):
    /// `monitor` grants receipt of every session's events, never the right to
    /// drive one *through this gate* (the spec's Permissions table lists it
    /// against "receive", never against the driving methods). Reading the write
    /// gate off the delivery policy would make one declaration mean two things
    /// and silently promote every observer into a driver of every session it can
    /// see. `permission/respond` is the sharpest case and the last to join:
    /// a monitor receives the `permission_request` it would be answering, so
    /// there the swap is not even a widening — it is handing the observer the
    /// tool-approval authority of every session on the machine.
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

/// The refusal a daemon descendant gets (REQ-569 BR-4, ADR-A).
///
/// Content-free like its neighbour, and identical whether the ancestry verdict
/// was [`Ancestry::Descendant`] or [`Ancestry::Indeterminate`]: the connection
/// is told the answer, never the daemon's confidence in it, or a probe could
/// map the daemon's process tree by watching which refusal it drew. It names no
/// remedy because there is none — this is the one refusal on this seam with no
/// consent path.
const ATTACH_FORBIDDEN_MESSAGE: &str =
    "this connection may not attach to or monitor sessions on this daemon";

/// The refusal a `monitor` declaration without a monitor-scope grant gets
/// (REQ-569 BR-2, verify F1).
///
/// Terminal, like [`ATTACH_FORBIDDEN_MESSAGE`] and unlike every other
/// `NOT_GRANTED` on this seam: there is no consent path to `monitor` at all, so
/// there is no remedy to name. See the F1 comment in `do_handshake` for why the
/// one that existed was removed rather than re-predicated.
const NOT_GRANTED_MESSAGE: &str =
    "no monitor-scope grant, and this daemon has no way to mint one over the socket";

/// The refusal a connection gets when it already has
/// [`MAX_PENDING_CONSENTS_PER_CONNECTION`] prompts outstanding (REQ-569 verify,
/// F4).
///
/// `NOT_GRANTED` rather than a new code: from the caller's side this *is* "no
/// grant, and none was sought", which is exactly what `NOT_GRANTED` already
/// says. It names the remedy, because unlike the monitor refusal above this one
/// stops applying as soon as the caller's own outstanding requests resolve.
const TOO_MANY_PENDING_MESSAGE: &str =
    "too many consent requests already outstanding on this connection; \
     wait for one to be answered";

/// The daemon-log sentence for an ancestry refusal (REQ-569 BR-4).
///
/// The one place [`Ancestry::Descendant`] and [`Ancestry::Indeterminate`] are
/// told apart. The client is refused identically either way, but the operator
/// reading this log is answering a different question — "is my tool child being
/// correctly excluded" versus "has the peer-pid lookup stopped working on this
/// machine" — and those have opposite remedies. Collapsing them would turn a
/// broken ancestry lookup into a silent, permanent refusal of every client,
/// indistinguishable from the gate working.
///
/// A function rather than a bare `eprintln!` so the distinction is assertable
/// (the [`monitor_declaration_line`] precedent), and it carries no session id,
/// no path, and no client-supplied string (conventions: privacy in logs).
fn ancestry_refusal_line(ancestry: Ancestry, what: &str) -> String {
    let because = match ancestry {
        Ancestry::Descendant => "it descends from this daemon's own process tree",
        Ancestry::Indeterminate => {
            "its process ancestry could not be determined, and this seam fails closed"
        }
        // Not reachable from the gate, which only logs a refusal — spelled out
        // rather than left to a catch-all so a future arm cannot land here
        // silently wearing one of the sentences above.
        Ancestry::NotDescendant => "it was not refused",
    };
    format!("tetond: refused {what} for a connection because {because}")
}

/// The refusal a connection gets when a user was asked and said no
/// (REQ-569 BR-5/BR-7).
const CONSENT_DENIED_MESSAGE: &str = "the request was declined";

/// The refusal a connection gets when the consent window closed unanswered
/// (REQ-569 BR-7, AC-6).
///
/// Names the remedy, because unlike a denial this one is worth retrying: the
/// prompt may have been rendered where nobody was looking.
const CONSENT_TIMEOUT_MESSAGE: &str =
    "the request was not answered in time and defaulted to declined; ask again";

/// The refusal an answer to somebody else's consent request gets
/// (REQ-569 BR-6).
///
/// Content-free, and identical whether the request exists and was routed
/// elsewhere or the connection simply is not a surface it was offered to: a
/// stranger must not be able to map who is attached to what by watching which
/// consent requests it is allowed to answer.
const CONSENT_NOT_OFFERED_MESSAGE: &str = "this consent request was not offered to this connection";

/// How much of a client's self-declared name a consent prompt carries.
///
/// Sixty-four characters is generous for "teton", "code-vscode" and the like,
/// and short enough that the whole descriptor stays renderable in one line of a
/// prompt a user has to read and decide on.
const REQUESTER_BUDGET: usize = 64;

/// What a consent prompt calls the connection that is asking (REQ-569 BR-6).
///
/// **Every character here is chosen by an unprivileged same-UID peer**, which
/// is what shapes the whole function. It is bounded (`client_name` is limited
/// only by `MAX_FRAME` on the wire, ~4 MiB, and this string is published to
/// other clients) and stripped of control characters, so a requester cannot
/// forge extra lines, move a cursor, or inject an ANSI sequence into whatever
/// surface renders the prompt — the same treatment REQ-568's monitor log line
/// gives the same field, applied here because the destination is a user's
/// screen rather than a log.
///
/// It carries the kind and the name and **nothing else**: no pid, no executable
/// path, no environment, no command line. Those would read as identity, and
/// this string is not identity — it is a hint, offered alongside a decision the
/// user is making. The identity claim on this seam is the ancestry gate, which
/// already ran.
fn requester_descriptor(params: &HandshakeParams) -> String {
    let name: String = params
        .client_name
        .chars()
        .filter(|c| !unsafe_in_a_prompt(*c))
        .take(REQUESTER_BUDGET)
        .collect();
    format!("{:?} client {name:?}", params.client_kind)
}

/// Whether `c` must never reach a security prompt a user reads (REQ-569 verify,
/// F8).
///
/// `char::is_control` is **Cc only** — 32 ASCII codes plus DEL — and every
/// character that actually reorders or hides text on a modern terminal is
/// somewhere else in Unicode. Filtering only Cc leaves the Trojan-Source
/// repertoire intact, and this string is rendered to a user who is deciding
/// whether to hand a peer their session: a name carrying `U+202E` can make the
/// prompt read as though a different client were asking, and the same string
/// then goes into the daemon log where it does it again.
///
/// So the rule is Cc **plus** the format and separator characters that carry
/// reordering, joining, or invisibility semantics. Named as ranges from the
/// Unicode blocks rather than as a general-category test, because that would
/// need a table crate for a set that is small, closed, and easier to audit
/// written out:
///
/// - `U+00AD`, `U+061C`, `U+180E` — isolated Cf oddments (soft hyphen, Arabic
///   letter mark, Mongolian vowel separator).
/// - `U+200B..=U+200F` — zero-width space/joiners and the LRM/RLM marks.
/// - `U+2028..=U+2029` — line and paragraph separator. **Not** `is_control`,
///   and they break a line in most renderers, which is the forged-second-line
///   problem the Cc filter was there to stop.
/// - `U+202A..=U+202E` — the bidi embedding and *override* controls.
/// - `U+2060..=U+2064`, `U+2066..=U+2069` — word joiner, invisible operators,
///   and the bidi isolates.
/// - `U+FEFF` — ZWNBSP/BOM.
/// - `U+FFF9..=U+FFFB` — interlinear annotation.
/// - `U+E0000..=U+E007F` — the deprecated tag block, which renders as nothing
///   at all.
fn unsafe_in_a_prompt(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{FEFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}')
}

/// The daemon-log sentence for a consent a connection granted **to itself**
/// (REQ-569 BR-6's second arm, TASK-109).
///
/// # Why this exists
///
/// BR-6's second arm is what makes the resume flow work: when no client is
/// attached to the target session there is nobody to ask, so the prompt is
/// rendered at the requesting connection and its user answers it. For an
/// *interactive* client that is exactly right — the person reopening the
/// session is the person being asked. For a headless same-UID process that is
/// not a daemon descendant, the same arm means it approves itself with no human
/// anywhere in the loop.
///
/// That is an accepted residual of ADR-A's perimeter (ancestry excludes the
/// daemon's own children; an arbitrary same-UID process is the ptrace-class
/// residual the spec records). Accepted — but it must not be **silent**. Every
/// other way a grant is minted has a second party who saw the request; this one
/// does not, and an operator reading the log has to be able to tell the two
/// apart. So the daemon says which happened, at the moment it happens.
///
/// A function rather than a bare `eprintln!` for [`monitor_declaration_line`]'s
/// reason: "the daemon says so" is then a claim a test can check.
///
/// `requester` is [`ConnState::requester`], which is already built from a
/// client-supplied name — so it is re-bounded and stripped of control
/// characters here too, exactly as REQ-568's monitor log line treats the same
/// field. Defence in depth rather than duplication: this function must not be
/// the place a future unbounded descriptor turns into a forged log line.
fn self_approval_line(requester: &str) -> String {
    // The descriptor is `{kind} client {name}` with the name already capped at
    // `REQUESTER_BUDGET`; twice that leaves the whole of a legitimate one
    // intact while still bounding anything that grows.
    const DESCRIPTOR_BUDGET: usize = REQUESTER_BUDGET * 2;
    let requester: String = requester
        .chars()
        .filter(|c| !unsafe_in_a_prompt(*c))
        .take(DESCRIPTOR_BUDGET)
        .collect();
    format!(
        "tetond: {requester} approved its own attach consent — no other client was attached \
         to that session, so the prompt was rendered at the connection that asked for it \
         (REQ-569 BR-6 second arm)"
    )
}

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

/// What a connection is shown of a session it may not see: ids and shape, never
/// content (REQ-569 BR-10, ADR-G).
///
/// `session_id`, `mode` and `phase` survive unconditionally — they are the
/// listing, and BR-10 is about the *payload*, not about which rows exist. A
/// caller that dropped rows instead would break `session/list`'s job (a client
/// still has to learn that a session it may ask to attach to is there) and would
/// turn the listing into an oracle answering "does this id exist" by omission.
///
/// `title` and `cwd` are the two content fields on the wire type: a title is
/// model-generated *from the user's prompt text*, and `cwd` is an absolute path
/// naming a repo on this machine. Both are boundary content wearing the costume
/// of metadata (LESSON-432), which is why they are the pair that goes.
///
/// Pure, and taking the answer rather than computing it: "may this connection
/// see this session" has exactly one definition ([`ConnState::may_receive`],
/// which already folds in `monitor`), and a second one derived here would be a
/// second answer to drift out of step with it (LESSON-484). What this function
/// owns is only *what reduction means*, so the redaction rule is table-testable
/// without a socket.
///
/// Omission is expressible without a wire change: both fields are already
/// `Option` with `skip_serializing_if = "Option::is_none"`, so a reduced row
/// carries no `title`/`cwd` key at all — not an empty string, which would be a
/// new value a client must learn to distinguish from a genuinely empty title.
fn reduce_for(summary: SessionSummary, visible: bool) -> SessionSummary {
    if visible {
        return summary;
    }
    SessionSummary {
        title: None,
        cwd: None,
        ..summary
    }
}

/// Drives one client connection from handshake to disconnect.
///
/// `peer` is the kernel's attestation of who is on the other end, read once at
/// `accept` (REQ-569 ADR-B). Its uid has already authorized the connection; its
/// pid is spent at the handshake, where the ancestry verdict is taken.
async fn handle_client(stream: UnixStream, daemon: Arc<Daemon>, peer: PeerIdentity) {
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
    // In-flight `session/attach` calls, for exactly the same reason (REQ-569
    // BR-6): an attach that needs consent awaits an `attach/consent` that may
    // arrive on *this* connection, so running it here would deadlock the loop
    // that has to read the answer.
    let mut attach_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut line = String::new();

    loop {
        // Reuse the buffer's capacity for the common small frame, but release it
        // after a near-cap one so a single large frame does not pin ~4 MiB for
        // the connection's lifetime (F8; ADR-D's bound is per-frame).
        if line.capacity() > LINE_RETAIN_CAP {
            line = String::new();
        } else {
            line.clear();
        }
        // BR-6/AC-5: the read is capped by construction. A fresh `take` every
        // iteration is load-bearing — `MAX_FRAME` is a per-*frame* budget, and a
        // `Take` hoisted out of the loop would spend it once across the whole
        // connection lifetime, refusing the second legal frame of a long-lived
        // client.
        let read = (&mut reader).take(MAX_FRAME).read_line(&mut line).await;
        let n = match read {
            Ok(0) => break, // EOF: the client disconnected.
            Ok(n) => n,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    // BR-6: an oversized frame whose MAX_FRAME cut split a UTF-8
                    // sequence (or any non-UTF-8 frame) is refused, not silently
                    // dropped — same refusal as the newline-terminated oversized
                    // case. `read_line` returns `InvalidData` (not `Ok`) when the
                    // budgeted bytes are not valid UTF-8, so it never reaches the
                    // length check below.
                    let _ = out_tx.try_send(error_string(
                        Id::Null,
                        error_code::INVALID_PARAMS,
                        "frame exceeds maximum length or is not valid utf-8",
                    ));
                }
                break; // any other read error: tear the connection down.
            }
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
            if let Some((sub, guard, state)) =
                do_handshake(&daemon, peer, id, params, &out_tx).await
            {
                handshaked = true;
                client_guard = Some(guard);
                let (forwarded_tx, forwarded_rx) = watch::channel(0u64);
                fence = Some(EventFence {
                    delivered: sub.delivered_counter(),
                    forwarded: forwarded_rx,
                });
                register_consent_surface(&daemon, &state, out_tx.clone());
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
        //
        // Unreachable today (`conn` and `handshaked` are set together), but a
        // bare `continue` would drop a request carrying an `id` and hang the
        // client — every other refusal answers, so this one does too (F9).
        let Some(conn) = conn.as_ref() else {
            let _ = out_tx.try_send(error_string(
                id,
                error_code::INTERNAL_ERROR,
                "connection state unavailable",
            ));
            continue;
        };

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

        // `session/attach` runs on its own task for `session/prompt`'s reason
        // (REQ-569 BR-6): it may await a consent decision that arrives on this
        // same connection, and the reader loop has to stay free to read it.
        if method == SessionAttachParams::METHOD {
            let daemon = Arc::clone(&daemon);
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let fence = fence.clone();
            attach_tasks.retain(|h| !h.is_finished());
            attach_tasks.push(tokio::spawn(async move {
                let response = handle_session_attach(&daemon, &conn, id, params).await;
                // The fence for the same reason `dispatch`'s responses take it:
                // any event already delivered to this client's subscription
                // must reach the wire ahead of the response ordering it.
                if let Some(fence) = fence {
                    fence.sync().await;
                }
                let _ = out_tx.send(response).await;
            }));
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

    // REQ-569 BR-6/ADR-C, and the order is the mechanism:
    //
    //   1. stop being a consent surface  → no new prompt is routed here, and
    //                                      no in-flight attach can pick this
    //                                      connection as an approver
    //   2. end the in-flight attaches    → each is awaiting a decision that can
    //                                      still *mint a grant*, so they must
    //                                      be finished before the release below
    //   3. release the grants            → nothing can add one after this point
    //
    // Aborting before awaiting is what makes step 2 bounded rather than up to
    // one consent window long, and awaiting after aborting is what makes it
    // *deterministic*: dropping a `JoinHandle` detaches the task, so a bare
    // abort would leave a task that might still mint a grant for a connection
    // whose grants have just been released — a credential with no subject and
    // nobody to revoke it (LESSON-501: state carried past its creator's
    // lifetime sheds its invariants).
    //
    // Unconditional: most connections were never granted anything, and
    // `release` on a connection holding nothing is a no-op. A connection that
    // never handshaked has no id, so it has neither surface nor grants.
    if let Some(state) = conn.as_ref() {
        daemon.surfaces.release(state.id);
    }
    for task in attach_tasks {
        task.abort();
        let _ = task.await;
    }
    if let Some(state) = conn.as_ref() {
        daemon.grants.release(state.id);
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
///
/// It is also where the connection's [`Ancestry`] is settled, once, from the
/// pid the kernel attested at `connect(2)` (REQ-569 BR-4). Computing it per
/// call would spend a kernel read on every request and — the part that matters —
/// would let the verdict change under a connection whose pid was reused, so the
/// gate a request meets could differ from the gate the handshake meant.
async fn do_handshake(
    daemon: &Daemon,
    peer: PeerIdentity,
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

    let ancestry = daemon.process.ancestry_of(peer.pid);
    let connection = daemon.grants.next_connection_id();

    // REQ-569 BR-2/BR-4: the monitor declaration is gated here, and the
    // *handshake itself* is what fails.
    //
    // Not a silent downgrade of `monitor` to false. A client that asked to watch
    // every session and was quietly given a connection that watches none would
    // go on believing it was monitoring — and the daemon would have turned a
    // refused request into a successful one, which is a guard that disables
    // itself the moment anyone stops reading its output (LESSON-443). The
    // refusal is the answer; the client decides what to do about it.
    //
    // Placed after negotiation so a version-incompatible monitor still gets its
    // version diagnosis, and before `admit()` so a refused monitor takes no
    // lifetime claim, publishes no attach event, and subscribes to nothing.
    //
    // Two reasons, two codes (BR-5): ancestry is checked first and is terminal,
    // so a daemon descendant is never told "ask for a grant" about a grant it
    // may not have. Everyone else without a monitor-scope grant is refused
    // `NOT_GRANTED`, immediately.
    if params.monitor {
        if !matches!(ancestry, Ancestry::NotDescendant) {
            eprintln!(
                "{}",
                ancestry_refusal_line(ancestry, "a monitor declaration")
            );
            let _ = out_tx.try_send(error_string(
                id,
                error_code::ATTACH_FORBIDDEN,
                ATTACH_FORBIDDEN_MESSAGE,
            ));
            return None;
        }
        if !daemon.grants.may_monitor(connection) {
            // **There is no consent path here, and that is the fix, not an
            // omission** (REQ-569 verify, F1).
            //
            // TASK-108 added one: a monitor request was routed to "any attached
            // peer other than the requester", on the theory that an attached
            // connection is a surface whose user demonstrably owns something.
            // It was mintable over the socket by one attacker holding two
            // connections. `session/create` is ungated by design, so connection
            // A created a throwaway session — which made A attached, and made A
            // a registered surface — and connection B then declared `monitor`.
            // The routing picked A as the approver, A answered, and B became a
            // daemon-wide observer of every session on the machine. No human was
            // involved, and it did not even read as a self-approval: two
            // different `ConnectionId`s, so the `self_approved_by` log line
            // stayed silent.
            //
            // No sound approver predicate exists over the primitives this
            // daemon has. It cannot distinguish an attacker's second connection
            // from the user's real client — that is the same ADR-A residual the
            // spec records for attach's self-render arm — and a peer-pid check
            // only forces the attacker to fork. So the capability keeps its
            // grant gate (BR-2/AC-4: `GrantScope::Monitor` and `may_monitor` are
            // untouched) and simply has no minter reachable from a socket.
            //
            // No `attach_refused` is published either. The route it would be
            // published on reaches nobody: this connection is not a registered
            // surface until its handshake succeeds, and there is no peer the
            // request was ever offered to.
            let _ = out_tx.try_send(error_string(
                id,
                error_code::NOT_GRANTED,
                NOT_GRANTED_MESSAGE,
            ));
            return None;
        }
    }

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

    Some((
        subscription,
        client_guard,
        ConnState::new(
            connection,
            ancestry,
            params.monitor,
            requester_descriptor(&params),
        ),
    ))
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
    // `client_name` is bounded only by MAX_FRAME on the wire (~4 MiB, and ~6×
    // that once `Debug` escapes it), so cap it to a fixed char budget before
    // formatting or one handshake could flood the daemon log. `chars().take`
    // keeps the prefix on a char boundary; the `{:?}` escaping stays (it is
    // what stops the newline forgery, not the truncation).
    const NAME_BUDGET: usize = 128;
    let name: String = params.client_name.chars().take(NAME_BUDGET).collect();
    format!(
        "tetond: {:?} client {:?} declared monitor at handshake: \
         it receives every session's events",
        params.client_kind, name
    )
}

/// Record a freshly handshaked connection as a consent surface (REQ-569 BR-6,
/// verify F2).
///
/// **Every** handshaked connection is registered — including one the ancestry
/// gate excludes — and that is the fix rather than an oversight.
///
/// The registry answers two different questions with one entry. The first is
/// *routing*: `anyone_attached_to(S)` decides whether an ungranted attach to `S`
/// is put to the connection that holds `S` (arm 1) or rendered at the
/// requester's own face (arm 2). The second is *delivery*: who is actually sent
/// the prompt frame.
///
/// Registration used to be conditional on [`ConnState::may_hold_session_access`],
/// which silently made the first question answer the second. A legitimate client
/// whose ancestry came back [`Ancestry::Indeterminate`] — a vanished pid, a
/// platform with no peer-pid option — held its session *invisibly*, so
/// `anyone_attached_to` said nobody held it, so a stranger attaching to that
/// session got the **self-render arm and approved itself** into somebody else's
/// session. A gate that fails closed at one door was fail-opening at the next.
///
/// So the two questions are split at the level they differ: the entry always
/// exists, and `may_answer` withholds only the frame. Authorization is unmoved —
/// [`handle_attach_consent`] refuses a descendant an *answer* regardless — so
/// routing to an excluded holder fails closed into a consent timeout instead of
/// fail-opening into a self-approval.
fn register_consent_surface(daemon: &Daemon, state: &ConnState, out: mpsc::Sender<String>) {
    daemon.surfaces.register(
        state.id,
        Arc::clone(&state.attached),
        out,
        state.may_hold_session_access(),
    );
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
            // REQ-569 BR-10: every session is still *listed* — what varies is
            // how much of each row this connection is shown.
            let result = SessionListResult {
                sessions: daemon
                    .sessions
                    .list()
                    .into_iter()
                    .map(|summary| {
                        let visible = conn.may_receive(Some(&summary.session_id));
                        reduce_for(summary, visible)
                    })
                    .collect(),
            };
            Some(ok_string(id, &result))
        }
        // `session/attach` is deliberately absent, exactly as `session/prompt`
        // is: both run on their own task (see `handle_client`), because both
        // await something this reader loop has to stay free to read.
        SessionClearParams::METHOD => Some(handle_session_clear(daemon, conn, id, params)),
        AttachConsentParams::METHOD => Some(handle_attach_consent(daemon, conn, id, params)),
        PermissionRespondParams::METHOD => {
            Some(handle_permission_respond(daemon, conn, id, params))
        }
        ModelConfirmParams::METHOD => Some(handle_model_confirm(daemon, id, params)),
        ModelListParams::METHOD => Some(ok_string(id, &daemon.runtime.model_list())),
        ModelSetParams::METHOD => Some(handle_model_set(daemon, id, params)),
        ModelStatusParams::METHOD => Some(ok_string(id, &daemon.runtime.model_status())),
        ConfigGetParams::METHOD => Some(handle_config_get(daemon, id)),
        ConfigSetParams::METHOD => Some(handle_config_set(daemon, id, params)),
        CostQueryParams::METHOD => Some(handle_cost_query(daemon, id)),
        WebOverrideParams::METHOD => Some(handle_web_override(daemon, conn, id, params)),
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
///
/// `conn` is the issuing connection: lifting a session's web taint is driving
/// that session, so it is gated on attachment (REQ-568 BR-4) exactly as
/// `session/clear` is — the check sits before the runtime is touched, so an
/// unattached caller cannot read a session's existence out of which refusal it
/// got (ADR-B).
fn handle_web_override(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: WebOverrideParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
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

/// Deliver a client's `permission/respond` to the waiting harness gate, if this
/// connection is attached to the session that raised the prompt (REQ-569 BR-9,
/// ADR-F).
///
/// Answering a permission prompt *is* driving the session — it decides whether
/// that session's tool call runs — so it is gated exactly like `session/prompt`,
/// `session/clear` and `web/override`, on [`ConnState::may_drive`]. Deliberately
/// not [`ConnState::may_receive`]: a `monitor` **does** receive every session's
/// `permission_request` (REQ-568 BR-2, unchanged here), and reading the write
/// gate off the delivery policy would hand every observer the authority to
/// approve every tool call it can see. Seeing a prompt and answering it are the
/// two things this REQ separates.
///
/// A request id with no waiter keeps the pre-existing behaviour untouched:
/// acknowledged, idempotent, nothing to gate. That is the same answer a late or
/// duplicate reply always got, and it is deliberately *not* a refusal — the
/// answering connection learns nothing from it about whether some other
/// session's prompt is outstanding (ADR-B's posture: a refusal must not become
/// an oracle).
///
/// **The refusal does not consume the waiter.** [`PendingPermissions::owner_of`]
/// is a read; `resolve` is the only thing that takes the waiter, and it runs
/// only past the gate. A refusal that had consumed the prompt would deny the
/// tool call of a user who was never asked — a stranger could silence any
/// session's prompt at will, which is a denial of service dressed as a security
/// check.
///
/// The two-step read-then-resolve is not a TOCTOU hole: request ids are
/// daemon-unique and never reused (BUG-161), so the only thing that can happen
/// between them is the rightful answer arriving first, which leaves this call on
/// the harmless "no waiter" path.
fn handle_permission_respond(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: PermissionRespondParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    let pending = daemon.runtime.pending();
    if let Some(owner) = pending.owner_of(&params.request_id) {
        if !conn.may_drive(&owner) {
            return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
        }
    }
    pending.resolve(&params.request_id, params.outcome);
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
            //
            // REQ-569 BR-1: this is also the moment the connection acquires the
            // one standing that needs no grant. Creating a session is the
            // capability — recorded here, where the session is made, rather than
            // inferred later from the attachment set (which a granted attach
            // also writes to).
            conn.record_created(summary.session_id.clone());

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
        // Two failures, two codes (REQ-569 verify, F9). A missing phase is the
        // caller's params; an entropy failure is this machine's, and telling a
        // caller their params were invalid sends them off fixing a request that
        // was never wrong (LESSON-456's shape: classify where the daemon knows).
        Err(err @ SessionCreateError::MissingPhase) => {
            error_string(id, error_code::INVALID_PARAMS, err.message())
        }
        Err(err @ SessionCreateError::NoEntropy) => {
            error_string(id, error_code::INTERNAL_ERROR, err.message())
        }
    }
}

/// Serializes a routed event into the wire frame a connection's outbound
/// channel carries.
///
/// The consent events are the daemon's only *routed* events (BR-6): every other
/// event goes on the bus and is filtered per connection by [`forward_events`],
/// while these two are addressed to named surfaces by
/// [`crate::consent::ConsentSurfaces`]. They still take their sequence number
/// from the bus, so a routed frame can never collide with a broadcast one on
/// the same connection.
///
/// The session, when there is one, rides on the envelope rather than in the
/// payload — [`Event`] is flattened, so a `session_id` field on the payload
/// would emit the key twice.
///
/// That envelope is session-scoped while one recipient — the requester — is by
/// definition *not* attached to that session, which looks like a hole in
/// REQ-568's scoping and is not one. These frames never cross
/// [`forward_events`], so the delivery policy is not being bypassed; and the id
/// is not news to anyone who receives it, because the requester is the
/// connection that named it and the other recipients are attached to it.
fn consent_event_frame(daemon: &Daemon, session_id: Option<SessionId>, event: Event) -> String {
    let envelope = EventEnvelope::new(daemon.events.next_seq(), session_id, event);
    // Infallible for these payloads (plain strings and closed enums); a
    // hypothetical failure costs the prompt a delivery, which the bounded
    // window already turns into a refusal.
    serde_json::to_string(&Notification::new(EVENT_METHOD, envelope)).unwrap_or_default()
}

/// Tell the surfaces a request was offered to how it ended (BR-5).
///
/// Published on the request's own route, so exactly the people who were asked
/// learn the answer — plus the requester, which
/// [`ConsentRoute::renders_outcome`] adds because it is the one connection that
/// must retire its own pending state and the one connection no
/// attachment-shaped delivery would reach.
///
/// For [`AttachRefusedReason::NoGrant`] that route is empty by construction:
/// nobody was attached anywhere, which is *why* the answer is "no grant" rather
/// than a prompt. It is published through the same site anyway, because the
/// alternative is a publisher that has to know which refusals are worth
/// announcing — and that is a second policy to keep in step with this one.
fn publish_attach_refusal(
    daemon: &Daemon,
    route: &ConsentRoute,
    session_id: Option<SessionId>,
    request_id: Option<RequestId>,
    scope: ConsentScope,
    reason: AttachRefusedReason,
) {
    let frame = consent_event_frame(
        daemon,
        session_id,
        Event::AttachRefused(AttachRefused {
            request_id,
            scope,
            reason,
        }),
    );
    daemon.surfaces.deliver_outcome(route, &frame);
}

/// Announce a grant the daemon just minted, to every handshaked connection
/// (REQ-569 verify, F6).
///
/// # Why the log line was not enough
///
/// TASK-109's answer to the self-approval residual was a sentence on the
/// daemon's stderr. As a *control* that is weak in three ways, and all three
/// favour the adversary: the daemon's stderr is read on startup failure and
/// almost never otherwise, the CLI's spawn path truncates it at 256 KiB, and it
/// is a file writable by the very same uid the whole perimeter is drawn against
/// — so the process that self-approved can erase the record of having done it.
/// A control an attacker can delete is a control that reports on honest users
/// only.
///
/// So the log line stays, and this joins it: a **daemon-scoped** event
/// (`session_id: None`, which REQ-568's delivery rule broadcasts to every
/// handshaked connection, attached or not). It is in-perimeter, it cannot be
/// suppressed by anything the requester controls, and it reaches a human who is
/// actually looking at a screen right now.
///
/// It carries the scope, the requester descriptor, and — the field that matters
/// — whether the grant was **self-approved**, so a reader does not have to
/// reconstruct which arm ran. Nothing here names the session: a grant
/// announcement goes to every connection, so putting a session id in it would
/// leak the id namespace to connections BR-10 keeps it from.
fn publish_grant_minted(
    daemon: &Daemon,
    scope: ConsentScope,
    requester: &str,
    self_approved: bool,
) {
    daemon.events.publish(
        None,
        Event::SessionGrantMinted(SessionGrantMinted {
            scope,
            requester: requester.to_owned(),
            self_approved,
        }),
    );
}

/// Put the question to a user and wait for the answer (BR-6/BR-7, ADR-E).
///
/// The whole consent round trip in one place, so the attach path and the
/// monitor path cannot drift into two subtly different flows: register the
/// waiter **before** publishing (an answer that arrives instantly must find
/// something to resolve), publish on the route, await under the daemon's
/// bounded window, and announce a refusal on the same route.
///
/// Defaults closed on every ending it does not understand. Nothing here mints
/// anything — the caller does that, and only on
/// [`ConsentOutcome::Granted`] — which is what keeps "a denied or timed-out
/// request leaves no partial grant state" a property of one branch rather than
/// of every path through two handlers (BR-7, LESSON-501).
async fn seek_consent(
    daemon: &Daemon,
    requester: &str,
    scope: ConsentScope,
    session_id: Option<SessionId>,
    route: &ConsentRoute,
) -> Option<ConsentOutcome> {
    let request_id = daemon.consents.next_request_id();
    // `None` is the F4 cap: this connection already holds
    // `MAX_PENDING_CONSENTS_PER_CONNECTION` prompts, so nothing is registered,
    // nothing is published, and the caller refuses. Counted inside the
    // registry's own lock, never here — see `PendingConsents::register`.
    let rx = daemon
        .consents
        .register(request_id.clone(), route.clone())?;

    // From here on there is a waiter in the registry, and it must not survive
    // this future. `in_flight` retires it on every exit including the one no
    // arm below can see: the whole future being *dropped* at its await point,
    // which is what a connection teardown does (F3).
    let mut in_flight = ConsentInFlight {
        daemon,
        route,
        request_id: request_id.clone(),
        session_id: session_id.clone(),
        scope,
        settled: false,
    };

    let frame = consent_event_frame(
        daemon,
        session_id.clone(),
        Event::AttachConsentRequested(AttachConsentRequested {
            request_id: request_id.clone(),
            scope,
            requester: requester.to_owned(),
        }),
    );
    daemon.surfaces.deliver_request(route, &frame);

    let outcome = daemon
        .consents
        .await_decision(&request_id, rx, daemon.consent_timeout)
        .await;
    in_flight.settled = true;
    let reason = match outcome {
        ConsentOutcome::Granted => return Some(outcome),
        ConsentOutcome::Denied => AttachRefusedReason::ConsentDenied,
        ConsentOutcome::TimedOut => AttachRefusedReason::ConsentTimeout,
    };
    publish_attach_refusal(daemon, route, session_id, Some(request_id), scope, reason);
    Some(outcome)
}

/// The consent request [`seek_consent`] is awaiting, retired on **every** way
/// out — including cancellation (REQ-569 verify, F3).
///
/// # The ending nobody wrote an arm for
///
/// `handle_client`'s teardown does `task.abort(); task.await;` over the
/// in-flight `session/attach` tasks. Aborting drops the future at its await
/// point, so `await_decision` never returns and none of its three endings runs.
/// Before this guard that left the [`crate::consent::Waiter`] in the daemon-wide
/// map **forever**: a `connect → attach → disconnect` loop grew it without
/// bound, and every leaked entry counted against the F4 per-connection cap for
/// an id that will never be answered.
///
/// `Drop` is the only thing that runs on cancellation, so the retirement lives
/// here rather than in the teardown block — a teardown-side list would have to
/// be kept in step with every future `seek_consent` call site, and the one that
/// forgot would leak silently.
///
/// It also publishes the [`AttachRefusedReason::RequesterGone`] the surfaces are
/// owed. The peer that rendered the prompt has a security dialog on screen for a
/// connection that no longer exists; without this it sits there until the user
/// answers a question about nobody.
///
/// `settled` marks the paths that already announced their own ending, so a
/// normal denial is not announced twice.
struct ConsentInFlight<'a> {
    daemon: &'a Daemon,
    route: &'a ConsentRoute,
    request_id: RequestId,
    session_id: Option<SessionId>,
    scope: ConsentScope,
    settled: bool,
}

impl Drop for ConsentInFlight<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.daemon.consents.forget(&self.request_id);
        publish_attach_refusal(
            self.daemon,
            self.route,
            self.session_id.clone(),
            Some(self.request_id.clone()),
            self.scope,
            AttachRefusedReason::RequesterGone,
        );
    }
}

/// Attach a connection to an existing session (`session/attach`), which is what
/// grants it that session's events from here on (REQ-568 BR-1) — and, since
/// REQ-569, is itself the thing that must be authorized (BR-1/BR-2/BR-4/BR-6).
///
/// **Four answers, in this order, and the order is the requirement.**
///
/// 1. *Ancestry* (BR-4, ADR-A). A connection out of the daemon's own process
///    tree is refused `ATTACH_FORBIDDEN` — before the params are even parsed,
///    let alone the registry consulted. There is no consent path from here and
///    never will be: no prompt is raised, so the daemon's own children can
///    neither obtain a grant nor make a user's screen light up by asking for
///    one.
/// 2. *Standing* (BR-1). A connection that created the session or already holds
///    an attach-scope grant attaches with **no prompt at all** — the
///    single-client create-and-prompt flow costs exactly what it used to
///    (AC-7).
/// 3. *Consent* (BR-6). Everyone else has the question put to a user, and the
///    answer is the only thing in this daemon that mints an attach grant.
/// 4. Only then is the session looked up and attached, exactly as before.
///
/// Refusals (1) and (3) both precede `daemon.sessions.get`, and that placement
/// is load-bearing rather than tidy: answering `UNKNOWN_SESSION` first for an
/// id the connection may not have would turn `session/attach` into an oracle
/// that confirms guessed session ids, which is the whole property BR-8
/// protects (ids are names, grants are credentials). A guessed id and a real
/// one raise the same prompt and draw the same refusal.
///
/// Which is also why a granted consent for a session that does not exist mints
/// a grant and *then* answers `UNKNOWN_SESSION`: the alternative is to check
/// existence before asking, which rebuilds the oracle. The residual entry is a
/// grant over an id that names nothing, keyed to this connection, and released
/// when it ends.
///
/// # Not on the reader loop
///
/// This is `async` and run on its own task ([`handle_client`]) for the reason
/// `session/prompt` is: it awaits a reply that has to be *read* — the
/// `attach/consent` that answers it may arrive on this very connection (BR-6's
/// second arm). Awaiting it inline would deadlock the loop that must deliver
/// the answer.
async fn handle_session_attach(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // (1) Ahead of the parse, not merely ahead of the registry. A daemon
    // descendant learns nothing at all here — not whether the session exists,
    // not whether its own request was well-formed, and not what a user would
    // have said, because no user is asked.
    if !conn.may_hold_session_access() {
        eprintln!("{}", ancestry_refusal_line(conn.ancestry, "session/attach"));
        return error_string(id, error_code::ATTACH_FORBIDDEN, ATTACH_FORBIDDEN_MESSAGE);
    }

    let params: SessionAttachParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    // (1b) A length the daemon could never have minted is refused at the parse
    // boundary (REQ-569 verify, F9). `session_id` is otherwise bounded only by
    // `MAX_FRAME` — ~4 MiB — and a granted consent stores it verbatim as a
    // `HashSet` key in the grant registry, so an unbounded id is an unbounded
    // allocation keyed to a connection. Length only, and checked before the
    // standing question so it costs nothing: this is a well-formedness rule, not
    // an existence oracle — every id of a plausible length still draws the same
    // refusal whether it names a live session or nothing at all (BR-8).
    if !sessions::within_minted_length(&params.session_id) {
        return error_string(id, error_code::INVALID_PARAMS, "invalid params");
    }

    // (2) The standing question, asked of the one module that answers it. The
    // creator's own attach comes through the same call rather than being
    // short-circuited above it, so there is a single definition of "may attach"
    // instead of one here and one in `grants` (LESSON-484).
    if daemon
        .grants
        .may_attach(conn.id, &params.session_id, &conn.created())
    {
        return attach_to(daemon, conn, id, &params.session_id);
    }

    // (3) No standing, so ask. **Which arm runs is decided here, once** (BR-6):
    //
    //   - Arm 1, `attached_to`: some connection is already attached to the
    //     target, so the prompt goes to it. That is the good case — the user
    //     approving is the user who already has the session open.
    //   - Arm 2, `requester_itself`: nothing is attached to the target, so the
    //     requester renders its own prompt. This is the resume flow BR-6
    //     requires to keep working after the last client left, and it is sound
    //     *only* because step (1) already refused every connection out of the
    //     daemon's own process tree — "ask the requester" can never mean "ask a
    //     tool child".
    //
    // The check races an attach or a disconnect by nature, and deliberately is
    // not locked against: the worst it can produce is a prompt offered to the
    // requester while a peer was also entitled to answer, or offered to a peer
    // that then leaves — which the bounded window already ends.
    let route = if daemon.surfaces.anyone_attached_to(&params.session_id) {
        ConsentRoute::attached_to(conn.id, params.session_id.clone())
    } else {
        ConsentRoute::requester_itself(conn.id)
    };
    let Some(outcome) = seek_consent(
        daemon,
        &conn.requester,
        ConsentScope::Attach,
        Some(params.session_id.clone()),
        &route,
    )
    .await
    else {
        // The F4 cap: this connection already has the most prompts outstanding
        // it is allowed, so no fourth one is put in front of a user. Refused
        // before anything was registered or published.
        return error_string(id, error_code::NOT_GRANTED, TOO_MANY_PENDING_MESSAGE);
    };

    match outcome {
        // The one place an attach-scope grant is minted, and it mints exactly
        // one, at exactly the scope that was asked for and for exactly the
        // session that was named (LESSON-495).
        ConsentOutcome::Granted => {
            daemon
                .grants
                .grant(Grant::attach(conn.id, params.session_id.clone()));
            // REQ-569 verify, F6: announce it in-perimeter, at the moment it is
            // minted. `self_approved_by` reads off the *route*, which is sound
            // because arm 2 is the only arm the requester may answer at all —
            // so "the route was the self-render arm" and "the requester
            // answered it" are the same fact here.
            publish_grant_minted(
                daemon,
                ConsentScope::Attach,
                &conn.requester,
                route.self_approved_by(conn.id),
            );
            attach_to(daemon, conn, id, &params.session_id)
        }
        // (4) Both refusals mint nothing — there is no branch here that
        // touches the grant registry, which is what makes BR-7 a property of
        // the code's shape rather than of remembering to undo something.
        ConsentOutcome::Denied => {
            error_string(id, error_code::CONSENT_DENIED, CONSENT_DENIED_MESSAGE)
        }
        ConsentOutcome::TimedOut => {
            error_string(id, error_code::CONSENT_TIMEOUT, CONSENT_TIMEOUT_MESSAGE)
        }
    }
}

/// The authorized tail of [`handle_session_attach`]: look the session up and
/// attach. REQ-568's behaviour, unchanged.
fn attach_to(daemon: &Daemon, conn: &ConnState, id: Id, session_id: &SessionId) -> String {
    match daemon.sessions.get(session_id) {
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

/// Deliver a user's decision to the request waiting on it (`attach/consent`,
/// REQ-569 BR-6, ADR-E).
///
/// **Receiving the prompt is not standing to answer it.** The route the request
/// was raised on is read back and asked whether *this* connection is one of the
/// surfaces it was offered to — the same predicate that decided delivery, so
/// there is one rule rather than a delivery rule and an authorization rule to
/// drift apart. A `monitor` is the case that makes the point: it receives every
/// session's events, and it may answer none of these (LESSON-502 — seeing a
/// decision and making it are different rights).
///
/// **The refusal does not consume the waiter.** [`PendingConsents::route_of`]
/// is a read; `resolve` is the only thing that takes it, and it runs only past
/// the gate. A refusal that consumed the request would let any connected peer
/// cancel any pending consent at will — a denial of service dressed as a
/// security check (the [`handle_permission_respond`] shape, for the same
/// reason).
///
/// An unknown `request_id` is acknowledged rather than refused, with
/// `resolved: false`: the window may simply have closed. Answering it as a
/// refusal would tell the answering connection whether some other consent is
/// outstanding, and this seam does not hand out oracles.
///
/// # A daemon descendant may not answer either (BR-4)
///
/// The ancestry gate runs here too, and it is not belt-and-braces. A tool child
/// **can** hold a session — it creates its own, and `session/create` is not
/// gated — so it is a connection with a non-empty attachment set, which is
/// precisely what the monitor rule asks for in an approver. Without this check a
/// daemon-spawned child could approve a *peer's* monitor request and hand a
/// same-UID process sight of every session on the machine: BR-4 would hold on
/// the door and fail on the door handle. "May not hold session access" has to
/// mean "may not confer it" (LESSON-484 — the rule belongs where the decision
/// is made, and this is a decision).
fn handle_attach_consent(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    if !conn.may_hold_session_access() {
        eprintln!("{}", ancestry_refusal_line(conn.ancestry, "attach/consent"));
        return error_string(id, error_code::ATTACH_FORBIDDEN, ATTACH_FORBIDDEN_MESSAGE);
    }

    let params: AttachConsentParams = match serde_json::from_value(params) {
        Ok(params) => params,
        // A closed enum by design: an `outcome` this build cannot read is an
        // error, never a silent fallback — and here one of the two fallbacks
        // would mint a credential.
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    let Some(route) = daemon.consents.route_of(&params.request_id) else {
        return ok_string(id, &AttachConsentResult { resolved: false });
    };
    let attached = conn.attached();
    if !route.renders_request(conn.id, &attached) {
        return error_string(id, error_code::NOT_ATTACHED, CONSENT_NOT_OFFERED_MESSAGE);
    }

    let granted = matches!(params.outcome, AttachConsentOutcome::Granted);
    let outcome = match params.outcome {
        AttachConsentOutcome::Granted => ConsentOutcome::Granted,
        AttachConsentOutcome::Denied => ConsentOutcome::Denied,
    };
    let resolved = daemon.consents.resolve(&params.request_id, outcome);
    // REQ-569 TASK-109: the one grant nobody but the requester took part in, and
    // therefore the one an operator has to be able to see (see
    // [`self_approval_line`]).
    //
    // All three conditions, and each rules out a false entry: `resolved` because
    // an answer that reached no waiter mints nothing (the window had closed, or
    // another surface answered first), `granted` because a denial mints nothing
    // either, and `self_approved_by` because a peer-approved grant is a decision
    // two connections took part in. Logged after `resolve` rather than before
    // it for the first of those: what is being reported is a grant that is
    // actually about to exist.
    if resolved && granted && route.self_approved_by(conn.id) {
        eprintln!("{}", self_approval_line(&conn.requester));
    }
    ok_string(id, &AttachConsentResult { resolved })
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

    use teton_protocol::RequestId;

    use crate::consent::MAX_PENDING_CONSENTS_PER_CONNECTION;
    use crate::harness::permissions::{
        PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
    };

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

    /// A connection on `daemon` that created nothing, attached to nothing and
    /// declared nothing — the state every direct-dispatch test starts from.
    ///
    /// Its id is minted from the daemon it will talk to, because grants are
    /// keyed by connection: a `ConnState` carrying an id from some other
    /// registry would ask its questions of a namespace nothing answers in.
    ///
    /// Its ancestry is [`Ancestry::NotDescendant`] — the ordinary client the
    /// REQ-569 BR-4 gate lets through — so what these tests exercise is the
    /// *grant* gate behind it. The descendant cases say so explicitly.
    fn unattached(daemon: &Daemon) -> ConnState {
        conn_with_ancestry(daemon, Ancestry::NotDescendant)
    }

    /// A connection that declared `monitor` at the handshake (REQ-568's
    /// delivery policy, which REQ-569 does not change for a connection that
    /// got past the declaration gate).
    fn monitoring(daemon: &Daemon) -> ConnState {
        ConnState::new(
            daemon.grants.next_connection_id(),
            Ancestry::NotDescendant,
            true,
            TEST_REQUESTER.to_owned(),
        )
    }

    /// A connection whose process this daemon classified as `ancestry`.
    fn conn_with_ancestry(daemon: &Daemon, ancestry: Ancestry) -> ConnState {
        ConnState::new(
            daemon.grants.next_connection_id(),
            ancestry,
            false,
            TEST_REQUESTER.to_owned(),
        )
    }

    /// What a consent prompt calls a fixture connection.
    const TEST_REQUESTER: &str = "Cli client \"test\"";

    /// A window short enough that a test can assert on the *timeout* arm
    /// without waiting out [`CONSENT_TIMEOUT`]'s human-sized window (BR-7).
    const TEST_CONSENT_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

    /// A daemon whose consent window a test can outlast.
    fn daemon_with_short_consent() -> Arc<Daemon> {
        Arc::new(Daemon::new().with_consent_timeout(TEST_CONSENT_WINDOW))
    }

    /// Register `conn` as a consent surface and return the channel a prompt
    /// routed to it would arrive on.
    ///
    /// In production this happens in `handle_client` at the handshake; a
    /// handler-level test has no handshake, so it says so explicitly.
    ///
    /// It goes through [`register_consent_surface`] — the *same* function the
    /// handshake calls — rather than reaching into `daemon.surfaces` itself.
    /// That is what makes the F2 rule testable at all: which connections are
    /// registered, and with what answering rights, is a decision, and a fixture
    /// that re-implemented it would pass no matter what the production seam did.
    fn as_surface(daemon: &Daemon, conn: &ConnState) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(16);
        register_consent_surface(daemon, conn, tx);
        rx
    }

    /// The `attach_consent_requested` frames on `rx`, as parsed event payloads.
    fn consent_prompts(rx: &mut mpsc::Receiver<String>) -> Vec<Value> {
        let mut prompts = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            let value: Value = serde_json::from_str(&frame).expect("a routed frame is JSON");
            if value["params"]["event"] == "attach_consent_requested" {
                prompts.push(value["params"].clone());
            }
        }
        prompts
    }

    /// The id `session/create` just minted, read back off its response
    /// (REQ-569 BR-8).
    ///
    /// A session id is 128 random bits, so a test can no longer *name* the
    /// session it created — it has to capture it. The parsing lives in one
    /// place so a fixture reads `let session = created_session_id(&created);`
    /// instead of repeating a literal that was only ever true while ids were a
    /// counter.
    fn created_session_id(response: &str) -> SessionId {
        let parsed: Value = serde_json::from_str(response)
            .unwrap_or_else(|e| panic!("session/create answered with non-JSON ({e}): {response}"));
        let id = parsed["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session/create carried no session_id: {response}"));
        SessionId::from(id)
    }

    #[test]
    fn dispatch_rejects_unknown_methods() {
        let daemon = Daemon::new();
        let response = dispatch(
            &daemon,
            &unattached(&daemon),
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
            &unattached(&daemon),
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(created.contains("session_id"));
        let session = created_session_id(&created);

        let listed = dispatch(
            &daemon,
            &unattached(&daemon),
            Id::Number(2),
            SessionListParams::METHOD,
            Value::Null,
        )
        .unwrap();
        assert!(listed.contains(&session.to_string()), "{listed}");
    }

    /// REQ-568 BR-1 + REQ-569 BR-1/BR-7: the ways a connection comes to see a
    /// session, and the ways it does not.
    ///
    /// Creating attaches the creator — checked *through* the handler rather
    /// than by calling `attach` directly, because "the creator is attached" is
    /// a property of `session/create`, not of the set. The creator may then
    /// re-attach to what it made, which is the standing REQ-569 leaves
    /// ungated — and, since TASK-108, the standing that costs **no prompt**
    /// (AC-7: the everyday single-client flow is unchanged).
    ///
    /// A connection that created nothing raises a consent request instead, and
    /// with nobody answering it is refused `CONSENT_TIMEOUT` — *identically*
    /// for the session that exists and for a name the registry never had. That
    /// pair is the assertion, not a detail of it: two different codes here
    /// would rebuild the existence oracle BR-8 closes, letting a client confirm
    /// a guessed session id by which refusal it drew.
    #[tokio::test]
    async fn create_attaches_the_creator_and_an_unanswered_attach_is_refused() {
        let daemon = daemon_with_short_consent();
        let creator = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        assert!(
            creator.may_receive(Some(&session)),
            "the creator must see the session it just made"
        );

        // The creator's own attach is unchanged by REQ-569 — the one standing
        // that needs no grant, and the one that raises no prompt. Its own
        // surface is registered, so a prompt raised here would be visible.
        let mut creator_prompts = as_surface(&daemon, &creator);
        let reattached = handle_session_attach(
            &daemon,
            &creator,
            Id::Number(2),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        assert!(
            reattached.contains(&session.to_string()),
            "the creator may attach to what it created: {reattached}"
        );
        assert!(
            consent_prompts(&mut creator_prompts).is_empty(),
            "standing costs no prompt — AC-7's zero-new-steps claim"
        );

        let onlooker = unattached(&daemon);
        assert!(
            !onlooker.may_receive(Some(&session)),
            "a connection that did nothing must not see another's session"
        );

        for (case, target) in [
            ("a session that exists", session.to_string()),
            (
                "a name the registry never had",
                "sess-nonexistent".to_owned(),
            ),
        ] {
            let refused = handle_session_attach(
                &daemon,
                &onlooker,
                Id::Number(3),
                serde_json::json!({"session_id": target}),
            )
            .await;
            assert!(
                refused.contains(&error_code::CONSENT_TIMEOUT.to_string()),
                "{case}: an unanswered attach must be refused: {refused}"
            );
            assert!(
                !refused.contains(&error_code::UNKNOWN_SESSION.to_string()),
                "{case}: the refusal must not say whether the session exists: {refused}"
            );
        }
        assert!(
            !onlooker.may_receive(Some(&session)),
            "a refused attach must leave the set untouched"
        );
        assert!(
            !onlooker.may_receive(Some(&SessionId::from("sess-nonexistent"))),
            "a refused attach must not leave the name in the set"
        );
        assert!(
            daemon.grants.held_by(onlooker.id).is_empty(),
            "and it must leave no grant behind either (BR-7)"
        );

        // And the grant is what changes the answer — the same connection, the
        // same call, one registry entry later, and no prompt this time.
        daemon
            .grants
            .grant(Grant::attach(onlooker.id, session.clone()));
        let attached = handle_session_attach(
            &daemon,
            &onlooker,
            Id::Number(4),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        assert!(attached.contains(&session.to_string()), "{attached}");
        assert!(
            onlooker.may_receive(Some(&session)),
            "after the grant, attaching is the grant — the session's events are visible"
        );
    }

    /// Answer the consent prompt `rx` is about to receive, as `approver`.
    ///
    /// Spawned rather than inlined because the request and the answer genuinely
    /// live on two tasks in production: the requester is blocked awaiting a
    /// decision while a *different* connection's reader loop delivers it. A
    /// test that answered inline would be testing a flow the daemon does not
    /// have.
    fn answer_consent(
        daemon: &Arc<Daemon>,
        approver: &ConnState,
        mut rx: mpsc::Receiver<String>,
        outcome: &'static str,
    ) -> JoinHandle<Value> {
        let daemon = Arc::clone(daemon);
        let approver = approver.clone();
        tokio::spawn(async move {
            let prompt = loop {
                let frame = rx.recv().await.expect("a prompt must be routed here");
                let value: Value = serde_json::from_str(&frame).expect("a routed frame is JSON");
                if value["params"]["event"] == "attach_consent_requested" {
                    break value["params"].clone();
                }
            };
            let answered = handle_attach_consent(
                &daemon,
                &approver,
                Id::Number(99),
                serde_json::json!({
                    "request_id": prompt["request_id"],
                    "outcome": { "outcome": outcome },
                }),
            );
            assert!(
                answered.contains("\"resolved\":true"),
                "the answer must have decided something: {answered}"
            );
            prompt
        })
    }

    /// **AC-2 / BR-6, first arm.** A user at an already-attached client
    /// approves, and the requester attaches — holding exactly one grant, at
    /// exactly the scope it asked for.
    ///
    /// The grant count is asserted against the *registry*, not inferred from
    /// the response, because "the attach succeeded" and "exactly one
    /// attach-scope grant exists" are different claims and only the second one
    /// rules out a consent path that also handed out something broader
    /// (LESSON-495).
    #[tokio::test]
    async fn a_granted_consent_mints_exactly_one_attach_grant_and_the_attach_succeeds() {
        let daemon = daemon_with_short_consent();
        let holder = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &holder,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let holder_prompts = as_surface(&daemon, &holder);

        let newcomer = unattached(&daemon);
        let mut newcomer_prompts = as_surface(&daemon, &newcomer);
        let answering = answer_consent(&daemon, &holder, holder_prompts, "granted");

        let response = handle_session_attach(
            &daemon,
            &newcomer,
            Id::Number(2),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        let prompt = answering.await.expect("the approver task must not panic");

        assert!(
            response.contains(&session.to_string()),
            "an approved attach must succeed: {response}"
        );
        assert!(
            newcomer.may_receive(Some(&session)),
            "and the newcomer is attached, so REQ-568 delivery applies to it"
        );
        assert_eq!(
            prompt["session_id"].as_str(),
            Some(session.to_string().as_str()),
            "the prompt must name the session being asked for: {prompt}"
        );
        assert_eq!(prompt["scope"], "attach");
        assert_eq!(prompt["requester"], TEST_REQUESTER);

        // Exactly one grant, of exactly the requested scope, keyed to exactly
        // this connection and session.
        assert_eq!(
            daemon.grants.held_by(newcomer.id),
            vec![Grant::attach(newcomer.id, session.clone())]
        );
        assert_eq!(daemon.grants.len(), 1, "and nothing else was minted");
        assert!(
            !daemon.grants.may_monitor(newcomer.id),
            "an approved attach must not confer monitor (LESSON-495)"
        );
        assert_eq!(
            daemon.consents.pending_count(),
            0,
            "a decided request leaves no waiter behind"
        );
        assert!(
            consent_prompts(&mut newcomer_prompts).is_empty(),
            "arm 1 routes the prompt to the attached holder, never to the requester"
        );
    }

    /// **BR-7 / AC-6.** A denial and a timeout both mint **nothing** — asserted
    /// by looking in the grant registry afterwards, not by reading the error
    /// code back.
    ///
    /// The distinction matters because the error code is what the *handler*
    /// decided to say, and the claim is about what the daemon *kept*. A consent
    /// path that answered `CONSENT_DENIED` while leaving a grant behind would
    /// pass an error-code assertion and fail this one — and it is the failure
    /// that would matter, because the next `session/attach` from that
    /// connection would then walk straight through.
    ///
    /// The timeout half also pins the window: it resolves inside a bound the
    /// test sets, which is what makes "defaults closed" an observable behaviour
    /// rather than a promise.
    #[tokio::test]
    async fn a_denied_or_timed_out_consent_leaves_the_grant_registry_empty() {
        for (case, answer) in [("denied", Some("denied")), ("unanswered", None)] {
            let daemon = daemon_with_short_consent();
            let holder = unattached(&daemon);
            let created = handle_session_create(
                &daemon,
                &holder,
                Id::Number(1),
                serde_json::json!({"mode": "freeform"}),
            );
            let session = created_session_id(&created);
            let holder_prompts = as_surface(&daemon, &holder);

            let newcomer = unattached(&daemon);
            let mut newcomer_frames = as_surface(&daemon, &newcomer);
            let answering =
                answer.map(|outcome| answer_consent(&daemon, &holder, holder_prompts, outcome));

            let started = std::time::Instant::now();
            let refused = handle_session_attach(
                &daemon,
                &newcomer,
                Id::Number(2),
                serde_json::json!({"session_id": session.to_string()}),
            )
            .await;
            if let Some(answering) = answering {
                answering.await.expect("the approver task must not panic");
            }

            let expected = if answer.is_some() {
                error_code::CONSENT_DENIED
            } else {
                error_code::CONSENT_TIMEOUT
            };
            assert!(refused.contains(&expected.to_string()), "{case}: {refused}");

            // The claim, read off the registry rather than off the answer.
            assert!(
                daemon.grants.held_by(newcomer.id).is_empty(),
                "{case}: a refused consent must mint nothing for the requester"
            );
            assert!(
                daemon.grants.is_empty(),
                "{case}: nor for anyone else — {} grants live",
                daemon.grants.len()
            );
            assert_eq!(
                daemon.consents.pending_count(),
                0,
                "{case}: and no waiter may be left in the registry"
            );
            assert!(
                !newcomer.may_receive(Some(&session)),
                "{case}: nor may the connection have been attached"
            );

            // AC-6: the timeout resolves inside the bounded window rather than
            // whenever, and the refusal is announced with its own reason.
            if answer.is_none() {
                assert!(
                    started.elapsed() < TEST_CONSENT_WINDOW * 8,
                    "{case}: the window must bound the wait, and it took {:?}",
                    started.elapsed()
                );
            }
            let refusals: Vec<Value> = std::iter::from_fn(|| newcomer_frames.try_recv().ok())
                .map(|frame| serde_json::from_str::<Value>(&frame).expect("a routed frame is JSON"))
                .filter(|frame| frame["params"]["event"] == "attach_refused")
                .collect();
            assert_eq!(
                refusals.len(),
                1,
                "{case}: the requester must be told how its request ended: {refusals:?}"
            );
            assert_eq!(
                refusals[0]["params"]["reason"],
                if answer.is_some() {
                    "consent_denied"
                } else {
                    "consent_timeout"
                },
                "{case}: with the reason that actually happened"
            );
            assert_eq!(refusals[0]["params"]["scope"], "attach");
        }
    }

    /// **BR-6, second arm.** With nothing attached to the target, the prompt is
    /// rendered by the requester itself — and by nobody else.
    ///
    /// This is the resume flow (AC-3): the user's last client is gone, so the
    /// only surface left is the one they just opened. It is sound *only*
    /// because the ancestry gate already refused every connection out of the
    /// daemon's process tree, which the neighbouring test pins.
    ///
    /// The bystander is the control. It is a live, handshaked connection
    /// attached to a session of its own, and it must see nothing: a routing
    /// rule that fell back to "tell everyone" would put a stranger's attach
    /// request in front of a user who has no standing to answer it.
    #[tokio::test]
    async fn with_nothing_attached_the_requester_renders_its_own_prompt() {
        let daemon = daemon_with_short_consent();
        // A session whose creator is *not* a registered surface: the sessions
        // outlived the client that made them, which is the resume shape.
        let departed = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &departed,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        let bystander = unattached(&daemon);
        let elsewhere = handle_session_create(
            &daemon,
            &bystander,
            Id::Number(2),
            serde_json::json!({"mode": "freeform"}),
        );
        assert!(elsewhere.contains("session_id"));
        let mut bystander_prompts = as_surface(&daemon, &bystander);

        let resuming = unattached(&daemon);
        let resuming_prompts = as_surface(&daemon, &resuming);
        let answering = answer_consent(&daemon, &resuming, resuming_prompts, "granted");

        let response = handle_session_attach(
            &daemon,
            &resuming,
            Id::Number(3),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        let prompt = answering.await.expect("the approver task must not panic");

        assert!(
            response.contains(&session.to_string()),
            "the resume flow must succeed after one consent step: {response}"
        );
        assert_eq!(
            prompt["session_id"].as_str(),
            Some(session.to_string().as_str())
        );
        assert_eq!(
            daemon.grants.held_by(resuming.id),
            vec![Grant::attach(resuming.id, session.clone())]
        );
        assert!(
            consent_prompts(&mut bystander_prompts).is_empty(),
            "a connection attached to some other session is not asked about this one"
        );
    }

    /// **BR-6.** Receiving a prompt is not standing to answer it.
    ///
    /// A `monitor` is the sharp case: REQ-568 gives it sight of every session's
    /// events, so a consent flow that let "whoever got the frame" answer would
    /// hand every observer the power to admit anyone to any session. It is
    /// refused, and — the second half, and the one a naive fix breaks — the
    /// prompt is **still pending** afterwards, so the refusal cannot be used to
    /// cancel somebody else's consent request.
    #[tokio::test]
    async fn a_connection_the_prompt_was_not_offered_to_cannot_answer_or_cancel_it() {
        let daemon = daemon_with_short_consent();
        let holder = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &holder,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        as_surface(&daemon, &holder);

        let requester = unattached(&daemon);
        let route = ConsentRoute::attached_to(requester.id, session.clone());
        let request_id = daemon.consents.next_request_id();
        let _rx = daemon.consents.register(request_id.clone(), route);

        let watcher = monitoring(&daemon);
        for (case, conn) in [
            ("a monitor", &watcher),
            ("the requester itself", &requester),
        ] {
            let refused = handle_attach_consent(
                &daemon,
                conn,
                Id::Number(2),
                serde_json::json!({
                    "request_id": request_id.to_string(),
                    "outcome": { "outcome": "granted" },
                }),
            );
            assert!(
                refused.contains(&error_code::NOT_ATTACHED.to_string()),
                "{case} must not be able to answer this prompt: {refused}"
            );
            assert_eq!(
                daemon.consents.pending_count(),
                1,
                "{case}: a refused answer must leave the request standing"
            );
            assert!(
                daemon.grants.is_empty(),
                "{case}: and must certainly mint nothing"
            );
        }

        // The rightful surface still decides it, which is what makes the
        // refusals above a gate rather than a broken flow.
        let answered = handle_attach_consent(
            &daemon,
            &holder,
            Id::Number(3),
            serde_json::json!({
                "request_id": request_id.to_string(),
                "outcome": { "outcome": "denied" },
            }),
        );
        assert!(answered.contains("\"resolved\":true"), "{answered}");
        assert_eq!(daemon.consents.pending_count(), 0);
    }

    /// **BR-4, the door handle.** A daemon descendant cannot *approve* a
    /// consent request either, even one it would otherwise qualify for.
    ///
    /// This is the hole a gate placed only on `session/attach` leaves open. A
    /// tool child may create its own session — `session/create` is deliberately
    /// ungated, since a child holding its own session reaches nobody else — so
    /// it is a connection with a non-empty attachment set, which is exactly
    /// what BR-6's first arm looks for in an approver. Since F2 that is not a
    /// hypothetical routing accident either: an excluded connection is now
    /// deliberately *registered* as a surface so its held session counts, which
    /// makes this refusal the thing that keeps the arm safe.
    ///
    /// Both verdicts that refuse are checked, and the request is asserted to be
    /// still pending afterwards — a refusal that consumed the waiter would let
    /// a daemon child cancel every consent prompt on the machine instead.
    #[tokio::test]
    async fn a_daemon_descendant_may_not_approve_a_consent_request_either() {
        let daemon = daemon_with_short_consent();
        let requester = unattached(&daemon);

        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            let child = conn_with_ancestry(&daemon, ancestry);
            // The child holds a session of its own — the standing that would
            // otherwise make it an eligible approver.
            let created = handle_session_create(
                &daemon,
                &child,
                Id::Number(1),
                serde_json::json!({"mode": "freeform"}),
            );
            let own = created_session_id(&created);
            assert!(
                child.may_receive(Some(&own)),
                "{ancestry:?}: the fixture is only meaningful if the child is attached"
            );
            // The route a stranger's attach to the child's session would take.
            let route = ConsentRoute::attached_to(requester.id, own.clone());
            assert!(
                route.renders_request(child.id, &child.attached()),
                "{ancestry:?}: and only if the routing rule would otherwise have asked it — \
                 this is the non-vacuity of the refusal below"
            );

            let request_id = daemon.consents.next_request_id();
            let _rx = daemon
                .consents
                .register(request_id.clone(), route.clone())
                .expect("under the per-connection cap");
            let refused = handle_attach_consent(
                &daemon,
                &child,
                Id::Number(2),
                serde_json::json!({
                    "request_id": request_id.to_string(),
                    "outcome": { "outcome": "granted" },
                }),
            );
            assert!(
                refused.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                "{ancestry:?}: a daemon child must not confer what it may not hold: {refused}"
            );
            assert!(
                daemon.grants.is_empty(),
                "{ancestry:?}: and must certainly mint nothing"
            );
            assert_eq!(
                daemon.consents.pending_count(),
                1,
                "{ancestry:?}: nor may its refusal cancel somebody else's request"
            );
            daemon.consents.forget(&request_id);
        }
    }

    /// **REQ-569 verify, F2.** A session held by a connection that may not
    /// *answer* a consent request is still a session that is **held** — so a
    /// stranger attaching to it never gets the self-render arm.
    ///
    /// The defect this pins was a fail-open dressed as a gate. Consent surfaces
    /// were registered only `if state.may_hold_session_access()`, so a
    /// connection whose ancestry came back `Descendant` or `Indeterminate` held
    /// its session *invisibly*: `anyone_attached_to` answered false, the attach
    /// took BR-6's second arm, and the requester rendered — and answered — its
    /// own prompt. An attacker attaching to somebody else's session obtained
    /// drive rights over it by approving itself, and the daemon logged it as the
    /// ordinary resume flow.
    ///
    /// `Indeterminate` is the cell that shows the shape is not merely a
    /// hardening of the descendant case: that verdict is what a *legitimate*
    /// client gets from a vanished pid or a platform with no peer-pid option, so
    /// the old code turned a lookup failure into a way through the door
    /// underneath it.
    ///
    /// Two assertions per verdict, and the second is the one that fails against
    /// the bug: the requester is refused, and **no prompt is rendered at the
    /// requester's own surface** — the self-render arm's only observable
    /// signature at this layer.
    #[tokio::test]
    async fn a_session_held_by_a_connection_that_cannot_answer_is_still_held() {
        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            let daemon = daemon_with_short_consent();

            // The holder: excluded from *answering*, but a genuine holder of a
            // session it created (`session/create` is ungated by design).
            let holder = conn_with_ancestry(&daemon, ancestry);
            let created = handle_session_create(
                &daemon,
                &holder,
                Id::Number(1),
                serde_json::json!({"mode": "freeform"}),
            );
            let session = created_session_id(&created);
            let mut holder_prompts = as_surface(&daemon, &holder);

            // The attacker: an ordinary connection the ancestry gate lets
            // through, with no standing over `session` whatsoever.
            let attacker = unattached(&daemon);
            let mut attacker_prompts = as_surface(&daemon, &attacker);

            let response = handle_session_attach(
                &daemon,
                &attacker,
                Id::Number(2),
                serde_json::json!({"session_id": session.to_string()}),
            )
            .await;

            assert!(
                response.contains(&error_code::CONSENT_TIMEOUT.to_string()),
                "{ancestry:?}: routing must fail closed on the held session, not \
                 fall through to the requester: {response}"
            );
            assert!(
                consent_prompts(&mut attacker_prompts).is_empty(),
                "{ancestry:?}: the requester must never be handed its own prompt \
                 for a session somebody else holds — that is the self-approval"
            );
            assert!(
                consent_prompts(&mut holder_prompts).is_empty(),
                "{ancestry:?}: nor is the frame delivered to a connection that \
                 may not answer it"
            );
            assert!(
                daemon.grants.is_empty(),
                "{ancestry:?}: and nothing was minted"
            );
        }
    }

    /// **REQ-569 verify, F4.** A connection may not stack up consent prompts
    /// without limit.
    ///
    /// Nothing else on this seam rate-limits anything, and the prompt in
    /// question is a security dialog whose "yes" mints a credential — so an
    /// unbounded stream of them is consent fatigue aimed at a user, plus an
    /// unbounded pile of waiters and awaiting tasks in the daemon.
    ///
    /// The cap is asserted through the real handler with the requester's slots
    /// already full, so what is under test is the refusal `session/attach`
    /// issues rather than the registry method beneath it (which
    /// `consent::tests` covers directly). The prompt count is the second half:
    /// a cap that refused the *caller* but still published the prompt would
    /// leave the user-facing half of the problem exactly as it was.
    #[tokio::test]
    async fn a_connection_at_its_consent_cap_is_refused_without_raising_another_prompt() {
        let daemon = daemon_with_short_consent();
        let holder = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &holder,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let mut holder_prompts = as_surface(&daemon, &holder);

        let flooder = unattached(&daemon);
        let mut _held = Vec::new();
        for _ in 0..MAX_PENDING_CONSENTS_PER_CONNECTION {
            let request_id = daemon.consents.next_request_id();
            _held.push(
                daemon
                    .consents
                    .register(
                        request_id,
                        ConsentRoute::attached_to(flooder.id, session.clone()),
                    )
                    .expect("the fixture fills the cap exactly"),
            );
        }
        // Drain the prompts the fixture did not publish, so the count below is
        // about this call alone.
        assert!(consent_prompts(&mut holder_prompts).is_empty());

        let refused = handle_session_attach(
            &daemon,
            &flooder,
            Id::Number(2),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;

        assert!(
            refused.contains(&error_code::NOT_GRANTED.to_string()),
            "a connection past its cap is refused, fail-closed: {refused}"
        );
        assert!(
            consent_prompts(&mut holder_prompts).is_empty(),
            "and no further prompt is put in front of a user"
        );
        assert_eq!(
            daemon.consents.in_flight_for(flooder.id),
            MAX_PENDING_CONSENTS_PER_CONNECTION,
            "the refused request registered no waiter of its own"
        );

        // Non-vacuity: another connection, at zero, is still served.
        let honest = unattached(&daemon);
        let answering = answer_consent(&daemon, &holder, holder_prompts, "denied");
        let served = handle_session_attach(
            &daemon,
            &honest,
            Id::Number(3),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        answering.await.expect("the approver task must not panic");
        assert!(
            served.contains(&error_code::CONSENT_DENIED.to_string()),
            "one connection's flood must not refuse another's prompt: {served}"
        );
    }

    /// **REQ-569 verify, F9.** A `session_id` longer than one this daemon could
    /// have minted is refused at the parse boundary.
    ///
    /// A granted consent stores the id verbatim as a grant-registry key, and the
    /// wire bound on it is otherwise the ~4 MiB frame cap. The bound is
    /// asserted alongside a plausible-length id that draws the *ordinary*
    /// refusal, because the claim is "this is a well-formedness gate", not "this
    /// is a second access check": a fabricated but plausible id must still walk
    /// the same path a real one does (BR-8, no existence oracle).
    #[tokio::test]
    async fn a_session_id_longer_than_the_daemon_could_mint_is_refused_before_anything_else() {
        let daemon = daemon_with_short_consent();
        let caller = unattached(&daemon);
        let mut prompts = as_surface(&daemon, &caller);

        let refused = handle_session_attach(
            &daemon,
            &caller,
            Id::Number(1),
            serde_json::json!({"session_id": format!("sess-{}", "a".repeat(64 * 1024))}),
        )
        .await;
        assert!(
            refused.contains(&error_code::INVALID_PARAMS.to_string()),
            "an id the daemon could never have minted is refused: {refused}"
        );
        assert!(
            consent_prompts(&mut prompts).is_empty(),
            "and costs no user a prompt"
        );
        assert!(daemon.consents.pending_count() == 0 && daemon.grants.is_empty());

        // Non-vacuity: a fabricated id of a *plausible* length is not short-cut
        // here — it goes to the consent flow exactly as a real one would.
        let plausible = handle_session_attach(
            &daemon,
            &caller,
            Id::Number(2),
            serde_json::json!({"session_id": "sess-0123456789abcdefghjkmnpqrs"}),
        )
        .await;
        assert!(
            plausible.contains(&error_code::CONSENT_TIMEOUT.to_string()),
            "a plausible id draws the ordinary refusal, whether or not it names \
             anything: {plausible}"
        );
    }

    /// **REQ-569 verify, F6.** A minted grant is announced on the bus, daemon
    /// wide, and a self-approved one says so.
    ///
    /// The self-approval residual's only record used to be a line on the
    /// daemon's stderr: read on startup failure and almost never otherwise,
    /// truncated by the CLI's spawn path, and writable by the same uid the whole
    /// perimeter is drawn against — so the process that self-approved could
    /// erase the evidence of having done it. The log line stays; this is the
    /// half that is in-perimeter and unsuppressable.
    ///
    /// The absent `session_id` is asserted rather than assumed: it is what makes
    /// REQ-568's delivery rule broadcast the frame to *every* handshaked
    /// connection instead of to the attachees of the session that was just
    /// opened up — an announcement only the beneficiary can see is not an
    /// announcement.
    #[tokio::test]
    async fn a_self_approved_grant_is_announced_daemon_wide_and_named_as_self_approved() {
        let daemon = daemon_with_short_consent();
        // The resume shape: a session whose creator is no longer a surface, so
        // nothing is attached and the requester renders its own prompt.
        let departed = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &departed,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        let mut sub = daemon.events.subscribe(16);
        let resuming = unattached(&daemon);
        let prompts = as_surface(&daemon, &resuming);
        let answering = answer_consent(&daemon, &resuming, prompts, "granted");

        let response = handle_session_attach(
            &daemon,
            &resuming,
            Id::Number(2),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        answering.await.expect("the approver task must not panic");
        assert!(response.contains(&session.to_string()), "{response}");

        let announced: Vec<EventEnvelope> = std::iter::from_fn(|| sub.try_recv())
            .filter(|env| matches!(env.event, Event::SessionGrantMinted(_)))
            .collect();
        assert_eq!(
            announced.len(),
            1,
            "exactly one grant was minted, so exactly one announcement"
        );
        let Event::SessionGrantMinted(ref minted) = announced[0].event else {
            unreachable!("filtered above")
        };
        assert!(
            minted.self_approved,
            "the requester answered its own prompt; the event has to say so"
        );
        assert_eq!(minted.scope, ConsentScope::Attach);
        assert_eq!(minted.requester, TEST_REQUESTER);
        assert!(
            announced[0].session_id.is_none(),
            "a grant announcement is daemon-scoped, so every handshaked \
             connection is told: {:?}",
            announced[0].session_id
        );
    }

    /// **REQ-569 verify, F6, the other half.** A grant a second party approved
    /// is announced too — and is *not* named as self-approved.
    ///
    /// Without this the flag could be a constant `true` and the test above would
    /// still pass, which would make the one field a reader acts on meaningless.
    #[tokio::test]
    async fn a_peer_approved_grant_is_announced_without_the_self_approval_flag() {
        let daemon = daemon_with_short_consent();
        let holder = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &holder,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let holder_prompts = as_surface(&daemon, &holder);

        let mut sub = daemon.events.subscribe(16);
        let newcomer = unattached(&daemon);
        let answering = answer_consent(&daemon, &holder, holder_prompts, "granted");
        let response = handle_session_attach(
            &daemon,
            &newcomer,
            Id::Number(2),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        answering.await.expect("the approver task must not panic");
        assert!(response.contains(&session.to_string()), "{response}");

        let announced: Vec<EventEnvelope> = std::iter::from_fn(|| sub.try_recv())
            .filter(|env| matches!(env.event, Event::SessionGrantMinted(_)))
            .collect();
        assert_eq!(announced.len(), 1);
        let Event::SessionGrantMinted(ref minted) = announced[0].event else {
            unreachable!("filtered above")
        };
        assert!(
            !minted.self_approved,
            "a peer approved this one — flagging it as self-approved would make \
             the field noise a reader learns to skip"
        );
    }

    /// An answer to a request id nobody is waiting on is acknowledged, not
    /// refused — and says it decided nothing.
    ///
    /// A refusal would answer "is some consent outstanding right now?" for any
    /// connected peer, which is an oracle this seam does not hand out. The
    /// `resolved: false` is what stops a client reporting success for an answer
    /// that arrived after the window closed.
    #[test]
    fn an_answer_to_an_unknown_consent_request_decides_nothing_and_says_so() {
        let daemon = Daemon::new();
        let stranger = unattached(&daemon);
        let response = handle_attach_consent(
            &daemon,
            &stranger,
            Id::Number(1),
            serde_json::json!({
                "request_id": "consent-404",
                "outcome": { "outcome": "granted" },
            }),
        );
        assert!(response.contains("\"resolved\":false"), "{response}");
        assert!(daemon.grants.is_empty());
    }

    /// The descriptor a consent prompt carries is bounded and cannot forge a
    /// second line, however the peer spells its name.
    ///
    /// It goes to a *user's screen* by way of another client, and every
    /// character in it was chosen by an unprivileged same-UID peer — so this is
    /// REQ-568's monitor-log treatment applied one seam further out, where the
    /// consequence of getting it wrong is a forged prompt rather than a forged
    /// log line.
    #[test]
    fn the_requester_descriptor_is_bounded_and_carries_no_control_characters() {
        let hostile = HandshakeParams {
            client_kind: teton_protocol::ClientKind::Cli,
            client_name: format!(
                "teton\n\u{1b}[31mDANGER: this client is trusted\u{1b}[0m{}",
                "A".repeat(10_000)
            ),
            client_version: "0.1.0".to_owned(),
            protocol_min: teton_protocol::PROTOCOL_VERSION_MIN,
            protocol_max: teton_protocol::PROTOCOL_VERSION_MAX,
            monitor: false,
        };
        let descriptor = requester_descriptor(&hostile);
        assert!(
            !descriptor.contains('\n') && !descriptor.contains('\u{1b}'),
            "a peer must not be able to put a newline or an escape sequence in a \
             prompt a user reads: {descriptor:?}"
        );
        assert!(
            descriptor.chars().count() < REQUESTER_BUDGET + 32,
            "the descriptor must stay renderable in one line: {} chars",
            descriptor.chars().count()
        );
        // Non-vacuity: the ordinary name survives intact.
        let ordinary = HandshakeParams {
            client_name: "teton".to_owned(),
            ..hostile
        };
        assert!(
            requester_descriptor(&ordinary).contains("teton"),
            "an honest client is still named"
        );
    }

    /// **REQ-569 verify, F8.** The descriptor also drops the characters that
    /// reorder text, not just the thirty-three `char::is_control` answers to.
    ///
    /// `is_control` is Cc only, so every Trojan-Source character passed straight
    /// through into a security prompt a user reads and into the daemon log. A
    /// name carrying `U+202E` can make the prompt read as though a different
    /// client were asking — which is precisely the fact the user is being asked
    /// to decide on — and `U+2028` opens a second visual line without ever being
    /// a control character.
    ///
    /// Asserted on the *rendered* string rather than on the predicate, so it
    /// covers the seam a caller actually reaches, and both publication points
    /// are checked: the prompt and the self-approval log line.
    #[test]
    fn a_bidi_override_in_a_client_name_never_reaches_a_prompt_or_the_log() {
        // "teton" then RLO, then text that a renderer would draw right-to-left,
        // plus an isolate, a zero-width joiner and a paragraph separator.
        const HOSTILE_NAME: &str =
            "teton\u{202E}drowssap-ruoy-dnes\u{2069}\u{200D}\u{2028}trusted\u{FEFF}";
        let hostile = HandshakeParams {
            client_kind: teton_protocol::ClientKind::Cli,
            client_name: HOSTILE_NAME.to_owned(),
            client_version: "0.1.0".to_owned(),
            protocol_min: teton_protocol::PROTOCOL_VERSION_MIN,
            protocol_max: teton_protocol::PROTOCOL_VERSION_MAX,
            monitor: false,
        };

        let descriptor = requester_descriptor(&hostile);
        let line = self_approval_line(&descriptor);
        for hostile_char in ['\u{202E}', '\u{2069}', '\u{200D}', '\u{2028}', '\u{FEFF}'] {
            assert!(
                !descriptor.contains(hostile_char),
                "{hostile_char:?} reached a prompt a user reads: {descriptor:?}"
            );
            assert!(
                !line.contains(hostile_char),
                "{hostile_char:?} reached the daemon log: {line:?}"
            );
        }
        // Non-vacuity, and the reason this is a filter rather than an
        // allow-list: the legible text survives, including non-ASCII.
        assert!(descriptor.contains("teton"), "{descriptor:?}");
        let named = HandshakeParams {
            client_name: "téton-クライアント".to_owned(),
            ..hostile
        };
        assert!(
            requester_descriptor(&named).contains("téton-クライアント"),
            "an honest non-ASCII name must survive intact"
        );
    }

    /// REQ-569 BR-4 / ADR-A: a connection out of the daemon's own process tree
    /// is refused attach, before the session registry is touched.
    ///
    /// Both ancestry verdicts that refuse are checked, and both are checked
    /// against a session that genuinely exists *and* one that does not — four
    /// cells, all `ATTACH_FORBIDDEN`. The uniformity is the point twice over:
    /// the refusal must not leak whether the session exists, and it must not
    /// leak whether the daemon was sure about the ancestry.
    ///
    /// `NOT_GRANTED` is asserted absent in every cell, which is what pins the
    /// *ordering*. A descendant holds no grant either, so a gate that ran the
    /// grant check first would refuse it too — and the test would pass for the
    /// wrong reason.
    ///
    /// Since TASK-108 that ordering is worth more than a code. The
    /// `NOT_GRANTED` branch now **raises a consent prompt**, so a gate in the
    /// wrong order would not merely mislabel a refusal: it would put the
    /// daemon's own tool children in front of a user, asking to be let into
    /// their session — and a user who clicks yes on a prompt they did not
    /// expect would hand a grant to exactly the process BR-4 exists to
    /// exclude. So the assertion is that **no prompt is published for a
    /// descendant at all**, and the ordinary connection's prompt in the same
    /// test is the positive control: it proves the fixture *can* observe a
    /// prompt, so the descendant's silence is the gate rather than the wiring.
    #[tokio::test]
    async fn a_daemon_descendant_is_refused_attach_before_any_session_lookup_or_prompt() {
        let daemon = daemon_with_short_consent();
        let creator = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let mut creator_prompts = as_surface(&daemon, &creator);

        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            let child = conn_with_ancestry(&daemon, ancestry);
            let mut child_prompts = as_surface(&daemon, &child);
            for target in [session.to_string(), "sess-nonexistent".to_owned()] {
                let refused = handle_session_attach(
                    &daemon,
                    &child,
                    Id::Number(2),
                    serde_json::json!({"session_id": target}),
                )
                .await;
                assert!(
                    refused.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                    "{ancestry:?} attaching `{target}` must be forbidden: {refused}"
                );
                assert!(
                    !refused.contains(&error_code::NOT_GRANTED.to_string()),
                    "{ancestry:?}: the ancestry gate must answer first, not the grant gate: \
                     {refused}"
                );
                assert!(
                    !refused.contains(&error_code::UNKNOWN_SESSION.to_string()),
                    "{ancestry:?}: the refusal must not say whether the session exists: {refused}"
                );
                assert!(
                    !child.may_receive(Some(&session)),
                    "{ancestry:?}: a forbidden attach must grant nothing"
                );
            }

            // The claim TASK-108 adds: nobody was asked, so nobody could have
            // said yes. Checked at both surfaces — the session's holder, who
            // would render arm 1's prompt, and the child itself, who would
            // render arm 2's.
            assert!(
                consent_prompts(&mut creator_prompts).is_empty(),
                "{ancestry:?}: a daemon child must not be able to put a consent prompt \
                 in front of the session's own user"
            );
            assert!(
                consent_prompts(&mut child_prompts).is_empty(),
                "{ancestry:?}: nor render one for itself"
            );

            // Not even a grant lets it through: the ancestry gate is asked
            // first and is terminal, so minting one for a descendant — which
            // the consent path must never do — still changes nothing here.
            daemon
                .grants
                .grant(Grant::attach(child.id, session.clone()));
            let still_refused = handle_session_attach(
                &daemon,
                &child,
                Id::Number(3),
                serde_json::json!({"session_id": session.to_string()}),
            )
            .await;
            assert!(
                still_refused.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                "{ancestry:?}: a grant must not override the ancestry gate: {still_refused}"
            );
            daemon.grants.release(child.id);
        }

        // **The positive control**, in the same test and on the same surface:
        // an ordinary connection asking for the same session *does* raise a
        // prompt there. Without it, every assertion above would also pass on a
        // daemon that had simply stopped publishing consent prompts.
        let ordinary = unattached(&daemon);
        let refused = handle_session_attach(
            &daemon,
            &ordinary,
            Id::Number(4),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        assert!(
            refused.contains(&error_code::CONSENT_TIMEOUT.to_string()),
            "the control must reach the consent path: {refused}"
        );
        let prompts = consent_prompts(&mut creator_prompts);
        assert_eq!(
            prompts.len(),
            1,
            "the control must produce exactly the prompt the descendants did not: {prompts:?}"
        );
        assert_eq!(prompts[0]["scope"], "attach");
    }

    /// REQ-569 BR-4: the operator can tell "refused because it is a descendant"
    /// from "refused because we could not tell", even though the connection
    /// cannot.
    ///
    /// The two verdicts cost a connection exactly the same thing, which is the
    /// fail-closed policy — but they have opposite remedies for whoever runs
    /// the daemon (exclude the child, versus fix a peer-pid lookup that has
    /// stopped working), and a log that collapsed them would present a broken
    /// ancestry lookup as the gate working perfectly.
    #[test]
    fn an_indeterminate_ancestry_is_refused_like_a_descendant_but_logged_apart() {
        let descendant = ancestry_refusal_line(Ancestry::Descendant, "session/attach");
        let unknown = ancestry_refusal_line(Ancestry::Indeterminate, "session/attach");
        assert_ne!(
            descendant, unknown,
            "the two refusal reasons must be distinguishable in the log"
        );
        assert!(descendant.contains("descends from"), "{descendant}");
        assert!(unknown.contains("could not be determined"), "{unknown}");
        // Content-free, like every other refusal on this seam.
        assert!(!descendant.contains("sess-"), "{descendant}");
        assert!(!unknown.contains("sess-"), "{unknown}");

        // And the policy itself: only `NotDescendant` opens the gate.
        let daemon = Daemon::new();
        assert!(!conn_with_ancestry(&daemon, Ancestry::Descendant).may_hold_session_access());
        assert!(!conn_with_ancestry(&daemon, Ancestry::Indeterminate).may_hold_session_access());
        assert!(conn_with_ancestry(&daemon, Ancestry::NotDescendant).may_hold_session_access());
    }

    /// REQ-569 ADR-A: which process tree a daemon excludes is a property it
    /// carries, and the production constructors always carry their own.
    ///
    /// The one thing that must never happen by accident is a shipped daemon
    /// classifying its own children as strangers. `main` builds through
    /// [`Daemon::with_lifetime`], so this pins the arm `main` gets — and pins
    /// that [`DaemonProcess::Embedded`] is reachable only by a fixture saying
    /// so out loud.
    #[test]
    fn the_production_constructors_own_their_process() {
        let me = i32::try_from(std::process::id()).unwrap();
        let events = Arc::new(EventBus::new());
        let runtime = Arc::new(DaemonRuntime::minimal());
        let production = Daemon::with_runtime(Arc::clone(&events), Arc::clone(&runtime));
        assert_eq!(production.process, DaemonProcess::Own(me));

        let lifetime = Arc::new(LifetimeSupervisor::new(
            ShutdownPolicy::Never,
            PolicySource::Default,
            Arc::clone(&events),
        ));
        let production = Daemon::with_lifetime(events, runtime, lifetime);
        assert_eq!(production.process, DaemonProcess::Own(me));

        // The fixture constructor states the embedded topology, and it takes an
        // explicit call to state anything else.
        assert_eq!(Daemon::new().process, DaemonProcess::Embedded);
        assert_eq!(
            Daemon::new()
                .with_daemon_process(DaemonProcess::Own(me))
                .process,
            DaemonProcess::Own(me)
        );
    }

    /// REQ-569 BR-4: the daemon's own classification of a peer, over the three
    /// inputs it can get.
    ///
    /// The `Own` + absent-pid row is the one worth writing down: a platform that
    /// reports no peer pid must land on [`Ancestry::Indeterminate`], which the
    /// gate refuses. A `None` pid quietly reading as "not a descendant" would
    /// make the whole control evaporate on any arm whose kernel will not answer
    /// — the fail-open TASK-103 built the three-valued answer to prevent.
    #[test]
    fn a_peer_with_no_pid_is_unanswerable_not_admitted() {
        let me = i32::try_from(std::process::id()).unwrap();
        assert_eq!(
            DaemonProcess::Own(me).ancestry_of(None),
            Ancestry::Indeterminate
        );
        // This process is trivially a descendant of itself, which is the
        // in-process-harness topology and the reason `Embedded` exists.
        assert_eq!(
            DaemonProcess::Own(me).ancestry_of(Some(me)),
            Ancestry::Descendant
        );
        // pid 1 never descends from us.
        assert_eq!(
            DaemonProcess::Own(me).ancestry_of(Some(1)),
            Ancestry::NotDescendant
        );
        // An embedded daemon owns no process tree, so nothing came out of one.
        assert_eq!(
            DaemonProcess::Embedded.ancestry_of(Some(me)),
            Ancestry::NotDescendant
        );
        assert_eq!(
            DaemonProcess::Embedded.ancestry_of(None),
            Ancestry::NotDescendant
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

    /// REQ-569 BR-10, all eight cells of the redaction rule: visibility crossed
    /// with a title that is present or not and a `cwd` that is present or not.
    ///
    /// The cells where the field was already absent matter as much as the ones
    /// where it is dropped: a reduction is not allowed to *invent* a value
    /// (an empty string standing in for "redacted") on the way through, because
    /// a client cannot tell that apart from a session that genuinely has no
    /// title. `Option::is_none` is the only state either field ever reaches
    /// here, in both directions.
    ///
    /// Identity on the visible side is asserted as a whole-struct equality
    /// rather than field by field, so a field added to `SessionSummary` later
    /// cannot be silently dropped from an attached connection's view.
    #[test]
    fn reduce_for_keeps_ids_and_drops_only_content_when_not_visible() {
        use teton_protocol::{Phase, SessionMode};

        let full = SessionSummary {
            session_id: SessionId::from("sess-x"),
            mode: SessionMode::Structured,
            phase: Some(Phase::Spec),
            title: Some("refactor the payroll importer".to_owned()),
            cwd: Some(std::path::PathBuf::from("/Users/someone/work/payroll")),
        };

        // (case, title, cwd, visible)
        for (case, title, cwd, visible) in [
            ("both present, visible", true, true, true),
            ("both present, not visible", true, true, false),
            ("title only, visible", true, false, true),
            ("title only, not visible", true, false, false),
            ("cwd only, visible", false, true, true),
            ("cwd only, not visible", false, true, false),
            ("neither, visible", false, false, true),
            ("neither, not visible", false, false, false),
        ] {
            let input = SessionSummary {
                title: title.then(|| full.title.clone().unwrap()),
                cwd: cwd.then(|| full.cwd.clone().unwrap()),
                ..full.clone()
            };
            let reduced = reduce_for(input.clone(), visible);

            // Always, in every cell: the listing itself is untouched.
            assert_eq!(reduced.session_id, full.session_id, "{case}");
            assert_eq!(reduced.mode, full.mode, "{case}");
            assert_eq!(reduced.phase, full.phase, "{case}");

            if visible {
                assert_eq!(reduced, input, "{case}: a visible session is not reduced");
            } else {
                assert!(reduced.title.is_none(), "{case}: title must be dropped");
                assert!(reduced.cwd.is_none(), "{case}: cwd must be dropped");
            }
        }
    }

    /// REQ-569 BR-10 at the dispatch seam: the reduction is *wired*, and it is
    /// wired to the same predicate that gates event delivery.
    ///
    /// The table test above pins what reduction means; this pins that
    /// `session/list` performs it, and that the connection which created the
    /// session (auto-attached, REQ-568) is not reduced. Asserted on the response
    /// text because the omission being claimed is a fact about the JSON, not
    /// about the struct — `serde` is what turns `None` into an absent key.
    #[test]
    fn session_list_reduces_only_for_connections_that_may_not_see_the_session() {
        let daemon = Daemon::new();
        let creator = unattached(&daemon);
        let jail = std::env::temp_dir();
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform", "cwd": jail}),
        );
        // The id comes out of the response rather than being assumed: session
        // ids are the daemon's to choose, and a fixture that spells one out
        // tests the naming scheme instead of the redaction rule.
        let session = daemon.sessions.list()[0].session_id.clone();
        assert!(
            created.contains(&session.0),
            "the create response names the session it made: {created}"
        );
        assert!(daemon
            .sessions
            .set_title(&session, "the user's own words, echoed back"));

        let list = |conn: &ConnState| {
            dispatch(
                &daemon,
                conn,
                Id::Number(2),
                SessionListParams::METHOD,
                Value::Null,
            )
            .unwrap()
        };

        let onlooker = list(&unattached(&daemon));
        assert!(
            onlooker.contains(&session.0),
            "the row itself stays — BR-10 reduces the payload, not the listing: {onlooker}"
        );
        assert!(
            !onlooker.contains("title") && !onlooker.contains("own words"),
            "an unattached connection must be shown no title: {onlooker}"
        );
        assert!(
            !onlooker.contains("cwd") && !onlooker.contains(&jail.display().to_string()),
            "an unattached connection must be shown no cwd: {onlooker}"
        );

        let owner = list(&creator);
        assert!(
            owner.contains("own words") && owner.contains("cwd"),
            "the creator is attached to what it made and sees it whole: {owner}"
        );

        // And the monitor, whose sight comes from the same predicate rather than
        // from a second rule this handler would have had to remember.
        let monitor = list(&monitoring(&daemon));
        assert!(
            monitor.contains("own words"),
            "a monitor sees every session whole: {monitor}"
        );
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
            ..params.clone()
        });
        assert!(
            !forged.contains('\n'),
            "a client-supplied name must not break the line: {forged}"
        );

        // F4: `client_name` is bounded only by MAX_FRAME on the wire, so the log
        // line must bound it itself. A pathologically long name is truncated to
        // a fixed budget, so the emitted line stays short rather than flooding
        // the daemon log with megabytes per handshake.
        let flood = monitor_declaration_line(&HandshakeParams {
            client_name: "n".repeat(100_000),
            ..params
        });
        assert!(
            flood.len() < 512,
            "an over-long client name must be truncated, not logged whole: {} bytes",
            flood.len()
        );
    }

    /// REQ-569 BR-6 / TASK-109: the self-approval line names what it is, and a
    /// client that chose its own name cannot forge a second log line through it.
    ///
    /// The wording is a contract, not decoration:
    /// `attach_authorization::a_consent_the_requester_granted_itself_is_named_
    /// as_such_in_the_daemon_log` greps a spawned daemon's stderr for the same
    /// phrase, so a reworded sentence fails there rather than quietly ending the
    /// only visibility this residual has.
    #[test]
    fn a_self_approved_consent_is_named_as_such_and_cannot_forge_a_log_line() {
        let line = self_approval_line("Cli client \"teton-cli\"");
        assert!(
            line.contains("approved its own attach consent"),
            "the line must say what happened, not merely that something did: {line}"
        );
        assert!(line.contains("teton-cli"), "{line}");

        // The descriptor reaches here from a same-UID peer's chosen name. A
        // newline in it would write a second line under the daemon's prefix.
        let forged =
            self_approval_line("Cli client \"innocent\ntetond: listening on /tmp/other.sock\"");
        assert!(
            !forged.contains('\n'),
            "a client-supplied descriptor must not break the line: {forged}"
        );

        // And it is bounded, for `monitor_declaration_line`'s reason: the field
        // is capped upstream, and this function must not be where an
        // uncapped one turns into a flooded log.
        let flood = self_approval_line(&"n".repeat(100_000));
        assert!(
            flood.len() < 512,
            "an over-long descriptor must be truncated, not logged whole: {} bytes",
            flood.len()
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
        let conn = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        // Attached and live: clears, idempotently, and says how much went.
        let cleared = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
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
        let stranger = unattached(&daemon);
        for target in [session.to_string(), "sess-nonexistent".to_owned()] {
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
        conn.attach(SessionId::from("sess-nonexistent"));
        let ghost = dispatch(
            &daemon,
            &conn,
            Id::Number(4),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": "sess-nonexistent"}),
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
        let creator = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &creator,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        let prompt = serde_json::json!({
            "session_id": session.to_string(),
            "prompt": [{"type": "text", "text": "what is in this session?"}],
        });
        let (tx, mut rx) = mpsc::channel::<String>(4);

        let stranger = unattached(&daemon);
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
        let owner = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &owner,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        let monitor = monitoring(&daemon);
        assert!(
            monitor.may_receive(Some(&session)),
            "a monitor sees every session's events"
        );

        let refused = dispatch(
            &daemon,
            &monitor,
            Id::Number(2),
            SessionClearParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "watching a session is not driving it: {refused}"
        );
    }

    /// REQ-568 BR-4, the prompt gate specifically: a monitor that never attached
    /// cannot *prompt* a session it only watches.
    ///
    /// `session/prompt` bypasses `dispatch`, so its gate is in `spawn_prompt_turn`
    /// and not covered by the `session/clear` monitor test above. The mutation
    /// this pins is reading that gate off `may_receive` instead of `may_drive`:
    /// a monitor's `may_receive` of this session is `true`, so the swap would
    /// spawn a turn here — the handle would be `Some` and no refusal queued —
    /// handing a passive observer a prompt against every session it can see.
    #[tokio::test]
    async fn a_monitor_cannot_prompt_a_session_it_only_watches() {
        let daemon = Arc::new(Daemon::new());
        let owner = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &owner,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        let monitor = monitoring(&daemon);
        assert!(
            monitor.may_receive(Some(&session)),
            "a monitor sees the session's events — the receive side is not the gate"
        );

        let prompt = serde_json::json!({
            "session_id": session.to_string(),
            "prompt": [{"type": "text", "text": "drive this session"}],
        });
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let handle = spawn_prompt_turn(&daemon, &monitor, Id::Number(2), prompt, &tx, None);
        assert!(handle.is_none(), "a monitor's prompt must spawn no turn");
        let refused = rx.try_recv().expect("a refusal is queued for the monitor");
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "watching a session is not driving it: {refused}"
        );
    }

    // ------------------------------------------------------------------
    // REQ-569 BR-9 / AC-9: `permission/respond` and its owning session
    // ------------------------------------------------------------------

    /// Raise a **real** permission prompt in `session` — through the daemon's own
    /// event bus and its own pending registry, the two objects the handler under
    /// test reads — and return the `request_id` it published plus the handle the
    /// blocked tool call is waiting on.
    ///
    /// A gate constructed here rather than a turn driven end-to-end because a
    /// turn needs a provider that emits a tool call, and none of that is what
    /// these tests are about: what must be genuine is the *waiter*, its recorded
    /// owner, and the registry the handler consults — and this is the production
    /// wiring for all three (`runtime.rs` builds a session's gate over exactly
    /// this `Arc<PendingPermissions>`).
    async fn raise_a_prompt(
        daemon: &Arc<Daemon>,
        session: &SessionId,
    ) -> (RequestId, JoinHandle<PermissionDecision>) {
        let gate = PermissionGate::new(
            session.clone(),
            PermissionConfig::with_default(PermissionPolicy::Ask),
            Arc::clone(&daemon.events),
            Arc::clone(daemon.runtime.pending()),
        );
        // Subscribed before the prompt is published, or the event could be
        // raised into an empty bus and the read below would hang.
        let mut sub = daemon.events.subscribe(16);
        let decision = tokio::spawn(async move { gate.authorize("shell", None).await });
        loop {
            let envelope = sub.recv().await.expect("the bus outlives this test");
            if let Event::PermissionRequest(request) = envelope.event {
                assert_eq!(
                    envelope.session_id.as_ref(),
                    Some(session),
                    "the prompt must be scoped to the session that raised it"
                );
                return (request.request_id, decision);
            }
        }
    }

    /// A `permission/respond` frame answering `request_id` with `allow_once`.
    fn answer_allowing(request_id: &RequestId) -> Value {
        serde_json::json!({
            "request_id": request_id.to_string(),
            "outcome": {"outcome": "selected", "option_id": "allow_once"},
        })
    }

    /// REQ-569 BR-9/AC-9, and the whole reason this gate exists (LESSON-502): a
    /// `monitor` receives every session's `permission_request` and may answer
    /// none of them.
    ///
    /// The three claims are asserted together on one prompt because they only
    /// mean anything together:
    ///
    /// 1. the monitor **can see** the prompt (`may_receive` is `true`) — without
    ///    this the refusal below could be trivially true of a connection that was
    ///    never shown anything;
    /// 2. its answer is refused `NOT_ATTACHED`;
    /// 3. the waiter is **still pending** afterwards. A refusal that consumed the
    ///    prompt would deny the tool call of a user who was never asked — a
    ///    stranger could silence any session at will — so "refused" has to mean
    ///    "left alone", not "resolved unfavourably".
    ///
    /// This is the test the mutation check aims at: reading the gate off
    /// `may_receive` instead of `may_drive` makes claim (1) grant claim (2)'s
    /// opposite, and only this test notices.
    #[tokio::test]
    async fn a_monitor_may_see_a_permission_prompt_and_may_not_answer_it() {
        let daemon = Arc::new(Daemon::new());
        let owner = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &owner,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let (request_id, decision) = raise_a_prompt(&daemon, &session).await;

        let monitor = monitoring(&daemon);
        assert!(
            monitor.may_receive(Some(&session)),
            "a monitor receives this very prompt — that is what makes answering \
             it the bug this gate closes"
        );

        let refused = dispatch(
            &daemon,
            &monitor,
            Id::Number(2),
            PermissionRespondParams::METHOD,
            answer_allowing(&request_id),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "seeing a prompt is not answering it: {refused}"
        );
        assert_eq!(
            daemon.runtime.pending().pending_count(),
            1,
            "the refused answer must leave the prompt standing for its rightful \
             answerer, not consume it"
        );
        assert_eq!(
            daemon.runtime.pending().owner_of(&request_id),
            Some(session.clone()),
            "and the prompt must still belong to the session that raised it"
        );

        // The rightful answerer — the session's creator, attached by creation —
        // is untouched: the happy path still resolves the tool call.
        let accepted = dispatch(
            &daemon,
            &owner,
            Id::Number(3),
            PermissionRespondParams::METHOD,
            answer_allowing(&request_id),
        )
        .unwrap();
        assert!(
            !accepted.contains("error"),
            "the attached connection's own answer must be accepted: {accepted}"
        );
        assert_eq!(
            decision.await.unwrap(),
            PermissionDecision::Allowed,
            "the waiting tool call must receive the answer the user actually gave"
        );
        assert_eq!(daemon.runtime.pending().pending_count(), 0);
    }

    /// A request id with **no waiter** keeps its pre-existing behaviour for
    /// every connection: acknowledged, never refused.
    ///
    /// The gate only has an opinion when a prompt is outstanding. If a
    /// nonexistent id drew `NOT_ATTACHED` while a live one drew it too, the two
    /// answers would be indistinguishable — but a *duplicate* or late reply from
    /// the rightful client would start failing, and worse, the pair
    /// (`ok` vs `NOT_ATTACHED`) would become an oracle telling a stranger which
    /// request ids are currently pending somewhere on the machine.
    #[tokio::test]
    async fn an_unknown_request_id_is_acknowledged_rather_than_refused() {
        let daemon = Arc::new(Daemon::new());
        for conn in [unattached(&daemon), monitoring(&daemon)] {
            let response = dispatch(
                &daemon,
                &conn,
                Id::Number(1),
                PermissionRespondParams::METHOD,
                answer_allowing(&RequestId::from("perm-never-minted")),
            )
            .unwrap();
            assert!(
                !response.contains("error"),
                "an answer to a prompt nobody is waiting on is a no-op, not a \
                 refusal: {response}"
            );
        }
    }

    /// REQ-563 AC-10: `web/refresh` is a dispatchable method, and it answers
    /// with an outcome rather than the URL it was handed (BR-7 — the daemon
    /// never echoes an outgoing destination back).
    #[test]
    fn dispatch_answers_web_refresh_with_an_outcome_and_no_url() {
        let daemon = Daemon::new();
        let response = dispatch(
            &daemon,
            &unattached(&daemon),
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
            &unattached(&daemon),
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
            let response = dispatch(
                &daemon,
                &unattached(&daemon),
                Id::Number(1),
                method,
                Value::Null,
            )
            .unwrap();
            assert!(
                !response.contains("-32601"),
                "`{method}` must be a routed client method: {response}"
            );
        }
    }
}
