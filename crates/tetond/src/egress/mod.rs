//! The single egress choke point (architecture D-2).
//!
//! Every byte that leaves this machine for a remote provider — or, per ADR-003,
//! a remote MCP server — passes through here. This is the *only* place in the
//! whole workspace that constructs a real HTTP client ([`HttpTransport`], built
//! on `reqwest`); a CI deny-check ([`deny_http_client`]) fails the build if any
//! other crate declares an HTTP-client crate among its **direct** manifest
//! dependencies (it is a manifest-line scan, not a transitive `cargo tree`
//! analysis — see that module's docs for the limitation). Because the provider
//! crate carries no client of its own and only ever holds a `&dyn Transport`,
//! "an adapter cannot reach the network except through egress" is a compile-time
//! property, not a review convention.
//!
//! Three responsibilities converge here:
//! - **BR-1 (privacy boundary)** — a request whose content provenance intersects
//!   a `local-only` boundary is blocked before a single byte leaves; a
//!   `privacy_block` event is emitted with the offending path, the provider, and
//!   the action taken. Enforcement is provenance-based, not string-scanning; see
//!   [`provenance`] and [`inspector`].
//! - **BR-2 (cost)** — every *allowed* remote call is the hook where TASK-008
//!   records a `CostRecord`. That seam is marked in [`Egress::send`].
//! - **BR-7 (credentials)** — the adapter never sees a secret; the transport
//!   attaches the resolved credential header ([`HttpTransport::with_endpoint_auth`]),
//!   bound to the owning provider's endpoint so it can never leak onto an MCP or
//!   cross-provider request.
//!
//! ## How provenance reaches egress
//!
//! The [`Transport`] trait (owned by `teton-providers`, and deliberately not
//! modified by this crate) carries only method/url/headers/body — it has no
//! channel for provenance, because provenance is a property of the *assembled
//! context*, not of the raw request bytes. So the daemon's turn loop calls the
//! richer [`Egress::send`], passing the [`Provenance`] of the context it
//! assembled for that turn. For the adapter seam, [`Egress::scoped`] hands back a
//! [`TurnTransport`]: a `Transport` whose `execute` runs the same guard against a
//! baked-in per-turn provenance, so an adapter that only knows `&dyn Transport`
//! is still enforced.
//!
//! ## Provenance is derived from files a tool TOUCHED (REQ-544 C-1)
//!
//! A tool result's provenance is the set of repo-relative paths the tool
//! actually read or enumerated — the `read` path, `grep`'s matched files,
//! `glob`'s enumerated files, an MCP call's path-shaped arguments — not a literal
//! `path` argument. A tool whose touched files cannot be determined (`shell`
//! runs an arbitrary command) reports **unknown** provenance, which this layer
//! fail-closes: when any boundary is configured, a turn whose assembled context
//! carries an unknown-provenance block is blocked from remote egress exactly like
//! a boundary hit (see [`inspector::inspect`] and [`provenance::Provenance::is_unknown`]).
//!
//! ## The session-taint backstop (REQ-544 C-2)
//!
//! Provenance tracking is honest about *derived* content: a summary, snippet, or
//! tool result computed from a `local-only` file inherits that file's provenance
//! and is blocked ([`ContextBlock::derive`]). What per-request provenance cannot
//! by itself cover is a model-generated **paraphrase of boundary content emitted
//! in a LATER turn**: once a local model has read a secret, text it writes from
//! its own memory carries no file provenance.
//!
//! The daemon closes that gap with a real **per-session taint** backstop
//! (`crate::runtime`): once any tool result's provenance intersects a
//! `local-only` boundary **or** is unknown, the session is marked tainted and
//! *pinned to the local tier for every subsequent turn* — the router forces
//! local regardless of phase policy or heuristic. And when a remote turn is
//! blocked here mid-session, the daemon does not retry the blocked provider: it
//! taints the session and re-runs that turn on the local tier (REQ-544 M-1),
//! emitting exactly one `privacy_block`. So the residual paraphrase path is
//! held local, not merely hoped about.
//!
//! ## Residual limits that remain
//!
//! A black-box MCP server that reads a boundary file via an argument that is
//! neither a path-like key nor path-shaped (e.g. an opaque record id) still
//! produces an under-tagged result; the taint backstop only catches it once some
//! *other* signal (a path-shaped arg, an unknown `shell`, a boundary read) taints
//! the session. Machines with no local tier cannot serve a tainted session at
//! all — that turn fails closed with a specific privacy error rather than
//! degrading the guarantee.

pub mod inspector;
pub mod lookup;
pub mod provenance;
pub mod redact;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use teton_core::boundary::BoundaryMatcher;
use teton_core::entities::PrivacyBoundary;
use teton_protocol::events::{
    BlockCause, ByteSpan, Event, FindingKind as WireFindingKind, PrivacyAction, PrivacyBlock,
};
use teton_protocol::{ProviderId, SessionId};
use teton_providers::transport::{
    BlockDetail, ByteStream, HttpMethod, Transport, TransportError, TransportRequest,
    TransportResponse,
};

use crate::broadcast::EventBus;
use crate::cost::{CostAttribution, CostMeter};

pub use inspector::{inspect, Inspection, Violation};
pub use lookup::{
    to_protocol_web_tier, AddressClass, Authorship, LookupContext, LookupDetail, LookupKind,
    LookupOutcome, LookupRecord, LookupRecorder, LookupRequest, NoopLookupRecorder, TaintView,
};
pub use provenance::{assembled_provenance, ContextBlock, Provenance};
pub use redact::{RedactionGate, RedactionVerdict};

/// A failure at the egress choke point.
///
/// Every variant is content-free by construction — it carries at most a
/// config-authored path, a provider id, an action, or a transport failure class.
/// That is what makes error and telemetry paths safe under BR-1: an `EgressError`
/// may be logged, serialized, or surfaced to a client without leaking a byte of
/// boundary content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    /// An inspection inside the choke point refused the payload; the request was
    /// dropped before any network activity. Which inspection is `cause`
    /// (REQ-562 ADR-7) — a `local-only` boundary, a redaction finding, or a
    /// redaction scan that could not run.
    ///
    /// `path` is safe to surface for every cause: for a boundary block it is the
    /// boundary glob's target (not file content), and for a redaction block it
    /// is a fixed locus phrase naming the payload rather than quoting it (BR-6).
    #[error("{}", privacy_blocked_sentence(.path, .provider_id, .action, .cause))]
    PrivacyBlocked {
        /// Repo-relative path of the boundary source that would have leaked, or
        /// the non-secret locus of a redaction block.
        path: String,
        /// Provider the content would have reached.
        provider_id: ProviderId,
        /// What the choke point did instead.
        action: PrivacyAction,
        /// Which inspection refused it — the same value the `privacy_block`
        /// event carries, so the typed error and the event cannot disagree.
        cause: BlockCause,
    },
    /// The underlying transport failed (before any HTTP status was known).
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    /// The configured privacy boundaries did not compile. Egress refuses
    /// fail-closed rather than send content it cannot classify.
    #[error("privacy boundaries failed to compile; egress refused fail-closed")]
    BoundaryCompile,
    /// The real HTTP client could not be initialized.
    #[error("failed to initialize the HTTP client")]
    ClientInit,
}

impl EgressError {
    /// Collapse to a [`TransportError`] for the object-safe `Transport` seam.
    ///
    /// A privacy block manifests to an adapter as the dedicated,
    /// **non-retryable** [`TransportError::PrivacyBlocked`] — never a
    /// connect refusal — so it cannot be misclassified as a transient transport
    /// fault and retried against the blocked provider (REQ-544 M-1). The
    /// `privacy_block` event carries the full reason; the daemon reroutes the
    /// turn to the local tier.
    ///
    /// ## What survives the collapse, and what does not (REQ-562 BR-3)
    ///
    /// The [`BlockCause`] does not fit through this seam — `teton-providers`
    /// declares no protocol dependency, by design — so it is projected onto
    /// [`BlockDetail`], which names *which inspection* refused the payload and
    /// nothing else. That projection is lossy on purpose: the finding's kind and
    /// byte span stay behind, because the `privacy_block` event already carries
    /// them straight from the choke point and a byte range has no business
    /// travelling through an adapter.
    ///
    /// What must **not** be lost is which of the three it was. The daemon's
    /// turn-failure sentence is composed from this value, and a redaction block
    /// reported as a boundary sends the user looking for a `local-only` glob
    /// that does not exist — or, worse, tells someone whose scan could not run
    /// that something was found.
    #[must_use]
    pub fn into_transport_error(self) -> TransportError {
        match self {
            EgressError::Transport(t) => t,
            EgressError::PrivacyBlocked { cause, .. } => {
                TransportError::PrivacyBlocked(match cause {
                    BlockCause::Boundary => BlockDetail::Boundary,
                    BlockCause::Redaction { .. } => BlockDetail::Redaction,
                    BlockCause::ScanUnavailable => BlockDetail::ScanUnavailable,
                })
            }
            EgressError::ClientInit => TransportError::Connect,
            EgressError::BoundaryCompile => TransportError::Io,
        }
    }
}

/// The sentence [`EgressError::PrivacyBlocked`] renders, one per cause
/// (REQ-562 BR-3).
///
/// Three causes are three different problems with three different fixes, so
/// they are three sentences rather than one sentence with a noun swapped. The
/// scan-unavailable line says the scan **could not run** and never that it found
/// something: told the wrong one, a user goes looking for a secret that is not
/// there instead of for the local tier that is not loaded.
///
/// Nothing here interpolates payload content, for the same structural reason the
/// event renderer does not: a [`Finding`](redact::Finding) has no text field to
/// interpolate (ADR-5), so `path`, `kind` and the byte span are the whole
/// vocabulary available to this function.
fn privacy_blocked_sentence(
    path: &str,
    provider_id: &ProviderId,
    action: &PrivacyAction,
    cause: &BlockCause,
) -> String {
    match cause {
        // Unchanged from REQ-544, deliberately: this is the sentence in every
        // existing log, and a defaulted `cause` on an old frame reads as this.
        BlockCause::Boundary => {
            format!("privacy boundary blocked egress of `{path}` to provider `{provider_id}` ({action:?})")
        }
        BlockCause::Redaction { kind, span } => format!(
            "the redaction scan found {} at bytes {}-{} of {path}, so egress to provider \
             `{provider_id}` was refused ({action:?})",
            kind.user_label(),
            span.start,
            span.end
        ),
        BlockCause::ScanUnavailable => format!(
            "the redaction scan could not run on {path}, so egress to provider \
             `{provider_id}` was refused unscanned ({action:?})"
        ),
    }
}

