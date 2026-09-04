//! REQ-599 step 6: session taint and the egress sink that sets it.
//!
//! `SessionTaint` and the view the harness reads it through, the
//! `WebTaintOverride` a user can lift it with, the lookup recorder and seam,
//! and `TaintingPrivacySink` — the `PrivacyEventSink` that turns an egress
//! block into a pinned session, with the `CauseGate` predicates deciding which
//! causes taint and which do not.
//!
//! One subsystem: a privacy block is observed at the choke point, classified by
//! cause, and pinned onto the session so the *next* turn routes locally. Reading
//! it in one file is the argument for the split.
//!
//! **`block_in_place_if_multithread` stayed behind**, though it sat in the
//! middle of this range. It is a generic tokio seam (BUG-184), not taint
//! machinery, and `server.rs` calls it at three sites. Adjacency is not
//! membership — the same distinction that kept `redact_route` with
//! `RedactionGateImpl` in step 3.

use super::*;

/// Why a session is pinned to the local tier (REQ-614 System Model).
///
/// The cause is what decides whether a lift exists. It is recorded once — the
/// **first** cause wins — because a session pinned by reading `.env` and then
/// again by an opaque `shell` must not become liftable on the second event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaintCause {
    /// A `local-only` boundary was crossed. Permanent; no lift exists.
    BoundaryHit,
    /// A `shell` result whose reach the daemon could not prove entered context.
    /// The one liftable cause (`/shell allow`, REQ-614 BR-4/BR-5).
    UnknownShell,
    /// A provenance source failed the canonical form. Permanent.
    MalformedProvenance,
    /// An untrusted MCP server's content was refused. Permanent.
    McpUntrusted,
    /// The redaction scan found something in an outbound payload (REQ-562).
    /// Permanent.
    ///
    /// **Not in REQ-614's System Model table**, which lists four causes. The
    /// table was written from the shell-provenance side and missed a cause the
    /// daemon already pins with: `TaintingPrivacySink` taints on
    /// `BlockCause::Redaction` today. Folding it into `BoundaryHit` would have
    /// made the pin line say a boundary was crossed when none was — the class
    /// of false sentence REQ-562 BR-3 exists to forbid.
    RedactionFinding,
}

impl TaintCause {
    /// Whether `/shell allow` can lift a pin with this cause.
    ///
    /// Exactly one cause is liftable, and the match is exhaustive rather than
    /// `matches!(self, UnknownShell)` so that adding a fifth cause is a compile
    /// error here — the place where "is this safe to let the user undo?" has to
    /// be answered — instead of silently defaulting to permanent.
    #[must_use]
    pub fn liftable(self) -> bool {
        match self {
            TaintCause::UnknownShell => true,
            TaintCause::BoundaryHit
            | TaintCause::MalformedProvenance
            | TaintCause::McpUntrusted
            | TaintCause::RedactionFinding => false,
        }
    }

    /// The wire spelling, for events and the doctor report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TaintCause::BoundaryHit => "boundary_hit",
            TaintCause::UnknownShell => "unknown_shell",
            TaintCause::MalformedProvenance => "malformed_provenance",
            TaintCause::McpUntrusted => "mcp_untrusted",
            TaintCause::RedactionFinding => "redaction_finding",
        }
    }
}

/// Per-session privacy taint — the BR-1 backstop (REQ-544 C-2).
///
/// Once any tool result's provenance intersects a `local-only` boundary **or** is
/// unknown (a `shell` result), the session is marked tainted and pinned to the
/// local tier for every subsequent turn: the daemon consults this before
/// resolving a route and forces local regardless of phase policy or heuristic.
/// This is what catches the residual the per-request provenance check cannot — a
/// model paraphrasing boundary content it read on an earlier turn — because the
/// whole session is held local once it has seen boundary/unknown content. Shared
/// across turns via the [`DaemonRuntime`] `Arc`, so the pin lives as long as the
/// session (BR-4).
///
/// Since REQ-614 the pin is read through [`RoutePin`] rather than directly:
/// one cause is liftable, and composing the lift into the predicate is what
/// keeps all seven pinned-route sites honoring it.
#[derive(Debug, Default)]
pub struct SessionTaint {
    /// Session -> the **first** cause that pinned it (REQ-614).
    ///
    /// Was a `HashSet<SessionId>`. The cause is what makes a pin explicable to
    /// the user and what decides whether a lift exists; a bare set could say
    /// only *that* a session was pinned.
    tainted: Mutex<HashMap<SessionId, TaintCause>>,
}

impl SessionTaint {
    /// An empty taint set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `session` tainted — pinned to the local tier for all later turns
    /// (idempotent).
    ///
    /// Returns **whether this call was the clean→tainted transition**, which is
    /// what makes the pin announceable exactly once per session: the pin is a
    /// durable, session-wide consequence with no in-session undo, and a
    /// consequence that durable that nothing says out loud is one the user
    /// discovers as "why is this suddenly slower". Every production call site
    /// pairs a `true` with one [`taint_pin_line`] on stderr; a `false` is a
    /// re-mark and owes nothing, so a session pinned by a boundary read and then
    /// again by a redaction block still gets one line.
    ///
    /// Not `#[must_use]`: the several test call sites that only want the set
    /// mutated would each have to say so, and the announcement is a call-site
    /// concern (`template_fallback_line`'s shape) rather than something this
    /// type performs.
    pub fn mark(&self, session: &SessionId, cause: TaintCause) -> bool {
        // `or_insert` and not `insert`: the first cause wins, so a session
        // pinned permanently by a boundary read cannot be downgraded to a
        // liftable `unknown_shell` by a later opaque command.
        let mut tainted = self.tainted.lock().expect("taint mutex poisoned");
        let before = tainted.len();
        tainted.entry(session.clone()).or_insert(cause);
        tainted.len() != before
    }

    /// The same mark, on a path that must not panic (REQ-567 verify).
    ///
    /// The twin of
    /// [`SessionRegistry::try_commit_conversation`](crate::sessions::SessionRegistry::try_commit_conversation)
    /// and it exists for the same reason: the cancellation commit runs from
    /// `Drop`, a panic raised inside a drop that is itself running because of a
    /// panic aborts the whole daemon, and this pin is evaluated on that path
    /// ([`CarriedTurn::commit_now`](crate::carry::CarriedTurn)). A poisoned
    /// taint mutex must therefore not be an `expect`.
    ///
    /// [`PoisonError::into_inner`](std::sync::PoisonError::into_inner) is sound
    /// for *this* mutation specifically: the set is a plain `HashSet` of ids
    /// with no invariant spanning two operations, so a writer that panicked
    /// mid-insert left it consistent — and the fail-closed direction is to
    /// insert anyway. Refusing to pin because a lock was poisoned is precisely
    /// the failure this must not have.
    ///
    /// The explicit path keeps [`Self::mark`]'s `expect`: a poisoned set there
    /// is a bug to surface loudly, on a stack where surfacing it is safe.
    pub fn try_mark(&self, session: &SessionId, cause: TaintCause) -> bool {
        let mut tainted = match self.tainted.lock() {
            Ok(tainted) => tainted,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = tainted.len();
        tainted.entry(session.clone()).or_insert(cause);
        tainted.len() != before
    }

    /// Whether `session` is pinned to the local tier by a prior boundary/unknown
    /// exposure.
    #[must_use]
    pub fn is_tainted(&self, session: &SessionId) -> bool {
        self.cause(session).is_some()
    }

    /// The cause that pinned `session`, or `None` if it is not pinned.
    #[must_use]
    pub fn cause(&self, session: &SessionId) -> Option<TaintCause> {
        self.tainted
            .lock()
            .expect("taint mutex poisoned")
            .get(session)
            .copied()
    }
}

/// Per-session lift of the BR-13 restriction on **model-composed** web lookups
/// (REQ-563, architecture D-4).
///
/// A sibling of [`SessionTaint`] and deliberately not a field on it: taint is a
/// privacy fact the daemon establishes about a session, and this is a decision
/// the *user* made about that fact. Folding the second into the first would let
/// anything that can mark taint also unmark its consequence.
///
/// ## The setter is private, which is the whole of AC-12
///
/// [`Self::lift`] carries no `pub`, so it is reachable from this module and its
/// children and from nowhere else in the crate — in particular not from
/// `crate::harness::tools`, where a model's tool call lands. The single caller
/// is [`DaemonRuntime::web_override`], which is only reached from the
/// `web/override` client RPC. "The override is rejected when issued by the
/// model" is therefore not a runtime check that could be forgotten; a
/// model-issued override does not compile.
///
/// Session-scoped and never persisted (BR-13): a fresh session starts
/// restricted-on-taint again, because this lives in a process-lifetime set with
/// no writer to config.
#[derive(Debug, Default)]
pub struct WebTaintOverride {
    lifted: Mutex<HashSet<SessionId>>,
}

impl WebTaintOverride {
    /// An empty set — nothing lifted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lift the restriction for `session`, returning whether this call was the
    /// restricted→lifted transition.
    ///
    /// Idempotent, and the returned transition is what keeps the announcement
    /// honest the way [`SessionTaint::mark`]'s does: a second override of an
    /// already-lifted session is not a second lifting.
    ///
    /// **Intentionally not `pub`** — see the type docs.
    pub(super) fn lift(&self, session: &SessionId) -> bool {
        self.lifted
            .lock()
            .expect("web override mutex poisoned")
            .insert(session.clone())
    }

    /// Whether the user has lifted the restriction for `session`.
    #[must_use]
    pub fn is_lifted(&self, session: &SessionId) -> bool {
        self.lifted
            .lock()
            .expect("web override mutex poisoned")
            .contains(session)
    }
}

/// Per-session lift of a **liftable** taint pin — `/shell allow` (REQ-614 BR-5).
///
/// A sibling of [`SessionTaint`] and deliberately not a field on it, for
/// exactly the reason [`WebTaintOverride`] gives: taint is a privacy fact the
/// daemon establishes about a session, and this is a decision the *user* made
/// about that fact. Folding the second into the first would let anything that
/// can mark taint also unmark its consequence.
///
/// ## The setter is private, which is the whole of BR-5's last sentence
///
/// [`Self::lift`] carries no `pub`, so it is reachable from this module and its
/// children and from nowhere else in the crate — in particular not from
/// `crate::harness::tools`, where a model's tool call lands. The single caller
/// is the `shell/override` client RPC. "The model cannot lift its own pin" is
/// therefore not a runtime check somebody could forget; a model-issued lift
/// does not compile.
///
/// Session-scoped and never persisted: a fresh session starts pinned-on-taint
/// again.
#[derive(Debug, Default)]
pub struct ShellTaintOverride {
    lifted: Mutex<HashSet<SessionId>>,
}

impl ShellTaintOverride {
    /// An empty set — nothing lifted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lift the pin for `session`, returning whether this call was the
    /// pinned->lifted transition.
    ///
    /// Idempotent, and the returned transition is what keeps the ledger honest:
    /// a second `/shell allow` in a lifted session writes no row (BR-5).
    ///
    /// **Intentionally not `pub`** — see the type docs.
    ///
    /// The single production caller is `DaemonRuntime::shell_override`, the
    /// `shell/override` RPC handler.
    pub(super) fn lift(&self, session: &SessionId) -> bool {
        self.lifted
            .lock()
            .expect("shell override mutex poisoned")
            .insert(session.clone())
    }

