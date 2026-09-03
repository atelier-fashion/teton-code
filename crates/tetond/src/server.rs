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
//! `session/clear`, `session/set_cwd` (REQ-583), `web/override` and
//! `permission/respond` against a session this connection never attached are
//! refused with `NOT_ATTACHED` (REQ-568 BR-4, REQ-569 BR-9). The write gates
//! sit at the seams every client crosses — [`forward_events`] for reads, and
//! [`handle_session_clear`], [`handle_session_set_cwd`], [`spawn_prompt_turn`],
//! [`handle_web_override`], [`handle_permission_respond`] for writes — never in
//! a client, and never in the reader loop above them, so the direct-RPC tests
//! exercise the real gate (LESSON-484).
//!
//! REQ-572 adds the `web/setup_*` family to that list ([`handle_web_setup_plan`],
//! [`handle_web_setup_preview`], [`handle_web_setup_commit`]): enabling a
//! capability is driving the session that asked for it. The **one** that can
//! change something additionally *announces* its refusal into that session
//! ([`refuse_commit_without_session_access`]) — an RPC error reaches only the
//! caller, and BR-4 wants the user to see that somebody else reached for their
//! configuration. The two reads refuse silently and the announcement is
//! budgeted per connection, because a notice an unattached peer can fire at will
//! is a way to write into a stranger's transcript rather than a warning
//! (REQ-572 verify, FIX 1b/1c). All three bound the `session_id` first, at
//! REQ-569 F9's length ([`refuse_unmintable_session_id`]).
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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use teton_protocol::events::{
    bytes_figure, thousands, AttachConsentRequested, AttachRefused, AttachRefusedReason,
    BoundaryDefaultsApplied, ConsentScope, DaemonClientAttach, Event, EventEnvelope,
    PermissionRequest, PhaseTransition, ProviderSetupCompleted, ProviderSetupRejected,
    RouteDecided, SessionGrantMinted, UnboundedRootWarning, WebSetupRejected, EVENT_METHOD,
    SUBSCRIPTION_LAGGED_METHOD,
};
use teton_protocol::handshake::{self, HandshakeParams, HandshakeResult};
use teton_protocol::jsonrpc::{error_code, Id, Notification, Response, RpcError};
use teton_protocol::methods::{
    AttachConsentOutcome, AttachConsentParams, AttachConsentResult, ConfigGetParams,
    ConfigGetResult, ConfigSetParams, ConfigSetResult, CostQueryParams, ModelConfirmParams,
    ModelListParams, ModelSetParams, ModelStatusParams, PermissionRespondParams,
    PermissionRespondResult, ProjectsListParams, ProjectsListResult, PromptBlock, PromptTurnParams,
    ProviderSetupCommitParams, ProviderSetupPlanParams, ProviderSetupPreviewParams,
    ProviderTestParams, RootKind, RpcMethod, SessionAttachParams, SessionAttachResult,
    SessionClearParams, SessionCreateParams, SessionCreateResult, SessionListParams,
    SessionListResult, SessionPermissionsParams, SessionSetCwdParams, SessionSummary,
    SessionTranscriptParams, SkillSkipped, SkillView, SkillsListParams, SkillsListResult,
    SkillsPreflightParams, SkillsPreflightResult, WebOverrideParams, WebRefreshParams,
    WebSetupCommitParams, WebSetupPlanParams, WebSetupPreviewParams,
};
use teton_protocol::{RequestId, SessionId};

use teton_core::lifetime::{BlockingActivity, PolicySource, ShutdownPolicy};
use teton_core::session_root::bounded_field;

use crate::attest::{
    AttestationRefusal, AttestationRegistry, MechanismAvailability, PresenceVerifier,
    UnavailableReason,
};
use crate::auth::{self, PeerIdentity};
use crate::broadcast::{EventBus, Subscription, DEFAULT_CAPACITY};
use crate::consent::{
    ConsentOutcome, ConsentRoute, ConsentSurfaces, PendingConsents, CONSENT_TIMEOUT,
};
use crate::grants::{monitor_witness, ConnectionId, Grant, GrantRegistry};
use crate::harness::budget::{skill_fit, RouteBudget, SkillCaller, SkillFit, SkillStage};
use crate::harness::permissions::{AddressedPermissionDelivery, CommitmentAttestation};
use crate::harness::tools::ToolRegistry;
use crate::harness::turn_loop::{build_system_prompt, HarnessConfig};
use crate::lifetime::LifetimeSupervisor;
use crate::peer::{is_descendant_of, Ancestry, KernelParentOf, MAX_ANCESTRY_DEPTH};
use crate::runtime::{BoundaryPosture, ClientPresence, DaemonRuntime};
use crate::sessions::{self, validate_session_cwd, SessionCreateError, SessionRegistry};
use crate::skills::{expand, DirLister, RealFs, Skill, SkillRegistry};

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

/// How many `session_grant_minted` announcements one connection's requests may
/// put on every other client's screen inside
/// [`GRANT_ANNOUNCEMENT_WINDOW`] (REQ-569 re-verify, R3).
///
/// The announcement is daemon-scoped by design — the point of F6 is that
/// somebody *other* than the beneficiary sees a widened permission — and that is
/// exactly what makes it worth flooding: minting is triggered by the requester,
/// so a peer that self-approves in a loop writes one unsuppressable notice per
/// iteration onto the screen of every connected client, including clients that
/// were never asked anything. Rate-limited rather than verbose-gated, because
/// the daemon is the enforcement point and hiding it in one renderer would leave
/// every other client (and every programmatic consumer) flooded.
///
/// Three, matching [`crate::consent::MAX_PENDING_CONSENTS_PER_CONNECTION`]: it
/// is above what any legitimate client does in a minute — a resuming CLI is
/// granted one session — and low enough that a flooder is quiet by its fourth
/// iteration rather than its ten-thousandth. Nothing is lost by exceeding it:
/// the arrears ride out on the next announcement that gets through
/// (`SessionGrantMinted::suppressed`), so a burst becomes one notice that says
/// how much it stands for.
///
/// **Per connection, so it does not bound an attacker that reconnects.** Nothing
/// in this daemon caps concurrent connections, so N connections buy N × this
/// many notices. That is a real limit of this bound and it is stated rather than
/// implied: what it makes cheap is one connection's loop, which is the shape the
/// probe found. A cap that held across connections would have to live on
/// something an attacker cannot re-mint, and this daemon has no such subject —
/// the same wall ADR-A-1 hit.
const GRANT_ANNOUNCEMENTS_PER_WINDOW: u32 = 3;