/// The cause a blocking [`RedactionVerdict`] reports (ADR-7).
///
/// Derived from the verdict rather than passed alongside it, so the reported
/// cause and the decision to block cannot come from two different readings of
/// the same value: [`redact::decide`] blocks a *completed* scan only on a
/// high-confidence finding, so a blocking verdict either has one — which is what
/// `Redaction` names — or the scan never completed, which is `ScanUnavailable`.
///
/// The **first** high finding is the one reported. A payload can carry several;
/// naming one is enough to act on, and enumerating them would turn a block
/// notice into a map of where the secrets are.
///
/// ## An `Unavailable` verdict may still name what was found (2026-08-08)
///
/// The deterministic pattern pass has no window and sweeps the whole payload in
/// one piece, so it can complete on a payload whose *model* pass then fails on
/// some chunk. [`RedactionVerdict::evidence`] carries what it established, and
/// this function reads it exactly like a finding: `Unavailable` **with** High
/// evidence is `Redaction`, `Unavailable` without it is `ScanUnavailable`.
///
/// That is the truthful reading, not a lenient one. "The scan found a credential
/// at bytes 40-66" is a claim about a pass that ran to completion over every
/// byte; "the scan could not run" would be a claim about the *model* pass wearing
/// the whole scan's name, and it sends the user to debug an engine instead of to
/// the credential they are about to send. It is also strictly more conservative
/// downstream: `Redaction` pins the session where `ScanUnavailable` does not
/// ([`crate::runtime`]'s cause gate), so the evidence restores a pin the
/// deterministic pass had earned and a transient stall used to erase.
///
/// The direction here was **reversed on review evidence**: until 2026-08-08 the
/// pattern findings were discarded outright at the point the scan failed, and
/// `a_credential_in_a_scanned_chunk_does_not_outrank_a_chunk_that_failed` pinned
/// that. Its successor
/// (`harness::redact::tests::a_credential_the_pattern_pass_established_survives_a_failed_chunk`)
/// pins this.
fn block_cause(verdict: &RedactionVerdict) -> BlockCause {
    // Findings first, evidence second: on a completed scan there is no evidence
    // to consider, and on a failed one there are no findings — so the chain is
    // a single ordered read rather than a branch on outcome that could drift
    // from `decide`'s.
    let high = verdict
        .findings()
        .iter()
        .chain(verdict.evidence())
        .find(|f| f.is_high());
    if let Some(finding) = high {
        let span = finding.span();
        return BlockCause::Redaction {
            kind: wire_kind(finding.kind()),
            span: ByteSpan {
                start: span.start as u64,
                end: span.end as u64,
            },
        };
    }
    debug_assert_eq!(
        verdict.outcome(),
        redact::Outcome::Unavailable,
        "a completed scan blocks only on a high-confidence finding"
    );
    BlockCause::ScanUnavailable
}

/// The non-secret locus a redaction block reports as its `path` (ADR-7).
///
/// A fixed phrase, and a *noun phrase* rather than a sentence, because it has to
/// read correctly in two different renderings that this crate does not own:
///
/// - the cause-aware one — "…at bytes 1400-1436 of **the outbound payload**,
///   bound for anthropic";
/// - and the one a client predating [`BlockCause`] falls back to, where the
///   defaulted `Boundary` reading prints "**the outbound payload** would have
///   reached anthropic — call re-routed to the local tier".
///
/// Both are true and neither quotes a byte of the payload. Folding the kind and
/// span into this string would make the skew sentence more informative and the
/// cause-aware one — the sentence this REQ actually promises — read them twice.
///
/// It takes the cause so that a future locus that *does* vary by cause has one
/// place to vary in; today the answer is the same phrase, because "where the
/// blocked bytes are" does not depend on why they were refused.
fn redaction_locus(cause: BlockCause) -> &'static str {
    match cause {
        BlockCause::Redaction { .. } | BlockCause::ScanUnavailable => "the outbound payload",
        // Not reachable from the gate hook, which never mints a boundary cause.
        BlockCause::Boundary => "the outbound payload",
    }
}

/// The wire twin of a finding kind. Two enums, one meaning, and the map stated
/// once — here, at the only place a finding becomes an event.
fn wire_kind(kind: redact::FindingKind) -> WireFindingKind {
    match kind {
        redact::FindingKind::Secret => WireFindingKind::Secret,
        redact::FindingKind::Credential => WireFindingKind::Credential,
        redact::FindingKind::Pii => WireFindingKind::Pii,
        redact::FindingKind::Unknown => WireFindingKind::Unknown,
    }
}

/// A sink for `privacy_block` events emitted by the choke point.
///
/// Abstracted so the choke point does not depend on the concrete daemon event
/// bus (and so tests can capture emitted events). The daemon wires its
/// [`EventBus`]; a [`NoopSink`] is available where events are irrelevant.
pub trait PrivacyEventSink: Send + Sync {
    /// Publish a `privacy_block` event, scoped to `session_id` when known.
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock);
}

/// The production sink: broadcast to attached clients over the daemon event bus.
impl PrivacyEventSink for EventBus {
    fn privacy_block(&self, session_id: Option<SessionId>, block: PrivacyBlock) {
        self.publish(session_id, Event::PrivacyBlock(block));
    }
}

/// A sink that drops events — for contexts with no subscribers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl PrivacyEventSink for NoopSink {
    fn privacy_block(&self, _session_id: Option<SessionId>, _block: PrivacyBlock) {}
}

/// Per-call context the choke point needs but the [`TransportRequest`] cannot
/// carry: which provider the call targets, which session it belongs to, and the
/// action to record if it is blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressContext {
    /// Provider the request targets (named in any `privacy_block` event).
    pub provider_id: ProviderId,
    /// Owning session, when the call is session-scoped.
    pub session_id: Option<SessionId>,
    /// The action a block should report. Defaults to
    /// [`PrivacyAction::ReroutedToLocal`]: the remote call is prevented and the
    /// turn is to be served on the local tier.
    pub block_action: PrivacyAction,
    /// Billing attribution for this call (TASK-008, BR-2). When present *and* the
    /// choke point holds a cost meter, an allowed forward is recorded as one
    /// `CostRecord`; when absent, the call forwards unmetered (e.g. a
    /// non-billable probe). Carries the phase and model the choke point cannot
    /// otherwise know.
    pub cost: Option<CostAttribution>,
}

impl EgressContext {
    /// Context for `provider_id`, defaulting the block action to
    /// [`PrivacyAction::ReroutedToLocal`] and no session scope.
    #[must_use]
    pub fn new(provider_id: impl Into<ProviderId>) -> Self {
        Self {
            provider_id: provider_id.into(),
            session_id: None,
            block_action: PrivacyAction::ReroutedToLocal,
            cost: None,
        }
    }

    /// Scope the context to `session_id`.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Override the action reported on a block.
    #[must_use]
    pub fn with_action(mut self, action: PrivacyAction) -> Self {
        self.block_action = action;
        self
    }

    /// Attach billing attribution so an allowed forward is recorded as a
    /// `CostRecord` (TASK-008, BR-2).
    #[must_use]
    pub fn with_cost(mut self, attribution: CostAttribution) -> Self {
        self.cost = Some(attribution);
        self
    }
}

/// The egress choke point: a privacy/cost guard wrapping an inner [`Transport`].
///
/// In production `T` is [`HttpTransport`]; tests substitute a capturing transport
/// so the egress-capture harness can assert that no boundary content ever reaches
/// the wire. The guard is `T`-generic precisely so the same enforcement runs in
/// front of the real client and the test double.
pub struct Egress<T: Transport> {
    inner: T,
    boundaries: Vec<PrivacyBoundary>,
    sink: Arc<dyn PrivacyEventSink>,
    cost: Option<Arc<dyn CostMeter>>,
    /// The REQ-562 redaction scanner, present **iff** `[privacy] redact` is on
    /// (ADR-2). `None` is the off state in full: no branch inside the hot path,
    /// no scanner call, nothing that could claim a scan ran.
    redaction: Option<Arc<dyn RedactionGate>>,
    /// The REQ-563 **search** scanner, present iff the configured web tier is
    /// `search` (architecture D-6). A separate slot from `redaction` rather than
    /// a second reader of it, because the two are governed by different switches
    /// and answering to one flag would make the coupling BR-14 requires
    /// bypassable by turning `[privacy] redact` off.
    ///
    /// Absent, a `Search` lookup is **refused**, not sent unscanned — see
    /// [`Egress::lookup`].
    search_redaction: Option<Arc<dyn RedactionGate>>,
    /// The REQ-563 **fetch** scanner: the provider-parity slot (BR-2, BR-13).
    ///
    /// A third slot rather than a second reader of `redaction` for the same
    /// reason `search_redaction` is a second: the slots record *which promise*
    /// installed them, and a gate that answered to two switches could not be
    /// removed from one of them. This one is governed by `[privacy] redact`
    /// exactly as the provider gate is — parity means parity — so a lookup
    /// choke point built with `redact` off scans no fetch URL at all, which is
    /// the same "off" a provider payload gets.
    ///
    /// Absent is genuinely **off** here, unlike [`Self::search_redaction`]: the
    /// unconditional-scan promise BR-14 makes is about the *search* tier, and
    /// extending fail-closed behavior to fetch would refuse every fetch on a
    /// daemon with `redact = false`, which is a configuration BR-2 blesses.
    fetch_redaction: Option<Arc<dyn RedactionGate>>,
    /// Where a completed lookup attempt is recorded (BR-7) and announced (D-8).
    /// Absent means neither happens; it never changes a decision.
    lookup_recorder: Option<Arc<dyn lookup::LookupRecorder>>,
}