    /// Whether the user has lifted this session's pin.
    #[must_use]
    pub fn is_lifted(&self, session: &SessionId) -> bool {
        self.lifted
            .lock()
            .expect("shell override mutex poisoned")
            .contains(session)
    }
}

/// The one question every route asks about taint: **does this session's pin
/// force the local tier right now?** (REQ-614 ADR-614-4)
///
/// Seven call sites used to read `SessionTaint::is_tainted` directly — the turn
/// path's `dispatch_route` and the six duty routes. Teaching each of them about
/// the lift would be seven chances to miss one, and a missed one is a session
/// that stays pinned after the user lifted it with nothing failing. So the
/// *predicate* changed instead: the lift is honored at all seven by
/// construction, which is what "where the rule is a property of a method rather
/// than of its callers, put it on the method" means here.
///
/// Read-only by construction — it holds two `Arc`s and exposes no setter, the
/// same inversion [`SessionTaintView`] uses for the lookup seam. Nothing that
/// can reach a `RoutePin` gains the ability to mark or to lift.
#[derive(Clone)]
pub struct RoutePin {
    taint: Arc<SessionTaint>,
    lifted: Arc<ShellTaintOverride>,
}

impl RoutePin {
    /// Compose the two handles into the one read.
    #[must_use]
    pub fn new(taint: Arc<SessionTaint>, lifted: Arc<ShellTaintOverride>) -> Self {
        Self { taint, lifted }
    }

    /// Whether this session's turns must be served locally.
    ///
    /// `false` only when the pin's cause is liftable **and** the user has
    /// lifted it. A `boundary_hit` pin ignores the override set entirely, which
    /// is BR-3's "no lift exists for this cause" expressed as code rather than
    /// as a check somewhere upstream.
    #[must_use]
    pub fn pins(&self, session: &SessionId) -> bool {
        match self.taint.cause(session) {
            None => false,
            Some(cause) => !(cause.liftable() && self.lifted.is_lifted(session)),
        }
    }

    /// The cause pinning `session`, whether or not it has been lifted — what
    /// `/doctor` reports and what a refusal names.
    #[must_use]
    pub fn cause(&self, session: &SessionId) -> Option<TaintCause> {
        self.taint.cause(session)
    }

    /// Whether the user has already lifted this session's pin.
    #[must_use]
    pub fn is_lifted(&self, session: &SessionId) -> bool {
        self.lifted.is_lifted(session)
    }
}

/// The lookup seam's read of the two session flags (REQ-563 BR-13).
///
/// The choke point takes a [`TaintView`] rather than these two handles so it
/// does not depend on the daemon runtime — the same inversion
/// [`PrivacyEventSink`](crate::egress::PrivacyEventSink) uses. This is the
/// production implementation, and it only ever *reads*: the lookup gate has no
/// path to `mark` or to `lift`.
pub struct SessionTaintView {
    pub(super) taint: Arc<SessionTaint>,
    pub(super) overridden: Arc<WebTaintOverride>,
}

impl TaintView for SessionTaintView {
    fn is_tainted(&self, session: &SessionId) -> bool {
        self.taint.is_tainted(session)
    }

    fn is_overridden(&self, session: &SessionId) -> bool {
        self.overridden.is_lifted(session)
    }
}

/// Writes one `web_lookups` row and publishes one `web_lookup` event per lookup
/// attempt (REQ-563 BR-7, D-7/D-8).
///
/// Both obligations behind one call, because "exactly one row and exactly one
/// event per attempt" is one invariant: two separately-installable hooks could
/// be wired independently and then disagree about how many lookups a session
/// performed, which is precisely the question the ledger exists to answer.
///
/// ## No `privacy_block`, deliberately
///
/// A web block is announced here as a `web_lookup` carrying
/// `blocked_redact`, and **not** as a `privacy_block`. That is not an omission:
/// `privacy_block` is the event [`TaintingPrivacySink`] turns into a session-wide
/// local-tier pin, and a query the daemon refused to send establishes nothing
/// about the context this session is holding. Taint semantics stay owned by that
/// sink's rules (REQ-544 C-2, REQ-562's cause gate); the lookup path observes
/// them and never writes them.
pub(super) struct WebLookupRecorder {
    pub(super) ledger: CostLedger,
    pub(super) events: Arc<EventBus>,
}

impl LookupRecorder for WebLookupRecorder {
    fn web_lookup(&self, session_id: &SessionId, record: &LookupRecord) {
        // `Some(0)`: every lookup this build performs is genuinely free, and a
        // measured zero is not the guess REQ-557 BR-9 forbids. A metered search
        // backend arrives as a price, not as a schema change (D-7).
        let row = WebLookupRow {
            session_id: session_id.to_string(),
            kind: record.kind,
            host: record.host.clone(),
            bytes_in: record.bytes_in,
            duration_ms: record.duration_ms,
            outcome: record.outcome,
            usd_micros: Some(0),
        };
        if let Err(err) = self.ledger.record_web_lookup(&row) {
            // The lookup already happened; a ledger that could not be written is
            // an accounting failure, not a reason to fail the turn (BR-9). The
            // line names the failure class and no part of the destination.
            eprintln!("teton: a web lookup could not be recorded in the cost ledger ({err})");
        }
        self.events.publish(
            Some(session_id.clone()),
            Event::WebLookup(WebLookup {
                kind: record.kind,
                host: record.host.clone(),
                outcome: record.outcome,
                bytes_in: record.bytes_in,
                // REQ-563 BR-14's honesty half: a `blocked_redact` whose cause
                // is `scan_unavailable` must not be reported to the user as
                // "the scan refused your text". The ledger row keeps the fixed
                // eight-value outcome; the event carries the finer reading.
                cause: record.cause,
            }),
        );
    }
}

/// The daemon's implementation of the harness web tool's egress seam
/// (REQ-563 D-2/D-5).
///
/// It exists so the tool depends on a **trait** rather than on this runtime:
/// the harness must not reach for the router, the config or the keychain, and a
/// test of the tool's gate order must be able to say "this lookup reached
/// nothing" without standing a daemon up. Everything privileged lives on this
/// side of the seam.
///
/// One field per thing [`DaemonRuntime::web_lookup_egress`] needs, snapshotted
/// for the turn the tools were built for — which is the same lifetime
/// `run_one_attempt` gives its own `config`, because the registry is rebuilt on
/// every prompt turn. The *credential* is not snapshotted: it is resolved
/// inside `web_lookup_egress` as the choke point is built, per lookup (ADR-007).
pub(super) struct RuntimeLookupSeam {
    pub(super) runtime: Arc<DaemonRuntime>,
    pub(super) router: Router,
    pub(super) config: Config,
    pub(super) events: Arc<EventBus>,
    pub(super) session_id: SessionId,
}

#[async_trait::async_trait]
impl WebLookupSeam for RuntimeLookupSeam {
    async fn lookup(
        &self,
        request: &LookupRequest,
        hop_allowed: &(dyn for<'h> Fn(&'h str) -> bool + Send + Sync),
    ) -> Result<LookupOutcome, SeamError> {
        // Built here and dropped at the end of this call. A choke point cached
        // for the daemon's life would be holding a search key resolved when it
        // was built, which is exactly the staleness ADR-007 removed.
        let egress = self
            .runtime
            .web_lookup_egress(&self.router, &self.config, &self.events, &self.session_id)
            .map_err(|err| SeamError::Unavailable(err.to_string()))?;
        let taint = self.runtime.web_taint_view();
        let mut ctx = LookupContext::new(self.session_id.clone(), &*taint, hop_allowed);
        // The endpoint, and only the endpoint. The key rides the transport,
        // bound to this origin, and this module never sees it (BR-7).
        if let Some(endpoint) = self.config.web.search_endpoint.as_deref() {
            ctx = ctx.with_search_endpoint(endpoint);
        }
        Ok(egress.lookup(request, &ctx).await)
    }

    fn record_without_egress(&self, record: &LookupRecord) {
        // The same recorder the choke point uses, so a cache hit and a wire
        // lookup land in one table through one writer — "how many lookups did
        // this session perform" has one answer (BR-7).
        self.runtime
            .lookup_recorder(&self.events)
            .web_lookup(&self.session_id, record);
    }
}

/// A [`PrivacyEventSink`] that publishes the block **and** taints the session it
/// happened in (REQ-544 C-2, extended to the duty path by REQ-561).
///
/// The turn path marks taint from its own `is_privacy_blocked()` arm, which
/// works because a refused turn comes back as a typed error the runtime handles.
/// A refused **duty** does not: the seam turns it into a failure sentence, the
/// call site degrades by its own means (a mechanical truncation, the tool's own
/// unrefined result, an unnamed session, a deterministic drop), and the turn
/// carries on — correctly. So the one thing that *knows* a boundary was crossed
/// is the choke point, and marking there is enforcing the rule where the
/// decision is made rather than at whichever caller happens to notice
/// (LESSON-484).
///
/// The gap it closes is not hypothetical but it is currently *masked*: the
/// content that got the duty refused is still in the turn's context, so
/// `context_taint_cause` taints the session when the turn ends. That is an
/// incidental cover — it depends on the refusing content still being in `ctx`
/// at the end of the turn, which compaction and truncation are both entitled to
/// change — and it is exactly the almost-true invariant a later change builds
/// on.
///
/// ## Not every block establishes anything about the content (REQ-562)
///
/// The marking is gated on the cause. A boundary block and a redaction finding
/// each mean *this content crossed a line*, which is exactly what REQ-544 C-2's
/// pin is for. A [`BlockCause::ScanUnavailable`] means the scanner was busy,
/// stalled, or not loaded — it says **nothing** about the payload, because
/// nothing looked at it. Pinning on it would let one 120-second engine stall
/// permanently route the rest of a session to the local tier, on the strength
/// of a fact nobody established. The payload itself is still refused, which is
/// BR-3's fail-closed posture; what does not follow is the session-wide
/// consequence.
/// ## The gate is a field, because it is not the same at every choke point
///
/// A block through the **MCP** choke point is answered by a different rule
/// ([`mcp_cause_taints_the_session`]), so the sink is handed the rule its choke
/// point uses rather than reaching for one. The two constructors below are the
/// whole of the difference, and they sit next to each other so a reader who
/// finds one is told the other exists.
pub(super) struct TaintingPrivacySink {
    pub(super) events: Arc<EventBus>,
    pub(super) taint: Arc<SessionTaint>,
    /// Which causes pin, for the choke point this sink was built for.
    pub(super) taints: CauseGate,
}

/// Which block causes pin their session — a rule a [`TaintingPrivacySink`] is
/// handed rather than one it chooses.
pub(super) type CauseGate = fn(&BlockCause) -> bool;

impl TaintingPrivacySink {
    /// The sink for a **turn or duty** send: [`cause_taints_the_session`], where
    /// a boundary block and a redaction block both pin.
    pub(super) fn for_turn_path(events: Arc<EventBus>, taint: Arc<SessionTaint>) -> Self {
        Self {
            events,
            taint,
            taints: cause_taints_the_session,
        }
    }