/// The window [`GRANT_ANNOUNCEMENTS_PER_WINDOW`] is counted over.
///
/// A minute: long enough that a flood is genuinely quieted rather than merely
/// slowed, short enough that a legitimate client which is granted several
/// sessions over a working session is never silently capped for long.
const GRANT_ANNOUNCEMENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

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
    ///
    /// Behind an `Arc` since REQ-585: the daemon's
    /// [`AddressedPermissionDelivery`] route holds the **same** registry this
    /// field names, so a skill consent is put in front of the connection this
    /// daemon actually has live rather than a copy of one (LESSON-484 — one
    /// definition, in one place).
    pub surfaces: Arc<ConsentSurfaces>,
    /// Verified human presence, bound to one connection and one request
    /// (REQ-570 BR-6).
    ///
    /// Beside [`Self::grants`] rather than inside it because the two answer
    /// different questions and must not imply each other: a grant is standing
    /// that persists for the connection's life, an attestation is a single-use
    /// proof that a human was at the machine one moment ago.
    pub attestations: AttestationRegistry,
    /// What asks a human to prove presence (REQ-570 ADR-B).
    ///
    /// Boxed behind the trait so AC-7's "no mechanism available" posture can be
    /// injected on any platform — the fail-closed path is then testable on a
    /// developer's Mac and a headless Linux CI runner alike, rather than only
    /// being observable on hardware nobody runs the suite on.
    ///
    /// Default and CI builds get [`UnavailableVerifier`], which refuses: on a
    /// build with no mechanism, cross-session attach is **refused** rather than
    /// silently self-approved (BR-8, BR-11).
    pub verifier: Arc<dyn PresenceVerifier>,
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
    /// The filesystem a session's skill discovery reads through (REQ-585
    /// BR-1, ADR-4).
    ///
    /// A field for [`Self::verifier`]'s reason: the claim REQ-585 makes about
    /// discovery is a claim about *what was not opened* and *how often* — four
    /// listings at `session/create`, four more at `/cd`, and none per turn —
    /// and neither half is observable from outside the process. Behind the
    /// [`DirLister`] seam a fixture records every path handed to it, so "a
    /// two-turn session listed the four roots once" is a decidable fact rather
    /// than a comment.
    ///
    /// Production never sets it: [`RealFs`] is what both constructors take, and
    /// a fixture has to say [`Daemon::with_skill_lister`] out loud.
    pub skills_fs: Arc<dyn DirLister + Send + Sync>,
    /// The window a connection's grant-announcement allowance is counted over
    /// (R3, [`GRANT_ANNOUNCEMENTS_PER_WINDOW`]).
    ///
    /// A field for [`Self::consent_timeout`]'s reason: the production value is
    /// sized for a human reading a screen, and a test that asserted the
    /// *arrears* behaviour by waiting it out would spend a minute proving
    /// something it can prove in milliseconds. Production never sets it.
    pub grant_announcement_window: std::time::Duration,
    /// The route each session's last turn was decided on (REQ-589 ADR-11).
    ///
    /// Here rather than on the runtime because the *writer* is here: the memo
    /// is fed from the event bus this daemon owns, by an observer
    /// [`spawn_prompt_turn`] starts. Shared like the registry and the bus, and
    /// in memory only — a stamp is a fact about one live conversation and
    /// nothing here is ever persisted.
    pub stamped_routes: Arc<StampedRoutes>,
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
        let runtime = Arc::new(DaemonRuntime::minimal());
        let surfaces = wire_addressed_delivery(&runtime, &events);
        let verifier: Arc<dyn PresenceVerifier> = Arc::from(crate::attest::default_verifier());
        wire_commitment_attestation(&runtime, &verifier);
        Self {
            sessions: SessionRegistry::new(),
            events,
            runtime,
            lifetime,
            grants: GrantRegistry::new(),
            consents: PendingConsents::new(),
            surfaces,
            attestations: AttestationRegistry::new(),
            verifier,
            skills_fs: Arc::new(RealFs),
            consent_timeout: CONSENT_TIMEOUT,
            grant_announcement_window: GRANT_ANNOUNCEMENT_WINDOW,
            stamped_routes: Arc::new(StampedRoutes::new()),
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

    /// Replaces what asks a human to prove presence (REQ-570 AC-7).
    ///
    /// **The injection seam the acceptance criteria are written against.** AC-7
    /// requires the no-mechanism posture to be assertable on any platform, and
    /// AC-1 requires the opposite — a *working* verifier — to be exercised
    /// without a human touching a sensor in CI. Neither is reachable through the
    /// real mechanism, which by design cannot be satisfied without a person.
    ///
    /// A fixture has to name this out loud, exactly as it does
    /// [`Self::with_consent_timeout`]: production never sets it, so an
    /// always-succeeding verifier can never be reached by a build that ships.
    #[must_use]
    pub fn with_presence_verifier(mut self, verifier: Box<dyn PresenceVerifier>) -> Self {
        let verifier: Arc<dyn PresenceVerifier> = Arc::from(verifier);
        // Re-wired, not merely stored (REQ-591 D-1). The runtime's commitment
        // seam was filled in by the constructor from the *shipped* verifier, and
        // a fixture that injected an accepting one and got the fail-closed
        // answer on the durable writes would be testing the constructor rather
        // than its own daemon — the quiet-no-op shape this whole REQ is about.
        wire_commitment_attestation(&self.runtime, &verifier);
        self.verifier = verifier;
        self
    }

    /// Replaces the filesystem a session's skill discovery reads through
    /// (REQ-585 ADR-4).
    ///
    /// **The observation seam the cost criterion is written against.** Whether
    /// discovery was paid once or once per turn cannot be read off a registry —
    /// both produce the same rows — so the assertion has to be made against
    /// what was *opened*, and only a recording [`DirLister`] can say that.
    ///
    /// A fixture has to name this out loud, exactly as it does
    /// [`Self::with_presence_verifier`]: production takes [`RealFs`] in both
    /// constructors, so no shipped path can be reading a stand-in filesystem.
    #[must_use]
    pub fn with_skill_lister(mut self, fs: Arc<dyn DirLister + Send + Sync>) -> Self {
        self.skills_fs = fs;
        self
    }

    /// Replaces the window a connection's grant announcements are rate-limited
    /// over (R3).
    ///
    /// For fixtures that assert on the *arrears* — the "+K suppressed" count a
    /// bounded burst reports on its next announcement — which is only
    /// observable once a window has turned over.
    #[must_use]
    pub fn with_grant_announcement_window(mut self, window: std::time::Duration) -> Self {
        self.grant_announcement_window = window;
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
        let surfaces = wire_addressed_delivery(&runtime, &events);
        let verifier: Arc<dyn PresenceVerifier> = Arc::from(crate::attest::default_verifier());
        wire_commitment_attestation(&runtime, &verifier);
        Self {
            sessions: SessionRegistry::new(),
            events,
            runtime,
            lifetime,
            grants: GrantRegistry::new(),
            consents: PendingConsents::new(),
            surfaces,
            attestations: AttestationRegistry::new(),
            verifier,
            skills_fs: Arc::new(RealFs),
            // The production answer, and taken here rather than passed in so
            // `main` cannot ship a daemon that forgot to state it: this daemon
            // is its own process, and the children it spawns are what BR-4
            // excludes (ADR-A).
            process: DaemonProcess::Own(
                i32::try_from(std::process::id()).expect("a pid fits in i32"),
            ),
            consent_timeout: CONSENT_TIMEOUT,
            grant_announcement_window: GRANT_ANNOUNCEMENT_WINDOW,
            stamped_routes: Arc::new(StampedRoutes::new()),
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
    /// How much of this connection's grant-announcement budget is left
    /// (REQ-569 re-verify, R3).
    ///
    /// Kept **here**, on the connection, rather than in a daemon-wide map keyed
    /// by [`ConnectionId`]: the budget is per requesting connection, so this way
    /// it is created and destroyed with its subject and no release hook can
    /// forget it — the same argument ADR-C makes for grants dying with their
    /// connection, applied to a counter. Shared through the `Arc` like the other
    /// two, because a connection's handlers run on several tasks and a
    /// per-clone budget would be no budget at all.
    announcements: Arc<Mutex<GrantAnnouncementBudget>>,
    /// The sessions this connection has already had an
    /// [`Event::WebSetupRejected`] announced into (REQ-572 verify FIX 1c,
    /// re-keyed by BUG-166).
    ///
    /// The same argument [`Self::announcements`] records, with one correction
    /// the first cut got wrong: the notice's *audience* is the target session's
    /// own user, so the budget's key has to carry the session. A single
    /// per-connection bool meant a connection refused on session A and then on
    /// session B announced only into A — B's user, a different person watching
    /// a different transcript, was never told — and, worse, one refusal aimed
    /// at a session id that named nothing spent the bool on nobody and
    /// silenced every real notice the connection owed afterwards (the BUG-166
    /// burn attack). Keyed per (connection, session), the second notice into
    /// the *same* session is still suppressed — it says nothing the first did
    /// not, to the same reader — while the first notice into each session is
    /// guaranteed its landing.
    ///
    /// **Bounded by what the daemon minted, not by what a caller invents**:
    /// ids enter this set only after the registry answers for them
    /// ([`refuse_commit_without_session_access`] checks existence before it
    /// spends), so its size is capped by the real sessions this connection's
    /// lifetime overlaps — never by the ≤ 31-byte strings an attacker can mint
    /// for free, which is the allocation trap `session/attach`'s length gate
    /// exists for.
    ///
    /// No arrears figure, unlike [`Self::announcements`], and deliberately: a
    /// suppressed grant announcement carries information (a *different* grant
    /// was minted), so its count is owed to the reader; a suppressed rejection
    /// here is a byte-identical duplicate to the identical audience, and "3
    /// more identical sentences were not repeated" tells that reader nothing.
    ///
    /// **Per connection underneath, so it does not bound a peer that
    /// reconnects** — the same stated limit [`GRANT_ANNOUNCEMENTS_PER_WINDOW`]
    /// carries, and for the same reason: this daemon has no subject an
    /// attacker cannot re-mint.
    ///
    /// **Keyed by the notice as well as the session** (REQ-579 BR-12). Two setup
    /// flows publish a rejection now, and they say different things — one names
    /// an origin that reached for a session's web access, the other names the
    /// provider-setup method that was refused. A key that ignored which notice it
    /// was would let the first suppress the second, and the paragraph above only
    /// licenses suppressing a *byte-identical duplicate to the identical
    /// audience*. The added dimension is a fixed, code-supplied set of event
    /// names, so the bound stays "sessions this connection's lifetime overlaps"
    /// times a small constant — never anything a caller can mint.
    setup_rejections_announced: Arc<Mutex<HashSet<(&'static str, SessionId)>>>,
}

/// One connection's rolling allowance of grant announcements (R3).
///
/// A count and the instant its window opened. Not a token bucket and not a
/// per-event timestamp list: what this bounds is *notices on a human's screen*,
/// where the useful behaviour is "a few, then a summary", and a structure that
/// remembered every event would be another unbounded thing an attacker fills.
#[derive(Debug)]
struct GrantAnnouncementBudget {
    /// When the current window opened.
    opened: std::time::Instant,
    /// Announcements published in the current window.
    announced: u32,
    /// Announcements dropped since the last one that got through — reported on
    /// the next one, then cleared. Saturating, so a long flood cannot wrap it
    /// back to a reassuring small number.
    suppressed: u32,
}

impl GrantAnnouncementBudget {
    /// Ask for one announcement's worth of budget.
    ///
    /// `Some(arrears)` to publish, carrying how many were dropped since the last
    /// published one; `None` to stay quiet. Taking the arrears *out* on the way
    /// through is what makes the count a since-last-report figure rather than a
    /// running total a reader would have to difference.
    fn take(&mut self, now: std::time::Instant, window: std::time::Duration) -> Option<u32> {
        if now.duration_since(self.opened) >= window {
            self.opened = now;
            self.announced = 0;
        }
        if self.announced >= GRANT_ANNOUNCEMENTS_PER_WINDOW {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.announced += 1;
        Some(std::mem::take(&mut self.suppressed))
    }
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
            announcements: Arc::new(Mutex::new(GrantAnnouncementBudget {
                opened: std::time::Instant::now(),
                announced: 0,
                suppressed: 0,
            })),
            setup_rejections_announced: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether a grant minted for this connection may be announced now, and how
    /// many announcements were suppressed since the last one that was (R3).
    ///
    /// Counted under the budget's own lock, for the reason the consent cap is
    /// counted under the registry's: two `session/attach` tasks on one
    /// connection would otherwise check-then-act and turn a bound of three into
    /// a bound of "three per interleaving". The guard is dropped before the
    /// caller publishes — this decides, the caller does the I/O.
    fn may_announce_grant(&self, window: std::time::Duration) -> Option<u32> {
        self.announcements
            .lock()
            .expect("grant announcement budget lock poisoned")
            .take(std::time::Instant::now(), window)
    }

    /// Whether this connection's one `notice` announcement **for `session_id`**
    /// is still unspent, claiming it if so (REQ-572 verify FIX 1c, re-keyed by
    /// BUG-166 and again by REQ-579 — see
    /// [`Self::setup_rejections_announced`]).
    ///
    /// `notice` is the wire name of the event about to be published
    /// ([`Event::name`]), taken from the event itself rather than passed
    /// alongside it, so the budget a refusal spends and the sentence it publishes
    /// cannot name two different things.
    ///
    /// The insert happens under the set's own lock, for
    /// [`Self::may_announce_grant`]'s reason: two refused commits against one
    /// session arriving on two of this connection's tasks would otherwise both
    /// find the pair unclaimed and both publish, turning a bound of one into a
    /// bound of "one per interleaving". This decides; the caller does the I/O.
    ///
    /// The caller is responsible for asking only about sessions the registry
    /// answers for — this method records what it is told, and recording an
    /// uncheckable id here would grow the set by attacker-minted keys.
    fn may_announce_setup_rejection(&self, notice: &'static str, session_id: &SessionId) -> bool {
        self.setup_rejections_announced
            .lock()
            .expect("setup rejection announcement set poisoned")
            .insert((notice, session_id.clone()))
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
    "no monitor-scope grant, and no attached client is available to approve one";

/// The BR-10(a) standing check for a **daemon-wide** method (REQ-570 ADR-A,
/// closes BUG-162).
///
/// Seven methods took no connection context at all, so any handshaked same-UID
/// connection — including a daemon-spawned tool/MCP child that REQ-569 BR-4
/// otherwise excludes from session access — could commit a multi-gigabyte
/// download and a daemon-wide model change on the user's behalf.
///
/// **Why a standing rule and not "the connection that raised the request".**
/// BUG-162 proposes binding the answer to the raiser, and REQ-570's Permissions
/// table inherits that wording. For `model/confirm` there is no such connection:
/// `model_consent` raises the proposal from the first-run flow spawned beside
/// `serve`, which its own comment notes "may publish before the daemon accepts
/// its first connection", and publishes it `None`-scoped because local model
/// selection is a machine-wide fact. The raiser is the *daemon*. Inventing a
/// raiser (first-claim-wins) would hand the proposal to whichever connection
/// races fastest, which an attacker wins as easily as a user.
///
/// So the rule is the standing that already exists and is exactly right —
/// REQ-569's ancestry gate — which is what BUG-162's own *Expected Behavior*
/// names as the minimum bar: "answerable only by a connection entitled to answer
/// it — minimally, not by the daemon's own spawned children".
///
/// It inherits [`ConnState::may_hold_session_access`]'s fail-closed treatment of
/// [`Ancestry::Indeterminate`], so a vanished pid or an unreadable chain costs
/// the same as a confirmed descendant.
///
/// **What it does not claim.** It cannot distinguish a user's real CLI from a
/// non-descendant headless same-UID process; that distinction is unavailable to
/// this layer by construction and is what BR-10(b)'s attestation supplies. This
/// is a real reduction in blast radius — the daemon's own children lose the
/// capability outright — recorded at that strength rather than oversold.
///
/// Returns the refusal to hand back, or `None` to proceed. Called as a **single
/// line at the top of each of the seven handlers** rather than once in
/// [`dispatch`], deliberately: AC-11's mutation check requires removing *a
/// method's* check to make a test red, per method rather than for one
/// representative (LESSON-502), and a single shared gate has only one thing to
/// remove.
/// Borrows `id` and clones only on the refusal path, so the overwhelmingly
/// common "allowed" case leaves the caller's `id` untouched to pass on.
fn refuse_daemon_wide(conn: &ConnState, id: &Id) -> Option<String> {
    if conn.may_hold_session_access() {
        return None;
    }
    Some(error_string(
        id.clone(),
        error_code::ATTACH_FORBIDDEN,
        ATTACH_FORBIDDEN_MESSAGE,
    ))
}

/// **BR-10(b).** A daemon-wide *commitment* additionally needs a verified human.
///
/// [`refuse_daemon_wide`] is a standing check and stops at the daemon's own
/// children; REQ-569 ADR-A records that breaking the ancestry chain costs one
/// model-supplied shell word, so a non-descendant same-UID process still passes
/// it. That residual is tolerable for a config read. It is **not** tolerable for
/// a commitment whose blast radius is the whole machine — a model change, a
/// multi-gigabyte download, writing the `[web]` egress table, or (REQ-576)
/// rewriting the provider/privacy config — which is why four methods ask for
/// presence on top: `model/confirm`, `model/set`, (REQ-575) `web/setup_commit`,
/// and (REQ-576) `config/set`.
///
/// The split is the spec's rather than a convenience: `config/get`, `cost/query`
/// and `web/refresh` stay layer (a) only. Prompting a human to evict a cached
/// document would train them to click through the prompt that matters.
///
/// **Standing obligation (REQ-575 BR-5).** Any future method that durably
/// rewrites `config.toml` or live-swaps daemon-wide in-memory state must be
/// classified against BR-10(b) in its own architecture phase — the omission of
/// exactly that classification is how REQ-572 finding 7 arose (a daemon-wide
/// committing method shipped with no BR-10(b) analysis). `config/set` — the
/// largest blast radius of the four (`RegisterProvider` names an egress endpoint,
/// `SetPrivacyBoundary` rewrites the privacy boundary) — was the known next
/// candidate and is now gated (REQ-576), reversing its BUG-162 layer-(a)-only
/// posture. The four daemon-wide config-writers known today are all classified;
/// the obligation stands for the next one.
///
/// Unlike the consent path this does **not** record into the attestation
/// registry. There is no consent `request_id` here to bind to — these methods
/// are not answers to a request — so the check is live, used once immediately,
/// and never stored; BR-6's single-use property holds by construction rather
/// than by bookkeeping.
async fn refuse_unattested_commitment(
    daemon: &Daemon,
    conn: &ConnState,
    id: &Id,
) -> Option<String> {
    // **Where no mechanism exists this degrades to layer (a) — it does not
    // refuse.** The asymmetry with the consent path is the spec's, not a
    // convenience:
    //
    // BR-8 and BR-11 both scope the fail-closed refusal to *cross-session
    // attach* — "cross-session attach is refused", "cross-session attach does
    // not work there". Neither extends it to a daemon-wide commitment. AC-10
    // says a commitment "refuses when no valid attestation is **presented**",
    // which is about an absent or invalid proof on a platform that can produce
    // one, not about a platform that cannot.
    //
    // Refusing here would also be catastrophic rather than merely strict: the
    // `presence` feature is non-default, so a shipped build has no mechanism, so
    // `model/confirm` would refuse — and first-run model selection, the flow
    // REQ-547 exists for, would be impossible for every user. That is exactly
    // what AC-8's regression bar forbids ("zero new prompts or attestation
    // steps" for the ordinary flows).
    //
    // The reduced posture is **stated rather than silent**, which is the part of
    // BR-8 that does apply here.
    // The body is [`attest_commitment`], shared with the two consent-answer
    // writes REQ-591 D-1 gates. What stays here is what is local to an RPC: the
    // requester this connection announced, and a JSON-RPC frame to refuse with.
    match attest_commitment(daemon.verifier.as_ref(), conn.id) {
        CommitmentStanding::Attested => None,
        CommitmentStanding::NoMechanism(reason) => {
            eprintln!("{}", commitment_degraded_line(reason));
            None
        }
        CommitmentStanding::Refused(refusal) => {
            eprintln!("{}", attestation_refusal_line(&conn.requester, &refusal));
            Some(error_string(
                id.clone(),
                attestation_error_code(&refusal),
                refusal_message(&refusal),
            ))
        }
    }
}

/// BR-8's stated posture, in one place, for every daemon-wide commitment that
/// proceeds because this build can ask nobody.
///
/// A function rather than a bare `eprintln!` for [`attestation_refusal_line`]'s
/// reason: the sentence is the *whole* of "the reduced posture is stated rather
/// than silent", and an operator reading a log needs the same words whichever
/// commitment took the degraded path.
fn commitment_degraded_line(reason: UnavailableReason) -> String {
    format!(
        "teton-code: daemon-wide commitment allowed on connection standing alone — \
         this build has no presence mechanism ({}). BR-10(a) still applies; \
         BR-10(b) is unavailable here.",
        reason.describe()
    )
}

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

/// What the daemon records when a consent prompt is delivered to **no
/// surface at all** (BUG-163).
///
/// This is the failure that has no other symptom. The request is registered, the
/// window opens, and nothing is on the other end — so the only thing anyone
/// observes is a call that takes the full consent timeout and then reports a
/// timeout, which reads identically to a user who walked away from a prompt they
/// really were shown.
///
/// Those two need telling apart, because their remedies are opposite: one is a
/// person who declined to answer, the other is a daemon that asked nobody.
///
/// Names the **scope** and the **routing rule**, which together say what was
/// being asked and which rule produced an empty audience. Carries no session id,
/// no connection id and no client-supplied string (conventions: privacy in
/// logs) — [`ConsentRoute::arm`] exists to give the rule's name without its
/// subject.
fn undelivered_consent_line(scope: ConsentScope, arm: &str) -> String {
    let what = match scope {
        ConsentScope::Attach => "an attach",
        ConsentScope::Monitor => "a monitor",
    };
    format!(
        "tetond: {what} consent prompt reached no surface — the rule that chose the audience was \
         '{arm}', and it selected nobody. Nothing can answer this request, so it will wait out its \
         full consent window and then report a timeout"
    )
}

/// What the daemon records when it classifies a connection's ancestry as
/// anything other than [`Ancestry::NotDescendant`] (BUG-163).
///
/// [`ancestry_refusal_line`] already tells the two verdicts apart — but only on
/// the paths that **refuse**. A connection classified `Indeterminate` that
/// declares no monitor is not refused at handshake at all: it is admitted,
/// silently registered with `may_answer: false`, and then never offered a
/// consent frame. The user sees a request that waits out its window; the daemon
/// keeps no record that it decided anything.
///
/// That silence is what made BUG-163 cost two rounds of guessing — with the
/// classification unrecorded, there was no way to ask whether this seam was even
/// involved, so the investigation had to reason about mechanisms instead of
/// reading one line. This records the decision the daemon is already making.
///
/// Deliberately **not** logged for `NotDescendant`: that is every ordinary
/// connection, and a line per handshake would bury the two cases worth seeing.
///
/// Carries the peer pid, because "which process was this" is the first question
/// anyone reading it will ask, and it is the one fact that lets a walk be
/// reconstructed by hand. It carries no session id, no path, and no
/// client-supplied string (conventions: privacy in logs).
fn ancestry_classification_line(ancestry: Ancestry, peer_pid: Option<i32>) -> String {
    let (verdict, consequence) = match ancestry {
        Ancestry::Descendant => (
            "descends from this daemon's own process tree",
            "it is excluded from session access — this is the gate working",
        ),
        Ancestry::Indeterminate => (
            "has an ancestry this daemon could not determine",
            "it fails closed: no session access, and no consent prompt will be \
             delivered to it. If this connection is a legitimate client, that is \
             a peer-pid or process-walk problem, not a policy decision",
        ),
        // Not reached — the caller logs only the two above. Spelled out rather
        // than left to a catch-all so a future arm cannot land here silently
        // wearing one of the sentences above.
        Ancestry::NotDescendant => (
            "is outside this daemon's process tree",
            "it is eligible for session access",
        ),
    };
    let pid = match peer_pid {
        Some(pid) => pid.to_string(),
        None => "unknown".to_owned(),
    };
    format!("tetond: connection from pid {pid} {verdict} — {consequence}")
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
    // This connection's liveness, as seen by the turns it spawns (REQ-580
    // ADR-3). Held open for the reader loop's whole life and dropped at
    // teardown, *before* the drain below — so a turn still **held** for a
    // warming tier ends there and then, rather than sitting out the drain
    // window and running as a ghost when the tier opens. A turn that has
    // started ignores it and keeps REQ-565's drain semantics.
    let (connected, presence) = watch::channel(true);
    let presence = ClientPresence::watching(presence);
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
            if let Some(handle) = spawn_prompt_turn(
                &daemon,
                conn,
                id,
                params,
                &out_tx,
                fence.clone(),
                presence.clone(),
            ) {
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
        // `attach/consent` joins them for a third reason (REQ-570 BR-1): a
        // granting answer now runs an OS presence prompt, which parks on a human
        // for as long as they take. Left on the reader loop it would stall every
        // other RPC on this connection — including, on a single-client resume,
        // the very `session/attach` whose consent is being answered.
        // `model/confirm` and `model/set` join for BR-10(b): a daemon-wide
        // commitment now asks for presence too, and that prompt parks on a human
        // exactly as the consent one does. `web/setup_commit` joins them for the
        // same reason (REQ-575): writing the `[web]` egress table is a daemon-wide
        // commitment, so it now attests. `config/set` joins them for the same
        // reason (REQ-576): rewriting the provider/privacy config (an egress
        // endpoint, the privacy boundary) is the largest such commitment.
        // `provider/setup_commit` joins them for the same reason (REQ-579):
        // registering a provider writes an egress endpoint and rewrites the tier
        // table, which is the same blast radius `config/set` has by another
        // route. Each moved off the reader-loop `dispatch` to here rather than
        // parking every other RPC on this connection behind a Touch ID prompt.
        //
        // `provider/test` (REQ-581) is the one member that blocks on **the
        // network** rather than on a human: it makes one real completion request
        // to a vendor, which can take as long as a person deciding, and it
        // attests nothing (it changes no config). What it shares with the seven
        // above is the only thing this list is about — a method that waits on
        // something outside this process must not wait on the reader loop, or
        // every other RPC on the connection waits with it (LESSON-518). So the
        // name reads "a human *or* the network"; the branch chain below is the
        // routing.
        //
        // `session/transcript` (REQ-611 ADR-6) is the second member that waits
        // on neither: it waits on **the disk**, flushing the transcript sink so
        // that its reply describes a file the writer thread has really opened,
        // refused or resumed (`DaemonRuntime::session_transcript`). Same rule,
        // third kind of wait — and the branch name is now two-thirds
        // historical, kept because renaming it would touch every comment that
        // cites it while changing nothing about what it does.
        //
        // Membership in this list decides where the work *runs*. It does not
        // decide what teardown does with it: `provider/test` ends up on
        // `prompt_tasks` (drained) rather than `attach_tasks` (aborted), for the
        // reason spelled out at the push below, and `session/transcript` joins
        // it there for a reason of its own.
        let blocks_on_a_human = matches!(
            method,
            m if m == SessionAttachParams::METHOD
                || m == AttachConsentParams::METHOD
                || m == ModelConfirmParams::METHOD
                || m == ModelSetParams::METHOD
                || m == WebSetupCommitParams::METHOD
                || m == ProviderSetupCommitParams::METHOD
                || m == ConfigSetParams::METHOD
                || m == ProviderTestParams::METHOD
                || m == SessionTranscriptParams::METHOD
        );
        if blocks_on_a_human {
            let daemon = Arc::clone(&daemon);
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let fence = fence.clone();
            // REQ-581 verify F2 — **which teardown list this task joins**, and
            // it is not the one the branch is named after.
            //
            // Seven of the eight members are waiting on a *person*, and what
            // each can still do when its client vanishes is mint a grant. So
            // teardown `abort()`s them: a decision nobody is left to make is
            // better killed than completed, and killing it is what bounds the
            // grant release that follows (see the teardown comment).
            //
            // `provider/test` is none of that. It is a **billed call with a
            // durable row**: the vendor is charging for it whether or not
            // anyone is still on the socket, and aborting it at its await point
            // loses the ledger row, the health record and the `provider_tested`
            // event for money already spent — REQ-565's exact hole for turns,
            // arriving by another route and widest in the TTFB window, where the
            // request is out and nothing has come back yet. It also mints no
            // grant, so nothing in the release ordering below needs it dead
            // first.
            //
            // So it rides `prompt_tasks`, which teardown *drains* rather than
            // aborts — bounded by `TURN_DRAIN_TIMEOUT` on that side and by
            // `PROBE_DEADLINE` on the probe's own.
            let is_probe = method == ProviderTestParams::METHOD;
            // REQ-611 ADR-6: `session/transcript` is drained rather than
            // aborted, and not for the probe's reason — it spends no money and
            // has no ledger row. It is because the **switch has already
            // happened** by the time this task is awaiting anything: the sink
            // took it synchronously, and what remains after the flush is the
            // `transcript_state` publish that tells the session's *other*
            // attached connections the record started or stopped. Aborting
            // there would leave a session recording with nobody told, which is
            // the one outcome BR-15 exists to prevent. It carries no activity
            // guard: a flush is bounded by the disk, and `shutdown_transcripts`
            // closes the file on the way out whether or not this task ran.
            let drained_at_teardown = is_probe || method == SessionTranscriptParams::METHOD;
            // …and the drain alone is not enough to make that true.
            //
            // Teardown drops `client_guard` *before* it drains (the ordering is
            // deliberate and documented there), so if the disconnecting client
            // was the last one, `LifetimeState::on_disconnect` asks
            // `commit_or_defer` what to do while this probe is still awaiting
            // the vendor. With no activity claimed, that sees an idle daemon,
            // commits under the default `on-last-disconnect` policy, and `serve`
            // returns — `main` then reaches `_exit` with the drain still running,
            // and the ledger row, the health record and the `provider_tested`
            // event for a billed request are lost anyway. A drain nothing defers
            // to is a drain the process does not wait for.
            //
            // [`BlockingActivity::Turn`] rather than a variant of its own: a
            // probe is a billed call against a vendor with a durable row on the
            // far side of it, which is exactly what `Turn` means to the
            // lifetime — and a ninth variant would be new wire vocabulary every
            // client has to learn for a distinction none of them acts on.
            //
            // Taken here rather than inside the task so the claim exists before
            // `spawn` returns ([`spawn_prompt_turn`]'s reason, verbatim): a
            // client that disconnects in the gap between the two would otherwise
            // see an idle daemon and commit to exiting while this probe was
            // still starting. Moved into the task, so `Drop` releases it on
            // every exit path — completion, error, panic, or the drain's
            // timeout abort.
            let probe_guard = is_probe.then(|| daemon.lifetime.activity(BlockingActivity::Turn));
            let method = method.to_owned();
            let task = tokio::spawn(async move {
                let _probe_guard = probe_guard;
                let response = if method == SessionAttachParams::METHOD {
                    handle_session_attach(&daemon, &conn, id, params).await
                } else if method == AttachConsentParams::METHOD {
                    handle_attach_consent(&daemon, &conn, id, params).await
                } else if method == ModelConfirmParams::METHOD {
                    handle_model_confirm(&daemon, &conn, id, params).await
                } else if method == ModelSetParams::METHOD {
                    handle_model_set(&daemon, &conn, id, params).await
                } else if method == WebSetupCommitParams::METHOD {
                    handle_web_setup_commit(&daemon, &conn, id, params).await
                } else if method == ProviderSetupCommitParams::METHOD {
                    handle_provider_setup_commit(&daemon, &conn, id, params).await
                } else if method == ConfigSetParams::METHOD {
                    handle_config_set(&daemon, &conn, id, params).await
                } else if method == ProviderTestParams::METHOD {
                    handle_provider_test(&daemon, &conn, id, params).await
                } else if method == SessionTranscriptParams::METHOD {
                    handle_session_transcript(&daemon, &conn, id, params).await
                } else {
                    // Unreachable: the `blocks_on_a_human` `matches!` guard admits
                    // exactly the nine methods branched above. Made explicit rather
                    // than a catch-all so a future tenth member that updates the
                    // guard but forgets a branch fails loudly here instead of being
                    // silently misrouted into the last handler.
                    unreachable!("blocks_on_a_human admitted an unrouted method: {method}")
                };
                // The fence for the same reason `dispatch`'s responses take it:
                // any event already delivered to this client's subscription
                // must reach the wire ahead of the response ordering it.
                if let Some(fence) = fence {
                    fence.sync().await;
                }
                let _ = out_tx.send(response).await;
            });
            if drained_at_teardown {
                prompt_tasks.retain(|h| !h.is_finished());
                prompt_tasks.push(task);
            } else {
                attach_tasks.retain(|h| !h.is_finished());
                attach_tasks.push(task);
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
    //
    // REQ-581 verify F2: `provider/test` shares the spawn branch with these but
    // deliberately not this list. It mints no grant, so it does not belong to
    // step 2 — and it has already spent the user's money, so the abort that is
    // right for an undecided consent is exactly wrong for it. It is drained
    // below with the turns.
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
    //
    // REQ-580 ADR-3: the one exception is a turn that has not started — one
    // still held for a warming tier. It has no cost row to protect and nobody
    // to answer, so the connection's liveness is withdrawn *first*, and any
    // held turn ends on it before the drain even looks. Dropped rather than
    // sent, so a turn that subscribed late sees the closed channel and not a
    // stale `true`.
    //
    // REQ-581 verify F2: an in-flight `provider/test` is drained here too, and
    // for the identical reason rather than a related one. A probe is a billed
    // call against a vendor with a durable row, a health record and an event on
    // the far side of it; a client that closed its terminal during the TTFB
    // window has changed nothing about the fact that the money is spent. What
    // it is *not* is a consent: it mints no grant, so it does not need the
    // abort-then-await treatment the attaches above get. Its own
    // `PROBE_DEADLINE` bounds it well inside `TURN_DRAIN_TIMEOUT`.
    drop(client_guard);
    drop(connected);
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
    presence: ClientPresence,
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

    // F9's length rule, ahead of the `may_drive` hash below for the reason it
    // sits ahead of the setup handlers' (BUG-166 residual (c)): the id is
    // attacker-chosen and bounded only by `MAX_FRAME`, and a prompt is the
    // seam a caller can drive most freely.
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        let _ = out_tx.try_send(refusal);
        return None;
    }

    // REQ-585 ADR-3: exactly one of `prompt`/`skill`. A request carrying both is
    // a combination that was never valid, so refusing it narrows nothing — and
    // it is the one shape the daemon cannot resolve, since the two would
    // disagree about what this turn's text is.
    //
    // A both-**empty** request is deliberately NOT refused: `flatten_prompt(&[])`
    // returns `""` and such a turn runs today, so rejecting it would narrow an
    // existing method for third-party clients while `PROTOCOL_VERSION` is
    // asserted unchanged. The failure worth designing against — a raw
    // `/name args` line reaching a model — is already impossible: the CLI never
    // puts the typed line in `prompt` at all, so a dropped `skill` field yields
    // a visible empty turn rather than a leaked command line.
    //
    // A shape check, reading no session state, so it is no more of an existence
    // oracle than the `serde` failure above.
    if !params.prompt.is_empty() && params.skill.is_some() {
        let _ = out_tx.try_send(error_string(
            id,
            error_code::INVALID_PARAMS,
            "`prompt` and `skill` are exclusive: a turn is typed text or a `/name` \
             invocation, never both",
        ));
        return None;
    }

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
    // REQ-585: the invocation travels **beside** the flattened prompt, not
    // through it — a skill turn has no `PromptBlock`s to flatten, and
    // `raw_arguments` is the rest of the typed line verbatim (ADR-3), so it must
    // not be re-joined from tokens or folded into a text block anywhere on this
    // path. The daemon expands it; the client never composes a body.
    let skill = params.skill;
    // REQ-589 ADR-11: the stamp memo starts **before** the turn is spawned, so
    // the very first turn's `route_decided` lands in a subscription that
    // already exists. `subscribe` is synchronous, so this is an ordering
    // guarantee rather than a race the observer usually wins.
    observe_route_decisions(daemon);
    let runtime = Arc::clone(&daemon.runtime);
    let events = Arc::clone(&daemon.events);
    // The turn carries the registry, not just the summary read out of it: the
    // `title` duty (REQ-561 TASK-062) has to *write back* the name it derives
    // and take the once-per-session claim that keeps it from re-deriving one.
    // The summary above is a snapshot, so it cannot serve either purpose — and
    // its `cwd` is a pre-claim snapshot too: the runtime re-reads the root off
    // the registry once it holds the turn claim (REQ-583 verify), so a
    // `session/set_cwd` landing between this read and that claim moves the
    // turn rather than being run over.
    let daemon = Arc::clone(daemon);
    let out = out_tx.clone();
    // Read off the connection here, where it exists: the turn runs on its own
    // task and `ConnState` does not travel with it (REQ-585 ADR-7).
    let invoker = conn.id;

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
                skill,
                // REQ-585 ADR-7: the connection that typed the `/name`, so its
                // dynamic-context consent is put in front of that client and
                // answerable by nobody else.
                Some(invoker),
                presence,
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

    // BUG-163: record the verdict here, once, for every connection it applies
    // to — not only for the ones a later gate happens to refuse out loud.
    //
    // `Indeterminate` on a client that declares no monitor is admitted and then
    // silently denied every consent frame, so its only observable is a request
    // that waits out its window. Logging at the point of classification is what
    // makes the *next* occurrence answerable in one line instead of by
    // hypothesis.
    if !matches!(ancestry, Ancestry::NotDescendant) {
        eprintln!("{}", ancestry_classification_line(ancestry, peer.pid));
    }

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
            // **REQ-570 BR-5 / AC-2b: the consent path is back, under
            // attestation.** REQ-569 verify (F1) deleted it, and the note below
            // records why. What it could not do at the time was tell an
            // attacker's second connection from the user's real client — so the
            // capability kept its grant gate and had no minter at all, leaving a
            // REQ-568 feature dead.
            //
            // Three things now stand between a request and a monitor grant, and
            // the first is the one that actually breaks F1's attack:
            //
            // 1. A granting answer requires a presence attestation the daemon
            //    itself verified (`handle_attach_consent`). The attacker's
            //    second connection has to produce a human at the machine.
            // 2. The approver is never the requester, structurally
            //    (`ConsentRoute::any_attached_peer`), under any arm — BR-5.
            // 3. `session/create` is no longer ungated (BR-10(a)), so the
            //    attached-surface standing F1's attack opened with can no longer
            //    be manufactured by a daemon child.
            //
            // Refusal stays the default: if nobody holds a session there is no
            // peer to ask, and the request is refused rather than routed back to
            // the requester. A monitor is a whole-daemon read, and the
            // self-render arm that keeps *attach* usable would hand it to
            // whoever asked.
            let granted_now = if daemon.surfaces.anyone_attached_to_anything(connection) {
                let route = ConsentRoute::any_attached_peer(connection);
                let requester = requester_descriptor(&params);
                match seek_consent(daemon, &requester, ConsentScope::Monitor, None, &route).await {
                    Some(ConsentOutcome::Granted { attestation, .. }) => {
                        daemon
                            .grants
                            .grant(Grant::monitor(connection, monitor_witness()));
                        // Announced like any other mint (BR-9/AC-9), carrying
                        // what verified the human — the field that separates
                        // this from the path F1 removed.
                        eprintln!(
                            "teton-code: monitor grant minted for {requester} (attested: {})",
                            attestation.as_str()
                        );
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };

            if granted_now {
                // Fall through into the ordinary admission path below.
            } else {
                // The history this replaces (REQ-569 verify, F1): a monitor
                // request was routed to "any attached peer other than the
                // requester", on the theory that an attached connection is a
                // surface whose user demonstrably owns something. One attacker
                // holding two connections owned the whole exchange — A created a
                // throwaway session through the then-ungated `session/create`,
                // which made A an attached surface and so the eligible approver
                // for B's monitor request; A answered, and B became a daemon-wide
                // observer. No human was involved, and with two distinct
                // `ConnectionId`s it did not even read as a self-approval.
                //
                // The path is back because the missing piece arrived, not
                // because the reasoning changed: the daemon still cannot tell an
                // attacker's second connection from the user's real client, and
                // now it does not have to — it asks the *machine's* human
                // directly. See the three conditions above.
                //
                // This arm is what is left: no peer holds a session, so there is
                // nobody to ask. Refused rather than routed back to the
                // requester — a monitor is a whole-daemon read, and the
                // self-render arm that keeps attach usable would hand it to
                // whoever asked.
                //
                // No `attach_refused` is published: this connection is not a
                // registered surface until its handshake succeeds, so the route
                // it would go out on reaches nobody.
                let _ = out_tx.try_send(error_string(
                    id,
                    error_code::NOT_GRANTED,
                    NOT_GRANTED_MESSAGE,
                ));
                return None;
            }
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

    // Replay the local-model lifecycle (REQ-544 BR-9 / AC-8) to the client that
    // just attached, so it learns the state of the local tier on this machine:
    // probed, then awaiting a decision / disabled / ready. A machine with no
    // local tier has an empty sequence and emits nothing.
    //
    // **Routed to this connection, not published on the bus (BUG-177).** The
    // replay is this client's catch-up, not news: every client already attached
    // has had its own, and a daemon-scoped publish reaches all of them — so
    // every `teton doctor` in another terminal, and every `teton …` a session's
    // own shell tool spawned, re-announced `probe …` / `local model … ready`
    // into every open session (and reset each one's REQ-556 loading indicator
    // on the way). Delivery goes on the connection's own outbound instead: the
    // frames are queued right behind the handshake result on the same FIFO
    // channel, so the result is on the wire first and the replay precedes
    // anything this connection is answered next. The REQ-568 fence is not
    // involved — nothing here was delivered to the subscription — and the seq
    // numbers come from the bus like every routed frame's do
    // (`routed_event_frame`), so a replayed frame can never wear the number of
    // a broadcast one on this connection.
    //
    // Because it is replayed on *every* attach, every stage in it must be true of
    // the machine right now — see `runtime::startup_lifecycle`. A replayed
    // `download` or `ready` that described nothing would be repeated to every
    // client that ever connects, which is how a decorative sequence becomes a
    // daemon-wide lie.
    for lifecycle in daemon.runtime.lifecycle_events() {
        let _ = out_tx.try_send(routed_event_frame(
            daemon,
            None,
            Event::ModelLifecycle(lifecycle),
        ));
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
        // `session/transcript` (REQ-611 ADR-6) belongs beside `session/clear` —
        // same `may_drive` gate, same "one session, one user act", `ENDS_TURN`
        // false for both — and is deliberately absent from this match for
        // `session/attach`'s reason rather than for a different one: it awaits
        // the transcript sink's flush, so it runs on `handle_client`'s
        // `blocks_on_a_human` task. See [`handle_session_transcript`].
        // REQ-583 ADR-4: `session/set_cwd` sits beside `session/clear` for the
        // same reason it is modelled on it — no human, no network; it takes the
        // turn claim, rewrites one path, clears, publishes, and answers — so it
        // stays on the synchronous path and its two events ride the fence
        // ahead of its response.
        SessionSetCwdParams::METHOD => Some(handle_session_set_cwd(daemon, conn, id, params)),
        // `attach/consent` is deliberately absent alongside `session/attach`:
        // since REQ-570 it may run an OS presence prompt that parks on a human,
        // so it runs on its own task (see `handle_client`).
        PermissionRespondParams::METHOD => {
            Some(handle_permission_respond(daemon, conn, id, params))
        }
        // `model/confirm`, `model/set` and (REQ-576) `config/set` are deliberately
        // absent alongside `session/attach`: since REQ-570 BR-10(b) each may run an
        // OS presence prompt that parks on a human, so they run on their own task
        // (see `handle_client`'s `blocks_on_a_human`). `config/get` stays here — a
        // read is layer (a) only.
        ModelListParams::METHOD => Some(ok_string(id, &daemon.runtime.model_list())),
        ModelStatusParams::METHOD => Some(ok_string(id, &daemon.runtime.model_status())),
        ConfigGetParams::METHOD => Some(handle_config_get(daemon, conn, id)),
        CostQueryParams::METHOD => Some(handle_cost_query(daemon, conn, id)),
        WebOverrideParams::METHOD => Some(handle_web_override(daemon, conn, id, params)),
        // REQ-572: the setup *reads* are session-scoped, so they belong here
        // beside `web/override` and **not** in `refuse_daemon_wide`'s list — the
        // question they ask is "may this connection drive this session", which is
        // `may_drive`'s, not the ancestry gate's. `web/setup_commit` is NOT here:
        // since REQ-575 it is additionally a BR-10(b) commitment that may attest
        // (park on a human), so it runs on its own task in `handle_client`'s
        // `blocks_on_a_human` path — it keeps the same `may_drive` layer-(a) gate,
        // and adds presence on top.
        WebSetupPlanParams::METHOD => Some(handle_web_setup_plan(daemon, conn, id, params)),
        WebSetupPreviewParams::METHOD => Some(handle_web_setup_preview(daemon, conn, id, params)),
        // REQ-579: the provider trio's two reads, beside the web trio's for the
        // same reason and under the same gate. `provider/setup_commit` is NOT
        // here either — it is a BR-10(b) commitment that may park on a human, so
        // it runs on its own task in `handle_client`'s `blocks_on_a_human` path.
        // Nor is REQ-581's `provider/test`, for the other half of that path's
        // reason: it takes the same `may_drive` gate these reads take and adds no
        // presence check, but it *sends* — one completion request to a vendor —
        // so serving it here would park the reader loop for a round trip.
        ProviderSetupPlanParams::METHOD => {
            Some(handle_provider_setup_plan(daemon, conn, id, params))
        }
        ProviderSetupPreviewParams::METHOD => {
            Some(handle_provider_setup_preview(daemon, conn, id, params))
        }
        // REQ-585 ADR-2: `skills/list` is a read of a stored snapshot — no
        // human, no network, no filesystem — so it stays on the synchronous
        // path beside the other session-scoped reads.
        SkillsListParams::METHOD => Some(handle_skills_list(daemon, conn, id, params)),
        // REQ-589 BR-13: the pre-flight is a read too — the stored registry
        // snapshot, the stamped route, and `skill_fit` over the two. It opens
        // no file and resolves no route (ADR-11), so it belongs beside
        // `skills/list` on the synchronous path rather than on a task.
        SkillsPreflightParams::METHOD => Some(handle_skills_preflight(daemon, conn, id, params)),
        // REQ-584 BR-9. **Not on the synchronous path**: unlike `skills/list`,
        // which reads a stored snapshot, this may run BR-3's dev-folder scan —
        // up to eleven directory reads two levels deep, and on macOS the one
        // that can raise the Documents dialog. Parking the connection's reader
        // loop on that is the defect BUG-184 fixed for discovery.
        ProjectsListParams::METHOD => Some(handle_projects_list(daemon, id, params)),
        SessionPermissionsParams::METHOD => {
            Some(handle_session_permissions(daemon, conn, id, params))
        }
        WebRefreshParams::METHOD => Some(handle_web_refresh(daemon, conn, id, params)),
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
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    ok_string(id, &daemon.runtime.web_override(&params, &daemon.events))
}

/// What a gate-failed setup call is announced as (REQ-572 BR-4/AC-4).
///
/// A **kind**, never an identity: not the connection id, not the peer pid, not
/// the requester string the handshake carried. The notice exists to put the
/// refusal in front of the user, so it lands in a session transcript — and a
/// notice that fingerprinted the caller would write into that transcript the
/// very sort of thing the refusal is there to keep out (the payload's own doc
/// says so, and conventions forbid connection internals in an event).
///
/// One sentence, and only the commit publishes it: what the user needs to know
/// is that something which is not their session's client tried to *change* this
/// session's web access (see [`refuse_commit_without_session_access`] for why
/// the two reads stay silent).
const SETUP_REJECTED_ORIGIN: &str = "connection without session access";

/// Refuse a session-scoped call whose `session_id` is longer than this daemon
/// could ever have minted (REQ-572 verify FIX 1a — REQ-569 F9's rule — applied
/// to every `may_drive` seam by BUG-166 residual (c)).
///
/// Each of these handlers takes an attacker-chosen `session_id` bounded only by
/// [`MAX_FRAME`] — ~4 MiB — and each of them then does work proportional to it:
/// `may_drive` hashes it, and the setup commit's refusal used to clone it into
/// an event envelope that every subscriber of the bus holds. A length check
/// ahead of all of that costs a comparison and removes the amplification.
///
/// The seams: the three setup handlers (where FIX 1a introduced it), and —
/// since BUG-166 — `web/override`, `session/permissions`, `session/clear`, and
/// the `session/prompt` spawn, which hash the same attacker-chosen id through
/// the same gate and had no length rule in front of it. `session/attach` keeps
/// its own inline check (its refusal also has an allocation to bound, and a
/// different error-shape argument sits with it there);
/// `permission/respond` needs none, because its `may_drive` takes an owner the
/// *registry* resolved, never the caller's string.
///
/// Called as a line at the top of each handler rather than folded into
/// [`dispatch`], for [`refuse_daemon_wide`]'s reason: these seams are separate
/// lines a future edit drops one at a time, and the mutation check has to be
/// able to delete exactly one (LESSON-502/LESSON-508).
///
/// **Not an oracle** (ADR-B), and this is why it is length-only and comes
/// first: every id of a plausible length draws the same refusal whether it names
/// a live session or nothing at all. What it separates is "no daemon ever minted
/// this" from "this session is not yours", and only the first is a
/// well-formedness fact the caller already knows.
fn refuse_unmintable_session_id(id: &Id, session_id: &SessionId) -> Option<String> {
    (!sessions::within_minted_length(session_id))
        .then(|| error_string(id.clone(), error_code::INVALID_PARAMS, "invalid params"))
}

/// The REQ-572 BR-4 gate on a setup **commit**: [`ConnState::may_drive`], plus
/// the announcement a refusal owes the user.
///
/// The gate itself is [`handle_web_override`]'s, for its reason — changing what
/// a session's model may reach is driving that session. What this adds is the
/// *second* half of BR-4: an RPC error travels back to the caller and nowhere
/// else, so a refusal that stopped there would be visible only to the party it
/// refused. LESSON-505 is the standing form of that mistake, and AC-4 asks for
/// its opposite: the notice is published into the session whose configuration
/// was reached for, which is where its user is looking.
///
/// The event is session-scoped and delivered by the ordinary policy
/// ([`ConnState::may_receive`]), so it reaches the session's own clients. The
/// refused connection is unattached by construction — or it would not be on
/// this path — so it does not receive its own rejection unless it is a monitor
/// that was already entitled to that session's events, which is REQ-568's
/// settled receive-side policy and not this gate's to relitigate.
///
/// **It is not an oracle** (ADR-B). The caller's answer is byte-identical
/// whether the named session exists, belongs to somebody else, or never
/// existed. The *notice* is published only when the named session exists
/// (BUG-166) — a check the caller cannot observe, because it changes what the
/// session's own subscribers receive and never one byte of the refusal.
/// Publishing for a nonexistent id informed nobody entitled and cost two real
/// things: it spent the announcement budget on an audience of zero (the
/// burn attack — one junk-id refusal silenced every real notice the
/// connection owed afterwards), and it handed every monitor-scope subscriber
/// — whose delivery policy is "all sessions", [`ConnState::may_receive`] — a
/// stream of envelopes wearing attacker-chosen session ids.
///
/// **The commit only, and at most once per (connection, session)** (REQ-572
/// verify FIX 1b/1c, re-keyed by BUG-166 — a deviation from BR-4/AC-4's
/// "preview and commit", recorded in the architecture's spec-mapping table).
/// Two findings drove the original narrowing, and both are about the same
/// primitive:
///
///   - the preview is a *read*. It writes nothing, and `session/list` hands any
///     same-UID peer the session ids to aim at — so a refused preview that
///     published was a transcript-injection primitive on demand, at whatever
///     rate the attacker liked. [`handle_web_setup_plan`]'s cry-wolf rationale
///     applies to it unchanged, so the preview now answers `NOT_ATTACHED`
///     silently exactly as the plan does.
///   - the commit's notice is worth publishing — something tried to *change*
///     the capability — but a repeat into the **same session** says nothing
///     the first did not, so it is budgeted per (connection, session)
///     ([`ConnState::may_announce_setup_rejection`], the
///     [`ConnState::may_announce_grant`] precedent). The key carries the
///     session because the audience does: each targeted session's user hears
///     about this connection once, rather than the first target's user
///     hearing for everyone (BUG-166). A refused commit past the budget is
///     still refused; it is only the notice that stops.
///
/// Existence is checked **before** the budget is spent, and the ordering is
/// load-bearing twice over: a junk id must not burn anything, and — because
/// only ids the registry answered for ever reach the set — the set stays
/// bounded by sessions the daemon minted instead of strings the caller can
/// invent (the allocation trap `session/attach`'s length gate exists for).
///
/// Called as a line at the top of `commit` rather than folded into
/// [`dispatch`], for [`refuse_daemon_wide`]'s reason: the mutation check has to
/// be able to delete **one method's** check (LESSON-502/LESSON-508), and a gate
/// applied once for everybody has only one thing to delete.
/// **Two flows, one gate, one notice each** (REQ-579 BR-12). `rejection` is what
/// this seam's refusal is announced as, built only when there is an audience for
/// it — `web/setup_commit` publishes [`Event::WebSetupRejected`] and
/// `provider/setup_commit` publishes [`Event::ProviderSetupRejected`], and the
/// budget is keyed by the *event's own name* so neither can spend the other's
/// allowance. Sharing one key across both would make the suppressed notice a
/// different sentence from the published one, which is precisely what
/// [`ConnState::setup_rejections_announced`]'s "byte-identical duplicate"
/// argument does not license. The closure is what keeps the allocation on the
/// published path: a refusal aimed at a session that does not exist builds no
/// event at all.
fn refuse_commit_without_session_access(
    daemon: &Daemon,
    conn: &ConnState,
    id: &Id,
    session_id: &SessionId,
    rejection: impl FnOnce() -> Event,
) -> Option<String> {
    if conn.may_drive(session_id) {
        return None;
    }
    if daemon.sessions.contains(session_id) {
        let rejection = rejection();
        if conn.may_announce_setup_rejection(rejection.name(), session_id) {
            daemon.events.publish(Some(session_id.clone()), rejection);
        }
    }
    Some(error_string(
        id.clone(),
        error_code::NOT_ATTACHED,
        NOT_ATTACHED_MESSAGE,
    ))
}

/// Answer what enabling web lookup would involve (`web/setup_plan`, REQ-572
/// BR-1/BR-3).
///
/// Read-only: it derives from the config and the engine slot, writes nothing,
/// and remembers nothing (ADR-1 — there is no server-held flow state for a
/// caller to step into). It is gated all the same, for
/// [`handle_session_permissions`]'s reason: reading a session's posture is
/// still reading that session, and a gate that let the read through would make
/// the refusal an oracle for which sessions exist.
///
/// It deliberately publishes **no** rejection. BR-4's announcement is about
/// something trying to *change* this session's capability; firing it for every
/// refused read would hand any same-UID peer a way to write lines into a
/// stranger's session at will — a notice that cried wolf on demand, which is
/// how a real one stops being read.
///
/// [`DaemonRuntime::web_setup_plan`] takes no session id because it reads
/// nothing per-session (TASK-129). The id in the params exists for this gate,
/// which is where "may this connection ask" belongs.
fn handle_web_setup_plan(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: WebSetupPlanParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    ok_string(id, &daemon.runtime.web_setup_plan())
}

/// Render the `[web]` table a set of answers would write, without writing it
/// (`web/setup_preview`, REQ-572 BR-7).
///
/// Gated like the commit and **silent like the plan** on refusal (REQ-572
/// verify, FIX 1b — a recorded deviation from AC-4's "preview and commit", see
/// [`refuse_commit_without_session_access`]).
///
/// It once announced, on the reasoning that a preview is a step of the
/// enablement flow and a stranger stepping into that flow is worth telling the
/// user about. What that missed is that a preview *writes nothing*, so the
/// notice was the only effect an unattached caller could produce — and with
/// session ids readable from an ungated `session/list`, that made a refused
/// preview a same-UID transcript-injection primitive callable at will.
/// [`handle_web_setup_plan`]'s own cry-wolf argument applies to it word for
/// word: a notice that can be made to fire on demand is one users learn to read
/// past, which costs the commit's rejection the attention it exists for.
///
/// A candidate the daemon refuses comes back as the runtime's own
/// `WEB_SETUP_INVALID` carrying the validator's sentence — never as a preview
/// with a note attached.
fn handle_web_setup_preview(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: WebSetupPreviewParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    match daemon.runtime.web_setup_preview(&params) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Answer what registering a provider would involve (`provider/setup_plan`,
/// REQ-579 BR-3/BR-4/BR-7).
///
/// [`handle_web_setup_plan`]'s twin, gate for gate, and deliberately a copy of
/// it rather than a generalisation (ADR-1): read-only, session-scoped, and
/// **silent on refusal**.
///
/// The silence is the same argument, and it applies here at least as strongly.
/// A foreign connection is answered [`error_code::NOT_ATTACHED`] — the code the
/// web reads already use for a caller without session access; there is no
/// separate "rejected non-user" wire code, and REQ-579's own
/// `provider_setup_rejected_nonuser` is an **event**, published by the commit
/// alone (BR-12, LESSON-513). Publishing one here would hand any same-UID peer a
/// way to write lines into a stranger's session on demand, at whatever rate it
/// liked, which is how a notice that matters stops being read.
///
/// [`DaemonRuntime::provider_setup_plan`] takes no session id because it reads
/// nothing per-session. The id in the params exists for this gate, which is
/// where "may this connection ask" belongs.
fn handle_provider_setup_plan(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: ProviderSetupPlanParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    ok_string(id, &daemon.runtime.provider_setup_plan())
}

/// Render the rows a registration would write, without writing them
/// (`provider/setup_preview`, REQ-579 BR-9).
///
/// [`handle_web_setup_preview`]'s twin: gated like the commit and silent like
/// the plan. A preview *writes nothing*, so an announcement would be the only
/// effect an unattached caller could produce — and with session ids readable
/// from `session/list`, that makes a refused preview a same-UID
/// transcript-injection primitive callable at will. LESSON-513 is the standing
/// form of the rule this follows: the event belongs to the commit, whose refusal
/// is about something trying to **change** the configuration.
///
/// A candidate the daemon refuses comes back as the runtime's own
/// `PROVIDER_SETUP_INVALID` carrying the refusal's sentence — never as a preview
/// with a note attached, which is what the warnings are for.
fn handle_provider_setup_preview(
    daemon: &Daemon,
    conn: &ConnState,
    id: Id,
    params: Value,
) -> String {
    let params: ProviderSetupPreviewParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    match daemon.runtime.provider_setup_preview(&params.candidate) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Write the candidate `[web]` table and make the capability live
/// (`web/setup_commit`, REQ-572 BR-8/AC-3).
///
/// **This function is the entire path to that write**, and the same channel
/// argument [`handle_web_override`] records applies unchanged: the commit is a
/// client RPC, tool dispatch holds a `ToolContext` rather than a
/// `DaemonRuntime`, so a model that emits a tool call named `web/setup_commit`
/// reaches the tool registry, finds no such tool, and is told so. AC-4's "a
/// model tool call attempting the setup RPC is rejected" is a fact about which
/// channel this hangs off, not a check that could be omitted — and the
/// `may_drive` gate below is the *defense in depth* BR-4 asks for on top of it,
/// which is why it needs a test of its own rather than resting on the
/// structural argument (LESSON-508).
async fn handle_web_setup_commit(
    daemon: &Daemon,
    conn: &ConnState,
    id: Id,
    params: Value,
) -> String {
    let params: WebSetupCommitParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if let Some(refusal) =
        refuse_commit_without_session_access(daemon, conn, &id, &params.session_id, || {
            Event::WebSetupRejected(WebSetupRejected {
                origin: SETUP_REJECTED_ORIGIN.to_owned(),
            })
        })
    {
        return refusal;
    }
    // BR-10(b) / REQ-575: writing the `[web]` egress table is a daemon-wide
    // commitment, so it takes the same presence check `model/confirm` and
    // `model/set` take — the *same* function, degrading (not refusing) where no
    // mechanism exists (REQ-570 BR-8). Ordered **after** the session-access gate
    // deliberately: a caller that may not drive this session at all is refused
    // with no prompt appearing on anyone's screen (BR-2, the `model/confirm`
    // ordering rationale).
    if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await {
        return refusal;
    }
    match daemon.runtime.web_setup_commit(&params, &daemon.events) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Register the candidate provider, route the tiers it was given, and make both
/// live (`provider/setup_commit`, REQ-579 BR-10/BR-12/BR-15).
///
/// **This function is the entire path to that write**, and the channel argument
/// [`handle_web_setup_commit`] records applies unchanged: the commit is a client
/// RPC, tool dispatch holds a `ToolContext` rather than a `DaemonRuntime`, so a
/// model that emits a tool call named `provider/setup_commit` reaches the tool
/// registry, finds no such tool, and is told so (pinned by
/// `no_tool_can_commit_a_provider_setup_and_no_harness_source_names_it`). BR-12's
/// "the model cannot register a provider" is a fact about which channel this
/// hangs off, not a check that could be omitted — and the session gate below is
/// the *defense in depth* on top of it, which is why it needs a test of its own
/// rather than resting on the structural argument (LESSON-508 rule 2).
///
/// Three gates, in this order and for these reasons:
///
/// 1. [`refuse_unmintable_session_id`] — a `session_id` no daemon could have
///    minted is refused before anything hashes it;
/// 2. [`refuse_commit_without_session_access`] — `may_drive`, plus the
///    announcement BR-12 owes the *session's own user*, because an RPC error
///    travels back to the caller and nowhere else (LESSON-505). The commit
///    alone announces: `provider/setup_plan` and `provider/setup_preview` write
///    nothing, so a notice on them would be the only effect an unattached caller
///    could produce, at whatever rate it liked (LESSON-513);
/// 3. [`refuse_unattested_commitment`] — BR-10(b). Ordered **after** the session
///    gate deliberately: a caller that may not drive this session at all is
///    refused with no prompt appearing on anyone's screen (REQ-575's
///    `model/confirm` ordering rationale).
///
/// The completion event is published here rather than by the runtime, unlike
/// `web/setup_commit`'s: the rejection above is already this layer's to publish,
/// and one publisher for a flow is easier to reason about than two. It carries
/// no key and no endpoint (BR-2) — the payload has nowhere to put either — and
/// it fires only on a commit that **applied**: an `applied: false` answer
/// registered nothing, and the client says so in its own words.
async fn handle_provider_setup_commit(
    daemon: &Daemon,
    conn: &ConnState,
    id: Id,
    params: Value,
) -> String {
    let params: ProviderSetupCommitParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if let Some(refusal) =
        refuse_commit_without_session_access(daemon, conn, &id, &params.session_id, || {
            Event::ProviderSetupRejected(ProviderSetupRejected {
                // The method, never an identity and never the candidate: this
                // sentence lands in a user's transcript, and the thing it is
                // refusing carries a credential reference (the payload's own
                // doc, LESSON-525).
                method: ProviderSetupCommitParams::METHOD.to_owned(),
            })
        })
    {
        return refusal;
    }
    if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await {
        return refusal;
    }
    match daemon
        .runtime
        .provider_setup_commit(&params.candidate, params.expect_digest.as_deref())
    {
        Ok(result) => {
            if result.applied {
                daemon.events.publish(
                    Some(params.session_id.clone()),
                    Event::ProviderSetupCompleted(ProviderSetupCompleted {
                        // The id and the bindings are the daemon's answer, not
                        // the request's: two bindings naming one tier are one
                        // row, and what landed is what the user is owed. The kind
                        // and the model are the candidate's, because those two
                        // are written verbatim — a commit that returned `Ok` is
                        // the daemon saying this candidate is what it now holds.
                        provider_id: result.provider_id.clone(),
                        kind: params.candidate.kind,
                        model: params.candidate.model.trim().to_owned(),
                        bindings: result.bindings.clone(),
                        // The daemon's answer again, and specifically **not**
                        // `params.candidate.endpoint`: the commit result's host
                        // is the dial-time parser's reading of the endpoint that
                        // was actually written, so it carries no userinfo, path
                        // or query for a transcript to keep (LESSON-529).
                        dial_host: result.dial_host.clone(),
                    }),
                );
            }
            ok_string(id, &result)
        }
        Err(err) => error_from(id, err),
    }
}

/// Make one real call to a configured provider and report what came back
/// (`provider/test`, REQ-581 BR-1/AC-6).
///
/// Gated like the setup trio's **reads**, and deliberately not like their
/// commit:
///
/// 1. [`refuse_unmintable_session_id`] — a `session_id` no daemon could have
///    minted is refused before anything hashes it, as at every other
///    `may_drive` seam;
/// 2. `may_drive` → a **silent** [`error_code::NOT_ATTACHED`].
///    [`refuse_commit_without_session_access`] is not what runs here, and the
///    difference is the announcement: a probe that published its own refusal
///    would hand any same-UID peer a line in a stranger's transcript on demand,
///    at whatever rate it liked (LESSON-513, and
///    [`handle_provider_setup_plan`]'s cry-wolf argument word for word). The
///    notice belongs to the flows that try to **change** the configuration;
///    this one changes nothing.
///
/// There is no presence attestation, and the omission is a decision rather than
/// an oversight (BR-2): BR-10(b) is about a daemon-wide *commitment*, and this
/// method writes no config — the consent that matters is the client-side
/// confirm the user answers before the request is issued at all. What the gate
/// above buys is AC-6: a foreign connection, and a daemon-spawned descendant the
/// REQ-569 ancestry gate bars from session access, cannot make the user's
/// provider spend on their behalf. The refusal lands *before* the runtime is
/// touched, so nothing is dialed and nothing is billed.
///
/// It runs on `handle_client`'s own-task path rather than in [`dispatch`]
/// because it blocks on the network: one completion request to a vendor, which
/// can take as long as a Touch ID prompt does. Served inline it would park every
/// other RPC on this connection for the length of that round trip (LESSON-518).
///
/// The `provider_tested` announcement is [`DaemonRuntime::provider_test`]'s to
/// publish, on every outcome, because a call that *happened* is news the
/// session's other clients are owed — including where it left the health the
/// next turn routes by. A refusal here made no call, so it announces nothing.
async fn handle_provider_test(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: ProviderTestParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    match daemon
        .runtime
        .provider_test(&daemon.events, &params.session_id, &params.provider_id)
        .await
    {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Read or set a session's permission level (`session/permissions`, REQ-560
/// ADR-D).
///
/// **This function is the entire path to a session's level.** The setter behind
/// it is reached through the runtime, and tool dispatch holds a `ToolContext`
/// rather than a `DaemonRuntime` — so a model that emits a tool call named
/// `session/permissions`, or a tool result containing the text
/// `/permissions full`, reaches the tool registry, finds no such tool, and is
/// told so. The requirement's "never inferable from model output, tool output,
/// or file content" is a fact about which channel this code hangs off, not a
/// check that could be omitted.
///
/// Gated on attachment exactly as `web/override` is, and for the same reason:
/// changing what a session is allowed to run is driving that session. The check
/// sits before the runtime is touched, so an unattached caller cannot read a
/// session's existence out of which refusal it got (ADR-B).
///
/// A read (`level: None`) is gated identically. Reading a session's posture is
/// still reading that session, and splitting the gate would make the refusal an
/// oracle for which sessions exist.
fn handle_session_permissions(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SessionPermissionsParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    ok_string(
        id,
        &daemon.runtime.session_permissions(&params, &daemon.events),
    )
}

/// Switch or read a session's transcript (`session/transcript`, REQ-611 ADR-6).
///
/// **Modelled on [`handle_session_permissions`], including the argument that
/// makes BR-3 structural.** Tool dispatch holds a `ToolContext`, not a
/// `DaemonRuntime`, so a model that emits a tool call named
/// `session/transcript` — or a tool result containing the text `/transcript
/// off` — reaches the tool registry, finds no such tool, and is told so. "The
/// model cannot turn the record off" is a fact about which channel this
/// function hangs off, not a check that could be omitted or forgotten.
///
/// Gated on [`ConnState::may_drive`] for **all three** actions, `Status`
/// included, and that is ADR-6 rather than an over-tight read. On and off are
/// mutations; and the status answer names the *file*, which is boundary content
/// of the class REQ-569 BR-10 gives `cwd`. A monitor is entitled to see
/// `transcript_state` — that it is recording — and entitled to none of where.
/// Do not relax this to `may_receive` for the convenience of a read.
///
/// The gate sits before the runtime is touched, so an unattached caller cannot
/// read a session's existence out of which refusal it got (ADR-B).
///
/// # Why it is `async`, and what that costs
///
/// [`DaemonRuntime::session_transcript`] flushes the sink so that its answer
/// describes a file the writer thread has actually opened, refused or resumed —
/// see that method for why answering earlier would be answering wrongly. A
/// flush waits on the disk, and a method that waits on anything outside this
/// process must not wait on the reader loop (LESSON-518), so this runs on its
/// own task in [`handle_client`] rather than inline in [`dispatch`]. That is a
/// deliberate departure from its `session/permissions` twin, which computes its
/// answer in memory and can reply from the loop.
async fn handle_session_transcript(
    daemon: &Daemon,
    conn: &ConnState,
    id: Id,
    params: Value,
) -> String {
    let params: SessionTranscriptParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    ok_string(
        id,
        &daemon
            .runtime
            .session_transcript(&params, &daemon.events)
            .await,
    )
}

/// Evict a cached document so the next lookup re-fetches (`web/refresh`,
/// REQ-563 AC-10).
///
/// The same channel argument as [`handle_web_override`] applies: this is a
/// client RPC, and tool dispatch cannot reach one. It differs in being
/// fallible — a cached file that will not unlink is the one outcome that would
/// otherwise leave the user's next lookup silently reading the copy they asked
/// to drop, so it comes back as an error rather than as `absent`.
fn handle_web_refresh(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // BR-10(a) / BUG-162: see `refuse_daemon_wide`.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
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
    // Kept for the transcript hand-off below: `resolve_from` consumes the
    // waiter, so after it there is no longer anything to ask whose session this
    // answer belonged to.
    let owner = pending.owner_of(&params.request_id);
    if let Some(owner) = &owner {
        if !conn.may_drive(owner) {
            return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
        }
    }
    // `resolve_from`, never `resolve` (REQ-585 ADR-7): an **addressed** waiter
    // — a skill's dynamic-context consent — may only be answered by the
    // connection the question was put to, and `resolve` names no connection at
    // all, so it now refuses such a waiter outright. Naming the answering
    // connection here is what lets the entitled answer through and leaves an
    // older client's fall-through `prompter.ask` inert, with the prompt still
    // standing for the client that was actually asked. Every pre-REQ-585 prompt
    // is unaddressed and is unaffected: its delivery policy is attachment, and
    // the `may_drive` check above is where that is enforced.
    let resolved = pending.resolve_from(&params.request_id, params.outcome.clone(), conn.id);
    // REQ-611 BR-4 / BR-10: the answer, to the session's transcript, and only
    // when it actually settled the request. `resolve_from` answers `false` for
    // a request id with no waiter — a late or duplicate reply, which this
    // handler has always acknowledged rather than refused — and for an
    // addressed waiter this connection was not the addressee of. Recording
    // either would put an answer in the file that changed nothing, which is
    // exactly the kind of thing a record must not do.
    if resolved {
        if let Some(owner) = &owner {
            daemon.runtime.transcript_permission_decided(
                owner,
                &params.request_id,
                &params.outcome,
            );
        }
    }
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
async fn handle_model_confirm(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // BR-10(a) / BUG-162, the bug's headline case: this method commits a
    // multi-gigabyte download and a daemon-wide model change, and took no
    // connection context at all. See `refuse_daemon_wide` for why the check is a
    // standing rule rather than a raiser-identity one.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    // BR-10(b): and because the blast radius is the whole machine, a human too.
    // Ordered after (a) deliberately — a connection that may not act here at all
    // should be refused without a prompt appearing on somebody's screen.
    if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await {
        return refusal;
    }
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
async fn handle_model_set(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // BR-10(a) / BUG-162: the same daemon-wide commitment as `model/confirm`,
    // reached by a different door — so it takes the same two layers.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await {
        return refusal;
    }
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
fn handle_config_get(daemon: &Daemon, conn: &ConnState, id: Id) -> String {
    // BR-10(a) / BUG-162: exposes provider endpoints and `auth_ref` names (not
    // secret material) to any connection. Low severity — the same-UID
    // file-access refutation in the bug applies — but gated for the same reason
    // as its neighbours: one rule at seven seams, not six and an exception.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    ok_string(
        id,
        &ConfigGetResult {
            snapshot: daemon.runtime.config_snapshot(),
        },
    )
}

/// Apply a configuration mutation (`config/set`), rejecting it on validation
/// failure (e.g. a raw key in `auth_ref`).
///
/// **A BR-10(b) daemon-wide commitment (REQ-576).** `config/set` durably
/// rewrites `config.toml` and live-swaps the daemon-wide in-memory config, and
/// its `ConfigUpdate` reaches the egress boundary (`RegisterProvider` names a
/// remote endpoint) and the privacy boundary itself (`SetPrivacyBoundary`). That
/// blast radius is the whole machine, so — like `model/confirm`, `model/set` and
/// `web/setup_commit` — it runs the shared [`refuse_unattested_commitment`] on
/// top of the ancestry gate, and moved off the reader-loop `dispatch` onto
/// `handle_client`'s `blocks_on_a_human` task so the prompt cannot stall the
/// connection.
///
/// **This reverses the BUG-162 posture** once recorded here. That posture kept
/// config/set at layer (a) only, reasoning that config lives at
/// `base_dir/config.toml` — which a same-UID process can already edit — so the
/// RPC "removes immediacy, not capability." REQ-570/REQ-575 established that this
/// mitigation is insufficient for a *commitment*: the same "can edit the file
/// then restart" argument applies to `model/set`, which is gated anyway, and the
/// immediacy the RPC removes — a silent, no-restart live swap of the egress and
/// privacy config — is exactly the quiet path an attacker wants. So it attests.
async fn handle_config_set(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // BR-10(a) / BUG-162: the ancestry gate stops the daemon's own children.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    // BR-10(b) / REQ-576: and because the blast radius is the whole machine — an
    // egress endpoint, the privacy boundary — a human too. Degrades (not refuses)
    // where no presence mechanism exists (REQ-570 BR-8), so shipped builds and
    // `teton provider add` gain no new prompt. Ordered after the ancestry gate so
    // a caller that may not act here at all is refused without a prompt appearing.
    if let Some(refusal) = refuse_unattested_commitment(daemon, conn, &id).await {
        return refusal;
    }
    let params: ConfigSetParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    // The id, read before the update is moved onto the apply: the notice below
    // describes what was **recorded**, which only the applied config knows.
    let registered = match &params.update {
        teton_protocol::methods::ConfigUpdate::RegisterProvider(provider) => {
            Some(provider.id.clone())
        }
        _ => None,
    };
    match daemon.runtime.apply_config_update(params.update) {
        Ok(()) => ok_string(
            id,
            &ConfigSetResult {
                applied: true,
                // REQ-586 OQ-6 as amended: a registration that records a big
                // context window says so once, and `teton provider add
                // --max-context` is the surface that gets it here. Composed by
                // the daemon because every figure in it is the budget
                // derivation's, and a thin client re-deriving a budget is the
                // second source BR-8 forbids.
                budget_notice: registered
                    .and_then(|provider| daemon.runtime.provider_budget_notice(&provider)),
            },
        ),
        Err(err) => error_from(id, err),
    }
}

/// Serve the authoritative cost report from the ledger (`cost/query`, BR-2).
fn handle_cost_query(daemon: &Daemon, conn: &ConnState, id: Id) -> String {
    // BR-10(a) / BUG-162: returns a daemon-wide roll-up spanning *every* session
    // (phase names, provider ids, token counts) — a genuine cross-session
    // metadata read, directly adjacent to the payload reduction REQ-569 landed
    // for `session/list`.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    match daemon.runtime.cost_report() {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Create a session (`session/create`), attaching the creating connection to it
/// (REQ-568 BR-1).
/// Which boundary events a starting session owes its watchers (REQ-597 BR-5 and
/// the System Model's `boundary_defaults_applied`).
///
/// Pure, over plain data, so the *rule* can be tested exhaustively without a
/// daemon, a socket, or a home directory — the shape this codebase already uses
/// for decisions whose mechanism is awkward to reach. The caller publishes what
/// this returns and decides nothing itself.
///
/// # The warning's condition is a conjunction, and both halves carry weight
///
/// A broad root with the shipped set in force is the ordinary case and says
/// nothing. An empty set at a `project` root is a deliberate choice about a
/// directory the user chose. Only together do they mean *nothing is protected,
/// and this session can reach everything you own*.
///
/// The empty-set half reads the **effective** set, never the
/// `disable_default_boundaries` flag. After REQ-597 the flag alone is not the
/// condition: a config that opts out but declares one row of its own is still
/// protected by that row, and warning there would be crying wolf. Keying on the
/// flag would also fire this on every stock machine if the composer ever
/// regressed, turning the alarm into noise precisely when it started being
/// wrong.
fn session_start_boundary_events(posture: BoundaryPosture, kind: RootKind) -> Vec<Event> {
    let mut events = Vec::new();
    if posture.effective_is_empty && matches!(kind, RootKind::Home | RootKind::FilesystemRoot) {
        events.push(Event::UnboundedRootWarning(UnboundedRootWarning {
            root_kind: kind,
        }));
    }
    if posture.builtin_count > 0 {
        events.push(Event::BoundaryDefaultsApplied(BoundaryDefaultsApplied {
            count: posture.builtin_count,
        }));
    }
    events
}

fn handle_session_create(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    // BR-10(a) / BUG-162: the odd one of the seven. This handler already
    // *receives* the connection and simply never consulted the gate, so a daemon
    // descendant that BR-4 forbids from attaching could still create and drive
    // its **own** session, spending the user's provider credits — outside BR-4's
    // literal wording, inside REQ-569 ADR-A's rationale.
    if let Some(refusal) = refuse_daemon_wide(conn, &id) {
        return refusal;
    }
    let params: SessionCreateParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };

    // BUG-147: the cwd becomes this session's tool jail. Refuse a relative or
    // nonexistent one up front — jailing tools to a directory that isn't there
    // reproduces the every-tool-fails session this validates against. The
    // validator is the one `session/set_cwd` uses too (REQ-583 BR-6/BR-7: one
    // grammar, two spellings), and its refusal names the path.
    if let Some(cwd) = &params.cwd {
        if let Err(refusal) = validate_session_cwd(cwd) {
            return error_string(id, error_code::INVALID_PARAMS, &refusal.to_string());
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

            // REQ-585 BR-1: the session's skills, discovered once, here. Before
            // the response, so the `skills/list` the client sends on reading it
            // cannot arrive ahead of the registry it is asking for; and after
            // the create, because the registry is stored against the id that
            // create just minted.
            rebuild_session_skills(daemon, &summary.session_id, summary.cwd.as_deref());

            // REQ-583 BR-6: the root the daemon settled on, probed from the
            // same path — or the same fallback — every turn will jail to, so
            // what the CLI's banner and launch notice render is what the tools
            // will enforce (ADR-1: one derivation, on the side that enforces).
            //
            // REQ-611 moved this derivation above the publishes below rather
            // than leaving it between them: the transcript hand-off wants the
            // display form, and it has to run before the first session-scoped
            // event is published or the sink would be offered an envelope for a
            // session it has not been told about.
            let root = daemon.runtime.session_root_for(summary.cwd.as_deref()).view;

            // REQ-611 BR-2 / ADR-3: the sink learns the session exists, and
            // whether it records from its first record. Here rather than in
            // `SessionRegistry::create` because the registry owns none of this
            // (ADR-3) and holds none of its three inputs — the config's
            // `[transcript] enabled`, the root's display form derived one line
            // above, and the bus's sequence number.
            //
            // **Before every publish below.** With `enabled = true` the file is
            // opened by this call, so `transcript_opened` is the first record in
            // it (BR-2's "a session created while enabled opens a file before
            // its first prompt") and the session's own first events — a
            // structured session's `phase_transition`, the boundary posture —
            // land in it rather than arriving for a session the sink does not
            // yet know.
            daemon.runtime.transcript_session_created(
                &summary.session_id,
                &root.display,
                daemon.events.current_seq(),
            );

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
            // REQ-597 BR-5 / System Model. Both derived from one reading of the
            // config, and published here — before the response, alongside the
            // phase transition above — so a client reading the create result
            // cannot receive the session's first event after it.
            //
            // Published on the bus, session-scoped, rather than routed to the
            // creating connection: every attached client learns. Routing the
            // warning to the one connection would reproduce the failure
            // REQ-571 BR-4 names — an audit signal reaching only the party it
            // indicts.
            for event in session_start_boundary_events(daemon.runtime.boundary_posture(), root.kind)
            {
                daemon
                    .events
                    .publish(Some(summary.session_id.clone()), event);
            }

            ok_string(
                id,
                &SessionCreateResult {
                    session_id: summary.session_id,
                    root: Some(root),
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
/// A *routed* event is addressed to particular connections rather than
/// published: every other event goes on the bus and is filtered per connection
/// by [`forward_events`]. The daemon has three of them — the two consent events
/// REQ-569 BR-6 addresses to named surfaces through
/// [`crate::consent::ConsentSurfaces`], and the handshake's lifecycle replay,
/// which is one connection's catch-up and goes to that connection alone
/// (BUG-177; see [`do_handshake`]). They still take their sequence number from
/// the bus, so a routed frame can never collide with a broadcast one on the
/// same connection.
///
/// The session, when there is one, rides on the envelope rather than in the
/// payload — [`Event`] is flattened, so a `session_id` field on the payload
/// would emit the key twice.
///
/// A consent envelope is session-scoped while one recipient — the requester —
/// is by definition *not* attached to that session, which looks like a hole in
/// REQ-568's scoping and is not one. These frames never cross
/// [`forward_events`], so the delivery policy is not being bypassed; and the id
/// is not news to anyone who receives it, because the requester is the
/// connection that named it and the other recipients are attached to it.
fn routed_event_frame(daemon: &Daemon, session_id: Option<SessionId>, event: Event) -> String {
    routed_frame(&daemon.events, session_id, event)
}

/// [`routed_event_frame`] over the bus alone.
///
/// The daemon's fourth routed event — REQ-585's addressed permission request —
/// is built by [`AddressedRoute`], which holds the bus and the surface registry
/// and deliberately **not** the [`Daemon`] that owns them: the daemon holds the
/// runtime, the runtime holds the route, and a route holding the daemon back
/// would close that ring into a leak. Splitting the body rather than duplicating
/// it is what keeps the sequence number coming from one place — `next_seq`, the
/// bus's own counter — so a routed frame still cannot collide with a broadcast
/// one on the same connection (BUG-177).
fn routed_frame(events: &EventBus, session_id: Option<SessionId>, event: Event) -> String {
    let envelope = EventEnvelope::new(events.next_seq(), session_id, event);
    // Infallible for these payloads (plain strings and closed enums); a
    // hypothetical failure costs the prompt a delivery, which the bounded
    // window already turns into a refusal.
    serde_json::to_string(&Notification::new(EVENT_METHOD, envelope)).unwrap_or_default()
}

/// The daemon's implementation of REQ-585 ADR-7's addressed delivery: put one
/// [`PermissionRequest`] in front of one connection, and nobody else.
///
/// **Without this the feature asks nobody.** [`PermissionGate::authorize_skill`]
/// answers `SkillConsent::Unanswerable` on a gate with no route, deliberately —
/// the only honest fallback would be the event bus, and the bus is what
/// addressing exists to keep a skill consent off (a pre-REQ-585 client attached
/// to the same session would receive a subject it cannot recognize and answer it
/// by reading stdin).
///
/// It holds the two things the model layer must not know about — the live
/// surface registry and the bus's sequence counter — and nothing else.
struct AddressedRoute {
    surfaces: Arc<ConsentSurfaces>,
    events: Arc<EventBus>,
}

impl AddressedPermissionDelivery for AddressedRoute {
    fn deliver(
        &self,
        connection: ConnectionId,
        session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool {
        // Session-scoped on the envelope, exactly as the published form is: the
        // frame carries the same shape a client already parses, and only its
        // *delivery* is narrowed (ADR-7). The `seq` comes from the bus, so this
        // frame cannot wear a number a broadcast one on this connection will
        // also wear (BUG-177's shape).
        let frame = routed_frame(
            &self.events,
            Some(session_id.clone()),
            Event::PermissionRequest(request),
        );
        self.surfaces.deliver_to(connection, &frame)
    }
}

/// Build the surface registry this daemon will use and install the addressed
/// route over it on `runtime`, answering with the registry.
///
/// One function, called by both constructors, because a daemon that forgot to
/// wire it is a daemon whose skill consents are silently unanswerable — a
/// fail-closed hole with no error message anywhere. `set` is best-effort: two
/// daemons over one runtime is not a production shape, and the first one's route
/// is as good as the second's.
fn wire_addressed_delivery(
    runtime: &Arc<DaemonRuntime>,
    events: &Arc<EventBus>,
) -> Arc<ConsentSurfaces> {
    let surfaces = Arc::new(ConsentSurfaces::new());
    runtime.install_addressed_delivery(Arc::new(AddressedRoute {
        surfaces: Arc::clone(&surfaces),
        events: Arc::clone(events),
    }));
    surfaces
}

/// Give `runtime` the presence check its two durable consent-answer writes owe
/// BR-10(b) (REQ-591 D-1).
///
/// Wired here for [`wire_addressed_delivery`]'s reason — the verifier belongs to
/// the daemon and the daemon holds the runtime — and **replaceable**, which that
/// one is not: [`Daemon::with_presence_verifier`] states a verifier *after* the
/// constructor already wired the shipped one, and a first-writer-wins slot would
/// leave an injected verifier inert on exactly the paths a fixture installed it
/// to exercise.
///
/// That paragraph described the slot before it was one:
/// `install_commitment_attestation` was `let _ = OnceLock::set`, so the second
/// call really was discarded and this comment was a claim the code did not have.
/// `runtime::tests::a_typed_project_skill_is_acknowledged_first::an_injected_verifier_is_the_one_the_durable_write_asks`
/// is what now holds it to it.
fn wire_commitment_attestation(runtime: &Arc<DaemonRuntime>, verifier: &Arc<dyn PresenceVerifier>) {
    runtime.install_commitment_attestation(Arc::new(VerifiedCommitment {
        verifier: Arc::clone(verifier),
    }));
}

/// [`CommitmentAttestation`] over this daemon's [`PresenceVerifier`] — BR-10(b)
/// for the two writes that are answers rather than RPCs (REQ-591 D-1).
///
/// The body is [`refuse_unattested_commitment`]'s, reached through
/// [`attest_commitment`] so the two cannot drift: same availability degrade,
/// same connection-keyed synthetic binding id, same live single-use check with
/// nothing recorded. What differs is only what a refusal *is* — a JSON-RPC error
/// there, a sentence for the caller to log here, because there is no frame to
/// refuse: the human already answered the prompt, and the only thing being
/// withheld is the machine-wide half of their answer.
struct VerifiedCommitment {
    verifier: Arc<dyn PresenceVerifier>,
}

impl CommitmentAttestation for VerifiedCommitment {
    fn attest_daemon_wide_commitment(&self, addressee: ConnectionId) -> Result<(), String> {
        match attest_commitment(self.verifier.as_ref(), addressee) {
            CommitmentStanding::Attested => Ok(()),
            CommitmentStanding::NoMechanism(reason) => {
                eprintln!("{}", commitment_degraded_line(reason));
                Ok(())
            }
            CommitmentStanding::Refused(refusal) => Err(refusal_message(&refusal).to_owned()),
        }
    }
}

/// What a daemon-wide commitment's presence check answered — the shared body of
/// [`refuse_unattested_commitment`] and [`VerifiedCommitment`] (REQ-591 D-1).
///
/// Three outcomes rather than a `Result`, because "no mechanism on this build"
/// and "a human was verified" both proceed and must not be *told apart by
/// accident*: BR-8 requires the first to be stated rather than silent, and a
/// two-armed answer would let a caller print the wrong sentence for it.
enum CommitmentStanding {
    /// A human proved presence for this connection, just now.
    Attested,
    /// This build has no mechanism, so BR-10(b) is unavailable and BR-10(a)
    /// stands alone. Carries the reason, which the caller states.
    NoMechanism(UnavailableReason),
    /// A mechanism exists and was not satisfied.
    Refused(AttestationRefusal),
}

/// Ask `verifier` whether a human stands behind a daemon-wide commitment by
/// `subject`, right now (REQ-570 BR-10(b), REQ-591 D-1).
///
/// One body for every daemon-wide commitment this process performs — the four
/// gated RPCs through [`refuse_unattested_commitment`], and the two consent
/// answers through [`VerifiedCommitment`]. They are the same question about the
/// same machine, and BUG-162's lesson is what a second copy of it costs: a
/// `request_id` minted in one scope and honoured in a wider one, because two
/// places decided separately what "may this connection speak for the machine"
/// means.
///
/// The synthetic binding id is keyed to the connection, so two concurrent
/// commitments cannot share one human's answer. Nothing is recorded in the
/// attestation registry: there is no consent `request_id` to bind to, the check
/// is used once immediately, and BR-6's single-use property holds by
/// construction rather than by bookkeeping.
fn attest_commitment(verifier: &dyn PresenceVerifier, subject: ConnectionId) -> CommitmentStanding {
    if let MechanismAvailability::Unavailable(reason) = verifier.availability() {
        return CommitmentStanding::NoMechanism(reason);
    }
    let request = RequestId::from(format!("commit-{subject:?}"));
    match crate::runtime::block_in_place_if_multithread(|| verifier.verify(subject, &request)) {
        Ok(_) => CommitmentStanding::Attested,
        Err(refusal) => CommitmentStanding::Refused(refusal),
    }
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
    let frame = routed_event_frame(
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
/// # It names both parties, not just an arm (R1)
///
/// The first cut carried the scope, the requester descriptor, and
/// `self_approved` — and presented that flag as the thing a reader acts on. It
/// is not: `self_approved` is a fact about *connection ids*, so it is `false`
/// for the whole of BR-6's arm 1, **including** the case where one actor holds
/// two connections and has the first approve the second's request. That is the
/// same blindness that made the removed monitor consent path dangerous (ADR-A-1:
/// two different `ConnectionId`s, so it did not even register as self-approval),
/// and an announcement carrying only the flag renders it as the benign case.
///
/// So the approver's descriptor goes on the wire beside the requester's. The
/// daemon cannot decide whether two connections are two people — it never can,
/// which is ADR-A's residual — but it can hand a human the *relation* and let
/// them see that the same name asked and answered.
///
/// Nothing here names the session: a grant announcement goes to every
/// connection, so putting a session id in it would leak the id namespace to
/// connections BR-10 keeps it from.
///
/// # Bounded per requesting connection (R3)
///
/// Minting is attacker-triggerable — `session/attach` in a loop, self-approved —
/// and this event reaches *every* client, so an unbounded announcement is an
/// unbounded flood on screens belonging to people who are not being asked
/// anything. [`ConnState::may_announce_grant`] rate-limits it per connection and
/// hands back the arrears, which ride out on the next announcement that gets
/// through: the bound costs a reader the notices, never the knowledge that there
/// were notices.
/// # What made it attestable (REQ-570 BR-9, AC-9)
///
/// `attestation` is what makes `self_approved`'s blindness recoverable. That
/// flag is a fact about connection ids, so an attacker holding two connections
/// is announced as an ordinary peer approval; the attestation method cannot be
/// produced without a human at the machine, so anything other than `"none"` is
/// a claim about a *person* rather than about a pair of ids.
fn publish_grant_minted(
    daemon: &Daemon,
    conn: &ConnState,
    scope: ConsentScope,
    approver: &str,
    self_approved: bool,
    attestation: crate::attest::AttestationMethod,
) {
    let Some(suppressed) = conn.may_announce_grant(daemon.grant_announcement_window) else {
        return;
    };
    daemon.events.publish(
        None,
        Event::SessionGrantMinted(SessionGrantMinted {
            scope,
            requester: conn.requester.clone(),
            approver: approver.to_owned(),
            self_approved,
            suppressed,
            attestation: attestation.as_str().to_owned(),
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

    let frame = routed_event_frame(
        daemon,
        session_id.clone(),
        Event::AttachConsentRequested(AttachConsentRequested {
            request_id: request_id.clone(),
            scope,
            requester: requester.to_owned(),
        }),
    );
    // BUG-163: a prompt that reached **nobody** is the one outcome worth saying
    // out loud, and until now it was the one the daemon could not distinguish.
    //
    // `deliver_request` returns how many surfaces the frame reached, and this
    // call discarded it — so "asked three clients" and "asked no one" produced
    // byte-identical logs. The second is not a slow success: nothing will
    // answer, and the request is already committed to waiting out its full
    // consent window before anyone learns otherwise.
    //
    // Logged at delivery rather than at the timeout deliberately. The timeout is
    // 30s and BUG-163's test deadline is 20s, so a line hung off the timeout
    // path would never appear in the failing run that needs it — and more
    // generally, "nobody was asked" is knowable *now* and the 30-second wait
    // adds nothing to it.
    //
    // Downstream of every cause rather than one: whatever made the surface set
    // empty — an ancestry verdict, a routing arm that matched nothing, a
    // departure — this line fires. That is what makes it a better signal than
    // the ancestry line it complements.
    let delivered = daemon.surfaces.deliver_request(route, &frame);
    if delivered == 0 {
        eprintln!("{}", undelivered_consent_line(scope, route.arm()));
    }

    let outcome = daemon
        .consents
        .await_decision(&request_id, rx, daemon.consent_timeout)
        .await;
    in_flight.settled = true;
    // Approval returns the outcome whole, because it carries the approver's
    // descriptor the caller announces with (R1); the two refusals are mapped to
    // the reason a refusal is published under.
    let reason = match outcome {
        ConsentOutcome::Granted { .. } => return Some(outcome),
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
/// a grant, attempts the attach, and *then* answers `UNKNOWN_SESSION`: checking
/// existence before asking would rebuild the oracle. The grant is **retracted**
/// on that path (R2) — mint, attempt, revoke — so the timing and the frames a
/// caller can observe are exactly what a real id produces, while the registry
/// keeps nothing for a session it does not know. It used to keep the entry, and
/// a peer that self-approves a stream of fabricated ids therefore inserted one
/// permanent entry per guess, keyed by strings it chose.
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
        // Standing, so nothing was minted here and there is nothing to retract:
        // the caller's own creation record or an existing grant answered.
        return attach_to(daemon, conn, id, &params.session_id).response;
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
        ConsentOutcome::Granted {
            approver,
            attestation,
        } => {
            let minted = Grant::attach(conn.id, params.session_id.clone());
            daemon.grants.grant(minted.clone());
            // REQ-569 verify, F6: announce it in-perimeter, at the moment it is
            // minted, naming **both** parties (R1). `self_approved_by` reads off
            // the *route*, which is a sound answer to the question it asks —
            // arm 2 is the only arm the requester may answer at all — but that
            // question is "one connection or two", not "one actor or two". The
            // approver descriptor, which comes back on the outcome from
            // whichever connection actually answered, is what carries the rest.
            // REQ-570 AC-9: and the attestation method, which is the field that
            // makes `self_approved`'s blindness recoverable — a human had to be
            // at the machine for it to read anything but `none`.
            publish_grant_minted(
                daemon,
                conn,
                ConsentScope::Attach,
                &approver,
                route.self_approved_by(conn.id),
                attestation,
            );
            let attempt = attach_to(daemon, conn, id, &params.session_id);
            if !attempt.landed {
                // **A grant over a session that does not exist is not left
                // behind** (R2). The consent still ran in full — the prompt was
                // raised, the window was waited out, the same answer came back —
                // so nothing observable distinguishes a fabricated id from a
                // real one and the oracle stays shut. What changes is only the
                // residue: without this, a peer that self-approves fabricated
                // ids inserts a permanent registry entry per guess, keyed by
                // attacker-chosen strings, for the life of its connection.
                //
                // Retracted rather than never minted, and in that order, because
                // the mint has to precede the lookup for the timing to be
                // identical. The announcement above stands: a user really was
                // asked and really did answer, which is the news it reports.
                daemon.grants.revoke(&minted);
            }
            attempt.response
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

/// What [`attach_to`] did: the frame to answer with, and whether it landed.
///
/// The second field exists so the caller can retract a grant it minted for a
/// session the registry does not know (R2) without re-deriving that fact from
/// the response string or asking the registry a second time — a second lookup
/// would answer about a *later* moment than the attach did, and a session
/// created in between would make the two disagree.
struct AttachAttempt {
    /// The JSON-RPC frame for the requesting connection.
    response: String,
    /// `false` only when the session registry did not know the id.
    landed: bool,
}

/// The authorized tail of [`handle_session_attach`]: look the session up and
/// attach. REQ-568's behaviour, unchanged.
fn attach_to(daemon: &Daemon, conn: &ConnState, id: Id, session_id: &SessionId) -> AttachAttempt {
    match daemon.sessions.get(session_id) {
        Some(session) => {
            // Only a successful attach grants sight: a name the registry does
            // not know falls through to the error below with the set untouched,
            // so a client cannot pre-attach to a session id it guessed and
            // collect its events when someone later creates it.
            conn.attach(session.session_id.clone());
            AttachAttempt {
                response: ok_string(id, &SessionAttachResult { session }),
                landed: true,
            }
        }
        None => AttachAttempt {
            response: error_string(id, error_code::UNKNOWN_SESSION, "unknown session"),
            landed: false,
        },
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
/// The ancestry gate runs here too, and it is not belt-and-braces. It used to be
/// justified by a tool child being able to hold a session of its own, because
/// `session/create` was ungated; REQ-570 BR-10(a) closed that, so a descendant
/// can no longer acquire an attachment set over the socket at all. The check
/// stays, and the reason is now the general one rather than that specific path:
/// "may not hold session access" has to mean "may not confer it", or BR-4 would
/// hold on the door and fail on the door handle (LESSON-484 — the rule belongs
/// where the decision is made, and this is a decision).
///
/// # An approval needs a human behind it (REQ-570 BR-1, BR-3)
///
/// This is where REQ-569's residual closes. That REQ routed the prompt back to
/// the *requesting* connection when nothing was attached to the target session
/// — right for a user reopening their own CLI, and for a headless same-UID
/// process it meant the consent was self-issued with nobody involved.
///
/// The routing arm survives, because refusing it would break resume, which is
/// what REQ-565/567 exist to provide. What changed is that a **granting** answer
/// now mints nothing unless the daemon has itself verified a human is present.
///
/// **The daemon runs the presence check, not the client.** A client that
/// authenticated locally and reported "a human said yes" would be trivially
/// forgeable by the very process this is defending against — the claim would be
/// worth exactly as much as the self-approval it replaces. So the prompt is the
/// daemon's own, in the daemon's process, and the connection never handles the
/// proof.
///
/// A **denial** needs no attestation, and that asymmetry is deliberate. Refusing
/// access requires no proof of presence, and requiring one would mean an absent
/// or broken mechanism could keep a request pending rather than let it be
/// refused — fail-open in the one direction that matters.
async fn handle_attach_consent(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
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

    // BR-1 / BR-3: an approval mints a credential, so it needs a human. A
    // denial does not — see this function's docs on why that asymmetry is the
    // fail-closed direction.
    //
    // `None` on the denial path is not "unattested" leaking through: a denial
    // mints nothing, so there is no grant for a method to describe.
    let attested = if granted {
        match attest_presence(daemon, conn, &params.request_id).await {
            Ok(method) => method,
            Err(refusal) => {
                // Nothing is resolved and nothing minted: the prompt is
                // deliberately left standing for whoever may rightfully answer
                // it, exactly as the routing refusal above leaves it. A failed
                // presence check must not become a way for anyone to cancel a
                // pending consent.
                eprintln!("{}", attestation_refusal_line(&conn.requester, &refusal));
                return error_string(
                    id,
                    attestation_error_code(&refusal),
                    refusal_message(&refusal),
                );
            }
        }
    } else {
        crate::attest::AttestationMethod::None
    };

    let outcome = match params.outcome {
        // The approver's descriptor travels with the decision (R1). This is the
        // only place it can be picked up: the requesting task is parked on a
        // `oneshot` in another connection's handler and never learns who
        // answered otherwise, so the grant announcement could name only the
        // route — which reads "not self-approved" for an attacker's second
        // connection just as it does for a real second user.
        AttachConsentOutcome::Granted => ConsentOutcome::Granted {
            approver: conn.requester.clone(),
            attestation: attested,
        },
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

/// Ask a human to prove presence, and consume the resulting attestation
/// (REQ-570 BR-1, BR-6, BR-7).
///
/// The full round trip, in the one place it happens:
///
/// 1. **Availability first.** An unusable mechanism produces BR-8's posture
///    refusal rather than a prompt that cannot appear — and never a fall-through
///    to the unattested path, which is the whole of BR-11.
/// 2. **The prompt runs on the blocking pool.** It parks on a human for up to
///    thirty seconds, and the daemon's standing rule (ADR-006 E-3, LESSON-448,
///    pinned by `tests/nonblocking_inference.rs`) is that nothing which waits on
///    human or model time may sit on a tokio worker.
/// 3. **Record, then consume.** The registry round-trip is not ceremony: it is
///    what makes single-use, expiry and `(subject, request)` binding one
///    testable rule (BR-6) instead of an argument about call order, and it is
///    what AC-6 inspects to prove a refusal left nothing behind.
///
/// Returns `Ok(())` only when a human was verified for *this* connection and
/// *this* request. Every error arm mints nothing and leaves both registries as
/// it found them.
async fn attest_presence(
    daemon: &Daemon,
    conn: &ConnState,
    request_id: &RequestId,
) -> Result<crate::attest::AttestationMethod, AttestationRefusal> {
    if let MechanismAvailability::Unavailable(reason) = daemon.verifier.availability() {
        return Err(AttestationRefusal::Unavailable(reason));
    }

    let attestation = {
        let verifier = &daemon.verifier;
        let subject = conn.id;
        let request = request_id.clone();
        // The verifier is borrowed from the daemon and is not `'static`, and
        // this call is already on its own per-request task (see
        // `handle_client`), so moving the *thread* off the worker pool is the
        // right granularity. The flavor check lives in the helper.
        crate::runtime::block_in_place_if_multithread(|| verifier.verify(subject, &request))
    }?;

    let method = attestation.method();
    daemon.attestations.record(attestation);
    // Consumed immediately, by the same key it was minted under. The registry
    // is what enforces the binding rather than this call site trusting itself.
    daemon
        .attestations
        .consume(conn.id, request_id, std::time::Instant::now())?;
    Ok(method)
}

/// The wire code for an attestation refusal (BR-7 — the endings stay apart).
fn attestation_error_code(refusal: &AttestationRefusal) -> i64 {
    match refusal {
        AttestationRefusal::Required | AttestationRefusal::NotBound => {
            error_code::ATTESTATION_REQUIRED
        }
        AttestationRefusal::Failed => error_code::ATTESTATION_FAILED,
        AttestationRefusal::Cancelled => error_code::ATTESTATION_CANCELLED,
        AttestationRefusal::TimedOut | AttestationRefusal::Expired => {
            error_code::ATTESTATION_TIMEOUT
        }
        AttestationRefusal::Unavailable(_) => error_code::ATTESTATION_UNAVAILABLE,
    }
}

/// The client-facing message for an attestation refusal.
///
/// Content-free like its neighbours — no session id, no path, no prompt text
/// (conventions). The unavailable arm names its *cause* rather than failing
/// generically, which is AC-7b: a user on a headless Linux box is told polkit
/// has no agent, so the limitation is documented rather than discovered.
fn refusal_message(refusal: &AttestationRefusal) -> &'static str {
    match refusal {
        AttestationRefusal::Required | AttestationRefusal::NotBound => {
            "this approval needs a confirmed human present at the machine"
        }
        AttestationRefusal::Failed => "the presence check did not pass; nothing was granted",
        AttestationRefusal::Cancelled => "the presence check was dismissed; nothing was granted",
        AttestationRefusal::TimedOut | AttestationRefusal::Expired => {
            "the presence check was not answered in time; nothing was granted"
        }
        AttestationRefusal::Unavailable(reason) => reason.describe(),
    }
}

/// Operator-facing line for a refused attestation.
///
/// Carries the requester's bounded, control-stripped descriptor and the reason —
/// never a session id or any content. The counterpart to
/// [`self_approval_line`]: that one records the residual REQ-569 had to accept,
/// this one records it being refused.
fn attestation_refusal_line(requester: &str, refusal: &AttestationRefusal) -> String {
    format!(
        "teton-code: attach consent refused for {requester} — no verified presence ({})",
        refusal.code()
    )
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
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
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

/// Move a live session's root and clear its conversation (`session/set_cwd`,
/// REQ-583 BR-7 / ADR-4).
///
/// [`handle_session_clear`]'s shape, line for line, and for its reasons: parse,
/// bound the id ([`refuse_unmintable_session_id`]), decide the one thing this
/// layer decides — whether this connection may drive the session (REQ-568
/// BR-4; a monitor watches and drives nothing) — and hand the rest to the
/// runtime's single claim, which classifies unknown, busy, and (here) an
/// invalid path. Dispatched from the synchronous path, so the fence puts both
/// events — `context_cleared`, then `session_root_changed` — on the wire ahead
/// of this response: a client learns the conversation went and where the
/// session now stands before it is told how much went.
///
/// The gate comes before the runtime, so an unattached caller cannot read a
/// session's existence out of which refusal it got (ADR-B).
fn handle_session_set_cwd(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SessionSetCwdParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    // REQ-585 BR-1/AC-14: the project half of the registry is derived from the
    // root, so the root moving re-derives it — **inside** the move, ahead of the
    // `session_root_changed` publish, so a second attached client reacting to
    // that event cannot read the pre-move registry. The seam
    // (`Daemon::skills_fs`) still belongs to the daemon and is handed in; only
    // the timing moved. Dropping the stale `skill:project:*` grants happens
    // there too, where the session's gate is reachable (ADR-6).
    match daemon.runtime.set_session_cwd(
        &params,
        &daemon.sessions,
        &daemon.events,
        daemon.skills_fs.as_ref(),
    ) {
        Ok(result) => ok_string(id, &result),
        Err(err) => error_from(id, err),
    }
}

/// Derive this session's skill registry from the root it stands on and store it
/// (REQ-585 BR-1, ADR-1).
///
/// **Two derivation sites, ever**: this function, from
/// [`handle_session_create`], and the identical derivation inside
/// [`DaemonRuntime::set_session_cwd`], which owns the `/cd` half because the
/// rebuild has to land *before* `session_root_changed` reaches a second
/// attached client. There is no third, and that is the cost criterion rather
/// than an accident — discovery is four directory listings and
/// one file read per candidate, so a turn that re-derived it would pay that
/// while a user waits, for an answer that cannot have changed unless the root
/// did. The consequence is a snapshot, stated where the type is
/// ([`SkillRegistry`]): a skill file written mid-session is picked up at the
/// next `/cd`, and there is no watcher.
///
/// This function is the **probe** and the daemon's seam; the derivation itself
/// is `DaemonRuntime::store_session_skills`, which both call sites share so the
/// four globs are spelled once. What is added here is the reading of the root
/// ([`DaemonRuntime::session_root_for`]) and the `Daemon`-shaped arguments a
/// handler has in hand.
///
/// **The `/cd` half is not complete without dropping the project grants**, and
/// that is the other reason the move owns it. A remembered
/// `skill:project:<name>` grant authorizes a file under the old root, and a
/// rebuilt registry beside a stale grant is exactly LESSON-501's shape —
/// carried state that sheds its invariants silently. That drop reaches
/// `PermissionGate::drop_project_skill_grants` (REQ-585 TASK-201) through
/// `DaemonRuntime::drop_grants_expiring_on_root_change`, which needs the
/// private `session_gates` map — and which sheds more than the gate method's
/// name says: since REQ-587 TASK-215 the predicate is
/// `expires_on_session_root_change`, so BR-4's project-skill *acknowledgment*
/// goes with the grants. A *fresh* session has no grants and no gate, which is
/// why this function has nothing to drop.
fn rebuild_session_skills(daemon: &Daemon, session_id: &SessionId, cwd: Option<&Path>) {
    DaemonRuntime::store_session_skills(
        &daemon.sessions,
        session_id,
        &daemon.runtime.session_root_for(cwd),
        daemon.skills_fs.as_ref(),
        daemon.runtime.projects(),
    );
}

/// The `/name` commands this session dispatches (`skills/list`, REQ-585 BR-3,
/// ADR-1/ADR-2).
///
/// **This method is the capability handshake** (ADR-2): a client calls it after
/// `session/create` and again after every `session_root_changed`, and a daemon
/// that does not have it answers `METHOD_NOT_FOUND`, which the client reads as
/// an empty snapshot rather than as an error. Nothing here has to know that —
/// the point of proving a capability by a successful call is that the answer is
/// the same shape either way.
///
/// Gated on [`ConnState::may_drive`], exactly as [`handle_session_permissions`]
/// is and for its reason: this is a read of a session's content — file-authored
/// descriptions from a repo the connection may have no business seeing — and a
/// looser gate here would make the refusal an oracle for which sessions exist
/// (ADR-B). The length check comes first, so an attacker-chosen id is not
/// hashed through the attachment set before it is refused (BUG-166 residual).
///
/// It answers from the stored snapshot and opens nothing: the discovery this
/// reports was paid at [`rebuild_session_skills`]'s two call sites.
/// `projects/list` — the machine's known projects, rendered (REQ-584 BR-9).
///
/// **Ungated on the session**, unlike `skills/list`: there is no session in the
/// params and nothing session-scoped in the answer. A project list is a fact
/// about the *machine*, the same class as REQ-583's root display, and gating it
/// on an attachment would make `/projects` unusable from exactly the state it
/// exists for — a client that has not settled anywhere yet.
///
/// Returns the text the `projects` tool returns, from the one composition both
/// read (BR-9's one-renderer rule). The client styles; it does not restate.
fn handle_projects_list(daemon: &Daemon, id: Id, params: Value) -> String {
    let params: ProjectsListParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    // The scan is filesystem work, so it leaves the async worker for the same
    // reason discovery does (BUG-184).
    let view = crate::runtime::block_in_place_if_multithread(|| {
        crate::projects::locator_view(
            daemon.runtime.projects(),
            crate::session_root::home().as_deref(),
            crate::projects::scan::ScanBudget::default(),
            &crate::projects::scan::ScanObserver::default(),
            params.query.as_deref().filter(|q| !q.is_empty()),
            params.allow_scan,
        )
    });
    ok_string(
        id,
        &ProjectsListResult {
            rendered: teton_core::projects::render_locator(&view),
        },
    )
}

fn handle_skills_list(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SkillsListParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    let registry = daemon.sessions.skills(&params.session_id);
    ok_string(id, &skills_list_result(&registry))
}

/// The ceiling, in characters, on a skill's `description` — 200.
///
/// The requirement's figure (BR-1's entity table), chosen against the corpus it
/// has to render: 5 of the 17 shipped ADLC skill descriptions are longer than
/// this, and every one of them is a paragraph that would take a `/help` row off
/// the screen. Counted in characters rather than bytes for
/// [`bounded_field`](teton_core::session_root::bounded_field)'s reason — the row
/// is for a person, and a CJK description should get its full 200.
const SKILL_DESCRIPTION_MAX_CHARS: usize = 200;

/// The ceiling, in characters, on a skill's `argument-hint`.
///
/// The one-line display ceiling REQ-583 already sized for a person, reused
/// rather than a fresh number: the hint shares a `/help` row with the
/// description and is by nature a few words (`[path]`, `<REQ-id> [--force]`).
const SKILL_HINT_MAX_CHARS: usize = teton_core::session_root::DISPLAY_MAX_CHARS;

/// The ceiling, in characters, on the name a *skipped* entry carries.
///
/// [`crate::skills::MAX_NAME_LEN`], because a name that passed
/// `is_valid_skill_name` fits it exactly — but an entry skipped *for* its name
/// never passed anything, so the bound is what stops a 4,000-character
/// directory name from riding a diagnostic onto a screen.
const SKILL_SKIPPED_NAME_MAX_CHARS: usize = crate::skills::MAX_NAME_LEN;

/// The wire view of a registry: every row, bounded and neutralized **here**,
/// before it leaves the process (REQ-585 BR-3, LESSON-517).
///
/// It takes no `HOME` and no session root, and that is the shape BUG-187 left
/// behind: the display rule needs a skill's source *and* the root it was
/// discovered under, so it is applied once at discovery and this function only
/// bounds what it is handed.
///
/// Descriptions, hints, skipped paths and skipped names are all *file bytes* —
/// written by whoever owns the repo the session stands in, which is not always
/// the person reading the screen. They are bounded at the wire rather than at
/// the renderer because there is more than one renderer: the CLI defuses again
/// through `Surface::line` (ADR-009's two-ends shape), and the phase-2 VS Code
/// client will not have that function at all. A 4,000-character description
/// with a bidi override in it must therefore be harmless *as protocol*, not
/// merely as terminal output.
///
/// The one field that is **not** bounded here is [`SkillView::name`], and the
/// asymmetry is the point: a registered name matched
/// `^[a-z0-9][a-z0-9_-]{0,63}$` before discovery would register it (BR-2), so it
/// is ASCII, one line and at most 64 characters by construction. Bounding it
/// again would say the invariant is not trusted, in the one place it is
/// actually enforced.
fn skills_list_result(registry: &SkillRegistry) -> SkillsListResult {
    SkillsListResult {
        skills: registry
            .skills()
            .iter()
            .map(|skill| SkillView {
                name: skill.name.clone(),
                source: skill.source,
                description: skill
                    .description
                    .as_deref()
                    .map(|text| bounded_field(text, SKILL_DESCRIPTION_MAX_CHARS)),
                argument_hint: skill
                    .argument_hint
                    .as_deref()
                    .map(|hint| bounded_field(hint, SKILL_HINT_MAX_CHARS)),
                // The daemon knows the two contests it can see; the third — a
                // reserved built-in name — is the client's, so this says what
                // *this* side found and never `None` where it found something.
                shadowed: skill.shadowed.map(|by| by.to_string()),
                // REQ-587 BR-3's two flags, **as the file wrote them** and not
                // composed with `shadowed` above. The composition is the
                // client's and its order is not open: a row is read as
                // *shadowed* first and *model-only* only when nothing shadows
                // it (`Skill::user_dispatch` holds that decision, so `/help`'s
                // mark cannot pick a different one).
                //
                // Composing here instead would put "the user may not type
                // this" on every shadowed row — the wire spelling of the
                // model-only state — and a client rendering the mark straight
                // off the flag would call another file's name a model-only
                // skill. The flags stay the file's; the *authority* on who may
                // actually invoke a name is the registry's pair of resolvers
                // (`dispatchable_by_user` / `invocable_by_model`), daemon-side,
                // where ADR-1 keeps every rule with teeth.
                model_invocable: skill.model_invocable,
                user_invocable: skill.user_invocable,
            })
            .collect(),
        skipped: registry
            .skipped()
            .iter()
            .map(|entry| SkillSkipped {
                name: bounded_field(
                    &entry.name.clone().unwrap_or_default(),
                    SKILL_SKIPPED_NAME_MAX_CHARS,
                ),
                // BR-1's entity table: never an absolute path. A refusal that
                // said `/Users/jane/.claude/skills/broken/SKILL.md` would carry
                // a username into a transcript and, through `/help`, into a
                // screenshot — and one that said `/tmp/ci-4f2a/repo/.claude/…`
                // would carry the working tree's location (BUG-187). The
                // spelling is discovery's, which is the only place that had the
                // session root; this method answers from a stored snapshot and
                // has none.
                path: bounded_field(
                    &entry.path_display,
                    teton_core::session_root::DISPLAY_MAX_CHARS,
                ),
                // The daemon's own words, from the daemon's own enum: no file
                // bytes reach this string (BR-1's named reasons).
                reason: entry.reason.to_string(),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// skills/preflight — the refusal, reachable before it happens (REQ-589 BR-13)
// ---------------------------------------------------------------------------

/// The route a session's last turn was **decided** on, as this daemon
/// announced it (REQ-589 ADR-11).
///
/// # Why this memo exists at all
///
/// `/doctor` must report against the route the session is actually on, and a
/// diagnostic may not *decide* one: resolving a route consults provider health
/// and can wake the local tier, so a question about the current route would
/// change it. ADR-11's answer is to report the **stamped** budget or to say
/// there is none, which needs somewhere for the stamp to live — the `Route` and
/// the `HarnessConfig` that carry it are per-turn values that are gone by the
/// time anybody asks.
///
/// # It is fed by the announcement, not by a second derivation
///
/// The writer is [`record_route_decisions`], which reads `route_decided` off
/// the bus. That event *is* the stamp: `Router::budget_for` derives the pair
/// once, where the route is decided, and the same value is put on the wire.
/// Nothing here derives, estimates or re-classifies anything — the four fields
/// a refusal quotes (both currencies, the bound, and whether the floor raised
/// the pair) are copied out of the event verbatim.
///
/// # What is **not** recoverable from the announcement, and why that is safe
///
/// [`RouteBudget`] carries three fields the event does not: `window_label` and
/// the two `digest` thresholds. Neither `ContextManager::would_seed_fit` nor
/// the refusal composer reads any of them (`skill_refusal` quotes the pair, the
/// bound, the floor flag and the provider id and nothing else), so they are
/// left at their empty values rather than guessed. That is the deliberate
/// choice: a plausible-looking guess would make a future reader of one of them
/// produce a sentence that is subtly wrong, while an empty label makes it
/// visibly wrong — and
/// `the_preflight_quotes_the_live_refusals_sentence_for_the_same_skill` is what
/// fails when it happens.
///
/// The fourth field, `provider_id`, is carried as the event spells it. On the
/// route it is derived, `budget::sanitized_provider_id` has already replaced
/// everything outside `[A-Za-z0-9._:-]`, and for every id a user writes the two
/// spellings are the same bytes. They could differ for an id containing
/// something exotic, and the one clause that would show it is the
/// `unknown window` bound's remedy — so this reads a provider id exactly where
/// the `/verbose` route line already reads the same field off the same event,
/// and it never reaches a context block (this value's label is empty by
/// construction, so `truncate_to_budget` has nothing to write).
///
/// # Known limitation, and it is on the surface rather than in this comment
///
/// The pre-flight cannot measure the *live* turn's system prompt: that one is
/// assembled per turn from the session's probed root, its web capability and
/// its full tool set, none of which a diagnostic may build without doing the
/// work the turn does. [`preflight_system_prompt`] uses the daemon's default
/// harness prompt instead, and [`PREFLIGHT_FLOOR`] says so on every answer.
#[derive(Debug, Default)]
pub struct StampedRoutes {
    /// The last announced budget per session. Never persisted, and not pruned:
    /// a `SessionId` is not reused, exactly as the runtime's own session-scoped
    /// memos reason.
    stamped: Mutex<HashMap<SessionId, RouteBudget>>,
    /// Whether an observer task is already draining the bus into this memo.
    observing: AtomicBool,
}

impl StampedRoutes {
    /// A memo with no stamps and no observer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what `decided` announced for `session`.
    ///
    /// Returns whether anything was stored. A `route_decided` that carries no
    /// budget pair is not a stamp — it is what a daemon predating REQ-586
    /// emitted — and storing a half of one would put a surface in front of a
    /// figure nobody derived.
    pub fn record(&self, session: &SessionId, decided: &RouteDecided) -> bool {
        let (Some(tokens), Some(bytes), Some(bound)) =
            (decided.budget_tokens, decided.budget_bytes, decided.bound)
        else {
            return false;
        };
        let budget = RouteBudget {
            budget_tokens: usize::try_from(tokens).unwrap_or(usize::MAX),
            budget_bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
            bound,
            // Absent on a daemon predating the field; `false` is what that
            // daemon meant — it floored nothing it could report.
            floored: decided.bound_floored.unwrap_or(false),
            provider_id: Some(decided.provider_id.0.clone()),
            // Not on the wire, and not read by anything this value is handed
            // to. See the type's own docs for why they are left empty rather
            // than reconstructed.
            window_label: String::new(),
            digest_threshold_tokens: 0,
            digest_threshold_bytes: 0,
        };
        self.stamped
            .lock()
            .expect("stamped route mutex poisoned")
            .insert(session.clone(), budget);
        true
    }

    /// The budget `session`'s last turn was decided on, or `None` when no turn
    /// has decided one (ADR-11's "no route decided yet").
    #[must_use]
    pub fn stamped(&self, session: &SessionId) -> Option<RouteBudget> {
        self.stamped
            .lock()
            .expect("stamped route mutex poisoned")
            .get(session)
            .cloned()
    }

    /// Drop every stamp.
    ///
    /// Called when the observer stops for any reason, including the bus
    /// evicting it for lagging. A stamp the observer can no longer keep current
    /// is a route that may already have been re-decided, and reporting a route
    /// the session is no longer on is worse than reporting none: "no route
    /// decided yet" is a state the surface knows how to say.
    pub fn forget_all(&self) {
        self.stamped
            .lock()
            .expect("stamped route mutex poisoned")
            .clear();
    }

    /// Claim the right to run the one observer, returning whether this caller
    /// got it.
    fn claim_observer(&self) -> bool {
        !self.observing.swap(true, Ordering::SeqCst)
    }

    /// Release the observer claim, so the next prompt turn starts a fresh one.
    fn release_observer(&self) {
        self.observing.store(false, Ordering::SeqCst);
    }
}

/// Start the one [`StampedRoutes`] observer for this daemon, if it is not
/// already running (REQ-589 ADR-11).
///
/// Called from [`spawn_prompt_turn`], which is the only place a route is about
/// to be decided — so the memo costs nothing on a daemon nobody has prompted,
/// and it exists before the turn that will publish into it. The subscription is
/// registered **synchronously**, before the turn task is spawned, which is what
/// makes the first turn's own decision observable rather than a race.
fn observe_route_decisions(daemon: &Arc<Daemon>) {
    if !daemon.stamped_routes.claim_observer() {
        return;
    }
    let subscription = daemon.events.subscribe(DEFAULT_CAPACITY);
    let routes = Arc::clone(&daemon.stamped_routes);
    tokio::spawn(record_route_decisions(subscription, routes));
}

/// Drain `route_decided` into the memo until the subscription ends.
///
/// Ends on daemon teardown, or if the bus evicts this subscriber for lagging.
/// Either way every stamp is dropped and the claim released: the next prompt
/// turn subscribes again, and until it does `/doctor` says there is no route
/// rather than naming a stale one.
async fn record_route_decisions(mut subscription: Subscription, routes: Arc<StampedRoutes>) {
    while let Some(envelope) = subscription.recv().await {
        if let (Some(session), Event::RouteDecided(decided)) =
            (&envelope.session_id, &envelope.event)
        {
            routes.record(session, decided);
        }
    }
    routes.forget_all();
    routes.release_observer();
}

/// `skills/preflight` — which of this session's skills will not fit, before
/// anyone types one (REQ-589 BR-13, ADR-11).
///
/// Gated on [`ConnState::may_drive`] exactly as [`handle_skills_list`] is, and
/// for its reason: this reads a session's content — file-authored skill bodies
/// from a repo the connection may have no business seeing — and a looser gate
/// would make the refusal an oracle for which sessions exist (ADR-B). The
/// length check comes first for the same reason it does there.
///
/// Answers from the stored registry snapshot and the stamped route, and opens
/// nothing: no filesystem, no network, and — ADR-11's rule — **no router
/// resolution**. A session whose turns have decided no route is told so.
fn handle_skills_preflight(daemon: &Daemon, conn: &ConnState, id: Id, params: Value) -> String {
    let params: SkillsPreflightParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_) => return error_string(id, error_code::INVALID_PARAMS, "invalid params"),
    };
    if let Some(refusal) = refuse_unmintable_session_id(&id, &params.session_id) {
        return refusal;
    }
    if !conn.may_drive(&params.session_id) {
        return error_string(id, error_code::NOT_ATTACHED, NOT_ATTACHED_MESSAGE);
    }
    let registry = daemon.sessions.skills(&params.session_id);
    let stamped = daemon.stamped_routes.stamped(&params.session_id);
    ok_string(
        id,
        &SkillsPreflightResult {
            rendered: render_preflight(&registry, stamped.as_ref(), params.verbose),
        },
    )
}

/// ADR-11's answer for a session no turn has routed yet.
///
/// It says which question it is declining to answer and what would make it
/// answerable, because "no route decided yet" on its own reads like a fault.
const NO_ROUTE_DECIDED: &str = "skills: no route decided yet — this session has sent no turn, so \
     there is no stamped route to measure against. Send one and ask again; a diagnostic does not \
     resolve a route on your behalf.";

/// BR-13's stated limitation, said out loud on every answer that has one.
///
/// Two halves, and both are why the answer is a **floor** rather than a
/// clearance:
///
/// * Only the `Body` stage can be measured before a skill runs. A skill whose
///   `` !`command` `` output pushes it over is refused at Stage B, and nothing
///   before the commands run can know that — so a skill absent from this list
///   is not thereby promised to fit.
/// * The system prompt measured here is the daemon's **default** harness
///   prompt. A live turn's prompt additionally carries its session's
///   environment block and whatever tools that session exposes, so a live
///   measurement is normally the larger of the two.
const PREFLIGHT_FLOOR: &str = "  (a floor, not a clearance: only the `Body` stage can be measured \
     before a skill runs, so a skill whose dynamic-context output pushes it over is not named \
     here — and the system prompt measured is this daemon's default, which a live turn's only \
     grows.)";

/// The system prompt the pre-flight measures against.
///
/// [`build_system_prompt`] is the one composer — the same function the turn
/// path calls — handed the daemon's default harness with this route's stamped
/// budget on it. It is **not** the live turn's prompt, and cannot be: that one
/// is assembled per turn from the session's probed root, its web capability and
/// its full tool set, none of which a diagnostic may build without doing the
/// work the turn does. What it is instead is a prompt this daemon really
/// composes, which keeps `SkillStage::Body`'s clause — *"the body alone, with
/// the system prompt, comes to"* — a true sentence, and which
/// [`PREFLIGHT_FLOOR`] names out loud.
fn preflight_system_prompt(budget: &RouteBudget) -> String {
    build_system_prompt(
        &ToolRegistry::with_builtins(),
        &HarnessConfig::default().with_route_budget(budget.clone()),
    )
}

/// The Stage A text of one skill, exactly as a typed `/name` with no arguments
/// would produce it.
///
/// [`expand`] and [`Expansion::pending_text`](crate::skills::Expansion::pending_text)
/// are the turn path's own composition — same frame, same prose, same
/// `[dynamic context pending]` in each slot — so this measures the string the
/// session would measure rather than a second reading of the file's body.
///
/// The arguments are empty because nobody has typed any yet. That is the
/// honest pre-flight question and it is also the conservative one: an argument
/// is interpolated *into* the body, so a real invocation is at least this
/// large.
fn preflight_body(skill: &Skill) -> String {
    let expansion = expand(skill, "", &skill.path_display);
    expansion.pending_text(&expansion.user_frame())
}

/// Compose the pre-flight report (BR-13, AC-17, AC-19).
///
/// Every figure in it comes out of [`skill_fit`] — the same classifier, the
/// same estimator and the same composer the live refusal runs through — so
/// there is no second measurement here to drift from the first (LESSON-456,
/// and REQ-586's own verify M1). What this function contributes is the count,
/// the ordering and the caveat; it words no measurement of its own.
fn render_preflight(
    registry: &SkillRegistry,
    budget: Option<&RouteBudget>,
    verbose: bool,
) -> String {
    let Some(budget) = budget else {
        return NO_ROUTE_DECIDED.to_owned();
    };
    let system = preflight_system_prompt(budget);
    let mut dispatchable = 0usize;
    // The refusal sentence *is* the row: it opens with `` `/name` `` — the form
    // the user would type — and carries the measurement, the budget and the
    // bound. Prefixing it with the name again would be the same fact twice.
    let mut refusals: Vec<String> = Vec::new();
    for skill in registry.skills() {
        // BR-13's question is "will this be refused if I type it", so the set
        // is the one a `/name` can reach: `dispatchable_by_user` excludes a
        // shadowed row (another file owns the name) and a model-only one
        // (`user-invocable: false`). A model-invoked expansion is measured
        // mid-loop by `skill_append_fit` against a turn that already exists,
        // which is a different question and not one a diagnostic can answer.
        if !skill.dispatchable_by_user() {
            continue;
        }
        dispatchable += 1;
        if let SkillFit::TooLarge { message } = skill_fit(
            SkillCaller::User,
            SkillStage::Body,
            &skill.name,
            &system,
            &preflight_body(skill),
            budget,
            budget.provider_id.as_deref(),
        ) {
            refusals.push(message);
        }
    }

    let mut report = format!(
        "skills: {} of {} dispatchable skill(s) will not fit on this route{}",
        refusals.len(),
        dispatchable,
        // AC-19: the route's budget and bound, beside the count, under
        // `/verbose`. The figures are formatted through `teton_protocol`'s
        // `thousands`/`bytes_figure` and `BudgetBound::words` — the one number
        // vocabulary and the one bound vocabulary both sides of the wire read,
        // so this line and the refusals under it cannot spell one budget two
        // ways.
        if verbose {
            format!(
                " — budget {} words / {} (bound: {}).",
                thousands(budget.budget_tokens as u64),
                bytes_figure(budget.budget_bytes as u64),
                budget.bound.words()
            )
        } else {
            ".".to_owned()
        }
    );
    for message in &refusals {
        report.push_str("\n  ");
        report.push_str(message);
    }
    report.push('\n');
    report.push_str(PREFLIGHT_FLOOR);
    report
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
    use teton_protocol::permissions::PermissionLevel;

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

    /// Give `conn` a session it created, **without** going through the
    /// `session/create` RPC (REQ-570 TASK-002).
    ///
    /// Needed because BR-10(a) now gates that RPC on the ancestry check, so a
    /// `Descendant` / `Indeterminate` connection can no longer acquire
    /// session-holding standing over the socket at all — which is the point of
    /// the gate.
    ///
    /// The REQ-569 fixtures that hand a daemon child a session of its own are
    /// asking a *different* question: "even a descendant that **is** attached
    /// may not approve a consent request". That question is still live and still
    /// worth asserting, and it needs the child genuinely attached to be
    /// non-vacuous — so those fixtures establish the standing directly here
    /// rather than through the gate that now, correctly, refuses them.
    ///
    /// Deliberately not a bypass anything in production can reach: it pokes the
    /// session registry and the connection's own bookkeeping, which is exactly
    /// what `handle_session_create` does after its checks pass.
    fn seed_created_session(daemon: &Daemon, conn: &ConnState) -> SessionId {
        let summary = daemon
            .sessions
            .create(teton_protocol::SessionMode::Freeform, None, None)
            .expect("the test session registry accepts a freeform session");
        conn.record_created(summary.session_id.clone());
        summary.session_id
    }

    /// An ordinary connection that calls itself `requester` at the handshake.
    ///
    /// The descriptor has to be settable for the R1 tests: what a grant
    /// announcement has to make visible is the *relation* between the two
    /// descriptors, and a fixture where every connection shares one string can
    /// neither show two names differing nor show one name on both sides.
    fn named(daemon: &Daemon, requester: &str) -> ConnState {
        ConnState::new(
            daemon.grants.next_connection_id(),
            Ancestry::NotDescendant,
            false,
            requester.to_owned(),
        )
    }

    /// What a consent prompt calls a fixture connection.
    const TEST_REQUESTER: &str = "Cli client \"test\"";

    /// A window short enough that a test can assert on the *timeout* arm
    /// without waiting out [`CONSENT_TIMEOUT`]'s human-sized window (BR-7).
    const TEST_CONSENT_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

    /// A daemon whose consent window a test can outlast.
    fn daemon_with_short_consent() -> Arc<Daemon> {
        // A satisfiable presence mechanism, because these fixtures are about
        // REQ-569's *routing* — who is offered a prompt, who may answer it, what
        // a grant announcement says — and they predate attestation. Leaving them
        // on the fail-closed default would make every one of them assert
        // "attestation was unavailable", which is a different property with its
        // own tests below and would silently stop exercising the routing rules
        // these were written for.
        //
        // Tests that assert the *attestation* requirement construct their own
        // daemon and keep the shipped fail-closed verifier — see
        // `an_unattested_self_approval_mints_nothing`.
        Arc::new(
            Daemon::new()
                .with_consent_timeout(TEST_CONSENT_WINDOW)
                .with_presence_verifier(Box::new(crate::attest::AcceptingVerifier::default())),
        )
    }

    /// A short-window daemon that keeps the **shipped** fail-closed verifier.
    ///
    /// The counterpart to [`daemon_with_short_consent`]: that one installs a
    /// satisfiable mechanism so the REQ-569 routing fixtures keep testing
    /// routing, this one leaves presence genuinely unavailable so the REQ-570
    /// fixtures can test what happens when it is.
    fn daemon_with_short_consent_unattested() -> Arc<Daemon> {
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

    /// **REQ-570 AC-1 — the REQ-569 residual, closed.**
    ///
    /// A headless same-UID process requests attach to a session nothing is
    /// attached to, is handed its own prompt by BR-6's second routing arm, and
    /// answers itself `granted`. Under REQ-569 that minted a real grant with no
    /// human anywhere in the loop. It must now mint nothing.
    ///
    /// Uses the **shipped** fail-closed verifier — no `with_presence_verifier`,
    /// no seam — because the property is precisely what happens when presence
    /// cannot be established.
    ///
    /// The load-bearing assertion is the registry inspection, not the error
    /// code. AC-1 says "no grant is minted", and a test that only read the
    /// response would pass just as happily against a daemon that refused loudly
    /// and granted anyway.
    #[tokio::test]
    async fn an_unattested_self_approval_mints_nothing() {
        let daemon = daemon_with_short_consent_unattested();
        let requester = unattached(&daemon);
        let _prompts = as_surface(&daemon, &requester);

        // BR-6 arm 2: nothing is attached to the target, so the requester is
        // handed its own prompt. The fixture is only meaningful if that is the
        // arm taken.
        let route = ConsentRoute::requester_itself(requester.id);
        assert!(
            route.self_approved_by(requester.id),
            "this fixture must be exercising the self-render arm"
        );
        let request_id = daemon.consents.next_request_id();
        let _rx = daemon
            .consents
            .register(request_id.clone(), route)
            .expect("under the per-connection cap");

        let refused = handle_attach_consent(
            &daemon,
            &requester,
            Id::Number(1),
            serde_json::json!({
                "request_id": request_id.to_string(),
                "outcome": { "outcome": "granted" },
            }),
        )
        .await;

        assert!(
            refused.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
            "an unattested approval must be refused for want of presence: {refused}"
        );
        assert!(
            daemon.grants.is_empty(),
            "AC-1: no grant may exist — the registry is the assertion, not the error code"
        );
        assert!(
            daemon.attestations.is_empty(),
            "AC-6: and no attestation may be left behind either"
        );
        assert_eq!(
            daemon.consents.pending_count(),
            1,
            "and the prompt is left standing for whoever may rightfully answer it"
        );
    }

    /// **AC-6.** Each ending mints nothing and leaves both registries empty.
    ///
    /// Asserted per ending rather than once, because "no partial state" is a
    /// claim about the ending nobody thought about.
    #[tokio::test]
    async fn every_attestation_ending_leaves_both_registries_empty() {
        use crate::attest::{UnavailableReason, UnavailableVerifier};

        for (case, verifier, expected) in [
            (
                "no mechanism at all",
                UnavailableVerifier::new(UnavailableReason::PlatformUnsupported),
                error_code::ATTESTATION_UNAVAILABLE,
            ),
            (
                "linux with no polkit agent",
                UnavailableVerifier::new(UnavailableReason::NoPolkitAgent),
                error_code::ATTESTATION_UNAVAILABLE,
            ),
        ] {
            let daemon = Arc::new(
                Daemon::new()
                    .with_consent_timeout(TEST_CONSENT_WINDOW)
                    .with_presence_verifier(Box::new(verifier)),
            );
            let requester = unattached(&daemon);
            let _prompts = as_surface(&daemon, &requester);
            let request_id = daemon.consents.next_request_id();
            let _rx = daemon
                .consents
                .register(
                    request_id.clone(),
                    ConsentRoute::requester_itself(requester.id),
                )
                .expect("under the cap");

            let refused = handle_attach_consent(
                &daemon,
                &requester,
                Id::Number(1),
                serde_json::json!({
                    "request_id": request_id.to_string(),
                    "outcome": { "outcome": "granted" },
                }),
            )
            .await;

            assert!(refused.contains(&expected.to_string()), "{case}: {refused}");
            assert!(daemon.grants.is_empty(), "{case}: no grant");
            assert!(daemon.attestations.is_empty(), "{case}: no attestation");
            assert_eq!(
                daemon.consents.pending_count(),
                1,
                "{case}: a refused answer must leave the prompt standing for whoever \
                 may rightfully answer it — consuming it would be a denial of service"
            );
        }
    }

    /// A **denial** needs no attestation (BR-1's deliberate asymmetry).
    ///
    /// Refusing access requires no proof of presence. Requiring one would let an
    /// absent mechanism keep a request pending rather than let it be refused,
    /// which is fail-open in the one direction that matters — so this runs
    /// against the fail-closed verifier and must still succeed.
    #[tokio::test]
    async fn a_denial_needs_no_presence_check() {
        let daemon = daemon_with_short_consent_unattested();
        let requester = unattached(&daemon);
        let _prompts = as_surface(&daemon, &requester);
        let request_id = daemon.consents.next_request_id();
        let _rx = daemon
            .consents
            .register(
                request_id.clone(),
                ConsentRoute::requester_itself(requester.id),
            )
            .expect("under the cap");

        let answered = handle_attach_consent(
            &daemon,
            &requester,
            Id::Number(1),
            serde_json::json!({
                "request_id": request_id.to_string(),
                "outcome": { "outcome": "denied" },
            }),
        )
        .await;

        assert!(
            answered.contains("\"resolved\":true"),
            "a denial must decide the request even with no mechanism available: {answered}"
        );
        assert!(daemon.grants.is_empty(), "and mint nothing");
    }

    /// **Every BR-10(b) commitment** — the methods that demand presence on top of
    /// their layer (a) gate (a model change, a multi-GB download, rewriting the
    /// provider/privacy config, writing the `[web]` egress table, or — REQ-579 —
    /// registering a provider and routing tiers at it).
    /// A single source so a sixth commitment is one edit, not two hand-synced
    /// lists — the drift `a_commitment_degrades_to_layer_a_where_no_mechanism_exists`
    /// and `only_a_daemon_wide_commitment_demands_presence` would otherwise court
    /// (REQ-576 review). The remaining `daemon_wide_methods()` entries
    /// (`config/get`, `cost/query`, `web/refresh`, `session/create`) are layer (a)
    /// only.
    ///
    /// The last two are **session-scoped** — their layer (a) gate is `may_drive`,
    /// not the ancestry gate — so they are BR-10(b) commitments that must NOT be
    /// added to [`daemon_wide_methods`]; they take their params from
    /// [`commitment_params`] instead.
    const COMMITMENT_METHODS: &[&str] = &[
        ModelConfirmParams::METHOD,
        ModelSetParams::METHOD,
        ConfigSetParams::METHOD,
        WebSetupCommitParams::METHOD,
        ProviderSetupCommitParams::METHOD,
    ];

    /// Params that get each [`COMMITMENT_METHODS`] entry *past parsing and past
    /// its layer (a) gate*, so a refusal in a presence test is the BR-10(b) gate
    /// answering and never something earlier wearing the same shape.
    ///
    /// `session` is the session the calling connection owns; the three
    /// daemon-wide entries ignore it and read their params from
    /// [`daemon_wide_methods`], which is still the single source for those.
    fn commitment_params(method: &str, session: &SessionId) -> Value {
        if method == WebSetupCommitParams::METHOD {
            return setup_params(session);
        }
        if method == ProviderSetupCommitParams::METHOD {
            return provider_setup_params(session);
        }
        daemon_wide_methods()
            .into_iter()
            .find(|(m, _)| m == &method)
            .unwrap_or_else(|| panic!("`{method}` has no params in either table"))
            .1
    }

    /// The seven daemon-wide methods BUG-162 enumerates (of which the three in
    /// [`COMMITMENT_METHODS`] are BR-10(b) commitments), with the params each
    /// needs to get *past* parsing — so a refusal below is the ancestry gate
    /// answering and never a malformed-params rejection wearing the same shape.
    fn daemon_wide_methods() -> Vec<(&'static str, Value)> {
        vec![
            (
                ModelConfirmParams::METHOD,
                serde_json::json!({"request_id": "model-0", "outcome": {"outcome": "accept"}}),
            ),
            (
                ModelSetParams::METHOD,
                serde_json::json!({"name": "small-fit", "confirmed_above_ram_floor": false}),
            ),
            (ConfigGetParams::METHOD, serde_json::json!({})),
            (
                // A **valid** `ConfigUpdate` (tag `op`), so config/set gets *past*
                // parsing exactly as this table's doc promises — the gate refusals
                // above are the ancestry/presence gates answering, never a
                // malformed-params rejection wearing the same shape, and the
                // degrade test reaches the runtime rather than dying at the parse.
                ConfigSetParams::METHOD,
                serde_json::json!({"update": {
                    "op": "set_privacy_boundary",
                    "path_glob": "daemon-wide-fixture/**",
                    "mode": "local_only",
                }}),
            ),
            (CostQueryParams::METHOD, serde_json::json!({})),
            (
                WebRefreshParams::METHOD,
                serde_json::json!({"url": "https://example.invalid/a"}),
            ),
            (
                SessionCreateParams::METHOD,
                serde_json::json!({"mode": "freeform"}),
            ),
        ]
    }

    /// Route a request the way [`handle_client`] does — the **single** routing
    /// authority the test module has, so no test re-encodes "which methods run
    /// off the reader loop" (the sync `#[test]` bridge [`route_setup`] delegates
    /// here rather than deciding it a second time).
    ///
    /// Eight methods run on their own task and are therefore absent from
    /// [`dispatch`]. Seven because they may park on a *human*:
    /// `session/attach`, `attach/consent`, `model/confirm`, `model/set`,
    /// (REQ-575) `web/setup_commit`, (REQ-576) `config/set`, and (REQ-579)
    /// `provider/setup_commit`. The eighth, (REQ-581) `provider/test`, because
    /// it parks on the *network* — one real completion request — which stalls
    /// the reader loop exactly as a prompt would. A test that reached only for
    /// `dispatch` would get `METHOD_NOT_FOUND` for those and could report a
    /// security gate as "passing" while never invoking it — so the routing is
    /// mirrored here rather than duplicated per test.
    ///
    /// The two setup commits are the odd ones out: their **layer (a)** gate is
    /// session-scoped (`may_drive`, via `refuse_commit_without_session_access`),
    /// not the ancestry gate the other daemon-wide methods use — so they are
    /// deliberately absent from [`daemon_wide_methods`], which cannot supply an
    /// attached owner, and reach the shared presence loops through
    /// [`commitment_params`] instead. `config/set`, by contrast, IS a
    /// `daemon_wide_method` (ancestry gate), so its commitment coverage rides the
    /// shared table.
    async fn route_for_test(
        daemon: &Daemon,
        conn: &ConnState,
        id: Id,
        method: &str,
        params: Value,
    ) -> String {
        if method == SessionAttachParams::METHOD {
            handle_session_attach(daemon, conn, id, params).await
        } else if method == AttachConsentParams::METHOD {
            handle_attach_consent(daemon, conn, id, params).await
        } else if method == ModelConfirmParams::METHOD {
            handle_model_confirm(daemon, conn, id, params).await
        } else if method == ModelSetParams::METHOD {
            handle_model_set(daemon, conn, id, params).await
        } else if method == WebSetupCommitParams::METHOD {
            handle_web_setup_commit(daemon, conn, id, params).await
        } else if method == ProviderSetupCommitParams::METHOD {
            handle_provider_setup_commit(daemon, conn, id, params).await
        } else if method == ConfigSetParams::METHOD {
            handle_config_set(daemon, conn, id, params).await
        } else if method == ProviderTestParams::METHOD {
            handle_provider_test(daemon, conn, id, params).await
        } else {
            dispatch(daemon, conn, id, method, params).expect("a routed method answers")
        }
    }

    /// **REQ-570 AC-10, layer (b) — every BR-10(b) commitment demands a verified
    /// human, driven off the one list.**
    ///
    /// [`only_a_daemon_wide_commitment_demands_presence`] asserts the *split*
    /// among the daemon-wide methods and cannot reach the two session-scoped
    /// commits — `daemon_wide_methods()` has no attached owner to offer them.
    /// This one loops [`COMMITMENT_METHODS`] itself, with a connection that owns
    /// the session it names, so a sixth commitment added to that list and never
    /// gated goes red here rather than shipping unguarded (LESSON-502).
    ///
    /// The verifier refuses rather than accepts, because "presence was demanded"
    /// is only observable when it can fail.
    #[tokio::test]
    async fn every_commitment_demands_a_verified_human() {
        for &method in COMMITMENT_METHODS {
            let daemon = Arc::new(Daemon::new().with_presence_verifier(Box::new(
                crate::attest::AlwaysFailsVerifier::new(
                    crate::attest::AttestationMethod::OsBiometric,
                ),
            )));
            let owner = unattached(&daemon);
            let session = a_session_owned_by(&daemon, &owner);
            assert!(owner.may_hold_session_access());

            let response = route_for_test(
                &daemon,
                &owner,
                Id::Number(1),
                method,
                commitment_params(method, &session),
            )
            .await;
            assert!(
                response.contains(&error_code::ATTESTATION_FAILED.to_string()),
                "`{method}` commits a durable change and must refuse without a \
                 verified human: {response}"
            );
        }
    }

    /// **REQ-570 AC-10, layer (a) — BUG-162.** Every daemon-wide method refuses a
    /// connection that fails the ancestry gate.
    ///
    /// Asserted **per method**, not for one representative, and that is the whole
    /// point: LESSON-502 says an invariant enforced at several seams needs a test
    /// at each seam, and these seven are seven separate one-line checks that a
    /// future edit can drop one at a time. A single representative test would go
    /// green with six of the seven gates deleted.
    ///
    /// Both refused ancestries are covered, because
    /// `may_hold_session_access` treats `Indeterminate` as `Descendant` — "I
    /// could not tell" must cost the same as "it did", or the guard's failure
    /// mode is open.
    #[tokio::test]
    async fn every_daemon_wide_method_refuses_a_connection_that_fails_the_ancestry_gate() {
        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            for (method, params) in daemon_wide_methods() {
                let daemon = Daemon::new();
                let child = conn_with_ancestry(&daemon, ancestry);
                let response =
                    route_for_test(&daemon, &child, Id::Number(1), method, params.clone()).await;
                assert!(
                    response.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                    "{ancestry:?} calling `{method}` must be refused by the ancestry gate: \
                     {response}"
                );
            }
        }
    }

    /// The non-vacuity half: an ordinary connection reaches all seven.
    ///
    /// Without this, the test above would still pass if a typo made every method
    /// name unroutable — it would be asserting that seven strings are not
    /// methods. What is asserted here is narrow on purpose: **not** that the call
    /// succeeds (several of these legitimately fail on an empty test daemon —
    /// an unknown catalog entry, no such pending proposal), only that whatever
    /// comes back is not the *ancestry* refusal.
    #[tokio::test]
    async fn an_ordinary_connection_is_not_refused_the_daemon_wide_methods_by_ancestry() {
        for (method, params) in daemon_wide_methods() {
            // A satisfiable mechanism, so the two BR-10(b) commitment methods
            // are not refused for a *different* reason and quietly stop
            // testing the ancestry question this asserts.
            let daemon = Daemon::new()
                .with_presence_verifier(Box::new(crate::attest::AcceptingVerifier::default()));
            let ordinary = unattached(&daemon);
            assert!(
                ordinary.may_hold_session_access(),
                "the fixture is only meaningful if this connection passes the gate"
            );
            let response =
                route_for_test(&daemon, &ordinary, Id::Number(1), method, params.clone()).await;
            assert!(
                !response.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                "`{method}` must not refuse an ordinary connection on ancestry: {response}"
            );
        }
    }

    /// **REQ-591 D-1 — the seam the two consent-answer writes consult is the
    /// same check the four gated RPCs run, over every posture a verifier has.**
    ///
    /// [`VerifiedCommitment`] is what carries BR-10(b) to the writes that are
    /// *answers* rather than methods — the acknowledgment's
    /// `[skills] trusted_project_roots` row and REQ-589's over-budget remedy.
    /// Those two are wired through seams and are tested against doubles at their
    /// own doors; what only this test can say is that the seam production wires
    /// them to gives the same three answers `refuse_unattested_commitment` does,
    /// because both go through [`attest_commitment`].
    ///
    /// **Three postures, and the middle one is the reason the enum has three
    /// arms.** `TETON_PRESENCE_ACCEPT=1` selects [`AcceptingVerifier`], `=fail`
    /// selects [`AlwaysFailsVerifier`], and a shipped build gets
    /// [`UnavailableVerifier`] — so these are the three real configurations,
    /// named by their types rather than by an environment this process would
    /// have to mutate globally.
    ///
    /// The unavailable arm **proceeds**, which is not the gate switched off: it
    /// is BR-10(b)'s own rule that where no mechanism exists the posture degrades
    /// to layer (a) rather than refusing. Refusing there would stop every shipped
    /// build from writing a trust row and take D-13's automation with it. What
    /// BR-8 requires instead is that the reduced posture be *stated*, which
    /// [`commitment_degraded_line`] is and which the last assertion pins.
    #[test]
    fn the_commitment_seam_answers_every_posture_a_verifier_has() {
        let subject = GrantRegistry::new().next_connection_id();

        let attested = VerifiedCommitment {
            verifier: Arc::new(crate::attest::AcceptingVerifier::default()),
        };
        assert_eq!(
            attested.attest_daemon_wide_commitment(subject),
            Ok(()),
            "a satisfied mechanism must let the durable half of an answer through, \
             or `p` writes nothing on any presence build"
        );

        let refusing = VerifiedCommitment {
            verifier: Arc::new(crate::attest::AlwaysFailsVerifier::new(
                crate::attest::AttestationMethod::OsBiometric,
            )),
        };
        let refusal = refusing
            .attest_daemon_wide_commitment(subject)
            .expect_err("a present-but-unsatisfied mechanism refuses");
        assert_eq!(
            refusal,
            refusal_message(&AttestationRefusal::Failed),
            "and it refuses in the daemon's own vocabulary — the same sentence the \
             RPC surface returns for the same refusal, because both read it off \
             `refusal_message`"
        );

        let unavailable = VerifiedCommitment {
            verifier: Arc::new(crate::attest::UnavailableVerifier::new(
                UnavailableReason::PlatformUnsupported,
            )),
        };
        assert_eq!(
            unavailable.attest_daemon_wide_commitment(subject),
            Ok(()),
            "BR-10(b) degrades to layer (a) where no mechanism exists — a refusal \
             here would break every shipped build's durable acknowledgment"
        );

        // The degrade is stated rather than silent (BR-8), and it says which of
        // the two layers is missing — an operator reading this line is deciding
        // whether their machine can enforce the check at all.
        let stated = commitment_degraded_line(UnavailableReason::PlatformUnsupported);
        assert!(
            stated.contains("BR-10(b) is unavailable here")
                && stated.contains("BR-10(a) still applies"),
            "the degraded posture must name what is and is not still enforced: {stated}"
        );
    }

    /// **REQ-570 AC-10, layer (b) — BR-10.** A daemon-wide *commitment* refuses
    /// when no valid attestation is presented; the read-only siblings do not.
    ///
    /// The negative half is as load-bearing as the positive one. If
    /// `config/get`, `cost/query` and `web/refresh` also demanded presence, a
    /// user would be prompted to evict a cached document — and a user prompted
    /// for trivia learns to click through the prompt that matters. The split is
    /// the spec's, so it is asserted rather than left to a reviewer to notice.
    #[tokio::test]
    async fn only_a_daemon_wide_commitment_demands_presence() {
        // Ancestry is satisfied throughout, and a mechanism **is** available, so
        // the only thing that can vary between these methods is layer (b).
        //
        // A refusing verifier is the fixture rather than an accepting one:
        // "presence was demanded" is only observable when it can fail.
        let commitments = COMMITMENT_METHODS;

        for (method, params) in daemon_wide_methods() {
            let daemon = Arc::new(Daemon::new().with_presence_verifier(Box::new(
                crate::attest::AlwaysFailsVerifier::new(
                    crate::attest::AttestationMethod::OsBiometric,
                ),
            )));
            let ordinary = unattached(&daemon);
            assert!(ordinary.may_hold_session_access());

            let response =
                route_for_test(&daemon, &ordinary, Id::Number(1), method, params.clone()).await;
            let refused_for_presence =
                response.contains(&error_code::ATTESTATION_FAILED.to_string());

            if commitments.contains(&method) {
                assert!(
                    refused_for_presence,
                    "`{method}` commits a machine-wide change and must refuse without \
                     a verified human: {response}"
                );
            } else {
                assert!(
                    !refused_for_presence,
                    "`{method}` is layer (a) only — demanding presence for it would \
                     train users to click through the prompt that matters: {response}"
                );
            }
        }
    }

    /// **AC-8, the regression this nearly broke.** With no mechanism at all, a
    /// daemon-wide commitment degrades to layer (a) rather than refusing.
    ///
    /// The `presence` feature is non-default, so **the shipped build has no
    /// mechanism**. An implementation that refused here would make
    /// `model/confirm` impossible to answer and first-run model selection — the
    /// whole of REQ-547 — unreachable for every user. BR-8 and BR-11 scope their
    /// fail-closed refusal to *cross-session attach* and say nothing about
    /// commitments, and AC-10 refuses on an attestation not **presented**, not
    /// on a platform that cannot produce one.
    ///
    /// So this asserts the product still works on the build CI runs, and that
    /// the reduced posture is not silently confused with a satisfied one.
    #[tokio::test]
    async fn a_commitment_degrades_to_layer_a_where_no_mechanism_exists() {
        // Every BR-10(b) commitment must degrade — not refuse — where no
        // mechanism exists. Driven off the shared [`COMMITMENT_METHODS`] so a
        // future commitment is one edit, not two hand-synced lists (this test does
        // not loop `daemon_wide_methods()`; it needs the *degrade* posture, so it
        // iterates only the commitment subset — and since REQ-579 that subset
        // includes the two session-scoped commits, whose params come from
        // `commitment_params` and whose layer (a) gate the connection clears by
        // owning the session it names).
        for &method in COMMITMENT_METHODS {
            let daemon = daemon_with_short_consent_unattested();
            let ordinary = unattached(&daemon);
            let session = a_session_owned_by(&daemon, &ordinary);
            let params = commitment_params(method, &session);

            let response = route_for_test(&daemon, &ordinary, Id::Number(1), method, params).await;
            assert!(
                !response.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
                "`{method}` must not be refused for want of a mechanism the shipped \
                 build does not have — that would brick first-run: {response}"
            );
            assert!(
                !response.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                "`{method}`: and layer (a) still admits an ordinary connection: {response}"
            );
            assert!(
                !response.contains(&error_code::NOT_ATTACHED.to_string()),
                "`{method}`: and the session-scoped layer (a) gate admits the \
                 connection that owns the session — a refusal here would make the \
                 degrade assertions above vacuous: {response}"
            );
        }
    }

    /// **AC-10's independence clause.** Layer (a) passes with no mechanism at all.
    ///
    /// BR-10 requires layer (a) to be shippable without (b), because BUG-162 is
    /// high severity and open and must not wait on OQ-1's mechanism. That is a
    /// claim about the *ancestry* refusal surviving when presence is
    /// unavailable, so it is asserted against the fail-closed verifier — the
    /// build CI actually runs.
    #[tokio::test]
    async fn layer_a_refuses_independently_of_any_attestation_mechanism() {
        for (method, params) in daemon_wide_methods() {
            let daemon = daemon_with_short_consent_unattested();
            let child = conn_with_ancestry(&daemon, Ancestry::Descendant);
            let response =
                route_for_test(&daemon, &child, Id::Number(1), method, params.clone()).await;
            assert!(
                response.contains(&error_code::ATTACH_FORBIDDEN.to_string()),
                "`{method}`: the ancestry refusal must answer first and must not \
                 depend on a mechanism being present: {response}"
            );
            assert!(
                !response.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
                "`{method}`: a connection that may not act here at all must be refused \
                 without a presence prompt appearing on somebody's screen: {response}"
            );
        }
    }

    /// **AC-8 regression bar, at this layer.** The creator path gains nothing.
    ///
    /// `session/create` is the one of the seven an ordinary client calls as its
    /// very first act, so a gate placed slightly wrong there would break every
    /// session in the product rather than fail a security test.
    #[test]
    fn the_ordinary_create_path_is_untouched_by_the_daemon_wide_gate() {
        let daemon = Daemon::new();
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
            "the creator is attached to what it just made, exactly as before"
        );
        assert!(
            daemon.grants.is_empty(),
            "and the creator path still mints no grant at all"
        );
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
            )
            .await;
            assert!(
                answered.contains("\"resolved\":true"),
                "the answer must have decided something: {answered}"
            );
            prompt
        })
    }

    /// Answer the next `count` consent prompts to arrive on `rx`, as `approver`.
    ///
    /// The sequential-flood shape: the requester's `session/attach` calls block
    /// one at a time, so a single answering task walking the same channel keeps
    /// the whole burst on the production two-task flow rather than short-cutting
    /// it. Used by the R2 and R3 tests, which are *about* what a stream of
    /// approvals leaves behind.
    fn answer_consents(
        daemon: &Arc<Daemon>,
        approver: &ConnState,
        mut rx: mpsc::Receiver<String>,
        outcome: &'static str,
        count: usize,
    ) -> JoinHandle<()> {
        let daemon = Arc::clone(daemon);
        let approver = approver.clone();
        tokio::spawn(async move {
            for _ in 0..count {
                let prompt = loop {
                    let frame = rx.recv().await.expect("a prompt must be routed here");
                    let value: Value =
                        serde_json::from_str(&frame).expect("a routed frame is JSON");
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
                )
                .await;
                assert!(
                    answered.contains("\"resolved\":true"),
                    "the answer must have decided something: {answered}"
                );
            }
        })
    }

    /// The `session_grant_minted` payloads published on `sub` so far.
    fn grant_announcements(sub: &mut Subscription) -> Vec<SessionGrantMinted> {
        std::iter::from_fn(|| sub.try_recv())
            .filter_map(|env| match env.event {
                Event::SessionGrantMinted(minted) => Some(minted),
                _ => None,
            })
            .collect()
    }

    /// A plausible but fabricated session id — right shape, right length, names
    /// nothing (BR-8: an id is a name, so guessing one must cost the same as
    /// knowing one).
    fn fabricated_session_id(n: usize) -> String {
        format!("sess-{n:0>26}")
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
            )
            .await;
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
        )
        .await;
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
            //
            // Seeded directly rather than through `session/create`: REQ-570
            // BR-10(a) now gates that RPC on the same ancestry check, so a child
            // cannot acquire this standing over the socket at all. The question
            // *this* test asks is the other one — whether a descendant that
            // genuinely holds a session may approve — and it needs the standing
            // to be real to be worth asking.
            let own = seed_created_session(&daemon, &child);
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
            )
            .await;
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
            // session it created.
            //
            // REQ-570 BR-10(a) closed the "`session/create` is ungated by
            // design" premise this fixture was originally written against — a
            // daemon child can no longer create one over the socket. The F2
            // property under test is unchanged and still matters: a session held
            // by a connection that may not answer is **still held**, so a
            // stranger's attach fails closed on a timeout instead of falling
            // through to a self-render. Seeded directly so the holder is a real
            // holder.
            let holder = conn_with_ancestry(&daemon, ancestry);
            let session = seed_created_session(&daemon, &holder);
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
        assert_eq!(
            minted.approver, TEST_REQUESTER,
            "and the approver descriptor is filled in on this arm too — the one \
             connection is both parties (R1)"
        );
        assert_eq!(minted.suppressed, 0, "nothing was rate-limited away");
        // REQ-570 AC-9. This arm is exactly the one the field exists for: the
        // requester answered its own prompt, so `self_approved` is the only
        // other signal — and under REQ-570 this grant exists at all only because
        // a human was verified. A `"none"` here would mean the self-approval
        // residual had reopened.
        assert_eq!(
            minted.attestation, "os_biometric",
            "the announcement must name what verified the human behind the grant"
        );
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

    /// **REQ-569 re-verify, R1.** The announcement names **who approved**, not
    /// just which route ran — because the route is what an attacker holding two
    /// connections controls.
    ///
    /// The adversarial half is the first case. One actor opens two connections,
    /// creates a throwaway session on the first so it counts as attached, then
    /// attaches from the second and approves it with the first. Two distinct
    /// `ConnectionId`s, so `self_approved` is **false** — the same false a real
    /// second user's approval produces. That is the blindness ADR-A-1 named as
    /// the reason the monitor path could not be predicated, arriving here on the
    /// announcement: rendered off the flag alone, an attacker approving itself
    /// reads as the benign case.
    ///
    /// What distinguishes them is the *relation between the parties*, so the
    /// event carries both descriptors and the assertion is on the pair. Neither
    /// case is a verdict — two honest clients may share a name, and the daemon
    /// says so rather than deciding for the reader (`format_grant_minted`).
    #[tokio::test]
    async fn a_peer_approved_grant_names_the_connection_that_approved_it() {
        // (case, approver's descriptor, requester's descriptor)
        let cases = [
            (
                "one actor, two connections, one name",
                "Cli client \"attacker\"",
                "Cli client \"attacker\"",
            ),
            (
                "a real second party",
                "Cli client \"holder\"",
                "Cli client \"newcomer\"",
            ),
        ];

        for (case, approver_name, requester_name) in cases {
            let daemon = daemon_with_short_consent();
            let holder = named(&daemon, approver_name);
            let created = handle_session_create(
                &daemon,
                &holder,
                Id::Number(1),
                serde_json::json!({"mode": "freeform"}),
            );
            let session = created_session_id(&created);
            let holder_prompts = as_surface(&daemon, &holder);

            let mut sub = daemon.events.subscribe(16);
            let newcomer = named(&daemon, requester_name);
            let answering = answer_consent(&daemon, &holder, holder_prompts, "granted");
            let response = handle_session_attach(
                &daemon,
                &newcomer,
                Id::Number(2),
                serde_json::json!({"session_id": session.to_string()}),
            )
            .await;
            answering.await.expect("the approver task must not panic");
            assert!(
                response.contains(&session.to_string()),
                "{case}: {response}"
            );

            let announced = grant_announcements(&mut sub);
            assert_eq!(announced.len(), 1, "{case}");
            assert!(
                !announced[0].self_approved,
                "{case}: two connection ids, so the flag says nothing — which is \
                 exactly why it cannot be the field a reader acts on"
            );
            assert_eq!(
                announced[0].requester, requester_name,
                "{case}: the announcement names who asked"
            );
            assert_eq!(
                announced[0].approver, approver_name,
                "{case}: and who answered — the half `self_approved` structurally \
                 cannot express"
            );
        }
    }

    /// **REQ-569 re-verify, R2.** A self-approved attach to a session that does
    /// not exist answers `UNKNOWN_SESSION` and leaves **no** registry entry.
    ///
    /// The handler must run the whole consent path for a fabricated id — asking
    /// existence first would turn `session/attach` into the oracle BR-8 exists to
    /// deny — and it mints before it looks the session up so the two paths cost
    /// the same. What it must not do is *keep* the entry: before this, a peer
    /// self-approving a stream of guesses inserted one permanent grant per guess,
    /// keyed by strings it chose, held for the life of its connection.
    ///
    /// Both halves are asserted, because either alone is passable by a wrong
    /// implementation: the empty registry alone is satisfied by checking
    /// existence up front (which reopens the oracle), and the `UNKNOWN_SESSION`
    /// alone is satisfied by today's residue. The non-vacuity tail is the third
    /// leg — a *real* id down the same path keeps its grant, so this is not a
    /// handler that stopped minting.
    #[tokio::test]
    async fn self_approved_attaches_to_fabricated_ids_leave_no_grant_behind() {
        const GUESSES: usize = 25;
        let daemon = daemon_with_short_consent();
        let guesser = unattached(&daemon);
        let prompts = as_surface(&daemon, &guesser);
        // Arm 2 throughout: nothing is attached to a session that does not
        // exist, so the requester renders — and answers — its own prompt.
        let answering = answer_consents(&daemon, &guesser, prompts, "granted", GUESSES);

        for n in 0..GUESSES {
            let response = handle_session_attach(
                &daemon,
                &guesser,
                Id::Number(2),
                serde_json::json!({"session_id": fabricated_session_id(n)}),
            )
            .await;
            assert!(
                response.contains(&error_code::UNKNOWN_SESSION.to_string()),
                "guess {n} must draw the same refusal it always did: {response}"
            );
        }
        // The oracle half, and it is asserted rather than assumed: the answering
        // task only finishes if all `GUESSES` prompts were actually raised. A
        // handler that "fixed" the residue by checking existence *before* asking
        // would satisfy every assertion above and hang here — bounded, so it
        // fails with this sentence instead of with a stalled suite.
        tokio::time::timeout(std::time::Duration::from_secs(10), answering)
            .await
            .expect(
                "every fabricated id must raise a prompt: an id that names \
                 nothing has to cost exactly the round trip a real one does, or \
                 `session/attach` is an existence oracle (BR-8)",
            )
            .expect("the approver task must not panic");

        assert!(
            daemon.grants.is_empty(),
            "{GUESSES} approved guesses, {} grants: a granted consent for a \
             session the registry does not know must leave nothing behind",
            daemon.grants.len()
        );

        // Non-vacuity: the same connection, the same consent round trip, a real
        // id — and the grant stays.
        let created = handle_session_create(
            &daemon,
            &unattached(&daemon),
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);
        let prompts = as_surface(&daemon, &guesser);
        let answering = answer_consents(&daemon, &guesser, prompts, "granted", 1);
        let response = handle_session_attach(
            &daemon,
            &guesser,
            Id::Number(3),
            serde_json::json!({"session_id": session.to_string()}),
        )
        .await;
        answering.await.expect("the approver task must not panic");
        assert!(response.contains(&session.to_string()), "{response}");
        assert_eq!(
            daemon.grants.held_by(guesser.id),
            vec![Grant::attach(guesser.id, session)],
            "an approved attach to a session that exists still mints and keeps \
             exactly one grant"
        );
    }

    /// **REQ-569 re-verify, R3.** The grant announcement is rate-limited per
    /// connection, and the bound is the daemon's, not a client's.
    ///
    /// The event is daemon-scoped so that somebody other than the beneficiary
    /// sees a widened permission — which is also what makes it worth flooding.
    /// Minting is requester-triggered, so without a bound a peer looping
    /// `session/attach` writes one unsuppressable notice per iteration onto
    /// every connected client's screen. Asserted on the *bus*, because a bound
    /// enforced in one renderer would leave every other client and every
    /// programmatic consumer flooded.
    ///
    /// The window is set absurdly long rather than short: what is under test is
    /// the count, and a window a slow burst could straddle would make the
    /// assertion depend on how busy the machine is.
    #[tokio::test]
    async fn grant_announcements_are_bounded_per_connection() {
        const ATTEMPTS: usize = 12;
        let daemon = Arc::new(
            Daemon::new()
                .with_consent_timeout(TEST_CONSENT_WINDOW)
                .with_grant_announcement_window(std::time::Duration::from_secs(3600))
                // Grants have to actually be minted for there to be
                // announcements to bound — see `daemon_with_short_consent`.
                .with_presence_verifier(Box::new(crate::attest::AcceptingVerifier::default())),
        );
        let flooder = unattached(&daemon);
        let prompts = as_surface(&daemon, &flooder);
        let mut bystander = daemon.events.subscribe(64);
        let answering = answer_consents(&daemon, &flooder, prompts, "granted", ATTEMPTS);

        for n in 0..ATTEMPTS {
            let _ = handle_session_attach(
                &daemon,
                &flooder,
                Id::Number(2),
                serde_json::json!({"session_id": fabricated_session_id(n)}),
            )
            .await;
        }
        answering.await.expect("the approver task must not panic");

        let announced = grant_announcements(&mut bystander);
        assert_eq!(
            announced.len(),
            GRANT_ANNOUNCEMENTS_PER_WINDOW as usize,
            "{ATTEMPTS} approvals from one connection may not become {ATTEMPTS} \
             notices on an uninvolved client's screen"
        );
        assert!(
            announced.iter().all(|minted| minted.suppressed == 0),
            "the arrears are reported by the *next* announcement to get through, \
             so every one inside the first window reports none: {announced:?}"
        );
    }

    /// **R3, the arithmetic.** The window rolls, and what it held back is
    /// reported rather than lost.
    ///
    /// Unit-level and over an injected clock, because the interesting behaviour
    /// is a boundary in time: driven through the handler it would need a real
    /// sleep, and a suite that sleeps to test arithmetic is a suite that goes
    /// flaky and then goes unrun. The handler test above pins the bound; this
    /// pins what happens either side of it.
    #[test]
    fn a_bounded_burst_of_announcements_reports_what_it_swallowed() {
        let window = std::time::Duration::from_secs(60);
        let start = std::time::Instant::now();
        let mut budget = GrantAnnouncementBudget {
            opened: start,
            announced: 0,
            suppressed: 0,
        };

        // The allowance, spent: every one of these publishes, and none of them
        // has arrears to report.
        for n in 0..GRANT_ANNOUNCEMENTS_PER_WINDOW {
            assert_eq!(budget.take(start, window), Some(0), "announcement {n}");
        }
        // Past it: silence, counted.
        for n in 0..7 {
            assert_eq!(budget.take(start, window), None, "suppressed {n}");
        }

        // The window rolls. The first announcement through carries everything
        // the bound swallowed — so quieting a flood costs a reader the notices,
        // never the knowledge that there were notices.
        let next_window = start + window;
        assert_eq!(budget.take(next_window, window), Some(7));
        assert_eq!(
            budget.take(next_window, window),
            Some(0),
            "and the arrears are reported once, not carried forward forever"
        );
    }

    /// An answer to a request id nobody is waiting on is acknowledged, not
    /// refused — and says it decided nothing.
    ///
    /// A refusal would answer "is some consent outstanding right now?" for any
    /// connected peer, which is an oracle this seam does not hand out. The
    /// `resolved: false` is what stops a client reporting success for an answer
    /// that arrived after the window closed.
    #[tokio::test]
    async fn an_answer_to_an_unknown_consent_request_decides_nothing_and_says_so() {
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
        )
        .await;
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

    /// BUG-163: a consent prompt that reached nobody says so, and says which
    /// rule chose the empty audience.
    ///
    /// The failure this pins has no other symptom. A request delivered to zero
    /// surfaces is registered, waits out the full consent window, and reports a
    /// timeout — which is indistinguishable in every log the daemon keeps from a
    /// user who was shown a prompt and ignored it. Opposite remedies, identical
    /// evidence, and `deliver_request`'s count was being discarded at the one
    /// call site that could tell them apart.
    ///
    /// Names the routing rule rather than its subject, so the line can say
    /// *which* arm selected nobody without putting a session id in a log.
    #[test]
    fn a_consent_prompt_that_reached_nobody_says_so_and_names_the_rule() {
        let attach = undelivered_consent_line(ConsentScope::Attach, "the requester itself");
        let monitor = undelivered_consent_line(ConsentScope::Monitor, "any attached peer");

        // The two scopes are different questions and must not read alike.
        assert_ne!(attach, monitor, "scope must be legible in the line");
        assert!(attach.contains("attach"), "{attach}");
        assert!(monitor.contains("monitor"), "{monitor}");
        // The rule that produced an empty audience is the diagnostic payload.
        assert!(attach.contains("the requester itself"), "{attach}");
        // And it must say the request is doomed, not merely slow — that is the
        // distinction from a prompt a user simply has not answered yet.
        assert!(
            attach.contains("reached no surface") && attach.contains("Nothing can answer"),
            "{attach}"
        );
        // Content-free, like every other line on this seam.
        for line in [&attach, &monitor] {
            assert!(!line.contains("sess-"), "{line}");
        }

        // The rule names themselves come from the route, never spelled twice.
        let daemon = Daemon::new();
        let conn = daemon.grants.next_connection_id();
        let requester = ConsentRoute::requester_itself(conn);
        assert_eq!(requester.arm(), "the requester itself");
        let attached = ConsentRoute::attached_to(conn, SessionId::from("sess-secret"));
        assert!(
            !attached.arm().contains("sess-"),
            "the arm names the rule, never its subject: {}",
            attached.arm()
        );
    }

    /// BUG-163: an `Indeterminate` classification is recorded at the moment it
    /// is made, and says what it costs the connection.
    ///
    /// The bug this pins is an **absence**: a client whose ancestry could not be
    /// determined and which declares no monitor is admitted, marked
    /// `may_answer: false`, and then never offered a consent frame — with
    /// nothing written down anywhere. Its only symptom is a request that waits
    /// out its window, which is indistinguishable from a dozen other causes.
    /// BUG-163 spent two refuted root causes on exactly that ambiguity.
    ///
    /// So this asserts the line exists, names the pid, tells the two verdicts
    /// apart, and — the part that makes it worth reading — says which one is the
    /// gate working and which one is a lookup that has stopped working.
    #[test]
    fn an_ancestry_verdict_is_recorded_where_it_is_decided_not_only_where_it_refuses() {
        let unknown = ancestry_classification_line(Ancestry::Indeterminate, Some(4321));
        let descendant = ancestry_classification_line(Ancestry::Descendant, Some(4321));

        assert_ne!(
            unknown, descendant,
            "the two verdicts must stay distinguishable here as well as at the refusals"
        );
        // The pid is the one fact that lets a walk be reconstructed by hand.
        assert!(unknown.contains("4321"), "{unknown}");
        // An operator must be able to tell "your tool child was correctly
        // excluded" from "this daemon's peer-pid lookup is broken" — opposite
        // remedies, and the whole reason this line exists.
        assert!(
            unknown.contains("could not determine") && unknown.contains("fails closed"),
            "{unknown}"
        );
        assert!(
            descendant.contains("this is the gate working"),
            "{descendant}"
        );
        // A missing peer pid is itself the interesting case; it must not render
        // as a plausible pid.
        let no_pid = ancestry_classification_line(Ancestry::Indeterminate, None);
        assert!(no_pid.contains("unknown"), "{no_pid}");
        // Content-free, like every other line on this seam.
        for line in [&unknown, &descendant, &no_pid] {
            assert!(!line.contains("sess-"), "{line}");
        }
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
    fn dispatch_routes_session_permissions_and_tells_attached_from_unattached() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);
        let created = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        let session = created_session_id(&created);

        // A bare read reports the configured default and changes nothing.
        let read = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            !read.contains("-32601"),
            "the method must be routed, not rejected as unknown: {read}"
        );
        assert!(read.contains("\"level\":\"guarded\""), "{read}");
        assert!(
            read.contains("\"changed\":false"),
            "a read is never a change: {read}"
        );

        // A set reports the new level and that it changed.
        let set = dispatch(
            &daemon,
            &conn,
            Id::Number(3),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": session.to_string(), "level": "full"}),
        )
        .unwrap();
        assert!(set.contains("\"level\":\"full\""), "{set}");
        assert!(set.contains("\"changed\":true"), "{set}");

        // Setting the level it already holds is not a change — so a CLI
        // confirmation cannot announce something that did not happen.
        let again = dispatch(
            &daemon,
            &conn,
            Id::Number(4),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": session.to_string(), "level": "full"}),
        )
        .unwrap();
        assert!(again.contains("\"changed\":false"), "{again}");

        // An unknown level is invalid params, not a silent fallback to a
        // posture nobody chose.
        let bogus = dispatch(
            &daemon,
            &conn,
            Id::Number(5),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": session.to_string(), "level": "unrestricted"}),
        )
        .unwrap();
        assert!(
            bogus.contains(&error_code::INVALID_PARAMS.to_string()),
            "an unknown level must be refused: {bogus}"
        );
        // …and it left the session where it was.
        let after = dispatch(
            &daemon,
            &conn,
            Id::Number(6),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(after.contains("\"level\":\"full\""), "{after}");

        // Not attached: refused before the runtime, and refused *identically*
        // for a session that exists and one that does not — the pair is the
        // assertion, since two different codes here would be the existence
        // oracle ADR-B refuses to build. Reads are gated too: reading a
        // session's posture is still reading that session.
        let stranger = unattached(&daemon);
        for target in [session.to_string(), "sess-nonexistent".to_owned()] {
            for body in [
                serde_json::json!({"session_id": target}),
                serde_json::json!({"session_id": target, "level": "plan"}),
            ] {
                let refused = dispatch(
                    &daemon,
                    &stranger,
                    Id::Number(7),
                    SessionPermissionsParams::METHOD,
                    body,
                )
                .unwrap();
                assert!(
                    refused.contains(&error_code::NOT_ATTACHED.to_string()),
                    "`{target}` unattached must be refused: {refused}"
                );
                assert!(
                    !refused.contains("\"level\""),
                    "a refused call must not report a level: {refused}"
                );
            }
        }
    }

    /// REQ-560 BR-6: the level is session-scoped and **writes nothing**.
    ///
    /// Asserted two ways, because the requirement has two halves: a second
    /// session in the same daemon starts at the configured default rather than
    /// inheriting the first's level, and the daemon's config is byte-identical
    /// afterwards. AC-6's full restart leg lives in the piped e2e; this is the
    /// in-process half that pins the write path itself.
    #[test]
    fn a_level_set_in_one_session_reaches_neither_config_nor_the_next_session() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);
        let before = daemon.runtime.default_permission_level();

        let first = created_session_id(&handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        ));
        let set = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": first.to_string(), "level": "full"}),
        )
        .unwrap();
        assert!(set.contains("\"level\":\"full\""), "{set}");

        // A second session in the same daemon is seeded from config, not from
        // its neighbour.
        let second = created_session_id(&handle_session_create(
            &daemon,
            &conn,
            Id::Number(3),
            serde_json::json!({"mode": "freeform"}),
        ));
        let read = dispatch(
            &daemon,
            &conn,
            Id::Number(4),
            SessionPermissionsParams::METHOD,
            serde_json::json!({"session_id": second.to_string()}),
        )
        .unwrap();
        assert!(
            read.contains("\"level\":\"guarded\""),
            "a new session inherited its neighbour's level: {read}"
        );

        // And the daemon's own seed — what `[permissions] default_level`
        // became at startup — is untouched, so the next session (and the next
        // daemon start) is unaffected by what this one typed.
        assert_eq!(
            before,
            daemon.runtime.default_permission_level(),
            "a session-scoped level reached the daemon's configured default"
        );
        assert_eq!(before, PermissionLevel::Guarded);
    }

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

    /// A unique, existing scratch directory for a session-root fixture; the
    /// caller removes it. Holds a project marker when `project` is set, so the
    /// probe classifies it as one.
    fn scratch_root(tag: &str, project: bool) -> std::path::PathBuf {
        let dir = temp_socket(tag).with_extension("dir");
        std::fs::create_dir_all(&dir).unwrap();
        if project {
            std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        }
        dir
    }

    /// REQ-583 BR-7 / ADR-4 at the dispatch seam: `session/set_cwd` is routed,
    /// gated on `may_drive` exactly as `session/clear` is (unattached and
    /// monitor connections are refused identically for a session that exists
    /// and one that does not — no existence oracle, ADR-B), and an attached
    /// caller naming a session the registry never had is told so by the
    /// runtime's classifier, not by the gate.
    ///
    /// The success arm reads the answer's `root` — kind and display are the
    /// probe's, over the path just set — and its `blocks_dropped`, and pins the
    /// wire order the fence guarantees on the socket: `context_cleared` then
    /// `session_root_changed`, both session-scoped.
    #[test]
    fn dispatch_routes_session_set_cwd_and_tells_attached_from_unattached() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);
        let start = scratch_root("cd-start", true);
        let target = scratch_root("cd-target", false);
        let created = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform", "cwd": start}),
        );
        let session = created_session_id(&created);
        let mut sub = daemon.events.subscribe(16);

        // Attached and live: moves, clears (nothing yet), and says where it now
        // stands.
        let moved = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            SessionSetCwdParams::METHOD,
            serde_json::json!({"session_id": session.to_string(), "cwd": target}),
        )
        .unwrap();
        assert!(
            !moved.contains("-32601"),
            "the method must be routed, not rejected as unknown: {moved}"
        );
        let parsed: Value = serde_json::from_str(&moved).unwrap();
        assert_eq!(
            parsed["result"]["blocks_dropped"].as_u64(),
            Some(0),
            "a session that has said nothing clears to zero, and says so: {moved}"
        );
        assert_eq!(
            parsed["result"]["root"]["kind"].as_str(),
            Some("plain"),
            "the answer carries the probe's kind for the new path: {moved}"
        );
        let expected = crate::session_root::probe(&target, crate::session_root::home().as_deref());
        assert_eq!(
            parsed["result"]["root"]["display"].as_str(),
            Some(expected.display.as_str()),
            "the answer's display is the probe's spelling of the new path: {moved}"
        );
        assert_eq!(
            daemon.sessions.get(&session).unwrap().cwd.as_deref(),
            Some(target.as_path()),
            "the registry's one stored fact moved"
        );

        // The two events, in order, both scoped to the session.
        let first = sub.try_recv().expect("context_cleared is published first");
        assert_eq!(first.session_id.as_ref(), Some(&session));
        assert!(
            matches!(first.event, Event::ContextCleared(_)),
            "the clear is announced first: {:?}",
            first.event
        );
        let second = sub.try_recv().expect("session_root_changed follows");
        assert_eq!(second.session_id.as_ref(), Some(&session));
        match second.event {
            Event::SessionRootChanged(changed) => {
                assert_eq!(changed.root, expected);
                let was =
                    crate::session_root::probe(&start, crate::session_root::home().as_deref());
                assert_eq!(
                    changed.previous_display, was.display,
                    "`previous_display` is the old root's spelling — what \
                     the CLI prints as \"moved from\""
                );
            }
            other => panic!("expected session_root_changed, got {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "exactly two events per move");

        // Not attached: refused before the runtime, and refused *identically*
        // for a session that exists and one that does not — the pair is the
        // assertion (ADR-B). A monitor is refused the same way: watching a
        // session is not driving it (REQ-568 BR-4).
        for stranger in [unattached(&daemon), monitoring(&daemon)] {
            for target_id in [session.to_string(), "sess-nonexistent".to_owned()] {
                let refused = dispatch(
                    &daemon,
                    &stranger,
                    Id::Number(3),
                    SessionSetCwdParams::METHOD,
                    serde_json::json!({"session_id": target_id, "cwd": start}),
                )
                .unwrap();
                assert!(
                    refused.contains(&error_code::NOT_ATTACHED.to_string()),
                    "moving `{target_id}` unattached must be refused: {refused}"
                );
                assert!(
                    !refused.contains("blocks_dropped") && !refused.contains("\"display\""),
                    "a refused move must report neither a count nor a root: {refused}"
                );
            }
        }
        assert_eq!(
            daemon.sessions.get(&session).unwrap().cwd.as_deref(),
            Some(target.as_path()),
            "a refused move leaves the root where it was"
        );
        assert!(sub.try_recv().is_none(), "a refused move announces nothing");

        // Attached to a name the registry never had: the runtime still
        // classifies it, and the gate did not take that answer away.
        conn.attach(SessionId::from("sess-nonexistent"));
        let ghost = dispatch(
            &daemon,
            &conn,
            Id::Number(4),
            SessionSetCwdParams::METHOD,
            serde_json::json!({"session_id": "sess-nonexistent", "cwd": start}),
        )
        .unwrap();
        assert!(
            ghost.contains(&error_code::UNKNOWN_SESSION.to_string()),
            "an unknown session must not move cheerfully: {ghost}"
        );

        // A bad path is refused by the same validator `session/create` uses,
        // naming the path (BR-6), and the root does not move.
        let bad = dispatch(
            &daemon,
            &conn,
            Id::Number(5),
            SessionSetCwdParams::METHOD,
            serde_json::json!({"session_id": session.to_string(), "cwd": "/nope/teton-cd"}),
        )
        .unwrap();
        assert!(
            bad.contains(&error_code::INVALID_PARAMS.to_string())
                && bad.contains("path `/nope/teton-cd` does not exist or is not a directory"),
            "the refusal is the validator's one sentence, naming the path: {bad}"
        );
        assert_eq!(
            daemon.sessions.get(&session).unwrap().cwd.as_deref(),
            Some(target.as_path()),
            "a refused move leaves the root where it was"
        );

        let _ = std::fs::remove_dir_all(&start);
        let _ = std::fs::remove_dir_all(&target);
    }

    /// REQ-583 BR-6 / ADR-1: `session/create` answers with the root the daemon
    /// settled on — probed from the cwd the client sent, or from the daemon's
    /// own fallback root when it sent none — and that fallback is the very
    /// value a turn without a cwd jails to (`DaemonRuntime::minimal`'s
    /// `repo_root` is the temp dir), so the banner renders what the tools will
    /// enforce.
    #[test]
    fn session_create_returns_the_probed_root_for_a_cwd_and_for_the_fallback() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);
        let home = crate::session_root::home();

        let project = scratch_root("create-root", true);
        let created = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform", "cwd": project}),
        );
        let parsed: Value = serde_json::from_str(&created).unwrap();
        let expected = crate::session_root::probe(&project, home.as_deref());
        assert_eq!(expected.kind, teton_protocol::methods::RootKind::Project);
        assert_eq!(
            parsed["result"]["root"]["kind"].as_str(),
            Some("project"),
            "a cwd holding a marker is a project root: {created}"
        );
        assert_eq!(
            parsed["result"]["root"]["display"].as_str(),
            Some(expected.display.as_str()),
            "{created}"
        );
        assert_eq!(
            parsed["result"]["root"]["project_name"].as_str(),
            expected.project_name.as_deref(),
            "{created}"
        );

        // No cwd: the fallback root, which is what the turn will jail to.
        let bare = handle_session_create(
            &daemon,
            &conn,
            Id::Number(2),
            serde_json::json!({"mode": "freeform"}),
        );
        let parsed: Value = serde_json::from_str(&bare).unwrap();
        let fallback = crate::session_root::probe(&std::env::temp_dir(), home.as_deref());
        assert_eq!(
            parsed["result"]["root"]["display"].as_str(),
            Some(fallback.display.as_str()),
            "a session that sent no cwd is told the daemon's fallback root: {bare}"
        );
        assert_eq!(
            parsed["result"]["root"]["kind"].as_str(),
            Some(match fallback.kind {
                teton_protocol::methods::RootKind::Project => "project",
                teton_protocol::methods::RootKind::Home => "home",
                teton_protocol::methods::RootKind::FilesystemRoot => "filesystem_root",
                teton_protocol::methods::RootKind::Plain => "plain",
            }),
            "{bare}"
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    /// REQ-583 BR-6 (AC-9's daemon half): a `session/create` whose cwd is
    /// relative, missing, or not a directory is refused with `INVALID_PARAMS`
    /// **naming the path** and the reason, and no session is created — never a
    /// session that starts and then fails on every tool (BUG-147).
    #[test]
    fn session_create_refuses_a_bad_cwd_naming_the_path() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);

        let missing = handle_session_create(
            &daemon,
            &conn,
            Id::Number(1),
            serde_json::json!({"mode": "freeform", "cwd": "/nope/teton-create"}),
        );
        assert!(
            missing.contains(&error_code::INVALID_PARAMS.to_string())
                && missing
                    .contains("path `/nope/teton-create` does not exist or is not a directory"),
            "the refusal is the validator's one sentence, naming the path: {missing}"
        );

        let relative = handle_session_create(
            &daemon,
            &conn,
            Id::Number(2),
            serde_json::json!({"mode": "freeform", "cwd": "relative/dir"}),
        );
        assert!(
            relative.contains(&error_code::INVALID_PARAMS.to_string())
                && relative.contains("path `relative/dir` must be an absolute path"),
            "the refusal is the validator's one sentence, naming the path: {relative}"
        );

        assert_eq!(
            daemon.sessions.count(),
            0,
            "a refused create must leave no session behind"
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
        let handle = spawn_prompt_turn(
            &daemon,
            &stranger,
            Id::Number(2),
            prompt.clone(),
            &tx,
            None,
            ClientPresence::unwatched(),
        );
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
        let accepted = spawn_prompt_turn(
            &daemon,
            &creator,
            Id::Number(3),
            prompt,
            &tx,
            None,
            ClientPresence::unwatched(),
        );
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
        let handle = spawn_prompt_turn(
            &daemon,
            &monitor,
            Id::Number(2),
            prompt,
            &tx,
            None,
            ClientPresence::unwatched(),
        );
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

    // ------------------------------------------------------------------
    // REQ-572 BR-4 / AC-4: the setup family and its session-access gate
    // ------------------------------------------------------------------

    /// A session created by `owner`, who is attached to it by having created it.
    fn a_session_owned_by(daemon: &Daemon, owner: &ConnState) -> SessionId {
        let created = handle_session_create(
            daemon,
            owner,
            Id::Number(1),
            serde_json::json!({"mode": "freeform"}),
        );
        created_session_id(&created)
    }

    /// Setup params a bare test daemon accepts as far as its own gates: the
    /// `fetch_any_url` tier needs no endpoint and no key, and — unlike `search`
    /// — is servable on a machine with no local model, so a refusal below is
    /// this seam's answer and never `WEB_SETUP_INVALID` wearing its shape.
    fn setup_params(session: &SessionId) -> Value {
        serde_json::json!({
            "session_id": session.to_string(),
            "tier": "fetch_any_url",
        })
    }

    /// The synchronous `#[test]` bridge to [`route_for_test`], so the setup
    /// family's pre-existing `#[test]` functions can drive `web/setup_commit`
    /// (which left the reader-loop `dispatch` in REQ-575 and may attest on its
    /// own task) without the whole group being rewritten to `#[tokio::test]`.
    ///
    /// **Routing is not decided here.** It delegates to [`route_for_test`] — the
    /// single routing authority — inside a throwaway current-thread runtime, so
    /// the "which methods run off the reader loop" decision cannot drift between
    /// two helpers. Returns `Option<String>` to stay a drop-in for `dispatch(...)`
    /// at the migrated `.unwrap()` call sites. With the default
    /// `UnavailableVerifier` the BR-10(b) check degrades and never parks, so
    /// every migrated test's outcome is byte-identical to when it called
    /// `dispatch` directly.
    fn route_setup(
        daemon: &Daemon,
        conn: &ConnState,
        id: Id,
        method: &str,
        params: Value,
    ) -> Option<String> {
        Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime for the async setup handler")
                .block_on(route_for_test(daemon, conn, id, method, params)),
        )
    }

    /// The `web_setup_rejected` events queued on `sub` right now.
    ///
    /// `try_recv` rather than an awaited `recv` under a timeout: `publish` is
    /// synchronous, so once `dispatch` has returned everything it was going to
    /// announce is already queued, and draining is a decidable fact rather than
    /// a race against the CI scheduler (LESSON-450).
    fn drain_rejections(sub: &mut Subscription) -> Vec<EventEnvelope> {
        let mut seen = Vec::new();
        while let Some(envelope) = sub.try_recv() {
            if matches!(envelope.event, Event::WebSetupRejected(_)) {
                seen.push(envelope);
            }
        }
        seen
    }

    /// **The mutation check AC-4 names, at the commit seam (LESSON-508 rule 2).**
    ///
    /// This test exists because the check it pins is *redundant with a
    /// structural property* — `web/setup_commit` hangs off the client RPC
    /// channel, and tool dispatch cannot reach a `DaemonRuntime`, so no model
    /// tool call can arrive here at all — and a redundant check is precisely
    /// the kind that gets deleted as dead weight by a later reader who
    /// rediscovers the structural argument and not the defense-in-depth reason.
    /// LESSON-508 rule 2: a seam whose enforcement is unreachable end-to-end
    /// still needs its own test, or "unreachable" quietly becomes "unchecked"
    /// the first time the wire shape changes.
    ///
    /// Deleting the `refuse_commit_without_session_access` line from
    /// [`handle_web_setup_commit`] fails this test twice over: the intruder's
    /// call reaches the runtime and comes back with something other than
    /// `NOT_ATTACHED`, and the session's subscriber is told nothing.
    ///
    /// The owner's own commit is the non-vacuity control. It is **not**
    /// asserted to succeed — a bare test daemon has no config path to write to,
    /// which is TASK-129's seam and pinned there — only that whatever it gets
    /// back is not this gate's refusal.
    #[test]
    fn a_commit_without_session_access_is_refused_and_the_session_is_told() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        // Subscribed after the create-time traffic, so what is drained below is
        // this call's doing and nothing else's.
        let mut sub = daemon.events.subscribe(16);

        let refused = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a connection that may not drive this session must not commit its \
             configuration: {refused}"
        );

        let announced = drain_rejections(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "the refusal must be announced once into the session it was aimed \
             at — an RPC error reaches only the attacker (BR-4, LESSON-505)"
        );
        assert_eq!(
            announced[0].session_id.as_ref(),
            Some(&session),
            "and it must be scoped to that session, not broadcast daemon-wide"
        );
        let Event::WebSetupRejected(rejection) = &announced[0].event else {
            unreachable!("filtered above");
        };
        assert_eq!(rejection.origin, SETUP_REJECTED_ORIGIN);
        assert!(
            !rejection.origin.chars().any(|c| c.is_ascii_digit()),
            "the origin names a kind, never an identity: a connection id or a \
             pid would put a number in this string ({})",
            rejection.origin
        );

        let owners_own = route_setup(
            &daemon,
            &owner,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            !owners_own.contains(&error_code::NOT_ATTACHED.to_string()),
            "the session's own client must reach the runtime: {owners_own}"
        );
        assert!(
            drain_rejections(&mut sub).is_empty(),
            "and a served commit must announce no rejection: {owners_own}"
        );
    }

    /// The same gate at the **preview** seam, asserted separately on purpose —
    /// and, since the REQ-572 verify pass, asserted to be *silent* (FIX 1b).
    ///
    /// LESSON-502: an invariant enforced at several seams needs a test at each
    /// one, because the seams are separate lines a future edit drops one at a
    /// time. A single representative test would stay green with the preview's
    /// check deleted — and a preview is where the enablement walkthrough a user
    /// is being led through would be hijacked, one step before the write.
    ///
    /// The silence is the recorded deviation from AC-4's "preview and commit"
    /// (architecture ADR-1's spec-mapping table). A preview writes nothing, so
    /// the notice was the *only* effect an unattached caller could produce; with
    /// session ids readable from an ungated `session/list` that made it a
    /// transcript-injection primitive any same-UID peer could fire at will. The
    /// gate stays; the announcement went to the commit alone.
    ///
    /// The absence is decided by **ordering**, not a timer, exactly as
    /// [`a_refused_plan_is_silent_while_a_refused_commit_is_not`] decides its
    /// own: the commit that follows on the same connection and subscription does
    /// publish one, so an empty drain cannot be a dead subscription.
    #[test]
    fn a_refused_preview_is_silent_while_a_refused_commit_is_not() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        let refused = dispatch(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupPreviewParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a preview reads a session's would-be configuration, so a connection \
             that may not drive it is refused: {refused}"
        );
        assert!(
            drain_rejections(&mut sub).is_empty(),
            "a refused preview writes nothing, so announcing it would hand an \
             unattached peer a line in a stranger's transcript on demand"
        );

        // The positive control, on the same connection and the same
        // subscription: what changes something still announces.
        route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert_eq!(
            drain_rejections(&mut sub).len(),
            1,
            "the subscription is live and the bus does carry this event — which \
             is what makes the silence above a fact rather than a broken fixture"
        );

        let served = dispatch(
            &daemon,
            &owner,
            Id::Number(4),
            WebSetupPreviewParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            served.contains("\"toml\""),
            "the session's own client must be shown what would be written: {served}"
        );
        assert!(drain_rejections(&mut sub).is_empty());
    }

    /// A `provider/setup_preview` candidate a bare test daemon accepts as far as
    /// its own gates: the recipe's own endpoint and model, a keychain reference,
    /// and no bindings — so a refusal below is this seam's answer and never
    /// `PROVIDER_SETUP_INVALID` wearing its shape.
    fn provider_setup_params(session: &SessionId) -> Value {
        serde_json::json!({
            "session_id": session.to_string(),
            "candidate": {
                "id": "kimi",
                "kind": "openai-compatible",
                "endpoint": "https://api.moonshot.ai/v1/chat/completions",
                "model": "kimi-k3",
                "key_ref": "keychain://teton/kimi",
            },
        })
    }

    /// Every event queued on `sub` right now, whatever it is.
    ///
    /// [`drain_rejections`] filters to one variant, which is the right question
    /// for the web trio and the wrong one for REQ-579's reads: their claim is
    /// that a refused read publishes **nothing at all**, and a filter would let
    /// a differently-named notice through.
    fn drain_everything(sub: &mut Subscription) -> Vec<EventEnvelope> {
        let mut seen = Vec::new();
        while let Some(envelope) = sub.try_recv() {
            seen.push(envelope);
        }
        seen
    }

    /// **REQ-579 AC-5, the plan seam.** The session's own connection is
    /// answered; a foreign one gets [`error_code::NOT_ATTACHED`] and the session
    /// is told nothing.
    ///
    /// Asserted at this seam separately from the preview's below, per
    /// LESSON-502: the two gates are separate lines a future edit drops one at a
    /// time, and a single representative test stays green with either deleted.
    ///
    /// The silence is LESSON-513's rule and REQ-572's hard-won one: `plan` and
    /// `preview` write nothing, so an announcement would be the *only* effect an
    /// unattached caller could produce — and with session ids readable from
    /// `session/list`, a notice that fires on demand is a transcript-injection
    /// primitive. REQ-579's `provider_setup_rejected_nonuser` event belongs to
    /// the commit, which is about something trying to **change** the config.
    ///
    /// Note the code: there is no `SETUP_REJECTED_NONUSER` wire code in this
    /// codebase, and the architecture's entity table said otherwise before
    /// TASK-152 corrected it. A foreign caller draws the same `NOT_ATTACHED` the
    /// web reads draw.
    #[test]
    fn a_provider_setup_plan_answers_its_own_session_and_refuses_a_foreign_one_silently() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        let refused = dispatch(
            &daemon,
            &intruder,
            Id::Number(2),
            ProviderSetupPlanParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a plan for a session this connection may not drive is refused: {refused}"
        );
        assert!(
            drain_everything(&mut sub).is_empty(),
            "a refused *read* must put nothing in the user's session (BR-12, \
             LESSON-513)"
        );

        let served = dispatch(
            &daemon,
            &owner,
            Id::Number(3),
            ProviderSetupPlanParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            served.contains("\"catalog\"") && served.contains("\"kimi\""),
            "the session's own client must be served the recipe catalog: {served}"
        );
        assert!(
            served.contains("\"tiers\"") && served.contains("\"think\""),
            "and every routable tier: {served}"
        );
        assert!(
            drain_everything(&mut sub).is_empty(),
            "a served read announces nothing either: it changed nothing"
        );
    }

    /// **REQ-579 AC-5, the preview seam** — the same gate, asserted separately
    /// (LESSON-502), and equally silent.
    ///
    /// A preview is where a walkthrough a user is being led through would be
    /// hijacked, one step before the write, which is why its gate gets its own
    /// test rather than resting on the plan's.
    #[test]
    fn a_provider_setup_preview_answers_its_own_session_and_refuses_a_foreign_one_silently() {
        // A real file behind the runtime: since REQ-579's verify pass a preview
        // on a daemon with nowhere to write is refused outright (BR-8 — the key
        // is stored after the confirm, so a preview that answered would cost a
        // keychain write for a commit that then refuses), and this test is about
        // the *gate*, not about that refusal.
        let (daemon, _path) = daemon_with_a_config_file("provider-preview-gate", Daemon::new());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        let refused = dispatch(
            &daemon,
            &intruder,
            Id::Number(2),
            ProviderSetupPreviewParams::METHOD,
            provider_setup_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a preview renders a session's would-be configuration, so a \
             connection that may not drive it is refused: {refused}"
        );
        assert!(
            drain_everything(&mut sub).is_empty(),
            "a refused preview writes nothing, so announcing it would hand an \
             unattached peer a line in a stranger's transcript on demand"
        );

        let served = dispatch(
            &daemon,
            &owner,
            Id::Number(3),
            ProviderSetupPreviewParams::METHOD,
            provider_setup_params(&session),
        )
        .unwrap();
        assert!(
            served.contains("\"toml\"") && served.contains("\"dial_host\""),
            "the session's own client must be shown what would be written: {served}"
        );
        assert!(drain_everything(&mut sub).is_empty());
    }

    /// A candidate the daemon refuses comes back as `PROVIDER_SETUP_INVALID` —
    /// **not** as the gate's `NOT_ATTACHED`, and not as a preview with a note
    /// attached.
    ///
    /// "You may not do this" and "this config would not load" are different
    /// answers, and folding them would tell an unattached connection its
    /// endpoint was the problem (the wire code's own doc says so).
    #[test]
    fn a_refused_provider_candidate_answers_with_the_setup_code_not_the_gate() {
        // With a file behind it, so the refusal below is the *candidate's* and
        // not the "nowhere to write" `CONFIG_REJECTED` a config-less daemon
        // answers first.
        let (daemon, _path) = daemon_with_a_config_file("provider-candidate-code", Daemon::new());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let mut params = provider_setup_params(&session);
        params["candidate"]["key_ref"] = serde_json::json!("sk-live-not-a-reference");

        let refused = dispatch(
            &daemon,
            &owner,
            Id::Number(2),
            ProviderSetupPreviewParams::METHOD,
            params,
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::PROVIDER_SETUP_INVALID.to_string()),
            "a raw key is a refused candidate, not a refused caller: {refused}"
        );
        assert!(
            !refused.contains("sk-live-not-a-reference"),
            "and the refusal must not echo the credential: {refused}"
        );
    }

    // ------------------------------------------------------------------
    // REQ-579 BR-10/BR-12/BR-15: the provider-setup commit
    // ------------------------------------------------------------------

    /// A hand-written config for the commit's gate tests to *not* have written
    /// to. A comment and a key this flow never touches, so "unchanged" is a
    /// claim with something to lose.
    const PROVIDER_SEED: &str = "# a config the user wrote by hand\neffort = \"high\"\n";

    /// A daemon whose runtime has a **real config file** behind it, seeded with
    /// [`PROVIDER_SEED`].
    ///
    /// A gate test on a config-less `Daemon::new()` cannot tell a refusal that
    /// stopped a write from a refusal that reached a runtime with nowhere to
    /// write: both answer without touching a disk. This one can — every
    /// "wrote nothing" assertion below reads the file's bytes (LESSON-519).
    fn daemon_with_a_config_file(tag: &str, daemon: Daemon) -> (Arc<Daemon>, std::path::PathBuf) {
        let dir = temp_socket(tag).with_extension("d");
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("config.toml");
        std::fs::write(&path, PROVIDER_SEED).expect("seed a config");
        let daemon = Daemon {
            runtime: Arc::new(crate::runtime::DaemonRuntime::minimal_with_config_file(
                &path,
            )),
            ..daemon
        };
        (Arc::new(daemon), path)
    }

    /// The commit form of [`provider_setup_params`], with no digest — the
    /// "do not check" case a caller with no preview to compare against sends.
    fn provider_commit_params(session: &SessionId) -> Value {
        provider_setup_params(session)
    }

    /// The `provider_setup_rejected_nonuser` envelopes queued on `sub` right now.
    ///
    /// Filtered to the variant, unlike [`drain_everything`], because the claim
    /// here is that *this* notice was published — a refused commit may
    /// legitimately share a bus with other traffic.
    fn drain_provider_rejections(sub: &mut Subscription) -> Vec<EventEnvelope> {
        let mut seen = Vec::new();
        while let Some(envelope) = sub.try_recv() {
            if matches!(envelope.event, Event::ProviderSetupRejected(_)) {
                seen.push(envelope);
            }
        }
        seen
    }

    /// **REQ-579 AC-10/AC-11, the commit seam: a connection that did not open
    /// the session is refused, writes nothing, and the session's own user is
    /// told.**
    ///
    /// Three separate claims, because a plausible bug satisfies any two:
    ///
    ///   - the caller gets [`error_code::NOT_ATTACHED`] — the code the web setup
    ///     family already uses for a caller without session access. There is no
    ///     `SETUP_REJECTED_NONUSER` wire code in this codebase, and the
    ///     architecture's entity table said otherwise before TASK-152 corrected
    ///     it;
    ///   - **the config file's bytes are unchanged**, asserted by reading them
    ///     rather than by the absence of an error (LESSON-519). This is what
    ///     fails if the gate is moved below the runtime call;
    ///   - the refusal is announced into the session it was aimed at, because an
    ///     RPC error reaches only the attacker (BR-12, LESSON-505).
    ///
    /// The last block is the mutation check on the announcement *budget's* key:
    /// the same connection's earlier web-setup refusal must not spend the
    /// provider notice's allowance, or the second flow's user hears nothing at
    /// all about a different attempt against their session.
    #[test]
    fn a_provider_setup_commit_without_session_access_is_refused_and_the_session_is_told() {
        let (daemon, config_path) =
            daemon_with_a_config_file("provider-commit-gate", Daemon::new());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let before = std::fs::read_to_string(&config_path).expect("read");
        let mut sub = daemon.events.subscribe(16);

        let refused = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a connection that may not drive this session must not register a \
             provider for it: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read"),
            before,
            "the refused commit wrote to the config file"
        );

        let announced = drain_provider_rejections(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "the refusal must be announced once into the session it was aimed \
             at — an RPC error reaches only the attacker (BR-12, LESSON-505)"
        );
        assert_eq!(
            announced[0].session_id.as_ref(),
            Some(&session),
            "and it must be scoped to that session, not broadcast daemon-wide"
        );
        let Event::ProviderSetupRejected(rejection) = &announced[0].event else {
            unreachable!("filtered above");
        };
        assert_eq!(rejection.method, ProviderSetupCommitParams::METHOD);
        assert!(
            !rejection.method.chars().any(|c| c.is_ascii_digit()),
            "the notice names a method, never an identity: a connection id or a \
             pid would put a number in this string ({})",
            rejection.method
        );
        // Nothing of the candidate rides along — not the id it tried to
        // register, and above all not its credential reference.
        let wire = serde_json::to_string(&announced[0]).expect("the envelope serializes");
        assert!(!wire.contains("keychain"), "{wire}");
        assert!(!wire.contains("moonshot"), "{wire}");

        // The owner's own commit is the non-vacuity control: it is **not**
        // asserted to succeed, only that whatever it gets back is not this
        // gate's refusal.
        let owners_own = route_setup(
            &daemon,
            &owner,
            Id::Number(3),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            !owners_own.contains(&error_code::NOT_ATTACHED.to_string()),
            "the session's own client must reach the runtime: {owners_own}"
        );
        assert!(
            drain_provider_rejections(&mut sub).is_empty(),
            "and a served commit must announce no rejection: {owners_own}"
        );
    }

    /// **One notice per (connection, session, *notice*)** — a web-setup refusal
    /// must not silence the provider-setup one.
    ///
    /// The budget suppresses "a byte-identical duplicate to the identical
    /// audience" ([`ConnState::setup_rejections_announced`]), and these two
    /// sentences are not duplicates: one says something reached for this
    /// session's web access, the other names the provider-setup method that was
    /// refused. Keyed on the session alone, the first would spend the second's
    /// allowance and the user would never hear about the second attempt.
    #[test]
    fn a_web_setup_rejection_does_not_spend_the_provider_setup_notice() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert_eq!(drain_rejections(&mut sub).len(), 1, "the web notice lands");

        route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert_eq!(
            drain_provider_rejections(&mut sub).len(),
            1,
            "the provider notice is a different sentence to the same reader, so \
             the web refusal must not have spent its allowance"
        );

        // And each is still budgeted in its own right: a repeat of either says
        // nothing the first did not.
        for (method, params) in [
            (WebSetupCommitParams::METHOD, setup_params(&session)),
            (
                ProviderSetupCommitParams::METHOD,
                provider_commit_params(&session),
            ),
        ] {
            route_setup(&daemon, &intruder, Id::Number(4), method, params).unwrap();
        }
        assert!(drain_everything(&mut sub).is_empty(), "{session}");
    }

    /// **AC-10 (REQ-579), the BR-10(b) leg: the commit meets the presence gate,
    /// and that gate is load-bearing.**
    ///
    /// [`a_web_setup_commit_refuses_when_the_presence_check_fails`]'s twin, and
    /// the same fixture the `TETON_PRESENCE_ACCEPT=fail` seam installs at the
    /// process boundary (`AlwaysFailsVerifier` — see
    /// [`crate::attest::verifier_from_env`]). With a *present-but-refusing*
    /// verifier the session's own attached client — one that clears every
    /// earlier gate — is still refused, with the attestation code, and the
    /// config file is untouched: proof the check fires before the runtime is
    /// reached rather than after it wrote.
    ///
    /// It is also the mutation test this seam owes (LESSON-508 rule 2): deleting
    /// the `refuse_unattested_commitment` line from
    /// [`handle_provider_setup_commit`] drops the owner's commit through to the
    /// runtime, which writes the file — so both halves go red, independently of
    /// the `web/setup_commit` and `config/set` seams.
    #[test]
    fn a_provider_setup_commit_refuses_when_the_presence_check_fails() {
        let (daemon, config_path) = daemon_with_a_config_file(
            "provider-commit-presence",
            Daemon::new().with_presence_verifier(Box::new(
                crate::attest::AlwaysFailsVerifier::new(
                    crate::attest::AttestationMethod::OsBiometric,
                ),
            )),
        );
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let before = std::fs::read_to_string(&config_path).expect("read");

        let refused = route_setup(
            &daemon,
            &owner,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the session's own client still meets the presence gate: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read"),
            before,
            "the refusal must land before the writer, not after it (LESSON-519)"
        );

        // The contrast, on the same fixture with the mechanism absent: the
        // shipped build has no presence feature, and a commitment degrades there
        // rather than refusing (REQ-570 BR-8) — no new prompt, and no more
        // permissive than `web/setup_commit`.
        let (degraded, degraded_path) =
            daemon_with_a_config_file("provider-commit-degrade", Daemon::new());
        let owner = unattached(&degraded);
        let session = a_session_owned_by(&degraded, &owner);
        let committed = route_setup(
            &degraded,
            &owner,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            !committed.contains(&error_code::ATTESTATION_FAILED.to_string())
                && !committed.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
            "no mechanism must degrade, not refuse: {committed}"
        );
        assert!(
            committed.contains("\"applied\":true"),
            "and the commit lands, proving it got past the degraded gate: {committed}"
        );
        assert!(
            std::fs::read_to_string(&degraded_path)
                .expect("read")
                .contains("id = \"kimi\""),
            "the degraded commit is what wrote the row"
        );
    }

    /// **AC-10 (REQ-579, BR-12): the session and length gates answer before the
    /// presence gate, so a caller that may not act triggers no prompt.**
    ///
    /// `AlwaysFailsVerifier` is the tripwire, exactly as in
    /// [`a_web_setup_commit_answers_the_earlier_gates_before_the_presence_gate`]:
    /// if the presence check ran first, each of these callers would receive
    /// `ATTESTATION_FAILED` instead of the refusal it actually earns, and a
    /// stranger or a malformed id would put an OS prompt on somebody's screen.
    #[test]
    fn a_provider_setup_commit_answers_the_earlier_gates_before_the_presence_gate() {
        let daemon = Daemon::new().with_presence_verifier(Box::new(
            crate::attest::AlwaysFailsVerifier::new(crate::attest::AttestationMethod::OsBiometric),
        ));
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);

        let unattached_refusal = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            unattached_refusal.contains(&error_code::NOT_ATTACHED.to_string())
                && !unattached_refusal.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the session gate must answer before the presence gate, so an unattached \
             caller never triggers a prompt: {unattached_refusal}"
        );

        let oversized = SessionId::from(format!("sess-{}", "a".repeat(4096)).as_str());
        let unmintable_refusal = route_setup(
            &daemon,
            &owner,
            Id::Number(3),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&oversized),
        )
        .unwrap();
        assert!(
            unmintable_refusal.contains(&error_code::INVALID_PARAMS.to_string())
                && !unmintable_refusal.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the length gate must answer before the presence gate: {unmintable_refusal}"
        );
    }

    /// **BR-15: a commit that applied announces it, and one that changed nothing
    /// does not.**
    ///
    /// The event is what lets the interactive surface print "registered; `think`
    /// now routes to it" without polling, and what tells a *second* client
    /// attached to the same session that routing moved under it (LESSON-505). It
    /// carries the id, the kind, the model and the bindings — and no key and no
    /// endpoint (BR-2): a pasted URL can smuggle a credential in its authority,
    /// so there is nowhere here to put one and nothing here that repeats one.
    ///
    /// The second commit is the negative half: `applied: false` registered
    /// nothing, and an event announcing a completed registration would be a
    /// sentence about a write that did not happen.
    #[test]
    fn a_provider_setup_commit_announces_only_what_it_applied() {
        let (daemon, _path) = daemon_with_a_config_file("provider-commit-event", Daemon::new());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let mut sub = daemon.events.subscribe(16);

        let committed = route_setup(
            &daemon,
            &owner,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(committed.contains("\"applied\":true"), "{committed}");

        let announced: Vec<_> = drain_everything(&mut sub)
            .into_iter()
            .filter(|envelope| matches!(envelope.event, Event::ProviderSetupCompleted(_)))
            .collect();
        assert_eq!(announced.len(), 1, "{announced:?}");
        assert_eq!(announced[0].session_id.as_ref(), Some(&session));
        let Event::ProviderSetupCompleted(completed) = &announced[0].event else {
            unreachable!("filtered above");
        };
        assert_eq!(completed.provider_id.0, "kimi");
        assert_eq!(completed.model, "kimi-k3");
        assert!(completed.bindings.is_empty(), "{:?}", completed.bindings);
        // The destination, read off the daemon's own commit answer rather than
        // echoed from the request's endpoint (REQ-579 verify FIX 3).
        assert_eq!(completed.dial_host, "api.moonshot.ai");

        // AC-4: no key, and no endpoint either — asserted on the *wire*, which is
        // where a field would have to appear to leak. The **host** is now
        // carried deliberately and is not the same fact: it is the dial-time
        // parser's answer, so it has no scheme, no path, no query and — the
        // reason the endpoint itself may not travel — no userinfo.
        let wire = serde_json::to_string(&announced[0]).expect("the envelope serializes");
        assert!(!wire.contains("keychain"), "{wire}");
        assert!(!wire.contains("://") && !wire.contains('@'), "{wire}");
        assert!(!wire.contains("/v1/chat/completions"), "{wire}");

        // The same commit again: the config already says exactly this.
        let unchanged = route_setup(
            &daemon,
            &owner,
            Id::Number(3),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(unchanged.contains("\"applied\":false"), "{unchanged}");
        assert!(
            drain_everything(&mut sub)
                .into_iter()
                .all(|envelope| !matches!(envelope.event, Event::ProviderSetupCompleted(_))),
            "a commit that registered nothing announced a completed registration"
        );
    }

    /// **Params that are not a provider-setup request are an invalid-params
    /// error at all three seams** — not a panic, not a silent no-op, and not a
    /// refusal wearing another code (REQ-579 verify FIX 6).
    ///
    /// [`dispatch_rejects_a_malformed_web_refresh`] for this flow's trio, and
    /// asserted as three iterations rather than one representative call
    /// (LESSON-502): each handler opens with its own `from_value` line, and a
    /// future edit that folds one of them into an `unwrap_or_default` — turning
    /// a malformed frame into a **default candidate** with a blank id and a
    /// blank credential reference — fails only its own iteration.
    ///
    /// The commit is driven through [`route_setup`] because it left the reader
    /// loop's `dispatch` (it may park on a presence prompt); the two reads are
    /// served by the same helper, which delegates to the one routing authority.
    #[test]
    fn dispatch_rejects_a_malformed_provider_setup_at_every_seam() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        for method in [
            ProviderSetupPlanParams::METHOD,
            ProviderSetupPreviewParams::METHOD,
            ProviderSetupCommitParams::METHOD,
        ] {
            let refused = route_setup(
                &daemon,
                &owner,
                Id::Number(1),
                method,
                serde_json::json!({"not": "a candidate"}),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::INVALID_PARAMS.to_string()),
                "`{method}` must answer -32602 for params it cannot read: {refused}"
            );
        }
    }

    /// **The provider trio is session-scoped, so none of it joins
    /// [`refuse_daemon_wide`]'s list** — `the_setup_methods_are_session_scoped_and_never_daemon_wide`
    /// for REQ-579's three, asserted separately because they are three more
    /// lines a future edit can move one at a time (LESSON-502).
    ///
    /// The enumeration half catches a future edit that adds one to the
    /// daemon-wide set — including the tempting "fix" of adding
    /// `provider/setup_commit` to `daemon_wide_methods()` so it can ride the
    /// shared presence loops. Its layer (a) gate asks "may this connection drive
    /// this session", which is `may_drive`'s question and not the ancestry
    /// gate's. The behavioural half catches the gate itself being swapped: a
    /// connection the ancestry gate would refuse is answered `NOT_ATTACHED`
    /// here, the same refusal any unattached peer gets.
    ///
    /// It doubles as the routing pin: `METHOD_NOT_FOUND` would fail every arm.
    #[test]
    fn the_provider_setup_methods_are_session_scoped_and_never_daemon_wide() {
        let names = [
            ProviderSetupPlanParams::METHOD,
            ProviderSetupPreviewParams::METHOD,
            ProviderSetupCommitParams::METHOD,
        ];
        for (method, _) in daemon_wide_methods() {
            assert!(
                !names.contains(&method),
                "`{method}` is session-scoped and must not be gated as daemon-wide"
            );
        }

        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            for method in names {
                let daemon = Daemon::new();
                let child = conn_with_ancestry(&daemon, ancestry);
                let response = route_setup(
                    &daemon,
                    &child,
                    Id::Number(1),
                    method,
                    provider_setup_params(&SessionId::from("sess-not-this-connections")),
                )
                .unwrap();
                assert!(
                    response.contains(&error_code::NOT_ATTACHED.to_string()),
                    "`{method}` must answer the session gate, not the ancestry \
                     gate and not `method not found`: {response}"
                );
            }
        }
    }

    /// **`provider/setup_commit` left the synchronous dispatch (REQ-579) — the
    /// reader loop cannot park on its presence prompt.**
    ///
    /// [`the_commit_left_the_reader_loop_dispatch_while_the_reads_stayed`]'s
    /// twin. It may attest, and a presence prompt parks on a human, so it runs
    /// on `handle_client`'s `blocks_on_a_human` task. The direct proof: `dispatch`
    /// answers `method not found` for it, while the two reads — which never
    /// attest — are still served there. Re-adding it to `dispatch`
    /// (reintroducing the parking hazard) turns this red.
    #[test]
    fn the_provider_commit_left_the_reader_loop_dispatch_while_the_reads_stayed() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);

        let commit = dispatch(
            &daemon,
            &owner,
            Id::Number(2),
            ProviderSetupCommitParams::METHOD,
            provider_commit_params(&session),
        )
        .unwrap();
        assert!(
            commit.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "provider/setup_commit must not be served inline by `dispatch` — it \
             runs on the blocks_on_a_human task so a presence prompt cannot park \
             the reader loop: {commit}"
        );

        for read in [
            ProviderSetupPlanParams::METHOD,
            ProviderSetupPreviewParams::METHOD,
        ] {
            let served = dispatch(
                &daemon,
                &owner,
                Id::Number(3),
                read,
                provider_setup_params(&session),
            )
            .unwrap();
            assert!(
                !served.contains(&error_code::METHOD_NOT_FOUND.to_string()),
                "`{read}` is a read that never attests, so it stays on the \
                 synchronous dispatch: {served}"
            );
        }
    }

    // ------------------------------------------------------------------
    // REQ-581 BR-1/AC-6: `provider/test` — the session gate, and the reader
    // loop it must not park
    // ------------------------------------------------------------------

    /// The provider id and model the REQ-581 fixtures register. Named once so a
    /// report assertion and the config that produced it cannot drift.
    const TEST_PROVIDER: &str = "kimi";
    const TEST_MODEL: &str = "kimi-k2";

    /// A loopback listener standing in for a provider endpoint, **counting**
    /// every connection it is dialed on.
    ///
    /// The count is what makes AC-6's "no request leaves the machine" checkable
    /// by inspection rather than inferred from an error code (LESSON-519): a
    /// gate that refuses before the runtime is touched leaves this at zero, and
    /// a refactor that dialed first fills it. Both ends are `127.0.0.1`, and
    /// the listener answers nothing at all — the probe fails fast as
    /// `unreachable`, which is the right *shape* of answer for a gate test and
    /// costs no scripted vendor response.
    struct DialCounter {
        port: u16,
        dials: Arc<AtomicU64>,
    }

    impl DialCounter {
        async fn bound() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback port");
            let port = listener.local_addr().expect("local addr").port();
            let dials = Arc::new(AtomicU64::new(0));
            let counted = Arc::clone(&dials);
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Counted *before* the drop, so a client that observes the
                    // reset observes a count that already includes it.
                    drop(socket);
                }
            });
            Self { port, dials }
        }

        fn endpoint(&self) -> String {
            format!("http://127.0.0.1:{}/v1/chat/completions", self.port)
        }

        fn dials(&self) -> u64 {
            self.dials.load(Ordering::SeqCst)
        }
    }

    /// A free loopback port with **nothing** behind it: bound only to learn a
    /// number the OS is not using, then released, so a probe's connect is
    /// refused immediately rather than waiting out a timeout.
    async fn closed_loopback_endpoint() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1/chat/completions")
    }

    /// A daemon whose one registered provider dials `endpoint`.
    ///
    /// Through a **real config file**, like [`daemon_with_a_config_file`], and
    /// for the same reason plus one more: `DaemonRuntime`'s config is private to
    /// its own module, so a server-level fixture registers a provider the way a
    /// user does — by writing the rows and letting the loader validate them.
    fn daemon_dialing(tag: &str, endpoint: &str) -> Arc<Daemon> {
        let dir = temp_socket(tag).with_extension("d");
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[[providers]]\nid = \"{TEST_PROVIDER}\"\nkind = \"openai-compatible\"\n\
                 endpoint = \"{endpoint}\"\nmodel = \"{TEST_MODEL}\"\n"
            ),
        )
        .expect("seed a config");
        Arc::new(Daemon {
            runtime: Arc::new(crate::runtime::DaemonRuntime::minimal_with_config_file(
                &path,
            )),
            ..Daemon::new()
        })
    }

    /// [`daemon_dialing`], under the **production default** shutdown policy.
    ///
    /// `Daemon::new()`'s fixture policy is [`ShutdownPolicy::Never`], which is
    /// right for a fixture — a daemon that could decide to exit would make
    /// unrelated tests race a shutdown they never asked for — and exactly wrong
    /// for a test *about* that decision: under `Never` a disconnect neither
    /// commits nor defers, so an assertion made against it holds for every
    /// implementation, guard or no guard.
    ///
    /// The supervisor is given the daemon's own bus, so its lifetime stages and
    /// the probe's `provider_tested` are numbered by one counter and can be put
    /// in order against each other.
    fn daemon_dialing_that_exits_with_its_last_client(
        tag: &str,
        endpoint: &str,
    ) -> (Arc<Daemon>, Arc<LifetimeSupervisor>) {
        let base = daemon_dialing(tag, endpoint);
        let lifetime = Arc::new(LifetimeSupervisor::new(
            ShutdownPolicy::OnLastDisconnect,
            PolicySource::Default,
            Arc::clone(&base.events),
        ));
        let daemon = Arc::new(Daemon {
            runtime: Arc::clone(&base.runtime),
            events: Arc::clone(&base.events),
            lifetime: Arc::clone(&lifetime),
            ..Daemon::new()
        });
        (daemon, lifetime)
    }

    /// A vendor that accepts, reads the whole request head, signals, and only
    /// **then** — half a second later — answers with [`PROBE_COMPLETION_SSE`].
    ///
    /// The two facts it manufactures are what both probe-teardown tests rest on:
    /// the probe is genuinely past the send and inside the TTFB window when the
    /// client leaves, and the window stays open long enough that teardown's
    /// decision is made first. An abort lands in microseconds, so neither test
    /// can pass on a race it merely won.
    ///
    /// Returns the port to dial, a receiver that fires once the vendor holds the
    /// request, and the vendor's task handle to abort at the end.
    async fn a_vendor_that_answers_slowly() -> (u16, mpsc::Receiver<()>, JoinHandle<()>) {
        use tokio::io::AsyncReadExt;

        let (entered_tx, entered_rx) = mpsc::channel::<()>(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let vendor = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("the probe dials");
            // Read the request head, so "the call was in flight" is a fact about
            // bytes the vendor holds rather than about a connect that happened.
            let mut head = Vec::new();
            let mut chunk = [0_u8; 1024];
            while let Ok(n) = socket.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = entered_tx.send(()).await;
            // A slow vendor, not a hung one.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{PROBE_COMPLETION_SSE}",
                PROBE_COMPLETION_SSE.len(),
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        (port, entered_rx, vendor)
    }

    /// A `provider/test` request naming the fixture provider.
    fn provider_test_params(session: &SessionId) -> Value {
        serde_json::json!({
            "session_id": session.to_string(),
            "provider_id": TEST_PROVIDER,
        })
    }

    /// The `provider_tested` envelopes queued on `sub` right now.
    fn drain_provider_tests(sub: &mut Subscription) -> Vec<EventEnvelope> {
        drain_everything(sub)
            .into_iter()
            .filter(|envelope| matches!(envelope.event, Event::ProviderTested(_)))
            .collect()
    }

    /// **REQ-581 AC-6: a connection that did not open the session is refused
    /// `NOT_ATTACHED`, the session is told nothing, and the provider is never
    /// dialed.**
    ///
    /// Three claims, because a plausible bug satisfies any two:
    ///
    ///   - the caller gets [`error_code::NOT_ATTACHED`] — the code every other
    ///     `may_drive` seam answers with, so a refused probe cannot be told apart
    ///     from a refused read (ADR-B);
    ///   - **nothing was dialed**, asserted by counting connections at the
    ///     endpoint rather than by the absence of a cost row (LESSON-519). This
    ///     is what fails if the gate is moved below the runtime call — a probe
    ///     that refused *after* sending has already spent the user's money;
    ///   - **nothing was announced.** Unlike `provider/setup_commit`, this
    ///     refusal is silent: a probe writes nothing, so a notice would be the
    ///     only effect an unattached caller could produce, at whatever rate it
    ///     liked (LESSON-513).
    ///
    /// The owner's own call is the non-vacuity control on the middle claim: the
    /// same params against the same fixture *do* reach the wire, so the zero
    /// above is the gate and not a fixture that could never have dialed.
    #[tokio::test]
    async fn a_provider_test_without_session_access_is_refused_silently_and_never_dials() {
        let provider = DialCounter::bound().await;
        let daemon = daemon_dialing("provider-test-gate", &provider.endpoint());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        let refused = handle_provider_test(
            &daemon,
            &intruder,
            Id::Number(2),
            provider_test_params(&session),
        )
        .await;
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a connection that may not drive this session must not spend its \
             provider's money: {refused}"
        );
        assert_eq!(
            provider.dials(),
            0,
            "the refusal must land before anything is dialed — a probe refused \
             after sending has already cost the user (AC-6)"
        );
        assert!(
            drain_everything(&mut sub).is_empty(),
            "a refused probe writes nothing, so announcing it would hand an \
             unattached peer a line in a stranger's transcript on demand \
             (LESSON-513)"
        );

        let served = handle_provider_test(
            &daemon,
            &owner,
            Id::Number(3),
            provider_test_params(&session),
        )
        .await;
        assert!(
            !served.contains(&error_code::NOT_ATTACHED.to_string()),
            "the session's own client must reach the runtime: {served}"
        );
        assert!(
            provider.dials() >= 1,
            "the fixture must be one that genuinely dials, or the zero above \
             proves nothing about the gate"
        );
    }

    /// **REQ-581 BR-3/BR-4: the session's creator gets a typed result, and the
    /// session hears about the call exactly once.**
    ///
    /// The endpoint is a closed port, so the outcome is `unreachable` — which is
    /// the point: what this seam owes is the *shape* of the answer (the report
    /// names what was tested, and one session-scoped announcement carries it),
    /// while every row of the outcome mapping table is the runtime's own test.
    ///
    /// "Exactly one, scoped to this session" is the claim worth pinning here: a
    /// second client attached to this session is owed the news and the health it
    /// left behind (LESSON-505), and a *daemon-scoped* publish would put an
    /// unrelated session's transcript in the path of it.
    #[tokio::test]
    async fn a_provider_test_from_the_sessions_creator_answers_and_announces_once() {
        use teton_protocol::methods::{ProviderTestOutcome, ProviderTestResult};

        let daemon = daemon_dialing("provider-test-owner", &closed_loopback_endpoint().await);
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let mut sub = daemon.events.subscribe(16);

        let answered = handle_provider_test(
            &daemon,
            &owner,
            Id::Number(2),
            provider_test_params(&session),
        )
        .await;
        let parsed: Value = serde_json::from_str(&answered)
            .unwrap_or_else(|e| panic!("provider/test answered with non-JSON ({e}): {answered}"));
        let result: ProviderTestResult = serde_json::from_value(parsed["result"].clone())
            .unwrap_or_else(|e| panic!("provider/test must answer a result ({e}): {answered}"));
        assert_eq!(result.provider_id.0, TEST_PROVIDER);
        assert_eq!(
            result.model, TEST_MODEL,
            "the report names the model that was actually asked for"
        );
        assert!(
            result.dial_host.contains("127.0.0.1"),
            "and the host it was asked of: {}",
            result.dial_host
        );
        assert!(
            matches!(result.outcome, ProviderTestOutcome::Unreachable { .. }),
            "nothing is listening on that port: {:?}",
            result.outcome
        );

        let announced = drain_provider_tests(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "one test announces exactly one `provider_tested`: {announced:?}"
        );
        assert_eq!(
            announced[0].session_id.as_ref(),
            Some(&session),
            "and it is scoped to the session that asked, never broadcast \
             daemon-wide: {:?}",
            announced[0]
        );
    }

    /// **REQ-581 AC-6, the two callers that are *not* the user.**
    ///
    /// A monitor sees every session's events and may drive none of them, and a
    /// daemon descendant — a tool-spawned `teton provider test … --yes`, the
    /// case architecture ADR-5 is written against — is barred from session
    /// access altogether. Both must be refused *before* the dial, or the model
    /// has found a way to make the user's provider spend on its behalf.
    ///
    /// The monitor half is the mutation this pins: `may_receive` is `true` for
    /// this session, so a gate read off the receive policy would send.
    #[tokio::test]
    async fn a_monitor_or_a_daemon_descendant_may_not_test_a_provider() {
        let provider = DialCounter::bound().await;
        let daemon = daemon_dialing("provider-test-others", &provider.endpoint());
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let mut sub = daemon.events.subscribe(16);

        let monitor = monitoring(&daemon);
        assert!(
            monitor.may_receive(Some(&session)),
            "a monitor sees the session's events — the receive side is not the gate"
        );

        let mut callers = vec![("monitor", monitor)];
        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            callers.push(("daemon child", conn_with_ancestry(&daemon, ancestry)));
        }
        for (what, caller) in callers {
            let refused = handle_provider_test(
                &daemon,
                &caller,
                Id::Number(2),
                provider_test_params(&session),
            )
            .await;
            assert!(
                refused.contains(&error_code::NOT_ATTACHED.to_string()),
                "a {what} must be answered by the session gate — not served, and \
                 not `method not found`: {refused}"
            );
        }
        assert_eq!(
            provider.dials(),
            0,
            "and none of them reached the wire (AC-6)"
        );
        assert!(
            drain_everything(&mut sub).is_empty(),
            "nor put anything in the user's session"
        );
    }

    /// **`provider/test` is not served inline by [`dispatch`]** — the membership
    /// half of its reader-loop claim.
    ///
    /// [`the_provider_commit_left_the_reader_loop_dispatch_while_the_reads_stayed`]'s
    /// twin, for the other reason a method leaves the synchronous path: this one
    /// waits on a vendor rather than on a human. Membership is not liveness
    /// (LESSON-518), which is why
    /// [`a_parked_provider_test_does_not_stall_the_connection`] exists as well —
    /// but a future edit that moves the branch back into `dispatch` fails here
    /// first, and says exactly what it broke.
    #[test]
    fn the_provider_test_never_joined_the_reader_loop_dispatch() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);

        let served = dispatch(
            &daemon,
            &owner,
            Id::Number(2),
            ProviderTestParams::METHOD,
            provider_test_params(&session),
        )
        .unwrap();
        assert!(
            served.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "provider/test must not be served inline by `dispatch` — it runs on \
             the blocks_on_a_human task so a round trip to a vendor cannot park \
             the reader loop: {served}"
        );
    }

    /// A raw JSON-RPC client on one end of a [`UnixStream::pair`], for the tests
    /// that need [`handle_client`] itself rather than a handler.
    ///
    /// A pair rather than a bound socket: what is under test is the reader
    /// loop's routing, not `serve`'s accept path, and a pair needs no path, no
    /// `bind`, and no cleanup on a failing assertion.
    struct PairedClient {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl PairedClient {
        /// Attach a client to `daemon` through a live [`handle_client`], and
        /// return it with the connection's task handle.
        fn attached_to(daemon: &Arc<Daemon>) -> (Self, JoinHandle<()>) {
            let (client, server) = UnixStream::pair().expect("a socket pair");
            let connection = tokio::spawn(handle_client(
                server,
                Arc::clone(daemon),
                // The uid is never read past `serve`'s accept check, which a
                // pair does not go through; the pid is, and any pid is
                // `NotDescendant` of a fixture daemon's `Embedded` process.
                PeerIdentity {
                    uid: 0,
                    pid: Some(1),
                },
            ));
            let (read_half, write_half) = client.into_split();
            (
                Self {
                    reader: BufReader::new(read_half),
                    writer: write_half,
                },
                connection,
            )
        }

        async fn send(&mut self, id: i64, method: &str, params: Value) {
            let mut frame = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .expect("a request serializes");
            frame.push('\n');
            self.writer
                .write_all(frame.as_bytes())
                .await
                .expect("the daemon is reading");
            self.writer.flush().await.expect("flush");
        }

        /// Read frames until the response to `id` arrives, failing if the
        /// response to `forbidden` shows up first.
        ///
        /// Events share this wire with responses, so "the next frame" is not the
        /// answer to anything in particular — the loop is what makes the claim
        /// about *responses* rather than about traffic.
        async fn response_to(&mut self, id: i64, forbidden: i64) -> Value {
            loop {
                let mut line = String::new();
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    self.reader.read_line(&mut line),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for the response to {id}"))
                .expect("the connection stays open");
                assert!(read > 0, "the daemon closed the connection");
                let frame: Value = serde_json::from_str(&line).expect("a daemon frame is JSON");
                let answered = frame.get("id").and_then(Value::as_i64);
                assert_ne!(
                    answered,
                    Some(forbidden),
                    "request {forbidden} answered before {id}: {line}"
                );
                if answered == Some(id) {
                    return frame;
                }
            }
        }
    }

    /// The handshake every [`PairedClient`] opens with.
    fn handshake_params() -> Value {
        serde_json::json!({
            "client_kind": "cli",
            "client_name": "server-unit-test",
            "client_version": "0",
            "protocol_min": teton_protocol::PROTOCOL_VERSION_MIN,
            "protocol_max": teton_protocol::PROTOCOL_VERSION_MAX,
        })
    }

    /// **REQ-581 AC-3 / LESSON-518: a `provider/test` waiting on a vendor does
    /// not stall the connection's reader loop.**
    ///
    /// This is the reason the method is on `handle_client`'s own-task path
    /// rather than in the synchronous `dispatch`, and the reason a *membership*
    /// test ([`the_provider_test_never_joined_the_reader_loop_dispatch`]) is not
    /// enough on its own: membership proves where the branch is, liveness proves
    /// what the branch buys.
    ///
    /// The loopback provider accepts the connection and then says **nothing at
    /// all**, which is a vendor taking its time — the provider transport carries
    /// no timeout by design (a long completion is not a stalled one), so the
    /// probe is genuinely parked for as long as this test wants it. While it is
    /// parked, a `session/list` on the *same* connection must be answered.
    ///
    /// The `entered` channel is what makes it non-vacuous: it proves the
    /// concurrent RPC was served *after* the probe had actually reached the wire
    /// and stopped there, not before it got going. On a multi-thread runtime for
    /// the REQ-575 precedent's reason — it is the production flavour, and a
    /// single worker could serve the second RPC only by the first having yielded
    /// anyway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_parked_provider_test_does_not_stall_the_connection() {
        let (entered_tx, mut entered_rx) = mpsc::channel::<()>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let parked = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("the probe dials");
            let _ = entered_tx.send(()).await;
            // Hold the connection — and the socket — open until released, so
            // the probe has nothing to complete on.
            let _ = release_rx.await;
            drop(socket);
        });

        let daemon = daemon_dialing(
            "provider-test-parked",
            &format!("http://127.0.0.1:{port}/v1/chat/completions"),
        );
        let (mut client, connection) = PairedClient::attached_to(&daemon);
        client
            .send(1, HandshakeParams::METHOD, handshake_params())
            .await;
        client.response_to(1, 3).await;
        client
            .send(
                2,
                SessionCreateParams::METHOD,
                serde_json::json!({"mode": "freeform"}),
            )
            .await;
        let created = client.response_to(2, 3).await;
        let session = created["result"]["session_id"]
            .as_str()
            .expect("session/create returns an id")
            .to_owned();

        // Fire the probe and do NOT await its response: it is parked at the
        // vendor and will not answer until this test lets it.
        client
            .send(
                3,
                ProviderTestParams::METHOD,
                serde_json::json!({"session_id": session, "provider_id": TEST_PROVIDER}),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the probe must actually reach the provider and park there")
            .expect("the parked server signals on accept");

        // The reader loop is free: a second RPC on the SAME connection is
        // answered while the probe is still waiting.
        client
            .send(4, SessionListParams::METHOD, serde_json::json!({}))
            .await;
        let listed = client.response_to(4, 3).await;
        assert!(
            listed["result"]["sessions"].is_array(),
            "the concurrent session/list must return a normal result while the \
             probe is parked off the reader loop: {listed}"
        );

        // Release the parked provider and end the connection, so neither task
        // outlives the test.
        let _ = release_tx.send(());
        connection.abort();
        parked.abort();
    }

    /// The SSE one scripted vendor answers a probe with: a delta, a usage chunk
    /// and the terminator — the shape both the adapter's parser and the cost
    /// meter's byte scan read, so one fixture proves the row and the outcome.
    const PROBE_COMPLETION_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    /// **REQ-581 verify F2: a client that disconnects mid-probe does not lose
    /// the call it has already paid for.**
    ///
    /// A `provider/test` runs on the same own-task branch as the seven consent
    /// methods, and teardown `abort()`s that branch's list unconditionally — so
    /// with the probe filed there, a user who closed their terminal while the
    /// request was in flight lost the ledger row, the health record and the
    /// `provider_tested` event for a call the vendor will still bill. That is
    /// REQ-565's hole for turns, arriving by a second route, and the window it
    /// is widest in (request out, nothing back) is the one a person is most
    /// likely to give up during.
    ///
    /// The fixture is built so that the *order* is the mechanism:
    ///
    ///   1. the vendor accepts, reads the whole request, and only then signals —
    ///      so the probe is genuinely past the send and inside the TTFB window;
    ///   2. the client's socket is dropped, which ends the reader loop and
    ///      starts teardown;
    ///   3. the vendor answers, half a second later.
    ///
    /// Filed under `attach_tasks`, step 2 kills the probe long before step 3 and
    /// both assertions below read zero. Filed under `prompt_tasks`, teardown
    /// waits, the completion lands, and the row and the event survive a client
    /// that is already gone. Neither assertion can be satisfied by the socket:
    /// the ledger and the event bus are daemon-scoped, which is the point — they
    /// outlive the connection that caused them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_survives_its_clients_disconnect_and_keeps_its_row() {
        let (port, mut entered_rx, vendor) = a_vendor_that_answers_slowly().await;

        let daemon = daemon_dialing(
            "provider-test-disconnect",
            &format!("http://127.0.0.1:{port}/v1/chat/completions"),
        );
        // Subscribed before the probe, so "the event landed" is read off the
        // daemon's own bus rather than off the socket the test is about to close.
        let mut sub = daemon.events.subscribe(16);
        let (mut client, connection) = PairedClient::attached_to(&daemon);
        client
            .send(1, HandshakeParams::METHOD, handshake_params())
            .await;
        client.response_to(1, 3).await;
        client
            .send(
                2,
                SessionCreateParams::METHOD,
                serde_json::json!({"mode": "freeform"}),
            )
            .await;
        let created = client.response_to(2, 3).await;
        let session = created["result"]["session_id"]
            .as_str()
            .expect("session/create returns an id")
            .to_owned();

        assert_eq!(
            daemon
                .runtime
                .cost_report()
                .expect("a report")
                .report
                .probe_calls,
            0,
            "the ledger must start empty, or `the row survived` is unfalsifiable"
        );

        client
            .send(
                3,
                ProviderTestParams::METHOD,
                serde_json::json!({"session_id": session, "provider_id": TEST_PROVIDER}),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the probe must reach the vendor before the client leaves")
            .expect("the vendor signals once it holds the request");

        // The user closes their terminal. Both halves go, so the reader loop
        // ends and teardown begins.
        drop(client);

        // Teardown must finish on its own — which, for a drained task, means
        // waiting out the vendor. Bounded so a regression to `abort()` cannot
        // hang the suite; generous so a loaded machine cannot fail it.
        tokio::time::timeout(std::time::Duration::from_secs(30), connection)
            .await
            .expect("the connection task must finish once the probe does")
            .expect("handle_client does not panic");

        let report = daemon.runtime.cost_report().expect("a report").report;
        assert_eq!(
            report.probe_calls, 1,
            "the vendor billed this call whether or not anyone was still on the \
             socket; aborting the task at its await point loses the row for money \
             already spent (REQ-565's hole, by another route)"
        );
        assert_eq!(
            report.total_calls, 1,
            "and it is one call, counted once: {report:?}"
        );

        let announced = drain_provider_tests(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "the outcome is still published: a second client attached to this \
             session is owed the news and the health it routes by, and it did not \
             disconnect. Saw: {announced:?}"
        );
        let Event::ProviderTested(tested) = &announced[0].event else {
            unreachable!("filtered by `drain_provider_tests`")
        };
        assert!(
            matches!(
                tested.outcome,
                teton_protocol::methods::ProviderTestOutcome::Reached { .. }
            ),
            "and it is the outcome the vendor actually produced, not a synthetic \
             failure minted by the teardown: {tested:?}"
        );

        vendor.abort();
    }

    /// **REQ-581 verify G1: an in-flight probe *defers* the shutdown its
    /// client's disconnect arms.**
    ///
    /// [`a_probe_survives_its_clients_disconnect_and_keeps_its_row`]'s other
    /// half, and the one that makes the first true on a daemon that is its own
    /// process. Filing the probe under `prompt_tasks` makes [`handle_client`]
    /// wait for it. It does not make the *daemon* wait: teardown drops the
    /// client guard **before** it drains, so a last client leaving asks the
    /// supervisor to decide while the probe is still inside the TTFB window.
    /// With no activity claimed, [`teton_core::lifetime::LifetimeState`] sees an
    /// idle daemon, commits under the default `on-last-disconnect` policy,
    /// `serve` returns, and `main` reaches its `_exit` — the drain then runs
    /// exactly as far as the process lets it, and the ledger row, the health
    /// record and the `provider_tested` event for money already spent are lost
    /// anyway, by the route the drain was added to close.
    ///
    /// So the claim here is a **sequence**, not a state: `client_disconnected`,
    /// then `daemon_shutdown_deferred` naming the blocker, and only then the
    /// probe's `provider_tested`. Read off the bus's own sequence numbers rather
    /// than off a phase sampled by a poll loop, so no assertion depends on this
    /// test out-running a 500 ms vendor.
    ///
    /// Non-vacuous by the mutation it was written against: delete the guard and
    /// the disconnect *commits* instead of deferring, so no
    /// `daemon_shutdown_deferred` is ever published and the first assertion
    /// fails. The two ordering assertions fail on the weaker mutations — a guard
    /// taken inside the task (the claim may not exist when the disconnect lands)
    /// or one held past the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_in_flight_defers_the_shutdown_rather_than_letting_it_commit() {
        use teton_core::lifetime::LifetimePhase;
        use teton_protocol::events::{DaemonLifetime, DaemonLifetimeStage};

        let (port, mut entered_rx, vendor) = a_vendor_that_answers_slowly().await;
        let (daemon, lifetime) = daemon_dialing_that_exits_with_its_last_client(
            "provider-test-defer",
            &format!("http://127.0.0.1:{port}/v1/chat/completions"),
        );
        // Generous, and drained once at the end: every assertion below is about
        // the *order* events were published in, so an eviction for lagging would
        // not weaken the test, it would delete it.
        let mut sub = daemon.events.subscribe(256);

        let (mut client, connection) = PairedClient::attached_to(&daemon);
        client
            .send(1, HandshakeParams::METHOD, handshake_params())
            .await;
        client.response_to(1, 3).await;
        client
            .send(
                2,
                SessionCreateParams::METHOD,
                serde_json::json!({"mode": "freeform"}),
            )
            .await;
        let created = client.response_to(2, 3).await;
        let session = created["result"]["session_id"]
            .as_str()
            .expect("session/create returns an id")
            .to_owned();
        assert_eq!(
            lifetime.client_count(),
            1,
            "the premise: this connection is the daemon's only client, so its \
             disconnect is the one that arms a shutdown"
        );

        client
            .send(
                3,
                ProviderTestParams::METHOD,
                serde_json::json!({"session_id": session, "provider_id": TEST_PROVIDER}),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the probe must reach the vendor before the client leaves")
            .expect("the vendor signals once it holds the request");

        // The user closes their terminal, with the request out and nothing back.
        drop(client);

        tokio::time::timeout(std::time::Duration::from_secs(30), connection)
            .await
            .expect("the connection task must finish once the probe does")
            .expect("handle_client does not panic");

        let seen = drain_everything(&mut sub);
        let disconnected = seen.iter().find_map(|envelope| match &envelope.event {
            Event::DaemonLifetime(DaemonLifetime {
                stage: DaemonLifetimeStage::ClientDisconnected { .. },
            }) => Some(envelope.seq),
            _ => None,
        });
        let deferred = seen.iter().find_map(|envelope| match &envelope.event {
            Event::DaemonLifetime(DaemonLifetime {
                stage: DaemonLifetimeStage::ShutdownDeferred { blocking_activity },
            }) => Some((envelope.seq, *blocking_activity)),
            _ => None,
        });
        let tested = seen.iter().find_map(|envelope| match &envelope.event {
            Event::ProviderTested(_) => Some(envelope.seq),
            _ => None,
        });

        let (deferred_seq, blocker) = deferred.unwrap_or_else(|| {
            panic!(
                "the last client's disconnect must DEFER, not commit: an \
                 unclaimed probe leaves the supervisor looking at an idle \
                 daemon, so `serve` returns and `main` _exit()s out from under \
                 the very drain that is supposed to keep this call's row. Saw: \
                 {seen:?}"
            )
        });
        assert_eq!(
            blocker,
            BlockingActivity::Turn,
            "and the blocker it names is the probe's claim — a probe is a billed \
             call with a durable row, which is what `Turn` means to the lifetime"
        );

        let disconnected_seq =
            disconnected.unwrap_or_else(|| panic!("the client disconnected: {seen:?}"));
        let tested_seq =
            tested.unwrap_or_else(|| panic!("the probe still published its outcome: {seen:?}"));
        assert!(
            disconnected_seq < deferred_seq,
            "the deferral is the answer to *this* disconnect, and answers come \
             after their question: disconnected@{disconnected_seq}, \
             deferred@{deferred_seq}"
        );
        assert!(
            deferred_seq < tested_seq,
            "and it was published while the probe was still in flight — a \
             deferral that only appeared after the outcome would be a claim taken \
             too late to protect anything: deferred@{deferred_seq}, \
             tested@{tested_seq}"
        );

        // The other end of the same claim: the guard is released when the probe
        // finishes, and *that* is what finally commits the shutdown. A claim
        // that outlived its task would wedge the daemon alive instead — the
        // standing-resident harm REQ-565 exists to remove, by this route.
        assert!(
            lifetime.is_committed(),
            "the probe finished, so its claim is gone and the deferred shutdown \
             must commit — a guard that leaked would hold the daemon open forever"
        );
        assert_eq!(lifetime.phase(), LifetimePhase::Committed);

        vendor.abort();
    }

    /// **One notice per (connection, session), however many commits it
    /// refuses** (REQ-572 verify FIX 1c, re-keyed by BUG-166 — the
    /// [`ConnState::may_announce_grant`] precedent).
    ///
    /// The refusal is unbounded and free to the caller, so an unbudgeted
    /// announcement is a line the attacker gets to write into a stranger's
    /// transcript once per RPC — which is both a flood and, worse, the way a
    /// genuine warning is buried under its own repetitions.
    ///
    /// Both halves matter and both are asserted: the *second* commit is still
    /// refused (the budget bounds the notice, never the enforcement), and the
    /// notice count for this (connection, session) pair's whole life is
    /// exactly one.
    #[test]
    fn a_connection_announces_at_most_one_setup_rejection_per_session() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        for id in [2, 3] {
            let refused = route_setup(
                &daemon,
                &intruder,
                Id::Number(id),
                WebSetupCommitParams::METHOD,
                setup_params(&session),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::NOT_ATTACHED.to_string()),
                "the budget bounds the announcement, never the refusal — commit \
                 {id} must still be refused: {refused}"
            );
        }

        assert_eq!(
            drain_rejections(&mut sub).len(),
            1,
            "two refused commits from one connection against one session \
             announced more than once: the second notice says nothing the first \
             did not, and a caller that can repeat it chooses how much of the \
             user's transcript it writes"
        );

        // A *different* connection has its own budget, which is the stated limit
        // of this bound (see `ConnState::setup_rejections_announced`): what is
        // capped is one connection's loop, not a peer that reconnects.
        let second = unattached(&daemon);
        route_setup(
            &daemon,
            &second,
            Id::Number(4),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert_eq!(
            drain_rejections(&mut sub).len(),
            1,
            "the budget is per connection, so a fresh connection's first refusal \
             is still announced — the alternative would silence the notice for \
             every client after the first offender"
        );
    }

    /// **The budget's key carries the session, because the audience does**
    /// (BUG-166 consequence 2).
    ///
    /// Under the original per-connection bool, a connection refused on session
    /// A and then on session B announced only into A — and B's user, a
    /// different person watching a different transcript, was never told that
    /// something tried to change *their* session's capability. Reverting the
    /// key to the connection alone fails this test's second drain.
    #[test]
    fn a_refusal_against_a_second_session_announces_into_that_session_too() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let first = a_session_owned_by(&daemon, &owner);
        let second = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&first),
        )
        .unwrap();
        let announced = drain_rejections(&mut sub);
        assert_eq!(announced.len(), 1);
        assert_eq!(announced[0].session_id.as_ref(), Some(&first));

        route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&second),
        )
        .unwrap();
        let announced = drain_rejections(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "the same connection aimed at a *different* session, and that \
             session's user is owed their own notice — a budget keyed on the \
             connection alone spends B's warning on A's transcript"
        );
        assert_eq!(
            announced[0].session_id.as_ref(),
            Some(&second),
            "and it must be scoped to the session that was aimed at"
        );
    }

    /// **A session id that names nothing buys no notice and burns no budget**
    /// (BUG-166 consequences 1 and 3's motive — the burn attack).
    ///
    /// Under the unconditional spend, one refused commit against a
    /// plausible-length id that named nothing published into the void, spent
    /// the connection's whole announcement budget on an audience of zero, and
    /// silenced every later notice the connection owed real sessions — an
    /// attacker's first call, needing no real session id at all, muted BR-4's
    /// announcement leg for the connection's life. Both halves are asserted:
    /// the junk-id refusal publishes **nothing** (a monitor-scope subscriber
    /// receives every session's events, so a phantom envelope wearing an
    /// attacker-chosen id is itself injected noise), and the same connection's
    /// next refusal against a real session still lands in that session.
    #[test]
    fn a_nonexistent_session_buys_no_notice_and_burns_no_budget() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        // Mintable length — `sess-` plus a 26-char body — so this passes the
        // F9 length gate and reaches the announcement seam it is aimed at.
        let phantom = SessionId::from("sess-aaaaaaaaaaaaaaaaaaaaaaaaaa");
        let refused = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&phantom),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "the refusal must stay byte-identical for a session that never \
             existed — the existence check gates the notice, never the answer \
             (ADR-B): {refused}"
        );
        assert!(
            drain_rejections(&mut sub).is_empty(),
            "a notice for a session nobody minted informs nobody entitled and \
             hands every monitor an envelope wearing an attacker-chosen id"
        );

        let refused = route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(refused.contains(&error_code::NOT_ATTACHED.to_string()));
        let announced = drain_rejections(&mut sub);
        assert_eq!(
            announced.len(),
            1,
            "the junk-id refusal above must not have spent this connection's \
             budget — that spend is the BUG-166 burn attack, and it silences \
             the one notice a real session's user is owed"
        );
        assert_eq!(announced[0].session_id.as_ref(), Some(&session));
    }

    /// **A `session_id` longer than this daemon could have minted is refused
    /// before anything else happens** (REQ-572 verify, FIX 1a; REQ-569 F9's
    /// rule at the setup family's three seams).
    ///
    /// The id is attacker-chosen and bounded only by [`MAX_FRAME`], and each
    /// handler then did work proportional to it — a hash on every `may_drive`,
    /// and, at the commit, a clone into an event envelope every subscriber
    /// holds. All three are asserted, because they are three separate lines
    /// (LESSON-502).
    ///
    /// `INVALID_PARAMS`, not `NOT_ATTACHED`: this is a well-formedness fact the
    /// caller already knows, so answering it reveals nothing about which
    /// sessions exist (ADR-B). And the commit's arm asserts the **silence**,
    /// which is the point of putting the check first: an oversized id must not
    /// buy the publish that a plausible one would.
    #[test]
    fn an_unmintable_session_id_is_refused_by_every_setup_method_before_anything_else() {
        let daemon = Daemon::new();
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);
        let oversized = SessionId::from(format!("sess-{}", "a".repeat(4096)).as_str());

        // REQ-585 ADR-2: `skills/list` joins the sweep, and joins **both**
        // loops. Its params are `{session_id}` alone, so it is well-formed
        // under either helper's shape — which is the property worth asserting
        // twice: serde ignores the extra keys, so what refuses the call is the
        // length check rather than a parse failure wearing its code
        // (LESSON-502).
        for method in [
            WebSetupPlanParams::METHOD,
            WebSetupPreviewParams::METHOD,
            WebSetupCommitParams::METHOD,
            SkillsListParams::METHOD,
            // REQ-589 BR-13: `skills/preflight` joins for `skills/list`'s
            // reason and under its gate. Its params carry `verbose` beside the
            // id, which `#[serde(default)]` makes optional — so it is
            // well-formed under both helpers' shapes and what refuses it is the
            // length check rather than a parse failure wearing its code.
            SkillsPreflightParams::METHOD,
        ] {
            let refused = route_setup(
                &daemon,
                &intruder,
                Id::Number(1),
                method,
                setup_params(&oversized),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::INVALID_PARAMS.to_string()),
                "`{method}` accepted an id no daemon could have minted, so an \
                 unattached peer sizes the work it costs: {refused}"
            );
        }

        // REQ-579's three, at the same seam and for the same reason. They are a
        // separate loop because they need a *well-formed* candidate: params the
        // handler cannot read would answer `INVALID_PARAMS` from the parse and
        // make every assertion above true of a method with no length check at
        // all (LESSON-502 — the vacuity is the failure mode).
        for method in [
            ProviderSetupPlanParams::METHOD,
            ProviderSetupPreviewParams::METHOD,
            ProviderSetupCommitParams::METHOD,
            SkillsListParams::METHOD,
            SkillsPreflightParams::METHOD,
        ] {
            let refused = route_setup(
                &daemon,
                &intruder,
                Id::Number(1),
                method,
                provider_setup_params(&oversized),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::INVALID_PARAMS.to_string()),
                "`{method}` accepted an id no daemon could have minted: {refused}"
            );
        }

        assert!(
            drain_rejections(&mut sub).is_empty(),
            "the length check must come before the publish, or a 4 MiB id still \
             buys a 4 MiB event envelope in every subscriber's queue"
        );
        assert!(
            drain_provider_rejections(&mut sub).is_empty(),
            "and the provider commit's own notice is budgeted by the same rule — \
             an oversized id must not buy the publish a plausible one would"
        );

        // Non-vacuity: the same call with a *mintable* id gets past this check
        // and reaches the session gate, so the refusals above are the length
        // rule and not the setup family being unroutable.
        let session = a_session_owned_by(&daemon, &unattached(&daemon));
        let gated = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            gated.contains(&error_code::NOT_ATTACHED.to_string()),
            "a plausible id must reach the session gate: {gated}"
        );
        let gated = route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            ProviderSetupCommitParams::METHOD,
            provider_setup_params(&session),
        )
        .unwrap();
        assert!(
            gated.contains(&error_code::NOT_ATTACHED.to_string()),
            "and so must a provider commit's, or its loop above proves nothing \
             about a length check either: {gated}"
        );
    }

    /// **F9's length rule at the driving seams the setup family's fix left
    /// out** (BUG-166 residual (c)): `web/override`, `session/permissions`,
    /// `session/clear`.
    ///
    /// Each hashes an attacker-chosen id through `may_drive` with nothing in
    /// front of it, so an unattached peer sized the work its refusal cost. The
    /// three are asserted in one loop because the claim is identical, but each
    /// iteration exercises its own handler's line — deleting any one of the
    /// three checks fails its iteration (LESSON-502).
    ///
    /// The non-vacuity control mirrors the setup family's: a *mintable* id
    /// draws `NOT_ATTACHED` from the same call, so the refusals above are the
    /// length rule and not the methods being unroutable.
    #[test]
    fn an_unmintable_session_id_is_refused_by_every_driving_method_before_the_gate() {
        let daemon = Daemon::new();
        let intruder = unattached(&daemon);
        let oversized = SessionId::from(format!("sess-{}", "a".repeat(4096)).as_str());

        // REQ-583: `session/set_cwd` joins the sweep. Its params carry a `cwd`
        // beside the id — a well-formed, existing one, so the only thing that
        // can refuse the plausible-id control is the gate and not the parser
        // (a parse failure would answer INVALID_PARAMS too and make the first
        // assertion vacuous for this method).
        let params_for = |method: &str, session_id: String| -> Value {
            if method == SessionSetCwdParams::METHOD {
                serde_json::json!({"session_id": session_id, "cwd": std::env::temp_dir()})
            } else {
                serde_json::json!({"session_id": session_id})
            }
        };
        // REQ-585: `skills/list` joins for `session/permissions`' reason — it is
        // a *read* behind the same gate, and reading a session's commands is
        // reading that session. This loop is where its gate leg is asserted:
        // the setup sweep above only pins the length check.
        for method in [
            WebOverrideParams::METHOD,
            SessionPermissionsParams::METHOD,
            SessionClearParams::METHOD,
            SessionSetCwdParams::METHOD,
            SkillsListParams::METHOD,
            SkillsPreflightParams::METHOD,
        ] {
            let refused = dispatch(
                &daemon,
                &intruder,
                Id::Number(1),
                method,
                params_for(method, oversized.to_string()),
            )
            .unwrap();
            assert!(
                refused.contains(&error_code::INVALID_PARAMS.to_string()),
                "`{method}` accepted an id no daemon could have minted: {refused}"
            );

            let gated = dispatch(
                &daemon,
                &intruder,
                Id::Number(2),
                method,
                params_for(method, "sess-plausible".to_owned()),
            )
            .unwrap();
            assert!(
                gated.contains(&error_code::NOT_ATTACHED.to_string()),
                "`{method}`: a plausible id must reach the session gate: {gated}"
            );
        }
    }

    /// The same rule at the **prompt** spawn, asserted apart from its dispatch
    /// siblings because `session/prompt` bypasses `dispatch` entirely — its
    /// gate lives in `spawn_prompt_turn`, which is a separate line a future
    /// edit drops separately (LESSON-502; BUG-166 residual (c)).
    #[tokio::test]
    async fn an_unmintable_session_id_is_refused_by_the_prompt_spawn() {
        let daemon = Arc::new(Daemon::new());
        let conn = unattached(&daemon);
        let oversized = format!("sess-{}", "a".repeat(4096));
        let prompt = serde_json::json!({
            "session_id": oversized,
            "prompt": [{"type": "text", "text": "hello"}],
        });
        let (tx, mut rx) = mpsc::channel::<String>(4);

        let handle = spawn_prompt_turn(
            &daemon,
            &conn,
            Id::Number(1),
            prompt,
            &tx,
            None,
            ClientPresence::unwatched(),
        );
        assert!(
            handle.is_none(),
            "an unmintable id must not spawn a turn task"
        );
        let refused = rx.try_recv().expect("a refusal is queued for the client");
        assert!(
            refused.contains(&error_code::INVALID_PARAMS.to_string()),
            "the length rule answers before the attachment gate: {refused}"
        );
        assert!(
            !refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "{refused}"
        );
    }

    /// The **plan** is gated too, and announces nothing — both halves together,
    /// because each is wrong without the other.
    ///
    /// Gated: reading a session's capability posture is still reading that
    /// session, and an ungated read would make the refusal an oracle for which
    /// sessions exist (ADR-B).
    ///
    /// Silent: BR-4's notice is about something trying to *change* the
    /// capability. If a refused read announced too, any same-UID peer could
    /// write lines into a stranger's session on demand — a notice that can be
    /// made to cry wolf is one users learn to skip past, which costs the
    /// commit's rejection the attention it exists for.
    ///
    /// The absence is decided by *ordering*, not by a timer: the commit that
    /// follows on the same connection and the same subscription does publish
    /// one, so the empty drain cannot be the bus being broken or the
    /// subscription being dead.
    #[test]
    fn a_refused_plan_is_silent_while_a_refused_commit_is_not() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);
        let mut sub = daemon.events.subscribe(16);

        let refused = dispatch(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupPlanParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::NOT_ATTACHED.to_string()),
            "a plan for a session this connection may not drive is refused: {refused}"
        );
        assert!(
            drain_rejections(&mut sub).is_empty(),
            "a refused *read* must not put a notice in the user's session"
        );

        // The positive control, on the same connection and subscription.
        route_setup(
            &daemon,
            &intruder,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert_eq!(
            drain_rejections(&mut sub).len(),
            1,
            "the subscription is live and the bus does carry this event — which \
             is what makes the silence above a fact rather than a broken fixture"
        );

        // And the session's own client is served the plan.
        let served = dispatch(
            &daemon,
            &owner,
            Id::Number(4),
            WebSetupPlanParams::METHOD,
            serde_json::json!({"session_id": session.to_string()}),
        )
        .unwrap();
        assert!(
            served.contains("\"state\""),
            "the attached client must be told what enabling would involve: {served}"
        );
    }

    /// **AC-4's last clause**: the three setup methods are session-scoped, so
    /// none of them joins [`refuse_daemon_wide`]'s list.
    ///
    /// Asserted twice, because the two halves catch different mistakes. The
    /// enumeration catches a future edit that adds one to the daemon-wide set;
    /// the behavioural half catches the gate itself being swapped — a
    /// connection the ancestry gate would refuse is answered `NOT_ATTACHED`
    /// here, the same refusal any unattached peer gets, so the setup family's
    /// question stays "may this connection drive this session" rather than
    /// "where did this process come from".
    ///
    /// It doubles as the routing pin: `METHOD_NOT_FOUND` would fail every arm.
    #[test]
    fn the_setup_methods_are_session_scoped_and_never_daemon_wide() {
        let names = [
            WebSetupPlanParams::METHOD,
            WebSetupPreviewParams::METHOD,
            WebSetupCommitParams::METHOD,
        ];
        for (method, _) in daemon_wide_methods() {
            assert!(
                !names.contains(&method),
                "`{method}` is session-scoped and must not be gated as daemon-wide"
            );
        }

        for ancestry in [Ancestry::Descendant, Ancestry::Indeterminate] {
            for method in names {
                let daemon = Daemon::new();
                let child = conn_with_ancestry(&daemon, ancestry);
                let response = route_setup(
                    &daemon,
                    &child,
                    Id::Number(1),
                    method,
                    setup_params(&SessionId::from("sess-not-this-connections")),
                )
                .unwrap();
                assert!(
                    response.contains(&error_code::NOT_ATTACHED.to_string()),
                    "`{method}` must answer the session gate, not the ancestry \
                     gate and not `method not found`: {response}"
                );
            }
        }
    }

    /// **AC-1 / AC-5 (REQ-575): the commit meets the BR-10(b) presence gate, and
    /// that gate is load-bearing.**
    ///
    /// With a *present-but-refusing* verifier, the session's own (attached)
    /// client — one that clears every earlier gate — is still refused, with the
    /// attestation code and not the runtime's `CONFIG_REJECTED`. That is the proof
    /// the check fires *before* the runtime is reached, so nothing is written.
    ///
    /// It is also the mutation test the new seam owes (LESSON-508 rule 2):
    /// deleting the `refuse_unattested_commitment` line from
    /// [`handle_web_setup_commit`] drops the owner's commit through to the runtime,
    /// which answers `CONFIG_REJECTED` (a bare daemon has no config path) — a
    /// different code, so this test goes red. It is red for a reason unique to
    /// this seam, independent of the `model/confirm`/`model/set` seams.
    #[test]
    fn a_web_setup_commit_refuses_when_the_presence_check_fails() {
        let daemon = Daemon::new().with_presence_verifier(Box::new(
            crate::attest::AlwaysFailsVerifier::new(crate::attest::AttestationMethod::OsBiometric),
        ));
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);

        let refused = route_setup(
            &daemon,
            &owner,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            refused.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the session's own client still meets the presence gate: {refused}"
        );
        assert!(
            !refused.contains(&error_code::CONFIG_REJECTED.to_string()),
            "the refusal is the presence gate, not the runtime — a CONFIG_REJECTED \
             here means the commit reached the runtime, i.e. the attestation line \
             was skipped: {refused}"
        );
    }

    /// **AC-4 (REQ-575, BR-2): the session and length gates answer before the
    /// presence gate, so a caller that may not act triggers no prompt.**
    ///
    /// `AlwaysFailsVerifier` is the tripwire: if the presence check ran first,
    /// each of these callers would receive `ATTESTATION_FAILED` instead of the
    /// refusal it actually earns. The point of the ordering is that a stranger, or
    /// a malformed id, never puts an OS prompt on anyone's screen.
    #[test]
    fn a_web_setup_commit_answers_the_earlier_gates_before_the_presence_gate() {
        let daemon = Daemon::new().with_presence_verifier(Box::new(
            crate::attest::AlwaysFailsVerifier::new(crate::attest::AttestationMethod::OsBiometric),
        ));
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);
        let intruder = unattached(&daemon);

        let unattached_refusal = route_setup(
            &daemon,
            &intruder,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            unattached_refusal.contains(&error_code::NOT_ATTACHED.to_string())
                && !unattached_refusal.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the session gate must answer before the presence gate, so an unattached \
             caller never triggers a prompt: {unattached_refusal}"
        );

        let oversized = SessionId::from(format!("sess-{}", "a".repeat(4096)).as_str());
        let unmintable_refusal = route_setup(
            &daemon,
            &owner,
            Id::Number(3),
            WebSetupCommitParams::METHOD,
            setup_params(&oversized),
        )
        .unwrap();
        assert!(
            unmintable_refusal.contains(&error_code::INVALID_PARAMS.to_string())
                && !unmintable_refusal.contains(&error_code::ATTESTATION_FAILED.to_string()),
            "the length gate must answer before the presence gate: {unmintable_refusal}"
        );
    }

    /// **AC-3 (REQ-575, BR-3): a build with no presence mechanism degrades — it
    /// does not refuse.**
    ///
    /// `Daemon::new()`'s default is the shipped `UnavailableVerifier`, so
    /// `refuse_unattested_commitment` returns `None` with a stderr notice and the
    /// commit is allowed past the BR-10(b) gate to the runtime — gaining no new
    /// prompt (REQ-570 BR-8's asymmetry). The owner's commit therefore reaches the
    /// runtime and answers the runtime's own `CONFIG_REJECTED` (no config path),
    /// never an attestation code. This is the non-vacuity contrast to
    /// [`a_web_setup_commit_refuses_when_the_presence_check_fails`]: same attached
    /// owner, opposite verifier, opposite outcome.
    #[test]
    fn a_web_setup_commit_degrades_where_no_presence_mechanism_exists() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);

        let committed = route_setup(
            &daemon,
            &owner,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            !committed.contains(&error_code::ATTESTATION_FAILED.to_string())
                && !committed.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
            "no mechanism must degrade, not refuse — the commit reaches the runtime \
             rather than being stopped at the presence gate: {committed}"
        );
        assert!(
            committed.contains(&error_code::CONFIG_REJECTED.to_string()),
            "with no config path the runtime is what answers, proving the commit got \
             past the degraded presence gate: {committed}"
        );
    }

    /// **AC-2 (REQ-576): config/set degrades where no presence mechanism exists,
    /// and lands.**
    ///
    /// The shared `a_commitment_degrades_to_layer_a_where_no_mechanism_exists`
    /// asserts only the *negative* (config/set is not refused for presence). This
    /// adds the *positive* landing proof its `web/setup_commit` sibling above
    /// already carries: with the default `UnavailableVerifier`,
    /// `refuse_unattested_commitment` returns `None` and a valid config/set reaches
    /// the runtime and **applies** (`applied: true` — in-memory on a config-less
    /// `Daemon::new()`, since `apply_config_update` skips the disk write when there
    /// is no path), rather than being stopped at the presence gate. (Degrade is
    /// behaviourally identical to no-gate here by design, so this pins "lands",
    /// not "gate present" — the latter is `only_a_daemon_wide_commitment_demands_presence`'s job.)
    #[test]
    fn a_config_set_degrades_where_no_presence_mechanism_exists() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);

        let applied = route_setup(
            &daemon,
            &conn,
            Id::Number(2),
            ConfigSetParams::METHOD,
            serde_json::json!({"update": {
                "op": "set_privacy_boundary",
                "path_glob": "degrade-fixture/**",
                "mode": "local_only",
            }}),
        )
        .unwrap();
        assert!(
            !applied.contains(&error_code::ATTESTATION_FAILED.to_string())
                && !applied.contains(&error_code::ATTESTATION_UNAVAILABLE.to_string()),
            "no mechanism must degrade, not refuse — config/set reaches the runtime \
             rather than being stopped at the presence gate: {applied}"
        );
        assert!(
            applied.contains("\"applied\":true"),
            "and it lands: a degraded config/set applies (in-memory on a config-less \
             daemon), proving it got past the degraded presence gate: {applied}"
        );
    }

    /// **`web/setup_commit` left the synchronous dispatch (REQ-575 ADR-1) — the
    /// reader loop cannot park on its presence prompt.**
    ///
    /// It may attest, and a presence prompt parks on a human, so like
    /// `model/confirm` it runs on `handle_client`'s `blocks_on_a_human` task
    /// rather than inline in `dispatch`. The direct proof it is no longer served
    /// on the reader loop: `dispatch` answers `method not found` for it, while the
    /// setup *reads* (`plan`, `preview`) — which never attest — are still served
    /// there. The full client path still reaches the commit (the
    /// `web_setup_flow` / `multi_client` integration suites drive it end to end),
    /// which is what makes this the "moved off the reader loop" fact rather than
    /// "removed". Re-adding it to `dispatch` (reintroducing the parking hazard)
    /// turns this red.
    #[test]
    fn the_commit_left_the_reader_loop_dispatch_while_the_reads_stayed() {
        let daemon = Daemon::new();
        let owner = unattached(&daemon);
        let session = a_session_owned_by(&daemon, &owner);

        let commit = dispatch(
            &daemon,
            &owner,
            Id::Number(2),
            WebSetupCommitParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            commit.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "web/setup_commit must not be served inline by `dispatch` — it runs on \
             the blocks_on_a_human task so a Touch ID prompt cannot park the reader \
             loop: {commit}"
        );

        let preview = dispatch(
            &daemon,
            &owner,
            Id::Number(3),
            WebSetupPreviewParams::METHOD,
            setup_params(&session),
        )
        .unwrap();
        assert!(
            !preview.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "web/setup_preview is a read that never attests, so it stays on the \
             synchronous dispatch: {preview}"
        );
    }

    /// **config/set left the synchronous dispatch (REQ-576).** Like the other
    /// BR-10(b) commitments it may attest, so it runs on `handle_client`'s
    /// `blocks_on_a_human` task, not inline in `dispatch`. Proof: `dispatch`
    /// answers `method not found` for it, while `config/get` — a read — stays.
    /// The full client path still reaches config/set (the daemon-wide commitment
    /// harness plus the config/set integration/e2e suites), which is what makes
    /// this "moved off the reader loop" rather than "removed". Its reader-loop
    /// liveness is inherited from the shared `blocks_on_a_human` machinery
    /// REQ-575's `a_parked_web_setup_commit_does_not_stall_the_connection` pins on
    /// a multi-thread runtime.
    #[test]
    fn config_set_left_the_reader_loop_dispatch_while_config_get_stayed() {
        let daemon = Daemon::new();
        let conn = unattached(&daemon);

        let set = dispatch(
            &daemon,
            &conn,
            Id::Number(2),
            ConfigSetParams::METHOD,
            serde_json::json!({"update": {"op": "register_provider", "id": "x",
                "kind": "openai-compatible", "endpoint": "http://127.0.0.1:9", "model": "m"}}),
        )
        .unwrap();
        assert!(
            set.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "config/set must not be served inline by `dispatch` — it runs on the \
             blocks_on_a_human task so a presence prompt cannot park the reader \
             loop: {set}"
        );

        let get = dispatch(
            &daemon,
            &conn,
            Id::Number(3),
            ConfigGetParams::METHOD,
            serde_json::json!({}),
        )
        .unwrap();
        assert!(
            !get.contains(&error_code::METHOD_NOT_FOUND.to_string()),
            "config/get is a read that never attests, so it stays on the \
             synchronous dispatch: {get}"
        );
    }
    // ── REQ-589 BR-13 / ADR-11: the pre-flight ──────────────────────────────

    /// A registry holding one skill whose body is `body`, discovered the way a
    /// session's is — through `discover`, so the row carries the same
    /// `path_display`, source and flags a real one does.
    struct PreflightTree {
        root: std::path::PathBuf,
    }

    impl PreflightTree {
        fn holding(name: &str, body: &str) -> Self {
            use std::sync::atomic::AtomicUsize;
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("tpre{:x}{seq:x}", std::process::id() & 0xffff));
            let commands = root.join(".claude").join("commands");
            std::fs::create_dir_all(&commands).expect("temp skill tree");
            std::fs::write(commands.join(format!("{name}.md")), body).expect("temp skill body");
            Self { root }
        }

        fn registry(&self) -> SkillRegistry {
            crate::skills::discover(
                None,
                &self.root,
                teton_protocol::methods::RootKind::Project,
                &RealFs,
            )
        }
    }

    impl Drop for PreflightTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The pair `Router::budget_for` stamps for a small declared window, and the
    /// `route_decided` that announces it — the two halves the memo bridges.
    fn stamped_pair(window: u32) -> (crate::harness::budget::RouteBudget, RouteDecided) {
        let derived = crate::harness::budget::derive(crate::harness::budget::BudgetInputs {
            window,
            cap: 0,
            reservation: crate::harness::budget::generation_reservation(),
            is_local: false,
            redact_scan: false,
            provider_id: Some("kimi"),
        });
        let decided = RouteDecided {
            category: None,
            tier: None,
            phase: None,
            provider_id: teton_protocol::ProviderId::from("kimi"),
            model: Some("kimi-k2".to_owned()),
            reason: "a fixture".to_owned(),
            effort: None,
            budget_tokens: Some(derived.budget_tokens as u64),
            budget_bytes: Some(derived.budget_bytes as u64),
            bound: Some(derived.bound),
            spend_ceiling_micro_cents: None,
            bound_floored: Some(derived.floored),
        };
        (derived, decided)
    }

    /// **AC-17, and the whole of LESSON-456 for this surface.** The sentence the
    /// pre-flight prints for a skill is byte-for-byte the sentence the *live*
    /// path prints when the same skill is refused on the same route.
    ///
    /// The two are reached through different entry points on purpose:
    /// `render_preflight` goes through `skill_fit`, while the live refusal is
    /// composed by `OverBudgetOffer::decline_refusal` — the sentence a declined
    /// offer produces in `runtime.rs`. If either grew a second estimator, a
    /// second number formatter or a second bound table, these strings part.
    ///
    /// It is also the guard on the memo's reconstruction: the live budget here
    /// is the full `RouteBudget` `derive` produced (window label, digest
    /// thresholds and all), the pre-flight's is the one `StampedRoutes::record`
    /// rebuilt from the announcement. Byte equality is the proof that the three
    /// fields the wire cannot carry reach neither the measurement nor the
    /// sentence.
    ///
    /// **Mutation**: have `render_preflight` compose its own figure line, or
    /// have `StampedRoutes::record` invent a `window_label` a future
    /// `skill_refusal` reads — either way these two strings stop matching.
    #[test]
    fn the_preflight_quotes_the_live_refusals_sentence_for_the_same_skill() {
        use crate::harness::budget::{OverBudgetOffer, PriorWindowRejection};
        use crate::harness::context::ContextManager;
        use crate::skills::SkillSource;

        let tree = PreflightTree::holding("bulky", &"word ".repeat(8_000));
        let registry = tree.registry();
        let (derived, decided) = stamped_pair(8_000);

        let routes = StampedRoutes::new();
        let session = SessionId::from("sess-preflight");
        assert!(
            routes.record(&session, &decided),
            "an announcement carrying the whole pair is a stamp"
        );
        let stamped = routes
            .stamped(&session)
            .expect("the stamp was just recorded");

        let rendered = render_preflight(&registry, Some(&stamped), false);

        // The live sentence: the same measurement, through the composer the
        // turn path uses for a declined offer.
        let skill = registry
            .skills()
            .iter()
            .find(|s| s.name == "bulky")
            .expect("the fixture registered");
        let system = preflight_system_prompt(&stamped);
        let measured = ContextManager::would_seed_fit(
            &system,
            &preflight_body(skill),
            derived.budget_tokens,
            derived.budget_bytes,
        );
        assert!(
            !measured.fits,
            "non-vacuity: the fixture must be over budget"
        );
        let live = OverBudgetOffer::new(
            "bulky",
            SkillStage::Body,
            measured,
            &derived,
            8_000,
            None,
            None,
        )
        .decline_refusal();

        assert!(
            rendered.contains(&live),
            "the pre-flight and the live refusal must quote one measurement.\n\
             pre-flight: {rendered}\nlive:       {live}"
        );
        // ...and the offer's *question* is a different sentence, so the
        // assertion above is not passing on a substring every arm shares.
        let question = OverBudgetOffer::new(
            "bulky",
            SkillStage::Body,
            measured,
            &derived,
            8_000,
            None,
            None,
        )
        .question(SkillSource::Project, PriorWindowRejection::None);
        assert_ne!(live, question);
    }

    /// **ADR-11.** A session no turn has routed is told there is no route —
    /// and the diagnostic resolves none to find out.
    ///
    /// The negative half is structural rather than asserted with a spy: nothing
    /// in `handle_skills_preflight`'s call graph holds a `Router`. What this
    /// pins is the sentence, and that a registry full of oversized skills does
    /// not produce a single measurement without a stamp.
    ///
    /// **Mutation**: default the missing stamp to `BudgetInputs::local()` — the
    /// report would name skills against a route the session is not on.
    #[test]
    fn a_session_with_no_decided_route_is_told_so_rather_than_measured() {
        let tree = PreflightTree::holding("bulky", &"word ".repeat(8_000));
        let registry = tree.registry();

        let rendered = render_preflight(&registry, None, true);

        assert_eq!(rendered, NO_ROUTE_DECIDED);
        assert!(
            !rendered.contains("bulky"),
            "no route means no measurement, so no skill is named: {rendered}"
        );
    }

    /// **AC-19.** `/verbose` puts the route's budget and bound beside the count;
    /// the count itself is reported either way.
    ///
    /// The figures come from `teton_protocol`'s `thousands`/`bytes_figure` and
    /// `BudgetBound::words` — the same three primitives the refusal sentences
    /// under this line are built from — so the summary and the detail cannot
    /// spell one budget two ways.
    ///
    /// **Mutation**: render the clause unconditionally, or drop it — either
    /// fails one of the two halves.
    #[test]
    fn verbose_names_the_routes_budget_and_bound_beside_the_count() {
        let tree = PreflightTree::holding("bulky", &"word ".repeat(8_000));
        let registry = tree.registry();
        let (_, decided) = stamped_pair(8_000);
        let routes = StampedRoutes::new();
        let session = SessionId::from("sess-verbose");
        routes.record(&session, &decided);
        let stamped = routes.stamped(&session).expect("stamped");

        let quiet = render_preflight(&registry, Some(&stamped), false);
        let loud = render_preflight(&registry, Some(&stamped), true);

        let clause = format!(
            "budget {} words / {} (bound: {})",
            thousands(stamped.budget_tokens as u64),
            bytes_figure(stamped.budget_bytes as u64),
            stamped.bound.words()
        );
        assert!(
            loud.contains(&clause),
            "`/verbose` owes AC-19's clause: {loud}"
        );
        assert!(
            !quiet.contains(&clause),
            "the clause is what `/verbose` adds; without it the line is the count: {quiet}"
        );
        for report in [&quiet, &loud] {
            assert!(
                report.contains("1 of 1 dispatchable skill(s) will not fit"),
                "the count is reported either way: {report}"
            );
        }
    }

    /// **BR-13's stated limitation, on the surface rather than in a doc.** The
    /// answer says it is a floor and names both reasons it is one.
    ///
    /// **Mutation**: drop the caveat and the report reads as a clearance — "the
    /// rest will fit" — which is the one claim BR-13 forbids it to make.
    #[test]
    fn the_preflight_answer_says_it_is_a_floor() {
        let tree = PreflightTree::holding("small", "a body\n");
        let registry = tree.registry();
        let (_, decided) = stamped_pair(128_000);
        let routes = StampedRoutes::new();
        let session = SessionId::from("sess-floor");
        routes.record(&session, &decided);
        let stamped = routes.stamped(&session).expect("stamped");

        let rendered = render_preflight(&registry, Some(&stamped), false);

        assert!(
            rendered.contains("0 of 1 dispatchable skill(s) will not fit"),
            "non-vacuity: a small body on a wide route fits: {rendered}"
        );
        assert!(
            rendered.contains(PREFLIGHT_FLOOR),
            "the floor is stated: {rendered}"
        );
        assert!(
            rendered.contains("`Body` stage"),
            "the first reason it is a floor: {rendered}"
        );
    }

    /// An announcement from a daemon that states no budget is **not** a stamp.
    ///
    /// `route_decided`'s budget fields are additive (REQ-586), so `None` means
    /// "a daemon that predates them". Storing half a pair would put the surface
    /// in front of a figure nobody derived; the honest answer is the one ADR-11
    /// already has a sentence for.
    ///
    /// **Mutation**: `unwrap_or_default()` the three fields — the memo would
    /// report a zero budget as the session's route, and every skill would be
    /// named.
    #[test]
    fn an_announcement_without_a_budget_pair_stamps_nothing() {
        let (_, full) = stamped_pair(8_000);
        let routes = StampedRoutes::new();
        let session = SessionId::from("sess-partial");

        for missing in [
            RouteDecided {
                budget_tokens: None,
                ..full.clone()
            },
            RouteDecided {
                budget_bytes: None,
                ..full.clone()
            },
            RouteDecided {
                bound: None,
                ..full.clone()
            },
        ] {
            assert!(!routes.record(&session, &missing));
            assert!(routes.stamped(&session).is_none());
        }

        assert!(routes.record(&session, &full));
        assert!(routes.stamped(&session).is_some());
    }

    /// The observer is started once, by the turn that is about to decide a
    /// route, and the announcement it hears is the stamp.
    ///
    /// **Mutation**: start the observer *after* spawning the turn task and the
    /// first turn's decision is a race; drop the claim and every turn starts
    /// another subscriber.
    #[tokio::test]
    async fn the_first_prompt_turn_starts_the_one_observer_and_its_announcement_is_the_stamp() {
        let daemon = Arc::new(Daemon::new());
        let session = SessionId::from("sess-observed");
        let (derived, decided) = stamped_pair(8_000);

        observe_route_decisions(&daemon);
        assert!(
            !daemon.stamped_routes.claim_observer(),
            "the claim is taken, so a later turn starts no second observer"
        );
        // Restore it, or this fixture would leave the memo unable to restart.
        daemon.stamped_routes.release_observer();
        assert!(daemon.stamped_routes.claim_observer());
        daemon.stamped_routes.release_observer();

        daemon
            .events
            .publish(Some(session.clone()), Event::RouteDecided(decided));

        // The observer runs on its own task; yield until it has drained.
        let mut stamped = None;
        for _ in 0..64 {
            stamped = daemon.stamped_routes.stamped(&session);
            if stamped.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stamped = stamped.expect("the announcement is the stamp");
        assert_eq!(stamped.budget_tokens, derived.budget_tokens);
        assert_eq!(stamped.budget_bytes, derived.budget_bytes);
        assert_eq!(stamped.bound, derived.bound);
        assert_eq!(stamped.floored, derived.floored);
        assert!(
            daemon
                .stamped_routes
                .stamped(&SessionId::from("sess-other"))
                .is_none(),
            "a stamp is scoped to the session the envelope named"
        );
    }

    /// An observer that stops forgets what it knew, and frees the claim so the
    /// next prompt turn can start a fresh one.
    ///
    /// The bus evicts a subscriber that falls behind, and an evicted observer
    /// cannot know a route was re-decided. Reporting the last route it *did*
    /// see would be a lie about the session's current one; "no route decided
    /// yet" is a state the surface already says truthfully.
    ///
    /// **Mutation**: drop the `forget_all` and a stale stamp outlives the
    /// observer that could no longer maintain it; drop the `release_observer`
    /// and the memo never restarts after one eviction.
    #[test]
    fn a_stopped_observer_leaves_no_stamp_and_no_claim() {
        let routes = StampedRoutes::new();
        let session = SessionId::from("sess-evicted");
        let (_, decided) = stamped_pair(8_000);

        assert!(routes.claim_observer());
        routes.record(&session, &decided);
        assert!(routes.stamped(&session).is_some());

        // What `record_route_decisions` does when its subscription ends.
        routes.forget_all();
        routes.release_observer();

        assert!(routes.stamped(&session).is_none());
        assert!(
            routes.claim_observer(),
            "the next prompt turn must be able to start a fresh observer"
        );
    }

    // -----------------------------------------------------------------------
    // REQ-597 BR-5 — which boundary events a starting session owes
    // -----------------------------------------------------------------------

    use crate::runtime::BoundaryPosture;

    /// Every combination of the two inputs, so the rule is stated once and
    /// exhaustively rather than sampled.
    ///
    /// The rows that matter most are the two that must NOT warn: a broad root
    /// with the shipped set in force (the stock machine — warning there would
    /// fire on everyone), and an opted-out config that still declares a row of
    /// its own (protected by that row — warning there would be crying wolf).
    ///
    /// **Mutation**: key the warning on the opt-out flag instead of the empty
    /// effective set, or drop either half of the conjunction, and a row here
    /// fails.
    #[test]
    fn the_unbounded_root_warning_needs_an_empty_set_and_a_broad_root() {
        let stock = BoundaryPosture {
            effective_is_empty: false,
            builtin_count: 13,
        };
        // Opted out, and nothing of the user's own left: the only way to an
        // empty set (BR-3).
        let bare = BoundaryPosture {
            effective_is_empty: true,
            builtin_count: 0,
        };
        // Opted out, but one unrelated user row survives. Not empty, so no
        // warning — this is the row that pins the condition to the *set*
        // rather than to the flag.
        let opted_out_with_own_row = BoundaryPosture {
            effective_is_empty: false,
            builtin_count: 0,
        };

        for (posture, kind, want_warning, why) in [
            (
                bare,
                RootKind::Home,
                true,
                "nothing protected, rooted at $HOME",
            ),
            (
                bare,
                RootKind::FilesystemRoot,
                true,
                "nothing protected, rooted at /",
            ),
            (
                bare,
                RootKind::Project,
                false,
                "a project root is a directory the user chose",
            ),
            (
                bare,
                RootKind::Plain,
                false,
                "a plain root is narrow enough not to warn",
            ),
            (
                stock,
                RootKind::Home,
                false,
                "the shipped set is in force — the stock machine",
            ),
            (
                stock,
                RootKind::FilesystemRoot,
                false,
                "likewise at the filesystem root",
            ),
            (
                opted_out_with_own_row,
                RootKind::Home,
                false,
                "opted out but still protected by the user's own row",
            ),
        ] {
            let events = session_start_boundary_events(posture, kind);
            let warned = events
                .iter()
                .any(|e| matches!(e, Event::UnboundedRootWarning(_)));
            assert_eq!(
                warned, want_warning,
                "{why} (kind {kind:?}, posture {posture:?})"
            );
        }
    }

    /// The warning carries the root it is about, so a client can say *where*
    /// rather than only *that*.
    #[test]
    fn the_warning_names_the_root_kind_that_raised_it() {
        for kind in [RootKind::Home, RootKind::FilesystemRoot] {
            let events = session_start_boundary_events(
                BoundaryPosture {
                    effective_is_empty: true,
                    builtin_count: 0,
                },
                kind,
            );
            let warning = events
                .iter()
                .find_map(|e| match e {
                    Event::UnboundedRootWarning(w) => Some(w),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{kind:?} must warn"));
            assert_eq!(warning.root_kind, kind);
        }
    }

    /// The companion event reports the count, and is silent when the opt-out
    /// left nothing to report.
    ///
    /// **Mutation**: publish it unconditionally and the opted-out row fails.
    #[test]
    fn the_defaults_applied_event_reports_its_count_and_is_silent_when_opted_out() {
        let applied = session_start_boundary_events(
            BoundaryPosture {
                effective_is_empty: false,
                builtin_count: 13,
            },
            RootKind::Project,
        );
        let count = applied
            .iter()
            .find_map(|e| match e {
                Event::BoundaryDefaultsApplied(a) => Some(a.count),
                _ => None,
            })
            .expect("a stock session reports the defaults it applied");
        assert_eq!(count, 13);

        let opted_out = session_start_boundary_events(
            BoundaryPosture {
                effective_is_empty: true,
                builtin_count: 0,
            },
            RootKind::Project,
        );
        assert!(
            !opted_out
                .iter()
                .any(|e| matches!(e, Event::BoundaryDefaultsApplied(_))),
            "no builtin rows were applied, so there is nothing to report"
        );
    }

    /// Both events name themselves on the wire exactly as the spec's Events
    /// table spells them.
    #[test]
    fn the_two_session_start_events_carry_their_spec_names() {
        let events = session_start_boundary_events(
            BoundaryPosture {
                effective_is_empty: true,
                builtin_count: 0,
            },
            RootKind::Home,
        );
        assert_eq!(events[0].name(), "unbounded_root_warning");
        assert_eq!(
            session_start_boundary_events(
                BoundaryPosture {
                    effective_is_empty: false,
                    builtin_count: 13,
                },
                RootKind::Project,
            )[0]
            .name(),
            "boundary_defaults_applied"
        );
    }
}