impl<T: Transport> Egress<T> {
    /// Build a choke point over `inner`, enforcing `boundaries`, emitting
    /// `privacy_block` events through `sink`.
    #[must_use]
    pub fn new(
        inner: T,
        boundaries: Vec<PrivacyBoundary>,
        sink: Arc<dyn PrivacyEventSink>,
    ) -> Self {
        Self {
            inner,
            boundaries,
            sink,
            cost: None,
            redaction: None,
            search_redaction: None,
            fetch_redaction: None,
            lookup_recorder: None,
        }
    }

    /// Install the cost meter (TASK-008): every allowed forward carrying a
    /// [`CostAttribution`] is recorded as one `CostRecord` (BR-2). Additive —
    /// a choke point without a meter forwards exactly as before.
    #[must_use]
    pub fn with_cost_meter(mut self, meter: Arc<dyn CostMeter>) -> Self {
        self.cost = Some(meter);
        self
    }

    /// Install the redaction scanner (REQ-562 ADR-2): every payload the
    /// provenance inspection allows is scanned before it is forwarded.
    ///
    /// Additive and mirroring [`Self::with_cost_meter`] — a choke point built
    /// without one behaves *exactly* as it did before REQ-562, which is what
    /// makes "off" checkable by scanner call count rather than by reading a flag
    /// (AC-13). The daemon calls this only when `[privacy] redact` is true.
    #[must_use]
    pub fn with_redaction_gate(mut self, gate: Arc<dyn RedactionGate>) -> Self {
        self.redaction = Some(gate);
        self
    }

    /// Install the **search** redaction scanner (REQ-563 D-6, BR-14).
    ///
    /// The same composite gate [`Self::with_redaction_gate`] takes, installed
    /// from a different switch: the daemon calls this whenever the configured
    /// web tier is `search`, **independently of `[privacy] redact`**. The two
    /// scans answer different promises — the provider one is the user's opt-in
    /// to being watched over, the search one is the condition on which the
    /// search tier exists at all — so they are two slots and not one flag read
    /// twice.
    ///
    /// Unlike every other slot on this choke point, leaving this one empty is
    /// not "off": [`Egress::lookup`] refuses a `Search` with no gate rather than
    /// sending the query unscanned (LESSON-492).
    #[must_use]
    pub fn with_search_redaction_gate(mut self, gate: Arc<dyn RedactionGate>) -> Self {
        self.search_redaction = Some(gate);
        self
    }

    /// Install the **fetch** redaction scanner: the provider-parity gate
    /// (REQ-563 BR-2, BR-13).
    ///
    /// BR-2 promises that a fetch tier gets "the same treatment a provider
    /// payload gets", and BR-13 says a user-pasted URL is "still
    /// redact-scanned". Both are one sentence: whenever `[privacy] redact` is
    /// on, the outgoing *URL string* of a `Fetch` is scanned before a byte of it
    /// reaches the wire, whatever authored it. The daemon therefore calls this
    /// from the same switch it calls [`Self::with_redaction_gate`] from, and
    /// with the same composite gate.
    ///
    /// Absent is **off**, not fail-closed — the opposite of
    /// [`Self::with_search_redaction_gate`]. The asymmetry is the requirements'
    /// own: BR-14 couples *search* egress to the scan unconditionally because
    /// the search tier's existence is conditioned on it, while BR-2 couples
    /// *fetch* to whatever `[privacy] redact` says. Making fetch fail closed
    /// would refuse every lookup on a `redact = false` daemon, which is a
    /// configuration the requirement blesses.
    ///
    /// Wave-2 wiring note: `DaemonRuntime::web_lookup_egress` must call this
    /// whenever `config.privacy.redact` is set, passing the same gate
    /// `redaction_gate` builds for the provider path.
    #[must_use]
    pub fn with_fetch_redaction_gate(mut self, gate: Arc<dyn RedactionGate>) -> Self {
        self.fetch_redaction = Some(gate);
        self
    }

    /// Install the web-lookup recorder (REQ-563 BR-7, D-7/D-8): every lookup
    /// attempt, whatever its ending, becomes one `web_lookups` row and one
    /// `web_lookup` event.
    ///
    /// Additive like [`Self::with_cost_meter`] — a choke point without one
    /// performs identical lookups and records none of them.
    #[must_use]
    pub fn with_lookup_recorder(mut self, recorder: Arc<dyn lookup::LookupRecorder>) -> Self {
        self.lookup_recorder = Some(recorder);
        self
    }

    /// The configured boundaries (read-only).
    #[must_use]
    pub fn boundaries(&self) -> &[PrivacyBoundary] {
        &self.boundaries
    }

    /// Which optional collaborators this choke point was built with.
    ///
    /// Test-only, and deliberately so: nothing in production should branch on
    /// whether a gate is installed — "off" is the absence of the collaborator,
    /// not a flag to consult (ADR-2). What a *test* needs is different: the
    /// daemon's wiring claims "the search gate is on the choke point exactly
    /// when the tier is `search`", and confirming that by performing a network
    /// lookup to see what happens would be a worse test of a worse property.
    ///
    /// Returns `(provider gate, search gate, lookup recorder)`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn installed(&self) -> (bool, bool, bool) {
        (
            self.redaction.is_some(),
            self.search_redaction.is_some(),
            self.lookup_recorder.is_some(),
        )
    }

    /// Whether the fetch-parity redaction gate is installed.
    ///
    /// A separate accessor rather than a fourth element on [`Self::installed`]
    /// so that adding the slot did not rewrite every existing assertion about
    /// the other three. Same test-only rationale as that one.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn fetch_redaction_installed(&self) -> bool {
        self.fetch_redaction.is_some()
    }

    /// Dispatch `request` for the context assembled with `provenance`.
    ///
    /// This is the choke point's primary API and the daemon's turn loop calls it.
    /// **Two** inspections run here, in this order (REQ-562 ADR-1):
    ///
    /// 1. **Provenance** — if `provenance` intersects a boundary, the request is
    ///    refused before any network activity, with cause `Boundary`.
    /// 2. **Redaction** — if a gate is installed, the exact bytes that would go
    ///    on the wire are scanned, and a blocking verdict refuses them with cause
    ///    `Redaction` or `ScanUnavailable`.
    ///
    /// The order is load-bearing, not incidental: a payload the boundary check
    /// refuses returns before the gate line is reached, so it costs **zero**
    /// scanner calls (AC-11). Running the model first would spend an inference on
    /// content that was never allowed to leave in the first place.
    ///
    /// Either refusal emits a `privacy_block` event and returns
    /// [`EgressError::PrivacyBlocked`]. A verdict that **forwards** with
    /// low-confidence findings writes one daemon-log line per finding
    /// ([`redact::forwarded_findings_report`]) — kind, confidence and span, no
    /// payload text — because a finding computed and then discarded is
    /// indistinguishable from one never made (BR-4, ADR-4).
    ///
    /// The `request` — whose body may contain
    /// the offending bytes — is dropped, never forwarded, never logged. A payload
    /// both inspections allow reaches the inner transport **byte-identical**:
    /// v1 detects, it never substitutes (BR-5, AC-9).
    pub async fn send(
        &self,
        request: TransportRequest,
        provenance: &Provenance,
        ctx: &EgressContext,
    ) -> Result<TransportResponse, EgressError> {
        // Fast path: content from no file, or no boundaries configured, can never
        // intersect a boundary — skip building a matcher entirely.
        if !provenance.is_empty() && !self.boundaries.is_empty() {
            let matcher =
                BoundaryMatcher::new(&self.boundaries).map_err(|_| EgressError::BoundaryCompile)?;
            if let Inspection::Blocked(violation) = inspect(provenance, &matcher, ctx.block_action)
            {
                let block = PrivacyBlock {
                    path: violation.path.clone(),
                    provider_id: ctx.provider_id.clone(),
                    action: violation.action,
                    // Named rather than defaulted: the provenance inspection is
                    // one of several things that can refuse a payload here
                    // (REQ-562 ADR-7), and this is the one that says *boundary*.
                    // `#[serde(default)]` exists for frames a v1 daemon wrote,
                    // not as a way for this emit site to leave the cause unsaid.
                    cause: BlockCause::Boundary,
                };
                self.sink.privacy_block(ctx.session_id.clone(), block);
                return Err(EgressError::PrivacyBlocked {
                    path: violation.path,
                    provider_id: ctx.provider_id.clone(),
                    action: violation.action,
                    cause: BlockCause::Boundary,
                });
            }
        }

        // The second inspection (REQ-562 ADR-1). It sits *here*: after the
        // provenance early-return above, before the forward below, on the exact
        // bytes that would go on the wire. There is no separate "text
        // projection" that could drift from what is actually sent — the scan
        // input is `request.body` read as lossy UTF-8, and the thing forwarded
        // on a clean verdict is that same `request`, untouched (LESSON-485:
        // scan the thing the guarantee is about).
        //
        // Lossy rather than strict because a body that is not valid UTF-8 must
        // still be scanned: refusing to look at it would be a scan that skips
        // exactly the payloads an exfiltrator would choose.
        //
        // What a span indexes, precisely: **the lossy text**, not `request.body`.
        // For every body this daemon actually sends the two are the same string
        // — an adapter serializes JSON, and `serde_json` emits valid UTF-8 — so
        // "bytes 1400-1436 of the outbound payload" is exact today.
        //
        // It would NOT be exact for a body that is not valid UTF-8, and the
        // earlier claim here that replacement shifts no offset is simply false:
        // U+FFFD is three bytes, so a single invalid byte becomes three and
        // every span after it is skewed by two. That is a reporting inaccuracy
        // in a case no current adapter can produce, and it is a locus string
        // rather than a pointer anything dereferences — but it is recorded
        // rather than asserted away, and an adapter that ever sends a non-UTF-8
        // body has to revisit it.
        //
        // Detection itself is unaffected either way: replacement never creates
        // or destroys an ASCII byte, so no pattern shape can be hidden or
        // conjured by it.
        if let Some(gate) = &self.redaction {
            let verdict = {
                let text = String::from_utf8_lossy(&request.body);
                gate.scan(&text).await
            };
            if redact::decide(&verdict) == redact::EgressDecision::Block {
                let cause = block_cause(&verdict);
                let path = redaction_locus(cause).to_owned();
                self.sink.privacy_block(
                    ctx.session_id.clone(),
                    PrivacyBlock {
                        path: path.clone(),
                        provider_id: ctx.provider_id.clone(),
                        action: ctx.block_action,
                        cause,
                    },
                );
                return Err(EgressError::PrivacyBlocked {
                    path,
                    provider_id: ctx.provider_id.clone(),
                    action: ctx.block_action,
                    cause,
                });
            }
            // The forwarded half of ADR-4's decision table. A `Findings` verdict
            // that forwards is Low-only (BR-4), and a low-confidence finding
            // that is computed and then dropped is indistinguishable from one
            // that was never made — nothing tells the operator, and OQ-2's
            // "what did the model catch that patterns did not?" has no
            // observable answer. So it goes to the daemon's log, which is this
            // process's stderr and therefore `tetond.log`.
            //
            // Kind, confidence and span only. `Finding` has no text field, so
            // "the report never quotes the payload" (BR-6) is a property of the
            // input rather than of this loop.
            for line in redact::forwarded_findings_report(&verdict) {
                eprintln!("{line}");
            }
        }

        // Cleared. TASK-008 wraps this forward to record a CostRecord (BR-2) from
        // the streamed usage — the single point where every remote call is billed.
        let response = self
            .inner
            .execute(request)
            .await
            .map_err(EgressError::Transport)?;
        // Bill the call iff the caller attached attribution and a meter is
        // installed; the meter wraps the response so recording happens from the
        // streamed usage when the body drains.
        match (&self.cost, &ctx.cost) {
            (Some(meter), Some(attribution)) => Ok(meter.meter_response(
                response,
                ctx.session_id.clone(),
                ctx.provider_id.clone(),
                attribution.clone(),
            )),
            _ => Ok(response),
        }
    }

    /// A per-turn, provenance-scoped [`Transport`] view for the adapter seam.
    ///
    /// Hand the returned `&dyn Transport` to an adapter: its `execute` runs the
    /// same guard as [`Egress::send`] against `provenance`, so an adapter that
    /// only knows `&dyn Transport` cannot bypass BR-1.
    #[must_use]
    pub fn scoped(&self, provenance: Provenance, ctx: EgressContext) -> TurnTransport<'_, T> {
        TurnTransport {
            egress: self,
            provenance,
            ctx,
        }
    }
}