    /// The sink for the **MCP** choke point: [`mcp_cause_taints_the_session`],
    /// where a redaction block pins and a boundary block keeps REQ-544's
    /// fold-without-pinning posture (user decision, 2026-08-08).
    pub(super) fn for_mcp_path(events: Arc<EventBus>, taint: Arc<SessionTaint>) -> Self {
        Self {
            events,
            taint,
            taints: mcp_cause_taints_the_session,
        }
    }
}

impl crate::egress::PrivacyEventSink for TaintingPrivacySink {
    fn privacy_block(
        &self,
        session_id: Option<SessionId>,
        block: teton_protocol::events::PrivacyBlock,
    ) {
        if let Some(session_id) = &session_id {
            // One line per session, on the transition only — `mark` reports it.
            if (self.taints)(&block.cause) && self.taint.mark(session_id, cause_of(&block)) {
                eprintln!("{}", taint_pin_line(taint_cause_word(&block.cause)));
            }
        }
        self.events.privacy_block(session_id, block);
    }

    /// Forwarded verbatim to the bus.
    ///
    /// No taint decision to make: the fail-closed consequence of a refused
    /// provenance assertion is already taken where it happens (the call's
    /// provenance is marked unknown), and this sink's job — deciding whether a
    /// *block* pins the whole session — has no bearing on it. What would be
    /// wrong is inheriting the trait's default and dropping the event, because
    /// this wrapper is a delivery path to a client (LESSON-505).
    fn provenance_rejected(
        &self,
        session_id: Option<SessionId>,
        rejected: teton_protocol::events::ProvenanceRejected,
    ) {
        self.events.provenance_rejected(session_id, rejected);
    }
}

/// Whether a block at the choke point establishes that content crossed a
/// privacy line, and therefore pins the session local (REQ-544 C-2, REQ-562).
///
/// Two of the three do. `Boundary` is C-2's original case: the turn's content
/// came from a `local-only` source, so a later paraphrase of it must not leave
/// either. `Redaction` is the same shape one layer in — the scan *found*
/// something in the outbound payload, and the model that produced it can
/// restate it next turn.
///
/// `ScanUnavailable` does not, and the asymmetry is the point. It means no scan
/// happened: no local tier, an over-cap payload, an engine error, a deadline.
/// "The scanner was busy" establishes nothing whatever about the content, and a
/// taint is a **durable, session-wide** consequence — a transient stall would
/// permanently pin every remaining turn to the local tier and there is no way
/// for the user to undo it short of a new session. The payload is still
/// blocked; that is the fail-closed part, and it is per-payload.
///
/// ## This is the **turn and duty** path's rule; MCP has its own
///
/// [`mcp_cause_taints_the_session`] answers the same question for the MCP choke
/// point, and answers it differently for `Boundary`. Two functions rather than
/// one with a flag: the difference is a decision about two surfaces, taken by
/// two different REQs, and a parameter would present it as a caller's option.
pub(super) fn cause_taints_the_session(cause: &BlockCause) -> bool {
    match cause {
        BlockCause::Boundary | BlockCause::Redaction { .. } => true,
        BlockCause::ScanUnavailable => false,
    }
}

/// Whether a block at the **MCP** choke point pins the session (REQ-562; user
/// decision, 2026-08-08).
///
/// One of the three pins here where two do on the turn path
/// ([`cause_taints_the_session`]), and the divergence is per cause, for reasons
/// about the causes rather than about MCP:
///
/// - **`Redaction` pins**, exactly as on the turn path and for the same reason:
///   the model authored those tool arguments, so a finding in them is a secret
///   *the model is holding*, and it can restate it next turn through an
///   ordinary remote call that this tool error does nothing to constrain. What
///   the scan established is a fact about the content, not about the surface it
///   was heading out through, so the two paths' different disposal of a block
///   does not reach it.
/// - **`Boundary` does not**, which is REQ-544's posture for this surface, kept
///   rather than re-derived. This is *not* a claim that a boundary block
///   establishes less here than on the turn path — it establishes exactly the
///   same thing. It is that REQ-544 chose to fold an MCP boundary refusal back
///   into the loop as an ordinary in-context tool error, and re-deciding that
///   inside a redaction change would silently change an earlier REQ's rule on a
///   surface this one did not set out to touch. Whether an MCP boundary block
///   should pin is REQ-544's question to reopen.
/// - **`ScanUnavailable` never pins**, on either path and for the identical
///   reason: nothing looked at the payload, so nothing about it was established
///   (see [`cause_taints_the_session`]). The payload is still refused; that
///   part is per-payload and fail-closed.
///
/// The asymmetry is therefore intended, and
/// `the_mcp_gate_pins_redaction_and_diverges_from_the_turn_path_on_boundary`
/// pins it *as* an asymmetry — so a later "make these consistent" edit turns a
/// test red instead of quietly re-deciding REQ-544.
pub(super) fn mcp_cause_taints_the_session(cause: &BlockCause) -> bool {
    match cause {
        BlockCause::Redaction { .. } => true,
        BlockCause::Boundary | BlockCause::ScanUnavailable => false,
    }
}

/// The same rule as [`cause_taints_the_session`], in the vocabulary the turn
/// path has.
///
/// The turn path never sees a [`BlockCause`] — the cause reaches it as a
/// [`BlockDetail`] through the `teton-providers` seam — so the rule is stated
/// twice in two type systems and `the_two_taint_gates_agree_cause_for_cause`
/// pins them to each other. One spelling would mean a `BlockCause` dependency
/// in `teton-providers`, which is the edge that crate exists without.
///
/// [`mcp_cause_taints_the_session`] is a **third** function and deliberately
/// *not* a third spelling of this rule: it answers the same question for a
/// different choke point and gives a different answer for `Boundary`. It is
/// therefore outside the agreement these two are held to.
pub(super) fn taints_the_session(detail: BlockDetail) -> bool {
    match detail {
        BlockDetail::Boundary | BlockDetail::Redaction => true,
        BlockDetail::ScanUnavailable => false,
    }
}

/// The [`TaintCause`] a `privacy_block` records (REQ-614).
///
/// Derived from the block's **path**, not from its `cause`, because that is
/// where the distinction this REQ turns on survives: `BlockCause::Boundary`
/// covers a real boundary path, an unknown-provenance sentinel and a
/// boundary-touch sentinel alike, and only the first and third are permanent.
/// A pin whose cause came from `BlockCause` alone would make `~/.ssh/config`
/// liftable.
fn cause_of(block: &teton_protocol::events::PrivacyBlock) -> TaintCause {
    use crate::egress::provenance::{MALFORMED_PROVENANCE_PATH, UNKNOWN_PROVENANCE_PATH};
    match &block.cause {
        BlockCause::Redaction { .. } => TaintCause::RedactionFinding,
        // Unreachable in production — `cause_taints_the_session` answers `false`
        // for it — and mapped rather than left to a catch-all so the map is
        // total. Permanent, which is the fail-closed direction for a cause
        // nothing is supposed to pin on.
        BlockCause::ScanUnavailable => TaintCause::BoundaryHit,
        BlockCause::Boundary => match block.path.as_str() {
            UNKNOWN_PROVENANCE_PATH => TaintCause::UnknownShell,
            MALFORMED_PROVENANCE_PATH => TaintCause::MalformedProvenance,
            // A real repo path, or `BOUNDARY_TOUCH_PATH`. Both mean a
            // `local-only` file was named, and neither lifts.
            _ => TaintCause::BoundaryHit,
        },
    }
}

/// A `local-only` boundary was crossed — REQ-544 C-2's original cause.
pub(super) const TAINT_BY_BOUNDARY: &str = "a `local-only` privacy boundary was crossed";
/// The redaction scan found something in an outbound payload (REQ-562).
pub(super) const TAINT_BY_REDACTION: &str =
    "the redaction scan found sensitive content in an outbound payload";
/// This turn's assembled context carried boundary or unknown-provenance content.
pub(crate) const TAINT_BY_CONTEXT: &str = "this turn read boundary or unknown-provenance content";
/// Unreachable: [`cause_taints_the_session`] and [`mcp_cause_taints_the_session`]
/// both answer `false` for `ScanUnavailable`, so no announcement is ever minted
/// for it. Present so the maps below are total, and worded so that a future
/// change which *does* pin on it produces a puzzling line rather than a panic in
/// the daemon.
pub(super) const TAINT_BY_UNSTATED_CAUSE: &str = "a blocked outbound payload";

pub(crate) fn taint_pin_line(cause: &'static str) -> String {
    format!(
        "tetond: privacy — this session is pinned to the local tier for the rest of its life \
         ({cause}); remote providers will not be used in it again."
    )
}

/// The class word a [`BlockCause`] announces its pin with.
pub(super) fn taint_cause_word(cause: &BlockCause) -> &'static str {
    match cause {
        BlockCause::Boundary => TAINT_BY_BOUNDARY,
        BlockCause::Redaction { .. } => TAINT_BY_REDACTION,
        BlockCause::ScanUnavailable => TAINT_BY_UNSTATED_CAUSE,
    }
}

/// The same word, from the turn path's [`BlockDetail`] vocabulary — the second
/// spelling [`taints_the_session`] already exists in, for the same reason.
pub(super) fn taint_detail_word(detail: BlockDetail) -> &'static str {
    match detail {
        BlockDetail::Boundary => TAINT_BY_BOUNDARY,
        BlockDetail::Redaction => TAINT_BY_REDACTION,
        BlockDetail::ScanUnavailable => TAINT_BY_UNSTATED_CAUSE,
    }
}

/// REQ-563 TASK-075 — the daemon half of the lookup seam: which switch
/// installs the search scanner, who may lift the taint restriction, and what
/// one lookup leaves behind.
#[cfg(test)]
mod web_lookup_seam {
    use super::*;
    use crate::classify::test_support::CountingEngine;
    use crate::egress::{
        Authorship, LookupContext, LookupDetail, LookupRecord, LookupRequest, TaintView,
    };
    use crate::harness::PermissionConfig;
    use crate::harness::{PermissionDecision, PermissionPolicy};
    use crate::runtime::testsupport::scratch_dir;
    use teton_core::config::WebTier;
    use teton_protocol::events::{WebLookupKind, WebLookupOutcome, WebTier as WireWebTier};
    use teton_protocol::methods::WebOverrideParams;