/// A provenance-scoped `Transport` produced by [`Egress::scoped`].
///
/// Bridges the object-safe [`Transport`] seam (which cannot carry provenance) to
/// the guarded [`Egress::send`] by baking in the current turn's provenance. A
/// block still emits its `privacy_block` event; the adapter observes a
/// transport-level refusal (see [`EgressError::into_transport_error`]).
pub struct TurnTransport<'a, T: Transport> {
    egress: &'a Egress<T>,
    provenance: Provenance,
    ctx: EgressContext,
}

#[async_trait]
impl<T: Transport> Transport for TurnTransport<'_, T> {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.egress
            .send(request, &self.provenance, &self.ctx)
            .await
            .map_err(EgressError::into_transport_error)
    }
}

/// A resolved credential bound to one provider endpoint (BR-7, REQ-544 M-3).
///
/// The credential headers are attached to an outbound request **only** when the
/// request targets the bound origin. This is the cross-contamination guard: a
/// provider's `x-api-key` / `Authorization: Bearer` can never ride along on an
/// MCP request or a different provider's request, even if a transport were
/// somehow reused across endpoints. The MCP egress path constructs a
/// credential-free transport ([`HttpTransport::new`]) and so carries no auth at
/// all.
#[derive(Debug, Clone)]
struct EndpointAuth {
    /// The ASCII-serialized origin (`scheme://host[:port]`) the credential is
    /// bound to, or `None` if the endpoint URL did not parse to a tuple origin
    /// (in which case the credential is never attached — fail-closed).
    origin: Option<String>,
    /// The credential header(s) to attach for requests to `origin`.
    headers: Vec<(String, String)>,
}

impl EndpointAuth {
    fn new(endpoint: &str, headers: Vec<(String, String)>) -> Self {
        Self {
            origin: origin_of(endpoint),
            headers,
        }
    }

    /// The credential headers to attach for a request to `url`: the bound
    /// headers when `url`'s origin matches, otherwise none.
    fn headers_for(&self, url: &str) -> &[(String, String)] {
        match (&self.origin, origin_of(url)) {
            (Some(bound), Some(target)) if *bound == target => &self.headers,
            _ => &[],
        }
    }
}

/// The ASCII origin (`scheme://host[:port]`) of `url`, or `None` when the URL
/// does not parse to a tuple (network-addressable) origin.
///
/// Exposed within the crate so the daemon can reject an `auth_ref` provider whose
/// endpoint would not bind a credential (REQ-544): a `None` here means
/// [`HttpTransport::with_endpoint_auth`] would silently attach the header to
/// nothing.
pub(crate) fn origin_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let origin = parsed.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// The real, network-backed [`Transport`] — the sole HTTP client in the tree.
///
/// Wraps a single `reqwest::Client` (connection-pooled, cheap to clone). Adapters
/// never construct this; the daemon builds one and hands it to [`Egress`]. Auth
/// is attached here, not by adapters (BR-7): a resolved credential passed to
/// [`HttpTransport::with_endpoint_auth`] is bound to that provider's endpoint and
/// injected onto — and only onto — requests to that endpoint's origin. A
/// transport built with [`HttpTransport::new`] carries no credential (the MCP
/// egress path uses it, so MCP requests stay header-free).
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    injected: Option<EndpointAuth>,
}

impl HttpTransport {
    /// Build a transport with a fresh HTTP client and no injected credential.
    /// This is the MCP egress path's transport — it never carries provider auth.
    pub fn new() -> Result<Self, EgressError> {
        Self::build(None, None, false)
    }

    /// Build a transport that injects `headers` (a resolved provider credential,
    /// BR-7) **only** onto requests whose URL targets `endpoint`'s origin. The
    /// binding is what keeps a credential from leaking onto an MCP request or a
    /// different provider's request (REQ-544 M-3).
    pub fn with_endpoint_auth(
        endpoint: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Self, EgressError> {
        Self::build(Some(EndpointAuth::new(endpoint, headers)), None, false)
    }

    /// Build the **web-lookup** transport: no credential, and a connect timeout
    /// (REQ-563).
    ///
    /// The distinction from [`Self::new`] is the timeout, and it exists because
    /// the two paths face different destinations. A provider endpoint is one
    /// host the user configured and the daemon talks to constantly; a lookup
    /// destination is an arbitrary host a model or a user named once, and an
    /// arbitrary host is entitled to accept a connection and then say nothing.
    /// Without a connect bound, a single such destination parks the turn
    /// forever.
    ///
    /// This bounds the *connect* phase only. The bound on the whole attempt —
    /// redirects, headers and body read included — is
    /// [`lookup::LOOKUP_TOTAL_TIMEOUT`], enforced at the seam rather than here,
    /// because it has to hold for every transport this choke point is built
    /// over (the test doubles included) and not only for the real client.
    ///
    /// Wave-2 wiring note: `DaemonRuntime::web_lookup_egress` must build its
    /// transport from this and from
    /// [`Self::for_lookup_with_endpoint_auth`] rather than from [`Self::new`]
    /// and [`Self::with_endpoint_auth`]. The seam-level bound holds either way;
    /// what is lost by not switching is the connect bound, which is the one
    /// that fails a black-holed destination fast instead of at the sixty-second
    /// ceiling.
    ///
    /// # Errors
    /// [`EgressError::ClientInit`] if the HTTP client cannot be constructed.
    pub fn for_lookup() -> Result<Self, EgressError> {
        Self::build(None, Some(lookup::LOOKUP_CONNECT_TIMEOUT), true)
    }

    /// The lookup transport with a search credential bound to `endpoint`'s
    /// origin (REQ-563 BR-7): [`Self::for_lookup`]'s timeouts,
    /// [`Self::with_endpoint_auth`]'s binding.
    ///
    /// The binding is one of the two things keeping the search key off a page
    /// fetch. The other is at the seam: [`Egress::lookup`] refuses a `Fetch`
    /// whose destination origin *is* the search endpoint's, so the case where
    /// the origin match would have attached the key never reaches this
    /// transport at all.
    ///
    /// # Errors
    /// [`EgressError::ClientInit`] if the HTTP client cannot be constructed.
    pub fn for_lookup_with_endpoint_auth(
        endpoint: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Self, EgressError> {
        Self::build(
            Some(EndpointAuth::new(endpoint, headers)),
            Some(lookup::LOOKUP_CONNECT_TIMEOUT),
            true,
        )
    }

    fn build(
        injected: Option<EndpointAuth>,
        connect_timeout: Option<Duration>,
        screen_dns: bool,
    ) -> Result<Self, EgressError> {
        // REQ-544 security (Low): do NOT auto-follow redirects. The daemon POSTs to
        // a single known provider/MCP endpoint and needs no redirect handling.
        // reqwest strips `Authorization` on a cross-host redirect but NOT custom
        // credential headers like `x-api-key`, so following a redirect could carry
        // a provider secret to an attacker-influenced host. Refusing redirects
        // outright closes that path (the caller sees the 3xx and stops).
        let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        // Applied only where a caller asked for it, so the provider `send()`
        // path's behavior is byte-for-byte what it was: a long-running
        // completion is not a stalled connection, and a bound tuned for a page
        // fetch would cut one off (REQ-563 review).
        if let Some(connect_timeout) = connect_timeout {
            builder = builder.connect_timeout(connect_timeout);
        }
        // REQ-563 SSRF closure: the lookup path resolves an arbitrary name a
        // model or a user supplied once, so it screens every resolved address
        // against the address-class policy and refuses a non-global answer at
        // connect time ([`lookup::GlobalOnlyResolver`]) — the closure the
        // `address_class_of_host` residual comment records as owed. The provider
        // `send()` path keeps the default resolver: its destination is one
        // endpoint the operator configured, and a screen there would second-guess
        // a host they chose (and is out of this closure's scope).
        if screen_dns {
            builder = builder.dns_resolver(Arc::new(lookup::GlobalOnlyResolver::system()));
        }
        let client = builder.build().map_err(|_| EgressError::ClientInit)?;
        Ok(Self { client, injected })
    }

    /// The final outbound header list for a request to `url`: the adapter's
    /// protocol headers plus the endpoint-bound credential headers, attached only
    /// when `url` matches the bound origin. A credential header replaces any
    /// protocol header of the same (case-insensitive) name, so injecting a header
    /// the adapter also sets (e.g. `anthropic-version`) cannot produce a
    /// duplicate. Exposed within the crate so the cross-contamination guarantee
    /// (BR-7) is unit-testable without a network round-trip.
    #[must_use]
    pub(crate) fn outbound_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Vec<(String, String)> {
        let creds = self
            .injected
            .as_ref()
            .map_or::<&[(String, String)], _>(&[], |a| a.headers_for(url));
        let mut out = Vec::with_capacity(headers.len() + creds.len());
        for (name, value) in headers {
            let shadowed = creds.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
            if !shadowed {
                out.push((name.clone(), value.clone()));
            }
        }
        out.extend(creds.iter().cloned());
        out
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let method = match request.method {
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Get => reqwest::Method::GET,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in self.outbound_headers(&request.url, &request.headers) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        // A GET sends no body. Attaching an empty one would set
        // `content-length: 0` on a retrieval request, which some origins answer
        // with a 411/400 — and a GET body has no meaning to any destination
        // this daemon reaches, so there is nothing to preserve by sending it.
        if request.method != HttpMethod::Get {
            builder = builder.body(request.body);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

        let status = response.status().as_u16();
        // The one response header this seam reports (REQ-563). The client is
        // built with `Policy::none()`, so a 3xx arrives here intact and the
        // caller — the bounded, host-checked fetch loop — needs to know where
        // it points. A header that is not valid ASCII is `None`: a redirect
        // target that cannot be read is one that cannot be followed, and
        // guessing at the bytes is how a host check gets pointed at the wrong
        // string.
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body: ByteStream = Box::pin(response.bytes_stream().map(|chunk| {
            chunk
                .map(|b| b.to_vec())
                .map_err(|e| classify_reqwest_error(&e))
        }));
        Ok(TransportResponse {
            status,
            location,
            body,
        })
    }
}

/// Map a `reqwest` failure to the transport's closed error taxonomy. Note this
/// classifies the *failure class* only — no URL, header, or body content is
/// carried into the error (BR-1 / conventions: nothing content-bearing in logs).
fn classify_reqwest_error(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() {
        TransportError::Connect
    } else {
        TransportError::Io
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A `Transport` that records what it was asked to send and returns an empty
    /// 200 — the unit-level stand-in for the network. (The integration harness in
    /// `tests/egress_capture.rs` uses a fuller version.) The record lives behind a
    /// shared `Arc` so a test can hold a handle after the transport is moved into
    /// the [`Egress`].
    #[derive(Default, Clone)]
    struct CaptureTransport {
        sent: Arc<Mutex<Vec<TransportRequest>>>,
    }

    #[async_trait]
    impl Transport for CaptureTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent.lock().unwrap().push(request);
            Ok(TransportResponse {
                location: None,
                status: 200,
                body: Box::pin(futures::stream::empty()),
            })
        }
    }

    #[derive(Default)]
    struct CapturingSink {
        events: Mutex<Vec<PrivacyBlock>>,
    }

    impl PrivacyEventSink for CapturingSink {
        fn privacy_block(&self, _session_id: Option<SessionId>, block: PrivacyBlock) {
            self.events.lock().unwrap().push(block);
        }
    }

    fn boundaries() -> Vec<PrivacyBoundary> {
        use teton_core::entities::BoundaryMode;
        vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
        }]
    }

    fn a_request(body: &str) -> TransportRequest {
        TransportRequest {
            method: HttpMethod::Post,
            url: "https://api.example.com/v1/messages".to_owned(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn a_clean_request_reaches_the_inner_transport() {
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()));
        let prov = Provenance::tainted_by("src/main.rs");
        let ctx = EgressContext::new("anthropic");
        let resp = egress
            .send(a_request("public code"), &prov, &ctx)
            .await
            .expect("clean request allowed");
        assert_eq!(resp.status, 200);
        // The inner transport received exactly the forwarded request.
        let forwarded = sent.lock().unwrap();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].body, b"public code");
    }

    #[tokio::test]
    async fn boundary_provenance_is_blocked_and_never_forwarded() {
        let egress = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        );
        let prov = Provenance::tainted_by("secrets/prod.env");
        let ctx = EgressContext::new("anthropic");
        let err = egress
            .send(a_request("API_KEY=super-secret-xyzzy"), &prov, &ctx)
            .await
            .expect_err("must be blocked");
        match err {
            EgressError::PrivacyBlocked {
                path,
                provider_id,
                action,
                cause,
            } => {
                assert_eq!(cause, BlockCause::Boundary);
                assert_eq!(path, "secrets/prod.env");
                assert_eq!(provider_id, ProviderId::from("anthropic"));
                assert_eq!(action, PrivacyAction::ReroutedToLocal);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_block_emits_a_privacy_block_event() {
        let sink = Arc::new(CapturingSink::default());
        let egress = Egress::new(CaptureTransport::default(), boundaries(), sink.clone());
        let prov = Provenance::tainted_by("secrets/prod.env");
        let ctx = EgressContext::new("deepseek").with_session("sess-under-test");
        let _ = egress.send(a_request("secret"), &prov, &ctx).await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "secrets/prod.env");
        assert_eq!(events[0].provider_id, ProviderId::from("deepseek"));
        assert_eq!(events[0].action, PrivacyAction::ReroutedToLocal);
    }

    #[tokio::test]
    async fn the_scoped_transport_enforces_the_boundary() {
        let sink = Arc::new(CapturingSink::default());
        let egress = Egress::new(CaptureTransport::default(), boundaries(), sink.clone());
        let scoped = egress.scoped(
            Provenance::tainted_by("secrets/prod.env"),
            EgressContext::new("anthropic"),
        );
        // Adapter-style call through the object-safe seam.
        let err = scoped
            .execute(a_request("secret"))
            .await
            .expect_err("scoped transport must refuse");
        // REQ-544 M-1: a block surfaces as the dedicated, non-retryable variant,
        // never a connect refusal.
        assert_eq!(
            err,
            TransportError::PrivacyBlocked(BlockDetail::Boundary),
            "and the cause travels with it, so the daemon can say which \
             inspection refused the turn"
        );
        // The event still fired even though the adapter only saw a transport error.
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_provenance_is_always_allowed() {
        let egress = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        );
        let ctx = EgressContext::new("anthropic");
        let resp = egress
            .send(a_request("system prompt only"), &Provenance::empty(), &ctx)
            .await
            .expect("empty provenance allowed");
        assert_eq!(resp.status, 200);
    }

    // -----------------------------------------------------------------------
    // REQ-562 — the redaction gate hook (ADR-1, ADR-2).
    // -----------------------------------------------------------------------

    /// A gate that answers with a canned verdict and records every payload it
    /// was handed.
    ///
    /// The **count** is the instrument for half these tests: "off means off" and
    /// "provenance runs first" are claims about how many times the scanner ran,
    /// not about what it returned, and a mock that only returned a verdict could
    /// not tell a skipped scan from a permissive one (LESSON-485).
    struct CountingGate {
        verdict: RedactionVerdict,
        seen: Mutex<Vec<String>>,
    }

    impl CountingGate {
        fn new(verdict: RedactionVerdict) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn payloads(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RedactionGate for CountingGate {
        async fn scan(&self, payload: &str) -> RedactionVerdict {
            self.seen.lock().unwrap().push(payload.to_owned());
            self.verdict.clone()
        }
    }

    fn a_high_finding() -> RedactionVerdict {
        RedactionVerdict::from_findings(vec![redact::Finding::pattern(
            redact::FindingKind::Credential,
            1400..1436,
        )])
    }

    fn a_low_finding() -> RedactionVerdict {
        RedactionVerdict::from_findings(vec![redact::Finding::model(
            redact::FindingKind::Pii,
            10..22,
        )])
    }

    #[tokio::test]
    async fn off_means_zero_scanner_calls_and_on_means_exactly_one() {
        // AC-13, both legs, by call count — and by the SAME gate object, so
        // "zero" is a fact about the choke point's construction rather than
        // about a gate that could not have counted anyway.
        let gate = CountingGate::new(RedactionVerdict::clean());
        let ctx = EgressContext::new("anthropic");

        let inner = CaptureTransport::default();
        let off_sent = inner.sent.clone();
        let off = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()));
        off.send(a_request("ordinary prompt"), &Provenance::empty(), &ctx)
            .await
            .expect("an un-gated choke point forwards");
        assert_eq!(
            gate.calls(),
            0,
            "with no gate installed nothing may claim a scan ran"
        );
        assert_eq!(off_sent.lock().unwrap().len(), 1);

        let inner = CaptureTransport::default();
        let on_sent = inner.sent.clone();
        let on = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()))
            .with_redaction_gate(gate.clone());
        on.send(a_request("ordinary prompt"), &Provenance::empty(), &ctx)
            .await
            .expect("a clean verdict forwards");
        assert_eq!(
            gate.calls(),
            1,
            "the same gate, now installed, scans exactly once per send"
        );
        assert_eq!(on_sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_payload_refused_by_provenance_is_never_scanned() {
        // AC-11: the boundary check returns before the gate line is reached, so
        // a refused payload costs zero inferences. Paired with the leg below,
        // where the identical gate on the identical choke point *does* run —
        // otherwise "zero calls" would also be satisfied by a gate that never
        // runs at all.
        let gate = CountingGate::new(RedactionVerdict::clean());
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()))
            .with_redaction_gate(gate.clone());
        let ctx = EgressContext::new("anthropic");