    fn runtime_with(web_tier: WebTier, redact: bool) -> DaemonRuntime {
        let runtime = DaemonRuntime::minimal();
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.web.tier = web_tier;
            config.web.search_endpoint = Some("https://search.example/api".to_owned());
            config.privacy.redact = redact;
        }
        runtime
    }

    fn router_for(runtime: &DaemonRuntime) -> Router {
        let config = runtime.config.lock().expect("config mutex").clone();
        build_router(&config, runtime.local_tier_available(), &BTreeMap::new())
    }

    /// A keychain that answers exactly one `(service, account)` pair.
    ///
    /// The injection point `keychain.rs` documents (`SecretResolver::with_backend`
    /// — "tests inject a fake that returns a canned secret so CI never touches
    /// the real store"), reused here so a `search_key_ref` can be *resolvable*
    /// in a test. Without a resolver that answers, `search_auth` returns `None`
    /// for the ordinary reason and the wiring below could not be observed at
    /// all.
    struct FakeKeychain {
        service: &'static str,
        account: &'static str,
        secret: &'static str,
    }

    impl crate::keychain::KeychainBackend for FakeKeychain {
        fn get(
            &self,
            service: &str,
            account: &str,
        ) -> Result<String, crate::keychain::BackendError> {
            if service == self.service && account == self.account {
                Ok(self.secret.to_owned())
            } else {
                Err(crate::keychain::BackendError::NotFound)
            }
        }
    }

    /// A loopback HTTP server that records the head of every request it is
    /// sent and answers each with `200 ok`.
    ///
    /// **Real sockets, deliberately.** The credential under test is attached
    /// by the production [`HttpTransport`] that
    /// [`DaemonRuntime::web_lookup_egress`] builds and does not hand back, so
    /// a fake `Transport` cannot see it: substituting one would replace the
    /// very object whose header behaviour is the claim. The wire is the only
    /// place `Authorization: Bearer …` exists, so the wire is where it is
    /// read. Nothing leaves the machine — both ends are `127.0.0.1`.
    #[derive(Clone)]
    struct CaptureServer {
        port: u16,
        heads: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CaptureServer {
        async fn bind() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback port");
            let port = listener.local_addr().expect("local addr").port();
            let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&heads);
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 512];
                    loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&chunk[..n]),
                        }
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    sink.lock()
                        .expect("capture mutex")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\
                              Connection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = socket.flush().await;
                }
            });
            Self { port, heads }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn requests(&self) -> Vec<String> {
            self.heads.lock().expect("capture mutex").clone()
        }

        /// Whether request `i` carried an `Authorization` header at all.
        /// Case-folded, because a header name is case-insensitive and the
        /// question is about the credential, not about its spelling.
        fn carried_auth(&self, i: usize) -> bool {
            self.requests()[i]
                .to_ascii_lowercase()
                .contains("\r\nauthorization:")
        }
    }

    /// A runtime configured to search `endpoint`, with `key_ref` resolvable
    /// through a fake keychain and an engine loaded so the BR-14 search scan
    /// can actually run.
    fn searching_runtime(
        endpoint: &str,
        key_ref: Option<&str>,
        engine: &CountingEngine,
    ) -> DaemonRuntime {
        let mut runtime = DaemonRuntime::minimal();
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.web.tier = WebTier::Search;
            config.web.search_endpoint = Some(endpoint.to_owned());
            config.web.search_key_ref = key_ref.map(ToOwned::to_owned);
        }
        runtime
            .engine
            .install("counting".to_owned(), engine.handle());
        runtime.local_available.store(true, Ordering::SeqCst);
        runtime.secret_resolver =
            crate::keychain::SecretResolver::with_backend(Box::new(FakeKeychain {
                service: "teton",
                account: "search",
                secret: SEARCH_SECRET,
            }));
        runtime
    }

    /// The credential the fake keychain answers with. A value no other
    /// fixture in this file uses, so finding it on a wire is unambiguous.
    const SEARCH_SECRET: &str = "srch-secret-4f1c9";

    /// A host check that permits every redirect hop — the *permissive*
    /// setting, so nothing below can be an accident of a restrictive one.
    fn allow_any_host(_host: &str) -> bool {
        true
    }

    // -- the search gate's switch (BR-14, D-6) -------------------------

    /// **`⇔ tier == search`, and `[privacy] redact` is not part of it.**
    ///
    /// Both halves matter and the sweep asserts both at once. A gate keyed
    /// on `redact` as well would let a user who declined provider-payload
    /// scanning send search queries unscanned, which is the coupling BR-14
    /// exists to prevent; a gate installed below `search` would scan a
    /// capability that cannot search.
    #[test]
    fn the_search_gate_is_installed_exactly_when_the_tier_is_search() {
        for tier in WebTier::ALL {
            for redact in [false, true] {
                let runtime = runtime_with(tier, redact);
                let config = runtime.config.lock().expect("config mutex").clone();
                let router = router_for(&runtime);
                let events = Arc::new(EventBus::new());
                let gate =
                    runtime.search_redaction_gate(&router, &config, &events, &SessionId::from("s"));
                assert_eq!(
                    gate.is_some(),
                    tier == WebTier::Search,
                    "tier {tier:?} with redact={redact} installed the wrong thing"
                );
            }
        }
    }

    /// **The fetch-parity gate is `⇔ [privacy] redact`, whatever the tier.**
    ///
    /// BR-2 promises a fetch "the same treatment a provider payload gets"
    /// and BR-13 says a user-pasted URL is "still redact-scanned" — one
    /// sentence, and its switch is the provider switch. A **different**
    /// switch from the search gate above, so the sweep runs both axes: a
    /// gate that keyed on the tier would leave a `fetch_any_url` daemon
    /// sending URLs unscanned with `redact = true`, and one that keyed on
    /// the tier for search would let `redact = false` turn off the scan
    /// BR-14 makes the search tier conditional on.
    #[test]
    fn the_fetch_parity_gate_is_installed_exactly_when_redact_is_on() {
        for tier in WebTier::ALL {
            for redact in [false, true] {
                let runtime = runtime_with(tier, redact);
                let config = runtime.config.lock().expect("config mutex").clone();
                let router = router_for(&runtime);
                let events = Arc::new(EventBus::new());
                let egress = runtime
                    .web_lookup_egress(&router, &config, &events, &SessionId::from("s"))
                    .expect("the lookup choke point must build");
                assert_eq!(
                    egress.fetch_redaction_installed(),
                    redact,
                    "tier {tier:?} with redact={redact}: the fetch gate follows \
                     `[privacy] redact` and nothing else"
                );
                // …and the two slots really are two: the search gate on the
                // same choke point still follows the tier.
                let (_, search_gate, _) = egress.installed();
                assert_eq!(search_gate, tier == WebTier::Search);
            }
        }
    }

    /// The provider gate still answers to its own switch — otherwise the
    /// test above would pass on an implementation that had merged the two.
    #[test]
    fn the_provider_gate_still_answers_to_the_privacy_switch() {
        for redact in [false, true] {
            let runtime = runtime_with(WebTier::Search, redact);
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = router_for(&runtime);
            let events = Arc::new(EventBus::new());
            let gate = runtime.redaction_gate(&router, &config, &events, &SessionId::from("s"));
            assert_eq!(gate.is_some(), redact);
        }
    }

    /// **The search gate is REQ-562's composite scanner, not a new one.**
    ///
    /// This is what makes LESSON-491 inherited rather than re-derived: the
    /// caps that matter — the total input cap, the chunk ceiling, and above
    /// all the *rendered-prompt* budget — live in `harness::redact::scan`,
    /// and the search gate is worth nothing if it does not go through it.
    ///
    /// The observable that pins it, on a runtime with no engine, is that a
    /// scan comes back `Unavailable` with `scanned() == false`: that verdict
    /// is minted by `harness::redact::scan`'s unresolved-route arm, so a
    /// gate returning it is a gate that ran that function. A hand-rolled
    /// scanner with its own caps would have to reproduce the failure mode to
    /// pass, and if it did it would be the same code.
    ///
    /// It pins the practical half of BR-14 too: on a machine with no local
    /// tier, every search query blocks (LESSON-492), which is why the search
    /// tier is not offered there at all.
    #[tokio::test]
    async fn the_search_gate_is_the_same_scanner_the_provider_gate_uses() {
        let runtime = runtime_with(WebTier::Search, true);
        let config = runtime.config.lock().expect("config mutex").clone();
        let router = router_for(&runtime);
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("s");

        let search = runtime
            .search_redaction_gate(&router, &config, &events, &session)
            .expect("tier is search");
        let provider = runtime
            .redaction_gate(&router, &config, &events, &session)
            .expect("redact is on");

        let by_search = search.scan("a query").await;
        let by_provider = provider.scan("a query").await;
        assert_eq!(by_search.outcome(), by_provider.outcome());
        assert_eq!(
            by_search.outcome(),
            crate::egress::redact::Outcome::Unavailable,
            "no engine is loaded, so the shared scan fails closed"
        );
        assert!(!by_search.scanned());
    }

    // -- the override (BR-13, AC-12) ------------------------------------

    #[test]
    fn the_override_lifts_the_restriction_the_lookup_gate_reads() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let mut sub = events.subscribe(16);
        let session = SessionId::from("sess-under-test");
        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);

        // Non-vacuity: the gate really is restricted before the RPC.
        let view = runtime.web_taint_view();
        assert!(view.is_tainted(&session));
        assert!(!view.is_overridden(&session));

        let result = runtime.web_override(
            &WebOverrideParams {
                session_id: session.clone(),
            },
            &events,
        );

        assert!(result.was_restricted);
        assert_eq!(
            result.tiers_restored,
            vec![
                WireWebTier::FetchUserUrl,
                WireWebTier::FetchAnyUrl,
                WireWebTier::Search
            ],
            "ascending, and `off` is never a restored tier"
        );
        assert!(
            runtime.web_taint_view().is_overridden(&session),
            "the flag the choke point reads is the flag the RPC set"
        );

        let envelope = sub.try_recv().expect("one web_taint_overridden");
        assert_eq!(envelope.session_id, Some(session));
        match envelope.event {
            Event::WebTaintOverridden(e) => assert_eq!(e.tiers_restored.len(), 3),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "exactly one");
    }

    /// The ceiling bounds what is restored: an override never grants a tier
    /// the machine was not configured for (BR-13's "grants no new tiers").
    #[test]
    fn the_override_restores_nothing_above_the_configured_ceiling() {
        let runtime = runtime_with(WebTier::FetchUserUrl, false);
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("sess-under-test");
        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);

        let result = runtime.web_override(
            &WebOverrideParams {
                session_id: session,
            },
            &events,
        );
        assert_eq!(result.tiers_restored, vec![WireWebTier::FetchUserUrl]);
    }

    /// A session that was never restricted gets an honest "nothing was" —
    /// not a confirmation of a lift that did not happen.
    #[test]
    fn overriding_an_unrestricted_session_confirms_nothing_and_announces_nothing() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let mut sub = events.subscribe(16);

        let result = runtime.web_override(
            &WebOverrideParams {
                session_id: SessionId::from("clean"),
            },
            &events,
        );
        assert!(!result.was_restricted);
        assert!(result.tiers_restored.is_empty());
        assert!(sub.try_recv().is_none());
    }

    /// **…and it does not pre-arm the override either.**
    ///
    /// The lift used to run unconditionally: the client was truthfully told
    /// "nothing was restricted", the flag was set anyway, and a boundary
    /// read *later in the same session* then found the restriction already
    /// lifted — so BR-13 never engaged for a session the user had only ever
    /// asked a question about. A user-only decision has to be a decision
    /// about something that exists.
    #[test]
    fn overriding_an_unrestricted_session_does_not_prearm_a_later_restriction() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("clean");

        let result = runtime.web_override(
            &WebOverrideParams {
                session_id: session.clone(),
            },
            &events,
        );
        assert!(!result.was_restricted);
        assert!(
            !runtime.web_taint_view().is_overridden(&session),
            "an override of nothing armed the flag the choke point reads"
        );

        // The restriction that arrives afterwards must actually restrict.
        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);
        let view = runtime.web_taint_view();
        assert!(view.is_tainted(&session));
        assert!(
            !view.is_overridden(&session),
            "BR-13 never engaged: the earlier no-op `/web allow` had disarmed it"
        );
    }

    /// BR-13's ledger half: a real lift is one append-only `web_overrides`
    /// row, naming the session and the tiers the user was told about.
    #[test]
    fn a_lift_writes_one_web_override_row_and_a_no_op_writes_none() {
        let runtime = runtime_with(WebTier::FetchAnyUrl, false);
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("sess-under-test");

        // A session that was never restricted lifts nothing and records
        // nothing — the row is evidence of a change, not of a keystroke.
        runtime.web_override(
            &WebOverrideParams {
                session_id: SessionId::from("clean"),
            },
            &events,
        );
        assert!(runtime.ledger.all_web_overrides().expect("read").is_empty());

        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);
        runtime.web_override(
            &WebOverrideParams {
                session_id: session.clone(),
            },
            &events,
        );
        let rows = runtime.ledger.all_web_overrides().expect("read");
        assert_eq!(rows.len(), 1, "exactly one row per lift");
        assert_eq!(rows[0].0, session.to_string());
        assert_eq!(
            rows[0].1,
            vec!["fetch_user_url".to_owned(), "fetch_any_url".to_owned()],
            "the row names the tiers the event named, in the config spelling"
        );

        // A second override removed nothing, so it records nothing — the
        // same rule the event follows.
        runtime.web_override(
            &WebOverrideParams {
                session_id: session,
            },
            &events,
        );
        assert_eq!(
            runtime.ledger.all_web_overrides().expect("read").len(),
            1,
            "a re-override is not a second lifting"
        );
    }

    #[test]
    fn a_second_override_is_acknowledged_without_announcing_a_second_lifting() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let mut sub = events.subscribe(16);
        let session = SessionId::from("sess-under-test");
        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);
        let params = WebOverrideParams {
            session_id: session,
        };

        let first = runtime.web_override(&params, &events);
        let second = runtime.web_override(&params, &events);
        assert!(first.was_restricted && second.was_restricted);
        assert!(sub.try_recv().is_some());
        assert!(
            sub.try_recv().is_none(),
            "a re-override is not a second lifting"
        );
    }

    /// **The override is unreachable from tool dispatch — by construction.**
    ///
    /// `WebTaintOverride::lift` carries no `pub`, so nothing outside this
    /// module tree can call it, and this reads the daemon's own source to
    /// pin the second half: there is exactly one call site, and it is the
    /// `web/override` RPC handler. The source scan follows
    /// [`crate::call_sites`]'s precedent — a marker nobody derives is a
    /// marker that rots — and it fails in both directions: add a second
    /// caller and it fires, delete the one there is and it fires too.
    ///
    /// **Scans the whole `runtime/` directory, not one file (REQ-599).**
    /// Before the split this was `include_str!("runtime.rs")`. A file-scoped
    /// scan would have kept passing while the call site moved to a sibling
    /// module — reporting zero, which this test reads as "the RPC handler no
    /// longer sets it" — or worse, missed a *second* call site added in a
    /// module the scan cannot see. A sweep's failure mode is seeing less
    /// (LESSON-585), and a decomposition is exactly the event that makes a
    /// single-file corpus stop being the corpus.
    #[test]
    fn the_taint_override_flag_has_exactly_one_setter_call_site() {
        // The needle is assembled at run time from fragments, so this
        // function's own source does not contain the string it searches for
        // — otherwise the scan finds itself and the test can only ever
        // measure its own text (LESSON-589).
        let needle = format!("self.web_{}.{}(", "override", "lift");

        // Recursive: `runtime/` is a module *tree*, and the first
        // `runtime/foo/mod.rs` would leave a flat scan's corpus silently
        // (REQ-602 BR-4, LESSON-594). `call_sites::scan::rust_files` is the
        // canonical walker — it also tolerates a directory vanishing
        // mid-walk (BUG-159) while keeping every other error loud.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime");
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        crate::call_sites::scan::rust_files(&dir, &mut paths);
        paths.sort();
        let sources: Vec<String> = paths
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("a runtime source is readable"))
            .collect();

        assert!(
            !sources.is_empty(),
            "vacuity floor: the scan found no sources under {}, so it could only pass",
            dir.display()
        );
        let joined = sources.join("\n");
        let lines: Vec<&str> = joined.lines().collect();

        let sites: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(&needle))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "the taint-override setter must have exactly ONE call site. A second \
             one is a second channel to a user-only decision (AC-12); zero means \
             the RPC handler no longer sets it."
        );

        // …and that one call site is the client-RPC handler, not some other
        // function that happens to hold a runtime.
        let enclosing = lines[..sites[0]]
            .iter()
            .rev()
            .map(|line| line.trim_start())
            .find(|line| line.starts_with("fn ") || line.starts_with("pub fn "))
            .expect("the call site is inside some function");
        assert!(
            enclosing.starts_with("pub fn web_override("),
            "the only setter call site must be the `web/override` RPC handler, \
             but it is in `{enclosing}`"
        );
    }

    // -- the recorder (BR-7, D-7/D-8) -----------------------------------

    #[test]
    fn one_lookup_writes_one_row_and_publishes_one_event_naming_only_the_host() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let mut sub = events.subscribe(16);
        let session = SessionId::from("sess-under-test");
        let recorder = runtime.lookup_recorder(&events);

        recorder.web_lookup(
            &session,
            &LookupRecord {
                kind: WebLookupKind::Search,
                host: "search.example".to_owned(),
                outcome: WebLookupOutcome::Completed,
                bytes_in: 4_096,
                duration_ms: 12,
                cause: None,
            },
        );

        let rows = runtime.ledger.all_web_lookups().expect("read the ledger");
        assert_eq!(rows.len(), 1, "exactly one row");
        assert_eq!(rows[0].host, "search.example");
        assert_eq!(rows[0].outcome, WebLookupOutcome::Completed);
        assert_eq!(rows[0].bytes_in, 4_096);
        assert_eq!(
            rows[0].usd_micros,
            Some(0),
            "free is a measured zero here, never an unpriced guess"
        );

        let envelope = sub.try_recv().expect("exactly one web_lookup");
        assert_eq!(envelope.session_id, Some(session));
        match envelope.event {
            Event::WebLookup(lookup) => {
                assert_eq!(lookup.host, "search.example");
                assert_eq!(lookup.outcome, WebLookupOutcome::Completed);
                assert_eq!(lookup.bytes_in, 4_096);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "and exactly one event");
    }

    /// Every refusal is recorded too (BR-7: the free ones as well), and the
    /// row count is the attempt count.
    #[test]
    fn every_outcome_is_recorded_including_the_ones_that_sent_nothing() {
        let runtime = runtime_with(WebTier::Search, false);
        let events = Arc::new(EventBus::new());
        let recorder = runtime.lookup_recorder(&events);
        let session = SessionId::from("sess-under-test");

        for outcome in WebLookupOutcome::ALL {
            recorder.web_lookup(
                &session,
                &LookupRecord {
                    kind: WebLookupKind::Fetch,
                    host: "docs.rs".to_owned(),
                    outcome,
                    bytes_in: 0,
                    duration_ms: 1,
                    cause: None,
                },
            );
        }

        let rows = runtime.ledger.all_web_lookups().expect("read the ledger");
        assert_eq!(rows.len(), WebLookupOutcome::ALL.len());
        for (row, outcome) in rows.iter().zip(WebLookupOutcome::ALL) {
            assert_eq!(row.outcome, outcome);
        }
    }

    // -- assembly --------------------------------------------------------

    /// The lookup choke point builds, carries a recorder, and carries the
    /// search gate exactly when the tier says so.
    #[test]
    fn the_lookup_choke_point_carries_the_recorder_and_the_tier_s_gate() {
        for tier in WebTier::ALL {
            let runtime = runtime_with(tier, false);
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = router_for(&runtime);
            let events = Arc::new(EventBus::new());
            let egress = runtime
                .web_lookup_egress(&router, &config, &events, &SessionId::from("s"))
                .expect("the lookup choke point must build");
            let (provider_gate, search_gate, recorder) = egress.installed();
            assert!(recorder, "every lookup has to be accountable (BR-7)");
            assert_eq!(search_gate, tier == WebTier::Search);
            assert!(
                !provider_gate,
                "the provider gate has no business on the lookup path"
            );
        }
    }

    // -- TASK-077: enable_permanent's write, and web/refresh -------------

    /// A runtime with a real config file and state directory — what the two
    /// durable web paths need and `minimal()` deliberately has not got.
    fn runtime_on_disk(tag: &str) -> (DaemonRuntime, PathBuf, PathBuf) {
        let dir = scratch_dir(tag);
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "").expect("seed an empty config");
        let mut runtime = DaemonRuntime::minimal();
        runtime.config_path = Some(config_path.clone());
        runtime.data_dir = dir.clone();
        (runtime, config_path, dir)
    }

    /// AC-2's persistence half, asserted against the **file**: a decision
    /// that only reached memory would not survive the restart the criterion
    /// is about, so the check is that a fresh load of the written bytes
    /// carries the tier.
    #[test]
    fn enable_permanent_writes_a_tier_a_restart_reads_back() {
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl] {
            let (runtime, config_path, _dir) = runtime_on_disk("persist-tier");
            assert_eq!(
                runtime.config.lock().expect("config mutex").web.tier,
                WebTier::Off,
                "non-vacuity: the ceiling really was off first (BR-1)"
            );

            runtime.persist_web_tier(tier).expect("the write must land");

            // The restart, simulated by the only thing a restart does with
            // this file: load it.
            let reloaded = load_config(Some(&config_path)).expect("the written config loads");
            assert_eq!(
                reloaded.web.tier, tier,
                "`[web] tier` did not survive a reload"
            );
            // And the live config agrees, so this turn is not running under
            // a ceiling different from the one on disk.
            assert_eq!(runtime.config.lock().expect("config mutex").web.tier, tier);
        }
    }

    /// A grant never lowers a ceiling. Consenting to fetch a page on a
    /// machine configured for `search` must leave `search` alone.
    #[test]
    fn persisting_a_lower_tier_never_demotes_the_configured_ceiling() {
        let (runtime, config_path, _dir) = runtime_on_disk("no-demote");
        // The ceiling is a *configured* one, so it is on disk as well as in
        // memory — the state a real start produces. Since REQ-574 a write
        // edits the document rather than replacing it, so a value that only
        // ever existed in memory is drift the seam deliberately leaves out
        // of the delta (BR-5/ADR-1); seeding the file is what makes this
        // test about the no-demote rule rather than about that.
        std::fs::write(
            &config_path,
            "[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n",
        )
        .expect("seed the configured ceiling");
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.web.tier = WebTier::Search;
            config.web.search_endpoint = Some("https://search.example/api".to_owned());
        }
        runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect("already durable at a higher tier");
        assert_eq!(
            runtime.config.lock().expect("config mutex").web.tier,
            WebTier::Search
        );
        // The *tier* axis is untouched. The write itself is not a no-op —
        // the answer's durable form is the consent list, which is the thing
        // the user was actually promised — but nothing here demotes.
        let reloaded = load_config(Some(&config_path)).expect("loads");
        assert_eq!(reloaded.web.tier, WebTier::Search);
        assert_eq!(
            reloaded.web.permission_allow,
            vec![WebTier::FetchUserUrl],
            "the answer was about `fetch_user_url` and nothing else"
        );
    }

    /// **`enable_permanent` writes something a restart can act on.**
    ///
    /// The bug this pins: the ceiling is checked *before* any prompt is
    /// raised, so every lookup that reaches one is already at or below the
    /// configured tier — which made the raise-only tier write a guaranteed
    /// no-op, while the decision was still reported as `Persistent` and the
    /// CLI still said "written to your config". The durable form of the
    /// answer is `[web] permission_allow`, and a daemon that reloads this
    /// file must not prompt again *about that tier*.
    #[test]
    fn enable_permanent_writes_a_permission_a_restart_reads_back() {
        let (runtime, config_path, _dir) = runtime_on_disk("persist-permission");
        {
            // The realistic shape: the ceiling is already where the lookup
            // needs it, so the tier raise has nothing to do. It is a
            // *configured* ceiling, so it is on disk as well as in memory —
            // the state a real start produces. Since REQ-574 a write edits
            // the document rather than replacing it, so a ceiling that only
            // ever existed in memory would make this test about drift
            // instead of about the permission it is named for.
            std::fs::write(&config_path, "[web]\ntier = \"fetch_any_url\"\n")
                .expect("seed the configured ceiling");
            let mut config = runtime.config.lock().expect("config mutex");
            config.web.tier = WebTier::FetchAnyUrl;
        }
        assert!(
            runtime
                .config
                .lock()
                .expect("config mutex")
                .web
                .permission_allow
                .is_empty(),
            "non-vacuity: the default really is ask-about-everything (BR-4)"
        );

        runtime
            .persist_web_tier(WebTier::FetchAnyUrl)
            .expect("the write must land");

        let reloaded = load_config(Some(&config_path)).expect("the written config loads");
        assert_eq!(
            reloaded.web.permission_allow,
            vec![WebTier::FetchAnyUrl],
            "the one thing `enable_permanent` durably changes did not reach the file"
        );

        // And a gate built from the reloaded config does not ask: this is
        // the restart, expressed as the thing a restart produces.
        let mut permissions = PermissionConfig::with_default(PermissionPolicy::Ask);
        permissions.apply_web_permission(&reloaded.web.permission_allow);
        assert_eq!(
            permissions.policy_for(crate::harness::tools::web::PERMISSION_KEY_FETCH_ANY_URL),
            PermissionPolicy::Allow,
            "the consented tier would still prompt after the user enabled it permanently"
        );
    }

    /// **One answer un-asks one tier, across a restart** (REQ-563 BR-3).
    ///
    /// The durable half of BR-3's breadth rule, and the reason the config key
    /// is a per-tier list rather than a two-valued switch. Answering "enable
    /// permanently" at a prompt about a URL *the user pasted* used to write
    /// `permission = "allow"`, which the daemon fanned onto all three consent
    /// keys — so one answer about the narrowest capability permanently
    /// stopped the prompts for URLs the **model** composes and for searches
    /// too, on every future session, from a file the user never re-read.
    ///
    /// The restart is simulated by the only thing a restart does with this
    /// file: load it, and build a gate from what it says.
    #[test]
    fn enable_permanent_at_one_tier_leaves_the_other_two_asking_after_a_restart() {
        let (runtime, config_path, _dir) = runtime_on_disk("per-tier-consent");
        {
            // The ceiling is at the top, so nothing here is refused by tier —
            // whatever is still asked is asked because consent says so. On
            // disk as well as in memory, for the reason
            // `persisting_a_lower_tier_never_demotes_the_configured_ceiling`
            // gives: since REQ-574 a memory-only ceiling is drift, and this
            // test is about consent rather than about drift.
            std::fs::write(
                &config_path,
                "[web]\ntier = \"search\"\nsearch_endpoint = \"https://search.example/api\"\n",
            )
            .expect("seed the configured ceiling");
            let mut config = runtime.config.lock().expect("config mutex");
            config.web.tier = WebTier::Search;
            config.web.search_endpoint = Some("https://search.example/api".to_owned());
        }

        // The answer: given at a `fetch_user_url` prompt, about a URL the
        // user pasted.
        runtime
            .persist_web_tier(WebTier::FetchUserUrl)
            .expect("the write must land");

        let reloaded = load_config(Some(&config_path)).expect("the written config loads");
        assert_eq!(reloaded.web.permission_allow, vec![WebTier::FetchUserUrl]);

        let mut permissions = PermissionConfig::with_default(PermissionPolicy::Ask);
        permissions.apply_web_permission(&reloaded.web.permission_allow);
        assert_eq!(
            permissions.policy_for(crate::harness::tools::web::PERMISSION_KEY_FETCH_USER_URL),
            PermissionPolicy::Allow,
            "the tier the user actually answered for still prompts"
        );
        for unanswered in [
            crate::harness::tools::web::PERMISSION_KEY_FETCH_ANY_URL,
            crate::harness::tools::web::PERMISSION_KEY_SEARCH,
        ] {
            assert_eq!(
                permissions.policy_for(unanswered),
                PermissionPolicy::Ask,
                "`{unanswered}` was permanently un-asked by an answer about a different \
                 capability"
            );
        }

        // A second answer, at a different tier, *adds* — it does not replace
        // the first, and it does not fan out either.
        runtime
            .persist_web_tier(WebTier::Search)
            .expect("the second write must land");
        let after = load_config(Some(&config_path)).expect("loads");
        assert_eq!(
            after.web.permission_allow,
            vec![WebTier::FetchUserUrl, WebTier::Search]
        );

        // And answering the same tier twice does not grow the list.
        runtime
            .persist_web_tier(WebTier::Search)
            .expect("idempotent");
        assert_eq!(
            load_config(Some(&config_path))
                .expect("loads")
                .web
                .permission_allow,
            vec![WebTier::FetchUserUrl, WebTier::Search]
        );
    }

    /// The gate builds from the **live** config, so the mapping above is
    /// reached on the real path and not only in the test above.
    #[tokio::test]
    async fn the_session_gate_reads_the_configured_web_permission() {
        use crate::harness::tools::web::PERMISSION_KEY_SEARCH;

        // A listed tier: the gate decides with no prompt at all, which is
        // what the user bought by answering "enable permanently" last
        // session.
        let runtime = Arc::new(runtime_with(WebTier::Search, false));
        runtime
            .config
            .lock()
            .expect("config mutex")
            .web
            .permission_allow = vec![WebTier::Search];
        let config = runtime.config.lock().expect("config mutex").clone();
        let events = Arc::new(EventBus::new());
        let gate = runtime.permission_gate_for(&SessionId::from("s"), &events, &config);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_SEARCH, None, WebTier::Search)
                .await,
            PermissionDecision::Allowed,
            "`[web] permission_allow = [\"search\"]` did not reach the gate"
        );

        // Non-vacuity: the default posture prompts instead. Observed as the
        // prompt appearing in the pending registry rather than by awaiting
        // the decision, which would never return with nobody to answer.
        let asking = Arc::new(runtime_with(WebTier::Search, false));
        let ask_config = asking.config.lock().expect("config mutex").clone();
        assert!(
            ask_config.web.permission_allow.is_empty(),
            "the shipped default asks about every tier (BR-4)"
        );
        let ask_events = Arc::new(EventBus::new());
        let ask_gate = asking.permission_gate_for(&SessionId::from("s"), &ask_events, &ask_config);
        let mut sub = ask_events.subscribe(4);
        let decide = ask_gate.authorize_web(PERMISSION_KEY_SEARCH, None, WebTier::Search);
        let answer = async {
            let request = loop {
                let env = sub.recv().await.expect("`ask` raised no prompt");
                if let Event::PermissionRequest(request) = env.event {
                    break request;
                }
            };
            asking.pending.resolve(
                &request.request_id,
                teton_protocol::methods::PermissionOutcome::Cancelled,
            );
        };
        let (denied, ()) = tokio::join!(decide, answer);
        assert_eq!(denied, PermissionDecision::Denied);
    }

    /// **One gate per session, not per turn** (REQ-563 verify, M-5).
    ///
    /// "Allow for this session" is a promise about the session. A gate
    /// rebuilt inside `run_prompt_turn` kept it for exactly one turn — and
    /// the re-prompt was invisible because the CLI answers it from its own
    /// grant cache, so a second client, or an ACP host, would have seen the
    /// question again.
    #[test]
    fn the_permission_gate_is_the_same_object_across_turns() {
        let runtime = Arc::new(runtime_with(WebTier::Search, false));
        let config = runtime.config.lock().expect("config mutex").clone();
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("sess-under-test");

        let first = runtime.permission_gate_for(&session, &events, &config);
        let second = runtime.permission_gate_for(&session, &events, &config);
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second turn built a second gate, so every session grant expired \
             with the turn that earned it"
        );

        // …and it really is per session: a different session gets its own.
        let other = runtime.permission_gate_for(&SessionId::from("sess-other"), &events, &config);
        assert!(
            !Arc::ptr_eq(&first, &other),
            "two sessions shared a grant map"
        );
    }

    /// A config the daemon would refuse to start on is refused *before* it is
    /// written: `[web] tier = "search"` with no endpoint fails to load, so
    /// persisting it would answer one consent prompt by bricking the next
    /// start.
    #[test]
    fn a_tier_that_would_not_load_is_never_written() {
        let (runtime, config_path, _dir) = runtime_on_disk("invalid-tier");
        let err = runtime
            .persist_web_tier(WebTier::Search)
            .expect_err("search with no endpoint must be refused");
        assert!(err.contains("would not load"), "{err}");
        assert_eq!(
            runtime.config.lock().expect("config mutex").web.tier,
            WebTier::Off,
            "a refused write must not have moved the live ceiling either"
        );
        assert_eq!(std::fs::read_to_string(&config_path).expect("read"), "");
    }

    /// With nowhere to write, "permanently" would outlive nothing — so it is
    /// reported rather than silently applied in memory. The gate turns this
    /// into a `session`-scoped consent event.
    #[test]
    fn a_runtime_with_no_config_file_refuses_to_claim_permanence() {
        let runtime = DaemonRuntime::minimal();
        assert!(runtime.config_path.is_none());
        let err = runtime
            .persist_web_tier(WebTier::FetchAnyUrl)
            .expect_err("no file, no permanence");
        assert!(err.contains("no configuration file"), "{err}");
    }

    /// The runtime really is the seam the permission gate writes through —
    /// the trait impl and the inherent method are one behaviour, not two.
    #[test]
    fn the_persistence_seam_is_the_runtime_s_own_write() {
        let (runtime, config_path, _dir) = runtime_on_disk("seam");
        let sink: &dyn WebTierPersistence = &runtime;
        sink.persist_web_tier(WebTier::FetchAnyUrl)
            .expect("the seam writes");
        assert_eq!(
            load_config(Some(&config_path)).expect("loads").web.tier,
            WebTier::FetchAnyUrl
        );
    }

    /// AC-10's refresh half: after `web/refresh`, the entry the next lookup
    /// would have hit is gone, so that lookup re-fetches.
    #[test]
    fn web_refresh_evicts_the_entry_the_next_lookup_would_have_hit() {
        let (runtime, _config_path, dir) = runtime_on_disk("refresh");
        let url = "https://docs.rs/serde";
        let cache = WebCache::new(&dir, 900);
        cache.put(url, "the cached reduction", false).expect("put");
        assert!(
            cache.get(url).is_some(),
            "non-vacuity: there is a hit to evict"
        );

        let result = runtime
            .web_refresh(&WebRefreshParams {
                url: url.to_owned(),
            })
            .expect("refresh");
        assert_eq!(result.outcome, WebRefreshOutcome::Evicted);
        assert!(
            cache.get(url).is_none(),
            "the next lookup must miss, which is what makes it re-fetch"
        );
    }

    /// Nothing cached is `absent`, not `evicted` and not an error: the user
    /// asked for the document not to be cached, and it is not.
    #[test]
    fn web_refresh_reports_an_uncached_url_as_absent() {
        let (runtime, _config_path, _dir) = runtime_on_disk("refresh-absent");
        let result = runtime
            .web_refresh(&WebRefreshParams {
                url: "https://docs.rs/never-fetched".to_owned(),
            })
            .expect("an uncached url is not a failure");
        assert_eq!(result.outcome, WebRefreshOutcome::Absent);
    }

    /// Refresh addresses the same entry the tool's own cache does. Keyed by
    /// a URL normalization the daemon owns, so a URL that differs only in
    /// the ways normalization folds still finds the stored copy.
    #[test]
    fn web_refresh_addresses_the_same_entry_the_tool_reads() {
        let (runtime, _config_path, dir) = runtime_on_disk("refresh-keying");
        let cache = WebCache::from_config(
            &dir,
            &runtime.config.lock().expect("config mutex").web.clone(),
        );
        cache
            .put("https://docs.rs/serde", "body", false)
            .expect("put");

        let result = runtime
            .web_refresh(&WebRefreshParams {
                url: "https://docs.rs/serde".to_owned(),
            })
            .expect("refresh");
        assert_eq!(
            result.outcome,
            WebRefreshOutcome::Evicted,
            "the refresh path and the tool's cache must agree on the key"
        );

        // …including where the two spellings differ. The tool caches under
        // the re-serialized URL, so `https://docs.rs` (no path) has to reach
        // the entry `https://docs.rs/` wrote — otherwise `/web refresh`
        // silently reports `absent` for a document that is on disk.
        cache.put("https://docs.rs/", "body", false).expect("put");
        let bare = runtime
            .web_refresh(&WebRefreshParams {
                url: "https://docs.rs".to_owned(),
            })
            .expect("refresh");
        assert_eq!(
            bare.outcome,
            WebRefreshOutcome::Evicted,
            "a spelling the re-serializer moves must still find its entry"
        );
    }

    // -- the search credential's wiring (BR-7, AC-7) --------------------

    /// **The search key rides the search request, to the search endpoint's
    /// origin, and nowhere else** (REQ-563 BR-7; REQ-544 M-3's guarantee,
    /// inherited).
    ///
    /// Every other test of this seam configures no `search_key_ref`, so the
    /// whole of [`DaemonRuntime::search_auth`] — the keychain read, the
    /// bearer header, the endpoint binding — was reachable only through code
    /// nothing observed: replacing its body with `None` left the suite green.
    /// This is the observation it was missing, and it is taken **off the
    /// wire** rather than off a mock, because the object that attaches the
    /// header is the production [`HttpTransport`] that `web_lookup_egress`
    /// builds internally.
    ///
    /// Four legs, one runtime each where the configuration differs:
    ///
    /// 1. a search → `Authorization: Bearer …` on the request to the
    ///    endpoint;
    /// 2. a **user-pasted fetch to a different origin** → no credential of
    ///    any kind, which is the cross-contamination guarantee;
    /// 3. a user-pasted fetch **at the endpoint's own origin** → refused
    ///    before the wire (the confused-deputy leg: verify wave 1 made the
    ///    origin-match case unreachable, so what used to be "the key would
    ///    ride a fetch" is now "that fetch does not happen");
    /// 4. the same search with **no `search_key_ref`** → no credential, so
    ///    leg 1 is reading the configuration and not a header this transport
    ///    always adds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_search_key_rides_only_the_search_request_and_only_to_its_endpoint() {
        let engine = CountingEngine::answering("NONE");
        let search = CaptureServer::bind().await;
        let elsewhere = CaptureServer::bind().await;
        let endpoint = search.url("/api");

        let runtime = searching_runtime(&endpoint, Some("keychain://teton/search"), &engine);
        let config = runtime.config.lock().expect("config mutex").clone();
        let router = router_for(&runtime);
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("search-auth");
        let egress = runtime
            .web_lookup_egress(&router, &config, &events, &session)
            .expect("the lookup choke point must build");
        let taint = runtime.web_taint_view();
        let ctx = LookupContext::new(session.clone(), taint.as_ref(), &allow_any_host)
            .with_search_endpoint(&endpoint);

        // --- leg 1: the search carries the key ------------------------
        let searched = egress
            .lookup(
                &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
                &ctx,
            )
            .await;
        assert_eq!(
            searched.outcome(),
            WebLookupOutcome::Completed,
            "the search must actually reach the endpoint, or there is no \
             request to read a header off: {:?}",
            searched.detail()
        );
        assert_eq!(search.requests().len(), 1, "exactly one search went out");
        let head = search.requests()[0].to_ascii_lowercase();
        assert!(
            head.contains(&format!("authorization: bearer {SEARCH_SECRET}")),
            "the resolved search credential did not ride the search request; \
             request head:\n{}",
            search.requests()[0]
        );

        // --- leg 2: a fetch elsewhere carries nothing -----------------
        //
        // User-pasted, because a model-composed loopback URL is refused by
        // the address-class floor before any header could be attached — and
        // the question here is about the *header*, not about that gate.
        let fetched = egress
            .lookup(
                &LookupRequest::fetch(elsewhere.url("/page"), Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(
            fetched.outcome(),
            WebLookupOutcome::Completed,
            "the fetch must reach the wire, or its missing header proves \
             nothing: {:?}",
            fetched.detail()
        );
        assert_eq!(elsewhere.requests().len(), 1);
        assert!(
            !elsewhere.carried_auth(0),
            "the search credential travelled to a host that is not the search \
             endpoint; request head:\n{}",
            elsewhere.requests()[0]
        );
        assert_eq!(
            search.requests().len(),
            1,
            "and the endpoint saw nothing new"
        );

        // --- leg 3: a fetch AT the endpoint's origin is refused --------
        //
        // The origin match is the one case where the transport's binding
        // would have attached the key to a fetch. The seam closes it a layer
        // earlier, so the guarantee is now a refusal rather than a header
        // assertion — and the refusal is what is asserted.
        let deputy = egress
            .lookup(
                &LookupRequest::fetch(search.url("/api"), Authorship::UserPasted),
                &ctx,
            )
            .await;
        assert_eq!(deputy.outcome(), WebLookupOutcome::RefusedDomain);
        assert_eq!(
            deputy.detail(),
            &LookupDetail::SearchEndpointFetch,
            "a fetch aimed at the search origin must be refused as one"
        );
        assert_eq!(
            search.requests().len(),
            1,
            "a refusal that reached the endpoint is not a refusal"
        );

        // --- leg 4: no key reference, no credential -------------------
        let unauthenticated = CaptureServer::bind().await;
        let bare_endpoint = unauthenticated.url("/api");
        let bare = searching_runtime(&bare_endpoint, None, &engine);
        let bare_config = bare.config.lock().expect("config mutex").clone();
        let bare_router = router_for(&bare);
        let bare_egress = bare
            .web_lookup_egress(&bare_router, &bare_config, &events, &session)
            .expect("the lookup choke point must build");
        let bare_taint = bare.web_taint_view();
        let bare_ctx = LookupContext::new(session, bare_taint.as_ref(), &allow_any_host)
            .with_search_endpoint(&bare_endpoint);
        let unauthenticated_search = bare_egress
            .lookup(
                &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
                &bare_ctx,
            )
            .await;
        assert_eq!(
            unauthenticated_search.outcome(),
            WebLookupOutcome::Completed
        );
        assert_eq!(unauthenticated.requests().len(), 1);
        assert!(
            !unauthenticated.carried_auth(0),
            "an endpoint with no `search_key_ref` must be reached without a \
             credential — an unauthenticated backend is a legitimate \
             configuration; request head:\n{}",
            unauthenticated.requests()[0]
        );
    }

    /// BUG-165 — `[web] search_auth` decides the shape the key rides, read
    /// off the same wire as the binding test above.
    ///
    /// The two shapes are the REQ's own example backends' spellings,
    /// neither of which is Bearer: Brave's bare key in a header of its own,
    /// and Kagi's scheme word on the standard header. The default —
    /// absent key ⇒ `Authorization: Bearer` — is already pinned by leg 1
    /// of the binding test, which is what makes this pair sufficient:
    /// together the three cover "another header", "another scheme", and
    /// "the unchanged default".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_configured_search_auth_shape_is_the_shape_on_the_wire() {
        let engine = CountingEngine::answering("NONE");
        let session = SessionId::from("search-auth-shape");
        let events = Arc::new(EventBus::new());

        let searched_with = |shape: &'static str| {
            let engine = &engine;
            let session = &session;
            let events = &events;
            async move {
                let backend = CaptureServer::bind().await;
                let endpoint = backend.url("/api");
                let runtime = searching_runtime(&endpoint, Some("keychain://teton/search"), engine);
                runtime.config.lock().expect("config mutex").web.search_auth =
                    Some(shape.to_owned());
                let config = runtime.config.lock().expect("config mutex").clone();
                let router = router_for(&runtime);
                let egress = runtime
                    .web_lookup_egress(&router, &config, events, session)
                    .expect("the lookup choke point must build");
                let taint = runtime.web_taint_view();
                let ctx = LookupContext::new(session.clone(), taint.as_ref(), &allow_any_host)
                    .with_search_endpoint(&endpoint);
                let ending = egress
                    .lookup(
                        &LookupRequest::search("tokio task pinning", Authorship::ModelComposed),
                        &ctx,
                    )
                    .await;
                assert_eq!(
                    ending.outcome(),
                    WebLookupOutcome::Completed,
                    "the search must reach the endpoint, or there is no \
                     request to read a header off: {:?}",
                    ending.detail()
                );
                assert_eq!(backend.requests().len(), 1, "exactly one search went out");
                backend
            }
        };

        // --- Brave's shape: a bare key in a header of its own ----------
        let brave = searched_with("X-Subscription-Token: {key}").await;
        let head = brave.requests()[0].to_ascii_lowercase();
        assert!(
            head.contains(&format!("x-subscription-token: {SEARCH_SECRET}")),
            "the key did not ride the configured header; request head:\n{}",
            brave.requests()[0]
        );
        assert!(
            !brave.carried_auth(0),
            "a shape naming its own header must not also send the Bearer \
             default — one credential, one header; request head:\n{}",
            brave.requests()[0]
        );

        // --- Kagi's shape: the standard header, another scheme word ----
        let kagi = searched_with("Authorization: Bot {key}").await;
        let head = kagi.requests()[0].to_ascii_lowercase();
        assert!(
            head.contains(&format!("authorization: bot {SEARCH_SECRET}")),
            "the scheme word in the template is the scheme word on the \
             wire; request head:\n{}",
            kagi.requests()[0]
        );
    }
}