        let err = egress
            .send(
                a_request("API_KEY=super-secret-xyzzy"),
                &Provenance::tainted_by("secrets/prod.env"),
                &ctx,
            )
            .await
            .expect_err("the boundary refuses it");
        assert!(matches!(
            err,
            EgressError::PrivacyBlocked {
                cause: BlockCause::Boundary,
                ..
            }
        ));
        assert_eq!(gate.calls(), 0, "a boundary block buys no scanner call");
        assert!(sent.lock().unwrap().is_empty());

        // The discriminating leg: same gate, same choke point, provenance the
        // boundary does not match.
        egress
            .send(
                a_request("public code"),
                &Provenance::tainted_by("src/main.rs"),
                &ctx,
            )
            .await
            .expect("an allowed payload is scanned and forwarded");
        assert_eq!(gate.calls(), 1);
    }

    #[tokio::test]
    async fn an_unavailable_scan_blocks_the_send_and_says_it_could_not_run() {
        // AC-3 at this layer, fail closed: the transport records zero requests,
        // and both the event and the typed error say the scan could not run —
        // never that it found something (BR-3).
        let sink = Arc::new(CapturingSink::default());
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), sink.clone())
            .with_redaction_gate(CountingGate::new(RedactionVerdict::unavailable()));

        let err = egress
            .send(
                a_request("ordinary prompt"),
                &Provenance::empty(),
                &EgressContext::new("anthropic").with_session("sess-under-test"),
            )
            .await
            .expect_err("an unscannable payload must not be sent");

        assert!(
            sent.lock().unwrap().is_empty(),
            "a guard that cannot run does not become a guard that passes everything"
        );
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cause, BlockCause::ScanUnavailable);
        assert_eq!(events[0].path, "the outbound payload");
        assert_eq!(events[0].provider_id, ProviderId::from("anthropic"));

        let rendered = err.to_string();
        assert!(rendered.contains("could not run"), "{rendered}");
        assert!(
            !rendered.contains("found"),
            "a scan that never ran cannot have found anything: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_high_confidence_finding_blocks_with_its_kind_span_and_locus() {
        // AC-1 at this layer. The span is the fixture's, so this pins that the
        // finding's own offsets reach the event rather than a placeholder.
        let sink = Arc::new(CapturingSink::default());
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), sink.clone())
            .with_redaction_gate(CountingGate::new(a_high_finding()));

        let err = egress
            .send(
                a_request("a prompt with a credential in it"),
                &Provenance::empty(),
                &EgressContext::new("anthropic"),
            )
            .await
            .expect_err("a high-confidence finding blocks");

        assert!(sent.lock().unwrap().is_empty(), "the bytes are dropped");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].cause,
            BlockCause::Redaction {
                kind: WireFindingKind::Credential,
                span: ByteSpan {
                    start: 1400,
                    end: 1436
                },
            }
        );
        // A locus, not a path: there is no file here, and there is no matched
        // text on the event to name one with (BR-6).
        assert_eq!(events[0].path, "the outbound payload");

        let rendered = err.to_string();
        assert!(rendered.contains("a credential"), "{rendered}");
        assert!(rendered.contains("1400-1436"), "{rendered}");
        assert!(
            !rendered.contains("credential in it"),
            "no payload content may reach the sentence: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_low_only_or_clean_verdict_forwards_the_exact_bytes() {
        // AC-9: v1 detects, it never substitutes. Both forwarding verdicts are
        // here because they reach the forward through different arms of
        // `decide`, and both must leave the request byte-identical.
        const BODY: &str = "a prompt the scan looked at and let through";
        for (name, verdict) in [
            ("clean", RedactionVerdict::clean()),
            ("low only", a_low_finding()),
        ] {
            let sink = Arc::new(CapturingSink::default());
            let inner = CaptureTransport::default();
            let sent = inner.sent.clone();
            let gate = CountingGate::new(verdict);
            let egress =
                Egress::new(inner, boundaries(), sink.clone()).with_redaction_gate(gate.clone());

            let resp = egress
                .send(
                    a_request(BODY),
                    &Provenance::empty(),
                    &EgressContext::new("anthropic"),
                )
                .await
                .unwrap_or_else(|e| panic!("{name}: must forward, got {e}"));
            assert_eq!(resp.status, 200, "{name}");

            let forwarded = sent.lock().unwrap();
            assert_eq!(forwarded.len(), 1, "{name}");
            assert_eq!(
                forwarded[0].body,
                BODY.as_bytes(),
                "{name}: the forwarded bytes must be byte-identical to the input"
            );
            // Non-vacuity: the scan ran, and it saw exactly those bytes.
            assert_eq!(gate.calls(), 1, "{name}");
            assert_eq!(gate.payloads(), vec![BODY.to_owned()], "{name}");
            assert!(
                sink.events.lock().unwrap().is_empty(),
                "{name}: a forward emits no privacy_block"
            );
        }
    }

    /// **The forwarded-findings report is actually wired into `send`** (BR-4,
    /// ADR-4).
    ///
    /// Asserted against this module's own source rather than against captured
    /// output, and the reason is a limitation worth naming: the report goes to
    /// the daemon's log, which is this process's **stderr** — the one output
    /// stream `libtest` does not capture, and a process-global file descriptor
    /// no test may redirect without racing every other test in the binary.
    ///
    /// So the split is: [`redact::forwarded_findings_report`] owns the *content*
    /// and is pinned by four unit tests beside it (kind, span, one line per
    /// finding, no matched text, and nothing at all for a clean or blocked
    /// verdict), and this pins the *call* — delete the loop in `send` and this
    /// turns red. The same instrument the duty seam uses for
    /// `bound_to_ceiling`.
    ///
    /// ## The needle covers the loop **body**, not just its header
    ///
    /// Matching only `forwarded_findings_report(&verdict)` accepts a loop that
    /// calls the report and then does nothing with the lines — "call kept,
    /// effect dropped", which is precisely the state this test exists to
    /// forbid: findings computed and discarded are indistinguishable from
    /// findings never made. So the whole statement is the needle, `eprintln!`
    /// included. Whitespace is normalized first because `rustfmt`'s indentation
    /// is not the thing under test.
    #[test]
    fn the_gate_arm_reports_a_forwarded_findings_verdict() {
        let source = crate::call_sites::scan::production_sources()
            .into_iter()
            .find(|(rel, _)| rel == "egress/mod.rs")
            .map(|(_, src)| crate::call_sites::scan::code_only(&src))
            .expect("this module is a production source");
        let normalized = source.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains(
                "for line in redact::forwarded_findings_report(&verdict) { eprintln!(\"{line}\"); }"
            ),
            "the gate arm must report a forwarded verdict's findings AND emit them; a \
             finding computed and then dropped is indistinguishable from one never made \
             (BR-4, ADR-4)"
        );
        assert_eq!(
            crate::call_sites::scan::count(&source, "forwarded_findings_report"),
            1,
            "exactly one report site, so one payload cannot be reported twice"
        );
    }

    #[tokio::test]
    async fn a_blocked_send_bills_nothing_while_an_allowed_one_still_bills() {
        /// A meter that counts the responses it was asked to wrap.
        #[derive(Default)]
        struct CountingMeter {
            metered: std::sync::atomic::AtomicUsize,
        }

        impl CostMeter for CountingMeter {
            fn meter_response(
                &self,
                response: TransportResponse,
                _session_id: Option<SessionId>,
                _provider_id: ProviderId,
                _attribution: CostAttribution,
            ) -> TransportResponse {
                self.metered
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                response
            }
        }

        // The gate hook must not move cost metering: a blocked send never
        // reaches `inner.execute`, so it bills nothing — today's behaviour for
        // boundary blocks — and an allowed forward bills exactly as before.
        let meter = Arc::new(CountingMeter::default());
        let ctx = EgressContext::new("anthropic")
            .with_session("sess-under-test")
            .with_cost(CostAttribution::new("claude-opus-4"));

        let blocked = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        )
        .with_cost_meter(meter.clone())
        .with_redaction_gate(CountingGate::new(a_high_finding()));
        let _ = blocked
            .send(a_request("blocked"), &Provenance::empty(), &ctx)
            .await
            .expect_err("blocked");
        assert_eq!(
            meter.metered.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a blocked send bills nothing"
        );

        let allowed = Egress::new(
            CaptureTransport::default(),
            boundaries(),
            Arc::new(CapturingSink::default()),
        )
        .with_cost_meter(meter.clone())
        .with_redaction_gate(CountingGate::new(RedactionVerdict::clean()));
        allowed
            .send(a_request("allowed"), &Provenance::empty(), &ctx)
            .await
            .expect("allowed");
        assert_eq!(
            meter.metered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an allowed forward meters exactly as it did before the gate existed"
        );
    }

    #[tokio::test]
    async fn the_gate_reads_the_exact_bytes_that_would_go_on_the_wire() {
        // ADR-1: no separate text projection that could drift from the wire.
        // A body that is not valid UTF-8 is still scanned — refusing to look at
        // it would skip exactly the payloads worth looking at.
        let gate = CountingGate::new(RedactionVerdict::clean());
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), Arc::new(CapturingSink::default()))
            .with_redaction_gate(gate.clone());
        let mut request = a_request("");
        request.body = vec![b'o', b'k', 0xff, b'!'];

        egress
            .send(
                request,
                &Provenance::empty(),
                &EgressContext::new("anthropic"),
            )
            .await
            .expect("a clean verdict forwards");

        assert_eq!(gate.payloads(), vec![String::from("ok\u{fffd}!")]);
        // And the forward is still the original bytes, not the lossy text.
        assert_eq!(sent.lock().unwrap()[0].body, vec![b'o', b'k', 0xff, b'!']);
    }

    #[test]
    fn a_redaction_block_reports_the_first_high_finding_and_never_a_low_one() {
        // `block_cause` is derived from the verdict, so it must agree with
        // `decide` about what blocked: the high finding, whichever position it
        // arrives in, and never a low one (which does not block at all).
        let verdict = RedactionVerdict::from_findings(vec![
            redact::Finding::model(redact::FindingKind::Pii, 0..4),
            redact::Finding::pattern(redact::FindingKind::Secret, 90..120),
            redact::Finding::pattern(redact::FindingKind::Credential, 200..230),
        ]);
        assert_eq!(
            block_cause(&verdict),
            BlockCause::Redaction {
                kind: WireFindingKind::Secret,
                span: ByteSpan {
                    start: 90,
                    end: 120
                },
            }
        );
        assert_eq!(
            block_cause(&RedactionVerdict::unavailable()),
            BlockCause::ScanUnavailable
        );
    }

    #[tokio::test]
    async fn an_unavailable_verdict_names_a_credential_only_when_the_pattern_pass_found_one() {
        // The 2026-08-08 reversal, and its discriminator, at the layer that
        // decides the cause. Both verdicts are `Unavailable` → Block; what
        // separates them is whether the deterministic pass had established
        // anything before the model pass failed.
        //
        // The consequence rides the cause rather than being decided again:
        // `Redaction` pins the session and `ScanUnavailable` does not
        // (`runtime::cause_taints_the_session`, held to its `BlockDetail` twin
        // by `the_two_taint_gates_agree_cause_for_cause`), so the row below is
        // also the row that restores a pin a transient stall used to erase.
        let established =
            RedactionVerdict::unavailable_with_evidence(vec![redact::Finding::pattern(
                redact::FindingKind::Credential,
                40..66,
            )]);
        assert_eq!(
            block_cause(&established),
            BlockCause::Redaction {
                kind: WireFindingKind::Credential,
                span: ByteSpan { start: 40, end: 66 },
            },
            "the pattern pass completed over the whole payload and found this; \
             reporting `could not run` throws away the only fact anyone \
             established"
        );
        assert_eq!(
            block_cause(&RedactionVerdict::unavailable()),
            BlockCause::ScanUnavailable,
            "and with nothing established there is nothing to name"
        );

        // A low finding cannot reach the cause through this door either: BR-4's
        // rule that a low-confidence finding never blocks is not routed around
        // by handing it to the evidence constructor.
        assert_eq!(
            block_cause(&RedactionVerdict::unavailable_with_evidence(vec![
                redact::Finding::model(redact::FindingKind::Pii, 10..22),
            ])),
            BlockCause::ScanUnavailable
        );

        // And the whole way through the choke point: the event a real send
        // emits carries the named cause, and the sentence names the credential
        // rather than telling the user to go debug an engine.
        let sink = Arc::new(CapturingSink::default());
        let inner = CaptureTransport::default();
        let sent = inner.sent.clone();
        let egress = Egress::new(inner, boundaries(), sink.clone())
            .with_redaction_gate(CountingGate::new(established));

        let err = egress
            .send(
                a_request("a prompt with a credential in it"),
                &Provenance::empty(),
                &EgressContext::new("anthropic").with_session("sess-under-test"),
            )
            .await
            .expect_err("an unavailable verdict still blocks");

        assert!(
            sent.lock().unwrap().is_empty(),
            "the bytes are still dropped"
        );
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].cause,
            BlockCause::Redaction {
                kind: WireFindingKind::Credential,
                span: ByteSpan { start: 40, end: 66 },
            }
        );
        let rendered = err.to_string();
        assert!(rendered.contains("a credential"), "{rendered}");
        assert!(rendered.contains("40-66"), "{rendered}");
        assert!(
            !rendered.contains("credential in it"),
            "no payload content may reach the sentence: {rendered}"
        );
    }

    #[test]
    fn egress_error_display_carries_no_content() {
        // AC-4: the typed error is safe to log — path + provider + action only.
        let err = EgressError::PrivacyBlocked {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
            cause: BlockCause::Boundary,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("secrets/prod.env"));
        assert!(rendered.contains("anthropic"));
        // Never any content bytes.
        assert!(!rendered.contains("API_KEY"));
    }

    #[test]
    fn privacy_blocked_maps_to_the_dedicated_non_retryable_transport_signal() {
        // REQ-544 M-1: a privacy block must NOT collapse into TransportError::Connect
        // (which would be classified transient and retried); it gets its own
        // dedicated, non-retryable variant.
        let err = EgressError::PrivacyBlocked {
            path: "secrets/x".to_owned(),
            provider_id: ProviderId::from("p"),
            action: PrivacyAction::ReroutedToLocal,
            cause: BlockCause::Boundary,
        };
        assert_eq!(
            err.into_transport_error(),
            TransportError::PrivacyBlocked(BlockDetail::Boundary)
        );
        // ClientInit is unrelated and still a connect refusal.
        assert_eq!(
            EgressError::ClientInit.into_transport_error(),
            TransportError::Connect
        );
    }

    /// **REQ-562 BR-3.** Each cause projects onto its own [`BlockDetail`] as the
    /// error collapses to the transport seam, so the daemon's turn-failure
    /// sentence can name which inspection refused the turn.
    ///
    /// Table-driven and exhaustive over the three, because the failure mode is a
    /// `match` arm quietly collapsing two causes into one value — which reads as
    /// working code and produces a sentence that sends the user after the wrong
    /// problem. The `assert_ne` pass is what catches a collapse that a
    /// per-row `assert_eq` on a single row would not.
    #[test]
    fn every_block_cause_projects_onto_its_own_transport_detail() {
        fn collapse(cause: BlockCause) -> TransportError {
            EgressError::PrivacyBlocked {
                path: "the outbound payload".to_owned(),
                provider_id: ProviderId::from("p"),
                action: PrivacyAction::ReroutedToLocal,
                cause,
            }
            .into_transport_error()
        }

        let rows = [
            (BlockCause::Boundary, BlockDetail::Boundary),
            (
                BlockCause::Redaction {
                    kind: WireFindingKind::Credential,
                    span: ByteSpan {
                        start: 1400,
                        end: 1436,
                    },
                },
                BlockDetail::Redaction,
            ),
            (BlockCause::ScanUnavailable, BlockDetail::ScanUnavailable),
        ];
        for (cause, expected) in rows {
            assert_eq!(
                collapse(cause),
                TransportError::PrivacyBlocked(expected),
                "{cause:?} must reach the seam as {expected:?}"
            );
        }
        // No two causes may arrive as the same detail.
        for (i, (cause, _)) in rows.iter().enumerate() {
            for (other, _) in rows.iter().skip(i + 1) {
                assert_ne!(
                    collapse(*cause),
                    collapse(*other),
                    "{cause:?} and {other:?} must not collapse into one signal"
                );
            }
        }
    }

    const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
    const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/v1/chat/completions";
    const MCP_ENDPOINT: &str = "https://mcp.example.com/rpc";
    const SECRET: &str = "sk-ant-SUPER-SECRET-xyzzy";

    /// The auth header injected for the Anthropic provider (mirrors the daemon's
    /// `provider_auth_headers`). The `anthropic-version` here is deliberately a
    /// *different* value than the adapter's to prove the injected one wins.
    fn anthropic_creds() -> Vec<(String, String)> {
        vec![
            ("x-api-key".to_owned(), SECRET.to_owned()),
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
        ]
    }

    /// The adapter's protocol headers for an Anthropic request.
    fn anthropic_protocol_headers() -> Vec<(String, String)> {
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
        ]
    }

    fn header_names(headers: &[(String, String)]) -> Vec<String> {
        headers
            .iter()
            .map(|(n, _)| n.to_ascii_lowercase())
            .collect()
    }

    #[test]
    fn a_provider_credential_reaches_only_its_owning_endpoint() {
        // REQ-544 M-3 / BR-7 cross-contamination guard: a credential bound to the
        // Anthropic endpoint must appear on a request to that endpoint and NEVER
        // on a request to an MCP endpoint or a different provider's endpoint.
        let transport = HttpTransport::with_endpoint_auth(ANTHROPIC_ENDPOINT, anthropic_creds())
            .expect("build endpoint-bound transport");

        // 1. The owning endpoint receives the credential.
        let owning = transport.outbound_headers(ANTHROPIC_ENDPOINT, &anthropic_protocol_headers());
        assert!(
            owning
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case("x-api-key") && v == SECRET),
            "the owning provider endpoint must carry x-api-key"
        );

        // 2. An MCP request through the same transport gets NO credential and no
        //    trace of the secret anywhere.
        let mcp = transport.outbound_headers(
            MCP_ENDPOINT,
            &[("content-type".to_owned(), "application/json".to_owned())],
        );
        assert!(
            !header_names(&mcp).iter().any(|n| n == "x-api-key"),
            "MCP request must never carry the provider credential header"
        );
        assert!(
            !mcp.iter().any(|(_, v)| v.contains(SECRET)),
            "the secret must never appear on an MCP request"
        );

        // 3. A different provider's endpoint likewise never sees the credential.
        let other = transport.outbound_headers(
            DEEPSEEK_ENDPOINT,
            &[("content-type".to_owned(), "application/json".to_owned())],
        );
        assert!(
            !other.iter().any(|(_, v)| v.contains(SECRET)),
            "the secret must never appear on another provider's request"
        );
    }

    #[test]
    fn an_injected_header_replaces_a_duplicate_protocol_header() {
        // Injecting `anthropic-version` (which the adapter also sets) must not
        // produce two copies on the wire — the injected credential-layer header
        // wins, case-insensitively.
        let transport = HttpTransport::with_endpoint_auth(ANTHROPIC_ENDPOINT, anthropic_creds())
            .expect("build transport");
        let out = transport.outbound_headers(ANTHROPIC_ENDPOINT, &anthropic_protocol_headers());
        let version_count = out
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("anthropic-version"))
            .count();
        assert_eq!(
            version_count, 1,
            "anthropic-version must appear exactly once"
        );
    }

    #[test]
    fn a_credential_free_transport_never_injects_anything() {
        // The MCP egress path builds this transport: it must add no headers of
        // its own to any request, to any endpoint.
        let transport = HttpTransport::new().expect("build credential-free transport");
        let protocol = vec![("content-type".to_owned(), "application/json".to_owned())];
        let out = transport.outbound_headers(MCP_ENDPOINT, &protocol);
        assert_eq!(
            out, protocol,
            "no credential-free transport may inject headers"
        );
    }

    #[test]
    fn the_lookup_transports_build_with_the_screening_resolver_installed() {
        // The lookup constructors install `lookup::GlobalOnlyResolver` on the
        // client (REQ-563 SSRF closure). This guards that wiring the resolver in
        // does not fail `build` — the screening *behavior* is covered by the
        // resolver's own tests in `egress::lookup`, which drive it with a fake
        // DNS map rather than the network.
        assert!(HttpTransport::for_lookup().is_ok());
        assert!(
            HttpTransport::for_lookup_with_endpoint_auth("https://search.example", vec![]).is_ok()
        );
    }

    #[test]
    fn origin_matching_ignores_the_path_but_distinguishes_host() {
        // Same origin, different path -> still the owning endpoint (a provider's
        // base URL and its /v1/... path share an origin).
        let transport =
            HttpTransport::with_endpoint_auth("https://api.anthropic.com", anthropic_creds())
                .expect("build transport");
        let same_origin = transport.outbound_headers(ANTHROPIC_ENDPOINT, &[]);
        assert!(same_origin.iter().any(|(n, _)| n == "x-api-key"));
        // A look-alike host must not match.
        let evil =
            transport.outbound_headers("https://api-anthropic.com.evil.test/v1/messages", &[]);
        assert!(!evil.iter().any(|(n, _)| n == "x-api-key"));
    }

    #[test]
    fn the_three_redaction_slots_are_installed_independently() {
        // Three switches, three slots (REQ-563 D-6): `[privacy] redact`
        // installs the provider gate *and* the fetch-parity gate, the `search`
        // tier installs the search gate. Sharing a slot would make the coupling
        // BR-14 requires bypassable by turning `[privacy] redact` off, and
        // reading one flag twice would make BR-2's parity clause unstateable.
        let gate = CountingGate::new(RedactionVerdict::clean());
        let build = || {
            Egress::new(
                CaptureTransport::default(),
                boundaries(),
                Arc::new(CapturingSink::default()),
            )
        };

        let bare = build();
        assert_eq!(bare.installed(), (false, false, false));
        assert!(!bare.fetch_redaction_installed());

        let provider_only = build().with_redaction_gate(gate.clone());
        assert_eq!(provider_only.installed(), (true, false, false));
        assert!(
            !provider_only.fetch_redaction_installed(),
            "the provider slot is not the fetch slot: the daemon installs both \
             from one switch, and each has to be asked for"
        );

        let parity = build()
            .with_redaction_gate(gate.clone())
            .with_fetch_redaction_gate(gate.clone());
        assert_eq!(parity.installed(), (true, false, false));
        assert!(parity.fetch_redaction_installed());

        let searching = build().with_search_redaction_gate(gate);
        assert_eq!(searching.installed(), (false, true, false));
        assert!(
            !searching.fetch_redaction_installed(),
            "and the search tier's own gate does not stand in for parity"
        );
    }
}

/// CI deny-check: the egress choke point must be the workspace's *only* HTTP
/// client (BR-1). Runs under `cargo test --workspace` — the same command CI's
/// test step invokes — so a regression fails the build, not just review.
///
/// ## Scope and limitation (REQ-544 minor — honest docs)
///
/// This check reads each sibling crate's `Cargo.toml` and scans its **directly
/// declared** shipping dependencies (`[dependencies]` / `[build-dependencies]`)
/// for a known HTTP-client crate. It is a small hand parser, **not** a
/// `cargo tree` / cargo-deny transitive analysis: it catches the realistic
/// regression — someone adding `reqwest` to a crate that should stay
/// transport-free — but it would **not** catch an HTTP client pulled in
/// *transitively* by some other direct dependency. Closing that residual gap
/// needs a real transitive check (e.g. `cargo deny` banned-crates, or parsing
/// `cargo tree`) in CI; this in-tree test deliberately trades that coverage for
/// zero extra dependencies and a fast, hermetic unit test.
#[cfg(test)]
mod deny_http_client {
    use std::path::{Path, PathBuf};

    /// Crate names that mean "this crate can open a network connection itself".
    /// Only `tetond` (this crate) is permitted any of them.
    const HTTP_CLIENT_CRATES: &[&str] = &[
        "reqwest",
        "hyper",
        "hyper-util",
        "isahc",
        "ureq",
        "curl",
        "surf",
        "attohttpc",
        "http-client",
        "actix-web",
        "awc",
    ];