/// REQ-614 TASK-392 — the cause, the lift, and the one predicate seven routes
/// share.
#[cfg(test)]
mod shell_pin {
    use super::*;

    fn pin() -> (Arc<SessionTaint>, Arc<ShellTaintOverride>, RoutePin) {
        let taint = Arc::new(SessionTaint::new());
        let lifted = Arc::new(ShellTaintOverride::new());
        let route = RoutePin::new(taint.clone(), lifted.clone());
        (taint, lifted, route)
    }

    /// BR-3. A boundary pin ignores the override set entirely — there is no
    /// lift for this cause, and that is a property of `RoutePin::pins` rather
    /// than a check someone remembered to put upstream.
    ///
    /// **Mutation**: drop `cause.liftable() &&` from `RoutePin::pins` and this
    /// goes red.
    #[test]
    fn a_boundary_hit_pin_is_never_liftable() {
        let (taint, lifted, route) = pin();
        let s = SessionId::from("perm");
        taint.mark(&s, TaintCause::BoundaryHit);
        assert!(route.pins(&s));
        lifted.lift(&s);
        assert!(
            route.pins(&s),
            "BR-3: a boundary hit stays pinned however many lifts are recorded"
        );
        assert!(!TaintCause::BoundaryHit.liftable());
    }

    /// BR-4/BR-5, and the benign twin of the test above: the one liftable cause
    /// really does lift.
    #[test]
    fn an_unknown_shell_pin_lifts_and_nothing_else_does() {
        let (taint, lifted, route) = pin();
        let s = SessionId::from("liftable");
        taint.mark(&s, TaintCause::UnknownShell);
        assert!(route.pins(&s), "pinned before the lift");
        assert!(lifted.lift(&s), "the first lift is the transition");
        assert!(!route.pins(&s), "BR-4: routing by category resumes");
        assert!(!lifted.lift(&s), "a second lift is not a transition");
        // A different session is untouched — the lift is per session.
        let other = SessionId::from("bystander");
        taint.mark(&other, TaintCause::UnknownShell);
        assert!(route.pins(&other));
    }