    /// `<workspace>/crates`, derived from this crate's manifest dir.
    fn crates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tetond lives under <workspace>/crates")
            .to_path_buf()
    }

    /// The crate names a manifest lists as *shipping* dependencies — `[dependencies]`
    /// and `[build-dependencies]`, inline or sub-table form. Comments and
    /// `[dev-dependencies]` are ignored (test-only clients cannot ship). This is a
    /// deliberately small hand parser so the check needs no toml dependency; it
    /// covers the manifest shapes this workspace actually uses.
    fn shipping_dependency_names(manifest: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut in_deps = false;
        for raw in manifest.lines() {
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let (Some(true), Some(true)) = (
                line.chars().next().map(|c| c == '['),
                line.chars().last().map(|c| c == ']'),
            ) {
                let section = line[1..line.len() - 1].trim();
                in_deps = false;
                if section == "dependencies" || section == "build-dependencies" {
                    in_deps = true;
                } else if let Some(crate_name) = section
                    .strip_prefix("dependencies.")
                    .or_else(|| section.strip_prefix("build-dependencies."))
                {
                    // Sub-table form `[dependencies.foo]` names one crate; its body
                    // lines are that crate's fields, not new dependencies.
                    names.push(crate_name.trim().to_owned());
                }
                continue;
            }
            if in_deps {
                let key = line
                    .split(['=', '.', ' ', '\t'])
                    .next()
                    .unwrap_or("")
                    .trim();
                if !key.is_empty() {
                    names.push(key.to_owned());
                }
            }
        }
        names
    }

    #[test]
    fn only_tetond_may_depend_on_an_http_client() {
        let dir = crates_dir();
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read crates dir") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "tetond" {
                continue; // the one crate allowed to hold the client
            }
            let manifest_path = entry.path().join("Cargo.toml");
            if !manifest_path.exists() {
                continue;
            }
            let manifest = std::fs::read_to_string(&manifest_path).expect("read manifest");
            for dep in shipping_dependency_names(&manifest) {
                if HTTP_CLIENT_CRATES.contains(&dep.as_str()) {
                    offenders.push(format!("crate `{name}` declares HTTP client `{dep}`"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "BR-1 requires the egress choke point be the ONLY HTTP client in the \
             workspace. Move network access behind `tetond`'s egress. Offenders: {offenders:?}"
        );
    }

    #[test]
    fn tetond_itself_declares_the_sole_http_client() {
        // Positive control: proves the client is actually wired here and that the
        // deny-list would fire if `tetond` were not the skipped crate.
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manifest = std::fs::read_to_string(manifest_path).expect("read tetond manifest");
        let names = shipping_dependency_names(&manifest);
        assert!(
            names
                .iter()
                .any(|n| HTTP_CLIENT_CRATES.contains(&n.as_str())),
            "tetond must construct the sole HTTP client (reqwest)"
        );
    }

    #[test]
    fn parser_finds_inline_dependencies_but_ignores_comments_and_dev_deps() {
        let manifest = "\
[package]
name = \"x\"

[dependencies]
serde = \"1\"
# reqwest = \"0.12\"  <- a comment, must be ignored
teton-core = { path = \"../teton-core\" }

[dependencies.tokio]
version = \"1\"
features = [\"macros\"]

[dev-dependencies]
reqwest = \"0.12\"
";
        let names = shipping_dependency_names(manifest);
        assert!(names.contains(&"serde".to_owned()));
        assert!(names.contains(&"teton-core".to_owned()));
        assert!(names.contains(&"tokio".to_owned()));
        // The commented and dev-only reqwest must NOT appear.
        assert!(!names.contains(&"reqwest".to_owned()));
    }

    #[test]
    fn parser_catches_a_planted_shipping_client() {
        let manifest = "[dependencies]\nserde = \"1\"\nhyper = { version = \"1\" }\n";
        let names = shipping_dependency_names(manifest);
        assert!(names.contains(&"hyper".to_owned()));
    }
}