    /// The first cause wins. A session pinned permanently by reading `.env` and
    /// then again by an opaque `shell` must not become liftable.
    ///
    /// **Mutation**: use `insert` instead of `entry().or_insert()` in
    /// `SessionTaint::mark` and this goes red.
    #[test]
    fn the_first_cause_wins_so_a_permanent_pin_cannot_be_downgraded() {
        let (taint, lifted, route) = pin();
        let s = SessionId::from("both");
        assert!(
            taint.mark(&s, TaintCause::BoundaryHit),
            "first is a transition"
        );
        assert!(
            !taint.mark(&s, TaintCause::UnknownShell),
            "second is a re-mark"
        );
        assert_eq!(taint.cause(&s), Some(TaintCause::BoundaryHit));
        lifted.lift(&s);
        assert!(route.pins(&s), "still permanently pinned");
    }

    /// BR-4's remedy sentence, and the permanent arm's refusal to make it.
    #[test]
    fn an_unknown_shell_pin_names_its_remedy() {
        let liftable = crate::runtime::taint_pin_reason_for("this turn", TaintCause::UnknownShell);
        assert!(liftable.contains("/shell allow"), "{liftable}");
        assert!(
            liftable.contains("shell result of unknown reach"),
            "{liftable}"
        );
        let permanent = crate::runtime::taint_pin_reason_for("this turn", TaintCause::BoundaryHit);
        assert!(!permanent.contains("/shell allow"), "{permanent}");
        assert!(permanent.contains("boundary_hit"), "{permanent}");
    }

    /// ADR-614-4 — **the sweep**, asserted as a region check rather than a count.
    ///
    /// Relocating a call keeps a count identical (LESSON-568), so this reads the
    /// bodies of the seven functions that force a local route and requires each
    /// to consult `route_pin()`. A site that reverted to the raw taint bit would
    /// still route correctly for an unlifted session and would silently ignore
    /// every lift — nothing else in the suite looks at all seven.
    ///
    /// **Mutation**: change any one of the seven back to
    /// `self.session_taint.is_tainted(session_id)` and this goes red naming it.
    #[test]
    fn every_pinned_route_site_reads_the_composed_predicate() {
        // Cut each corpus at its first column-0 `#[cfg(test)]`, or the tests
        // below (which legitimately call `is_tainted`) would be scanned too.
        let duty = include_str!("duty.rs");
        let duty = &duty[..duty.find("\n#[cfg(test)]").unwrap_or(duty.len())];
        let turn = include_str!("turn.rs");
        let turn = &turn[..turn.find("\n#[cfg(test)]").unwrap_or(turn.len())];

        // Floors: a corpus that has stopped containing the subject would satisfy
        // the prohibition vacuously (BUG-159).
        for (name, text, anchor) in [
            ("duty.rs", duty, "fn digest_route("),
            ("turn.rs", turn, "fn dispatch_route("),
        ] {
            assert!(text.contains(anchor), "{name} is not the file this scans");
        }

        let duty_sites = duty.matches("self.route_pin().pins(session_id)").count();
        assert_eq!(
            duty_sites, 6,
            "the six duty routes must all read the composed predicate; found {duty_sites}"
        );
        assert_eq!(
            turn.matches("self.route_pin().pins(session_id)").count(),
            1,
            "`dispatch_route` must read the composed predicate"
        );
        for (name, text) in [("duty.rs", duty), ("turn.rs", turn)] {
            assert!(
                !text.contains("session_taint.is_tainted(session_id)"),
                "{name} still forces a local route from the raw taint bit, which \
                 ignores every `/shell allow` lift"
            );
        }
    }

    /// The lift's setter is not `pub` — the property that makes "a model cannot
    /// lift its own pin" a compile-time fact rather than a runtime check.
    ///
    /// Asserted on the source, as `runtime_visibility.rs` does for
    /// `WebTaintOverride::lift`: there is no way to write a test that fails to
    /// compile on purpose.
    #[test]
    fn the_lift_setter_is_not_crate_visible() {
        let source = include_str!("taint.rs");
        let source = &source[..source.find("\n#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            source.contains("pub(super) fn lift(&self, session: &SessionId) -> bool"),
            "ShellTaintOverride::lift must stay `pub(super)`"
        );
        assert!(
            !source.contains("pub fn lift(&self, session: &SessionId)"),
            "a `pub fn lift` is reachable from `crate::harness::tools`, where a \
             model's tool call lands"
        );
    }
}

/// REQ-614 TASK-395 — the daemon half of `/shell allow`.
#[cfg(test)]
mod shell_override_rpc {
    use super::*;
    use teton_protocol::methods::ShellOverrideParams;

    fn params(session: &SessionId) -> ShellOverrideParams {
        ShellOverrideParams {
            session_id: session.clone(),
        }
    }

    /// AC-2. A `boundary_hit` pin is refused, and the refusal **names the
    /// cause** — "that did not work" without a reason is what sends a user
    /// looking for a command that does not exist.
    #[test]
    fn shell_allow_is_refused_on_a_boundary_hit_and_names_the_cause() {
        let runtime = DaemonRuntime::minimal();
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("perm");
        runtime
            .session_taint
            .mark(&session, TaintCause::BoundaryHit);

        let result = runtime.shell_override(&params(&session), &events);
        assert!(result.was_pinned);
        assert!(!result.lifted_now, "BR-3: no lift exists for this cause");
        assert_eq!(result.cause.as_deref(), Some("boundary_hit"));
        // And the route still pins.
        assert!(runtime.route_pin().pins(&session));
        // Nothing was written: a refused lift leaves no ledger row (assert the
        // absence — LESSON-550).
        assert!(runtime
            .ledger
            .all_shell_overrides()
            .expect("read")
            .is_empty());
    }

    /// AC-3. The liftable cause lifts, writes exactly one row, and a second
    /// call writes none.
    #[test]
    fn a_second_shell_allow_writes_no_row() {
        let runtime = DaemonRuntime::minimal();
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("liftable");
        runtime
            .session_taint
            .mark(&session, TaintCause::UnknownShell);

        let first = runtime.shell_override(&params(&session), &events);
        assert!(first.was_pinned && first.lifted_now);
        assert!(!runtime.route_pin().pins(&session), "routing resumes");
        assert_eq!(runtime.ledger.all_shell_overrides().expect("read").len(), 1);

        let second = runtime.shell_override(&params(&session), &events);
        assert!(second.was_pinned, "the session is still recorded as pinned");
        assert!(!second.lifted_now, "a second lift is not a transition");
        assert_eq!(
            runtime.ledger.all_shell_overrides().expect("read").len(),
            1,
            "BR-5: a second `/shell allow` writes no row"
        );
    }

    /// An unpinned session is a third outcome, distinct from both — a client
    /// that could not tell it from "already lifted" would confirm a lift that
    /// never happened.
    #[test]
    fn shell_allow_on_an_unpinned_session_changes_nothing() {
        let runtime = DaemonRuntime::minimal();
        let events = Arc::new(EventBus::new());
        let session = SessionId::from("clean");
        let result = runtime.shell_override(&params(&session), &events);
        assert!(!result.was_pinned);
        assert!(!result.lifted_now);
        assert!(result.cause.is_none());
        assert!(runtime
            .ledger
            .all_shell_overrides()
            .expect("read")
            .is_empty());
    }

    /// AC-7, the absence half. A `/shell allow` that arrives as **text** —
    /// inside a skill body, a `TETON.md` or a tool result — reaches no handler,
    /// so the session stays pinned and the ledger stays empty.
    ///
    /// The structural reason is that the lift is a client RPC and tool dispatch
    /// has no path to one; this asserts the consequence rather than the
    /// mechanism, because a mechanism can be re-implemented and a consequence
    /// cannot be argued with (LESSON-550: assert the absence).
    #[test]
    fn shell_allow_as_text_is_inert() {
        let runtime = DaemonRuntime::minimal();
        let session = SessionId::from("texty");
        runtime
            .session_taint
            .mark(&session, TaintCause::UnknownShell);

        // The mechanism, asserted rather than described: there is no **tool**
        // by any of these names, so a model that emits one gets "no such tool"
        // and the text never reaches a lift. This is the daemon-side half of
        // "a `/shell allow` line inside a tool result is data".
        let registry = crate::harness::tools::ToolRegistry::new();
        let names = registry.names();
        for forbidden in ["shell allow", "shell_allow", "shell/override"] {
            assert!(
                !names.contains(&forbidden),
                "a tool named {forbidden:?} would give the model a path to the lift: {names:?}"
            );
        }
        // And no skill may take the name either, so a repository-supplied
        // `.claude/skills/shell/SKILL.md` cannot shadow the row (LESSON-578: a
        // rule attached to one flow guards one door — this is the second).
        assert!(teton_protocol::methods::is_reserved_skill_name("shell"));

        // The consequence, which is what AC-7 actually claims: after all of
        // that, the session is still pinned and the ledger is still empty.
        assert!(
            runtime.route_pin().pins(&session),
            "the session must still be pinned"
        );
        assert!(
            runtime
                .ledger
                .all_shell_overrides()
                .expect("read")
                .is_empty(),
            "AC-7: the ledger has no row"
        );
    }
}
