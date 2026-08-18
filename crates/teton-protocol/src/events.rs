//! Daemon→client events.
//!
//! The daemon broadcasts events to subscribed clients (ADR-002's
//! event-subscription model). Every event travels inside an [`EventEnvelope`]:
//! shared metadata (optional session scope, a broadcast sequence number) plus a
//! tagged [`Event`] discriminated by a snake_case `event` name.
//!
//! The `event` names are the contract fixed by REQ-544's System Model → Events
//! table: `route_decided`, `privacy_block`, `phase_transition`, `cost_recorded`,
//! `provider_degraded`, `daemon_client_attach`. Three further events —
//! `session_update`, `permission_request`, `model_lifecycle` — carry the
//! streaming turn, permission prompts, and local-model lifecycle (BR-9); the
//! first two borrow ACP vocabulary. REQ-547 adds
//! `model_selection_proposed`/`model_selection_decided`, the consent round-trip
//! that gates the local tier before any weights are fetched. REQ-561 adds
//! `session_titled` (BR-9a), which announces the title the `title` category
//! produced for a session. REQ-563 adds the opt-in web-lookup family —
//! `web_lookup`, `web_consent_decided`, `web_taint_overridden` — where its
//! spec's ten-row Events table is deliberately folded onto three variants
//! (architecture D-8; the fold is spelled out above [`WebLookupOutcome`]).
//! REQ-567 adds `context_cleared` (BR-8), which announces that a session's
//! retained conversation was dropped on the user's say-so. REQ-579 adds
//! `provider_setup_completed` (BR-15) and `provider_setup_rejected_nonuser`
//! (BR-12), the guided provider-setup flow's two announcements — a commit that
//! landed, and a commit refused for not coming from the user. REQ-580 adds
//! `turn_queued` (BR-2), which says a prompt turn is being held for a local tier
//! that is still coming up rather than refused. REQ-581 adds `provider_tested`
//! (BR-3), the typed result of the one consented call a user's connection test
//! makes against a registered provider.
//!
//! This list is an index, not decoration: a new variant of [`Event`] that is not
//! named here makes the paragraph above wrong.

use serde::{Deserialize, Serialize};

use crate::effort::ResolvedEffort;
use crate::methods::{ProviderHealth, ProviderTestOutcome, TierBinding};
use crate::{
    Category, ClientKind, Phase, ProtocolVersion, ProviderId, ProviderKind, RequestId, SessionId,
    Tier, TurnId,
};

/// JSON-RPC notification method every broadcast event is delivered under. Its
/// params are an [`EventEnvelope`].
pub const EVENT_METHOD: &str = "event";

/// JSON-RPC notification method the daemon sends before dropping a subscription
/// it evicted for lagging (see `tetond::broadcast`). Its params are a
/// [`crate::jsonrpc::RpcError`] carrying
/// [`crate::jsonrpc::error_code::SUBSCRIPTION_LAGGED`].
///
/// Declared here, with the event vocabulary, rather than once per crate: the
/// daemon's `Notification::new` and the client's method-name match are two
/// halves of one wire contract, and a copy on each side agrees only until
/// someone edits one of them.
pub const SUBSCRIPTION_LAGGED_METHOD: &str = "subscription/lagged";

/// A broadcast event plus its shared envelope metadata.
///
/// The [`Event`] is internally tagged and flattened, so the wire form is a flat
/// object: `{ "session_id": …, "seq": …, "event": "route_decided", … }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Session this event belongs to, or `None` for daemon-scoped events
    /// (`daemon_client_attach`, `model_lifecycle`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<SessionId>,
    /// Monotonic per-stream sequence number for ordering and gap detection.
    pub seq: u64,
    /// The event itself.
    #[serde(flatten)]
    pub event: Event,
}

impl EventEnvelope {
    /// Wraps `event` with a sequence number and optional session scope.
    pub fn new(seq: u64, session_id: Option<SessionId>, event: Event) -> Self {
        Self {
            session_id,
            seq,
            event,
        }
    }

    /// The wire `event` name of the wrapped event (matches the spec table).
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        self.event.name()
    }
}

/// One broadcast event, discriminated on the wire by a snake_case `event` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Streaming update within a prompt turn. ACP: `session/update`.
    SessionUpdate(SessionUpdate),
    /// A session was given a title (REQ-561 BR-9a). Teton differentiator — no
    /// ACP equivalent.
    SessionTitled(SessionTitled),
    /// A model was selected for a step (spec: `route_decided`). Teton
    /// differentiator — no ACP equivalent.
    RouteDecided(RouteDecided),
    /// Boundary content would have gone remote (spec: `privacy_block`). Teton
    /// differentiator — no ACP equivalent.
    PrivacyBlock(PrivacyBlock),
    /// A provenance source could not be minted into an identity and was refused
    /// (REQ-571 ADR-D). Teton differentiator — no ACP equivalent.
    ProvenanceRejected(ProvenanceRejected),
    /// A model call completed and produced a cost record (spec: `cost_recorded`).
    CostRecorded(CostRecorded),
    /// An adapter fell back to another provider (spec: `provider_degraded`).
    ProviderDegraded(ProviderDegraded),
    /// Local-model lifecycle progress: download / benchmark / step-down (BR-9).
    ModelLifecycle(ModelLifecycle),
    /// The daemon proposes a local model and awaits an answer (REQ-547 BR-1).
    ModelSelectionProposed(ModelSelectionProposed),
    /// A model-selection decision was recorded (REQ-547 BR-4/BR-10).
    ModelSelectionDecided(ModelSelectionDecided),
    /// The harness needs a permission decision. ACP: `session/request_permission`.
    PermissionRequest(PermissionRequest),
    /// A structured-mode phase gate passed (spec: `phase_transition`).
    PhaseTransition(PhaseTransition),
    /// A client attached to the daemon (spec: `daemon_client_attach`).
    DaemonClientAttach(DaemonClientAttach),
    /// A moment in the daemon's own lifetime — a client counted in or out, a
    /// shutdown armed, deferred, or taken (REQ-565). Every stage the spec names
    /// as a separate event is a [`DaemonLifetimeStage`] on this one variant.
    DaemonLifetime(DaemonLifetime),
    /// A web lookup reached a terminal outcome (REQ-563 BR-7). Every way a
    /// lookup can end — including the ones the spec names as separate events —
    /// is a [`WebLookupOutcome`] on this one variant.
    WebLookup(WebLookup),
    /// A web-lookup consent decision was recorded (REQ-563 BR-4). The *prompt*
    /// that preceded it is a [`PermissionRequest`], not an event of its own.
    WebConsentDecided(WebConsentDecided),
    /// The user lifted this session's web taint restriction (REQ-563 BR-13).
    WebTaintOverridden(WebTaintOverridden),
    /// A local agent turn hit, missed, or evicted the KV prefix cache
    /// (REQ-564). Every ending is a [`PrefixCacheOutcome`] on this one variant.
    PrefixCache(PrefixCache),
    /// The user cleared a session's retained conversation (REQ-567 BR-8).
    ContextCleared(ContextCleared),
    /// The daemon is asking whether to let an ungranted connection attach, or
    /// monitor (REQ-569 BR-6). Answered by `attach/consent`.
    AttachConsentRequested(AttachConsentRequested),
    /// An attach or monitor request was refused (REQ-569 BR-5).
    AttachRefused(AttachRefused),
    /// The daemon minted a session grant (REQ-569 verify, F6). Daemon-scoped —
    /// it names no session — so every handshaked connection is told.
    SessionGrantMinted(SessionGrantMinted),
    /// A guided web-setup flow committed a config change (REQ-572 BR-14).
    WebSetupCompleted(WebSetupCompleted),
    /// A setup call was refused because it did not come from the user
    /// (REQ-572 BR-4).
    WebSetupRejected(WebSetupRejected),
    /// A guided provider-setup flow committed a registration (REQ-579 BR-15).
    ProviderSetupCompleted(ProviderSetupCompleted),
    /// A provider-setup **commit** was refused because it did not come from the
    /// user (REQ-579 BR-12).
    ///
    /// The wire name is `provider_setup_rejected_nonuser`, which the derived
    /// snake_case spelling does not produce — hence the explicit rename. It is
    /// the spec's own name for the event and says what the refusal *was*, not
    /// merely that there was one: a client that logs it is logging an attempt
    /// by something other than the user, which is a different security fact
    /// from a malformed candidate.
    #[serde(rename = "provider_setup_rejected_nonuser")]
    ProviderSetupRejected(ProviderSetupRejected),
    /// A turn dead-ended on a capability that is off or unconfigured
    /// (REQ-572 AC-2, architecture ADR-4).
    CapabilityDeadEnd(CapabilityDeadEnd),
    /// A prompt turn is being held for the local tier it needs, which is still
    /// coming up, rather than refused (REQ-580 BR-2).
    TurnQueued(TurnQueued),
    /// A user's connection test finished, with what came back and where it left
    /// the provider's health (REQ-581 BR-3/BR-4).
    ProviderTested(ProviderTested),
}

impl Event {
    /// The wire `event` name, identical to the serialized tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Event::SessionUpdate(_) => "session_update",
            Event::SessionTitled(_) => "session_titled",
            Event::RouteDecided(_) => "route_decided",
            Event::PrivacyBlock(_) => "privacy_block",
            Event::ProvenanceRejected(_) => "provenance_rejected",
            Event::CostRecorded(_) => "cost_recorded",
            Event::ProviderDegraded(_) => "provider_degraded",
            Event::ModelLifecycle(_) => "model_lifecycle",
            Event::ModelSelectionProposed(_) => "model_selection_proposed",
            Event::ModelSelectionDecided(_) => "model_selection_decided",
            Event::PermissionRequest(_) => "permission_request",
            Event::PhaseTransition(_) => "phase_transition",
            Event::DaemonClientAttach(_) => "daemon_client_attach",
            Event::DaemonLifetime(_) => "daemon_lifetime",
            Event::WebLookup(_) => "web_lookup",
            Event::WebConsentDecided(_) => "web_consent_decided",
            Event::WebTaintOverridden(_) => "web_taint_overridden",
            Event::PrefixCache(_) => "prefix_cache",
            Event::ContextCleared(_) => "context_cleared",
            Event::AttachConsentRequested(_) => "attach_consent_requested",
            Event::AttachRefused(_) => "attach_refused",
            Event::SessionGrantMinted(_) => "session_grant_minted",
            Event::WebSetupCompleted(_) => "web_setup_completed",
            Event::WebSetupRejected(_) => "web_setup_rejected",
            Event::ProviderSetupCompleted(_) => "provider_setup_completed",
            Event::ProviderSetupRejected(_) => "provider_setup_rejected_nonuser",
            Event::CapabilityDeadEnd(_) => "capability_dead_end",
            Event::TurnQueued(_) => "turn_queued",
            Event::ProviderTested(_) => "provider_tested",
        }
    }
}

// ---------------------------------------------------------------------------
// session_update (ACP: session/update)
// ---------------------------------------------------------------------------

/// A streaming update within a prompt turn. ACP: `session/update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUpdate {
    /// The specific update.
    pub update: SessionUpdatePayload,
}

/// The kinds of streaming update a turn can emit. ACP: `SessionUpdate` variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionUpdatePayload {
    /// A chunk of assistant text. ACP: `agent_message_chunk`.
    AgentMessageChunk {
        /// The text delta.
        text: String,
    },
    /// A tool call started. ACP: `tool_call`.
    ToolCall {
        /// Correlates the call with later updates.
        tool_call_id: String,
        /// Human-facing title.
        title: String,
        /// Current status.
        status: ToolCallStatus,
    },
    /// A tool call changed status. ACP: `tool_call_update`.
    ToolCallUpdate {
        /// The call being updated.
        tool_call_id: String,
        /// New status.
        status: ToolCallStatus,
    },
    /// A proposed file change. ACP: the `diff` content shape.
    Diff {
        /// Repo-relative path.
        path: String,
        /// Prior contents, or `None` for a new file.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        old_text: Option<String>,
        /// Proposed contents.
        new_text: String,
    },
    /// The agent's current plan. ACP: `plan`.
    Plan {
        /// Ordered plan entries.
        entries: Vec<PlanEntry>,
    },
}

/// Status of a tool call. ACP: `ToolCallStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Awaiting permission or scheduling.
    Pending,
    /// Executing.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// One entry in an agent plan. ACP: a `PlanEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// What the step will do.
    pub content: String,
    /// Step status.
    pub status: PlanEntryStatus,
}

/// Status of a plan entry. ACP: `PlanEntryStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    /// Not started.
    Pending,
    /// Underway.
    InProgress,
    /// Done.
    Completed,
}

// ---------------------------------------------------------------------------
// session_titled (Teton differentiator)
// ---------------------------------------------------------------------------

/// A session was given a title (REQ-561 BR-9a).
///
/// The title itself is not new state: [`crate::methods::SessionSummary::title`]
/// has always been on the wire and was simply never populated (ADR-6). This
/// event is what makes a change to it observable on the stream, so a client
/// learns the session was named without polling `session/list`.
///
/// A session is titled once — the emitter's contract is one event per session,
/// carrying a non-empty title, and none for a session that already has one.
/// Neither is enforceable by a wire type, so a consumer that renders the title
/// should treat an empty string as no title rather than as a name.
///
/// # Which session
///
/// The session is named by [`EventEnvelope::session_id`], as it is for every
/// other session-scoped event ([`RouteDecided`], [`PrivacyBlock`],
/// [`PhaseTransition`]). The wire object therefore still reads
/// `{ "session_id": …, "seq": …, "event": "session_titled", "title": … }` —
/// ADR-6's `SessionTitled { session_id, title }` shape, assembled by the
/// envelope rather than repeated inside the payload.
///
/// Repeating it here is not a stylistic choice but an unrepresentable one:
/// [`Event`] is internally tagged and flattened, so a `session_id` field on this
/// struct would land in the same JSON object as the envelope's, emit the key
/// twice, and fail to deserialize with serde's `duplicate field` error.
/// [`CostRecord`] can carry its own `session_id` because `cost_recorded`
/// *nests* it under `record` instead of flattening it. A contributor who adds
/// the field back turns `session_titled_round_trips_under_its_wire_name` red,
/// which is where that discovery is meant to happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitled {
    /// The title the `title` category produced for the session.
    pub title: String,
}

// ---------------------------------------------------------------------------
// route_decided (Teton differentiator)
// ---------------------------------------------------------------------------

/// The router picked a provider for a step (spec Events: `route_decided`).
///
/// Every routing decision emits this with its `reason` — the legibility promise
/// (BR-5). The `session` scoping lives in the [`EventEnvelope`].
///
/// REQ-558 AC-8: a decision names the **category** it was made for, the tier
/// that category resolved through, the provider, and a non-empty reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecided {
    /// The category this call was made for — the dispatch key in both session
    /// modes (REQ-558 BR-1).
    ///
    /// `None` only for a decision reached by the pre-category path (phase policy
    /// or the freeform heuristic) that TASK-050 replaces; once every route
    /// resolves through `teton_core::category::resolve`, every event names its
    /// category. The absence is the honest shape of a decision made without one,
    /// not a placeholder to be filled in by a caller.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub category: Option<Category>,
    /// The tier the category resolved through, **as reported by the resolution**
    /// — never recomputed from `category` (ADR-D, BR-6, AC-11). Set exactly when
    /// `category` is.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tier: Option<Tier>,
    /// Lifecycle phase in effect; `None` for a freeform turn.
    ///
    /// Retained for cost attribution and ADLC gating (REQ-558 BR-11), **not** as
    /// a routing key: `category` is what drove the decision above.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
    /// Provider selected.
    pub provider_id: ProviderId,
    /// Concrete model chosen, when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// The policy rule (or heuristic) that fired, as a user-facing sentence.
    pub reason: String,
    /// What this call put in its reasoning field(s) — the **effective** effort
    /// after the per-provider clamp, never the requested level (REQ-559 BR-5,
    /// AC-4). Reporting the request would make the event lie about the call.
    ///
    /// `Option` is for **wire additivity only**: a daemon that has this field
    /// always populates it. A frame from a daemon predating it carries no key
    /// and reads `None`, and a client predating it ignores a key serde does not
    /// require it to know — so this moves neither [`crate::PROTOCOL_VERSION`]
    /// nor [`crate::PROTOCOL_VERSION_MIN`], exactly as `PrivacyBlock::cause` did
    /// not (REQ-562 ADR-7).
    ///
    /// `None` therefore means "a daemon that predates effort", which is a
    /// different claim from `Omit` — "effort does not apply here, and here is
    /// why". Keeping them distinct is what lets the surface say which one it is
    /// (BR-6).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<ResolvedEffort>,
}

// ---------------------------------------------------------------------------
// privacy_block (Teton differentiator)
// ---------------------------------------------------------------------------

/// Boundary content would have entered a remote call (spec: `privacy_block`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyBlock {
    /// Where the blocked content lives: a repo-relative path for a boundary
    /// block, and for any other cause a non-secret locus naming the same thing
    /// (the offending bytes are never the value here — BR-6).
    pub path: String,
    /// Provider the content would have reached.
    pub provider_id: ProviderId,
    /// What the daemon did instead.
    pub action: PrivacyAction,
    /// Which inspection refused the payload.
    ///
    /// `#[serde(default)]` with [`BlockCause::Boundary`] as the default is the
    /// compatibility posture (REQ-562 ADR-7): every block a build predating this
    /// field could emit *was* a boundary block, so the default is the historical
    /// fact rather than a filler value. A frame carrying no `cause` key
    /// therefore reads correctly here, and a build that has never heard of
    /// `cause` ignores the key serde does not require it to know — which is why
    /// this addition does not move [`crate::PROTOCOL_VERSION`] the way REQ-558's
    /// `ConfigSnapshot` re-typing did.
    #[serde(default)]
    pub cause: BlockCause,
}

/// Which inspection inside the egress choke point refused the payload.
///
/// The three are different problems with different fixes, so they are different
/// values rather than three readings of one sentence: content crossed a declared
/// boundary, the redaction scan found something, or the redaction scan could not
/// run at all (REQ-562 BR-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum BlockCause {
    /// Provenance intersected a declared privacy boundary — the block REQ-544
    /// shipped, and the reading every `cause`-less frame gets.
    #[default]
    Boundary,
    /// The redaction scan (REQ-562) found something in the outbound payload.
    ///
    /// Kind and span, and deliberately nothing else: a variant with a field able
    /// to hold the matched text is a variant that will eventually hold it, and a
    /// secret detector that echoes the secret has moved it rather than caught it
    /// (BR-6). The report is actionable from `path` + `kind` + `span` alone.
    Redaction {
        /// What the finding looked like.
        kind: FindingKind,
        /// Byte range within the outbound payload.
        span: ByteSpan,
    },
    /// The redaction scan **could not run** — no local tier able to serve it, a
    /// payload past the input cap, an engine error, or a deadline — and the
    /// payload was blocked unscanned (BR-3, fail closed). This says nothing
    /// about what the payload contains, because nothing looked.
    ScanUnavailable,
}

/// What a redaction finding looked like (REQ-562 System Model: `Finding.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A secret: a key, token, or other value whose whole purpose is to be
    /// unguessable.
    Secret,
    /// A credential: a username/password pair, an authorization header, a
    /// connection string.
    Credential,
    /// Personally identifying information.
    Pii,
    /// Sensitive-looking, and the classifier declined to say more. Fail-closed
    /// vocabulary: unclassified is a report, not a pass.
    Unknown,
}

impl FindingKind {
    /// The noun phrase a **person** reads, for a sentence like *"the redaction
    /// scan found a credential at bytes 1400-1436"*.
    ///
    /// Distinct from the serde name (`"credential"`), which is the wire value
    /// and must never drift for compatibility — this one is prose and may.
    ///
    /// ## Why it lives on the protocol type
    ///
    /// Both ends render it, and they had **byte-identical** private copies:
    /// `tetond`'s `egress::wire_kind_label` composes the daemon's typed-error
    /// sentence, and `teton`'s `session_ui::finding_kind_label` composes the
    /// CLI's `privacy_block` line. Two spellings of one fact drift, and the
    /// drift is user-visible on the one surface that explains a privacy
    /// decision — a user comparing the daemon log against what the CLI printed
    /// should not have to work out whether two wordings mean the same finding.
    ///
    /// It is safe at this layer for the reason the variants are: naming the
    /// *class* of thing found is exactly what [`FindingKind`] is, and it can
    /// never name the thing itself (BR-6) because the type carries no text.
    #[must_use]
    pub const fn user_label(self) -> &'static str {
        match self {
            FindingKind::Secret => "a secret",
            FindingKind::Credential => "a credential",
            FindingKind::Pii => "personal information",
            FindingKind::Unknown => "a sensitive-looking string",
        }
    }
}

/// A half-open byte range `[start, end)` within the outbound payload.
///
/// Offsets only — locating a finding is what this carries, and quoting it is
/// what it structurally cannot (BR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    /// First byte of the finding.
    pub start: u64,
    /// One past the last byte of the finding.
    pub end: u64,
}

/// The action the egress choke point took on a would-be boundary violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyAction {
    /// The offending content was removed from the outbound payload.
    Stripped,
    /// The whole call was re-routed to the local tier.
    ReroutedToLocal,
}

// ---------------------------------------------------------------------------
// provenance_rejected (REQ-571 ADR-D — Teton differentiator)
// ---------------------------------------------------------------------------

/// A provenance source could not be minted into a canonical identity, so the
/// daemon refused to trust it (spec: `provenance_rejected`).
///
/// A privacy verdict keys on "which repo file did this content come from?".
/// When something *asserts* a source the daemon cannot turn into a
/// repo-root-relative identity — an absolute path, a `..`-bearing one — the
/// honest answer is not "then there is no source here": that assertion may well
/// have named a boundary file, and dropping it would fail **open** on exactly
/// the value that matters. The daemon fails closed instead, and says so here.
///
/// ## Why it is on the protocol at all (LESSON-505)
///
/// An audit signal that reaches only daemon stderr is a weak control against an
/// adversary running as the same user: stderr is the one surface that adversary
/// can most easily silence, and no client ever sees it. A refusal that changes
/// what the session may do is something the *user* has to be able to see, so it
/// is broadcast like every other privacy decision.
///
/// ## Wire compatibility: [`crate::PROTOCOL_VERSION`] does not move
///
/// [`Event`] is internally tagged on `event`, so this variant is a purely
/// additive tag value: no existing frame changes shape, no existing field is
/// re-typed, and nothing that could already be sent parses differently. The
/// asymmetric case — a client that has never heard of `provenance_rejected`
/// receiving one — cannot arise, because a client only ever receives frames
/// from the daemon it handshook with, and a daemon that can emit this variant
/// is a build that has the variant. This is the same reasoning that kept
/// `PrivacyBlock::cause` off the version counter (REQ-562 ADR-7), and the
/// opposite of REQ-558's `ConfigSnapshot` re-typing, which changed a shape both
/// ends already exchanged and therefore *did* move it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRejected {
    /// The offending source, sanitized and length-capped by the daemon.
    ///
    /// This is **attacker-influenced text** — a remote MCP server names the
    /// paths it claims to have touched — so it is reported for diagnosis and is
    /// never treated as a path. The daemon strips control characters (which
    /// could otherwise forge a line in whatever renders this) and truncates
    /// before the value reaches this field; a consumer still renders it as
    /// untrusted data.
    pub source: String,
    /// The tool whose call carried the assertion, when the refusal happened
    /// where that is known.
    ///
    /// `None` for the redundant egress-inspection guard, which sees an
    /// assembled provenance long after it left any single tool and cannot
    /// honestly name one. Not an "unknown tool" — a refusal raised somewhere
    /// that has no tool, which is a different sentence to render.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    /// Why the source was refused.
    pub reason: ProvenanceRejection,
}

/// Why a provenance source was refused (REQ-571 ADR-D).
///
/// The canonical form of a provenance identity is a non-empty, repo-root
/// -relative, `/`-separated path with no `.`, `..`, or empty segment. Each
/// variant names one way a source failed that, because they are different
/// problems: an absolute path is a claim about another part of the filesystem,
/// a `..` is a traversal only the filesystem can resolve, and an empty one
/// names nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRejection {
    /// The source is absolute. Boundary globs are authored repo-root-relative,
    /// so an absolute source would silently match none of them.
    Absolute,
    /// The source retains a `..` segment. It is never collapsed lexically:
    /// `a/link/../b` need not be `a/b`, so only the filesystem can say what it
    /// resolves through, and a caller that skipped canonicalization is refused.
    ParentTraversal,
    /// The source retains a `.` or empty segment, so it is not the canonical
    /// spelling boundary matching is defined against.
    NotCanonical,
    /// Nothing was left after normalization — an empty string or a lone `.`.
    /// There is no file to attribute.
    Empty,
}

// ---------------------------------------------------------------------------
// cost_recorded
// ---------------------------------------------------------------------------

/// A completed model call's cost record (spec entity `CostRecord`).
///
/// One record per model call; the cost meter is derived only from these (BR-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRecord {
    /// Session that incurred the cost.
    pub session_id: SessionId,
    /// Phase, or `None` in freeform mode.
    ///
    /// Retained alongside `category` (REQ-558 BR-11): the phase is what the
    /// spend is *attributed* to, the category is what it was *for*.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
    /// The routing category the call was made for (REQ-558), or `None` for a
    /// call recorded with no category attribution — including every row written
    /// by a build that predates categories, which is why the ledger column is
    /// nullable rather than backfilled with a guess.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub category: Option<Category>,
    /// Provider billed.
    pub provider_id: ProviderId,
    /// Concrete model billed.
    pub model: String,
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cost in integer micro-dollars (1e-6 USD). Spec entity field `usd`, sent
    /// as an integer so money never rounds on the wire.
    pub usd_micros: i64,
    /// Prompt tokens whose KV was reused from a resident local prefix (REQ-564
    /// BR-9), or `None` for a call with no prefix cache — every remote call,
    /// and every row a pre-REQ build wrote.
    ///
    /// Omitted from the wire when absent, so a client built against the older
    /// shape reads the same bytes it always did.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cached_tokens: Option<u64>,
    /// Of [`Self::output_tokens`], how many the provider attributed to reasoning
    /// (REQ-559 BR-10), or `None` where it reported none — every Anthropic call,
    /// every local call, and every row a pre-REQ build wrote.
    ///
    /// A **subset of** `output_tokens`, never added to it. Today's totals are
    /// already correct because both providers' aggregate counts include
    /// reasoning tokens; this column says how much of that total was thinking,
    /// and nothing sums the two.
    ///
    /// `None` is **unreported**, never `0` — `teton cost` renders the word
    /// rather than a number, because a `0` standing in for "the provider didn't
    /// tell us" is displaying an estimate as an actual (BR-11, REQ-544 BR-2).
    ///
    /// Same shape as [`Self::cached_tokens`] and for the same reason: omitted
    /// from the wire when absent, so a client built against the older shape
    /// reads the same bytes it always did and neither
    /// [`crate::PROTOCOL_VERSION`] nor [`crate::PROTOCOL_VERSION_MIN`] moves.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_tokens: Option<u64>,
    /// Whether this row is a **connection test** rather than a turn (REQ-581
    /// BR-5).
    ///
    /// A probe is a model call and is billed like one: same request path, same
    /// tokens, same price table, one ordinary row. It is *counted* apart so
    /// `teton cost` can say "1 probe" instead of showing a user a call they
    /// asked no question for as though it were a turn.
    ///
    /// `false` for every turn, and for every row written before this REQ —
    /// where the ledger keeps `NULL`, the honest value for a column whose
    /// concept did not exist yet, and the wire reads that absence as `false`
    /// because a pre-REQ daemon made no probes to report.
    ///
    /// **Omitted from the wire when `false`**, which is
    /// [`Self::cached_tokens`]'s rule in a `bool`'s spelling: a client built
    /// against the older shape reads exactly the same bytes it always did, and
    /// neither [`crate::PROTOCOL_VERSION`] nor [`crate::PROTOCOL_VERSION_MIN`]
    /// moves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probe: bool,
}

/// Event payload wrapping a [`CostRecord`] (spec Events: `cost_recorded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRecorded {
    /// The record for the completed call.
    pub record: CostRecord,
}

// ---------------------------------------------------------------------------
// provider_degraded
// ---------------------------------------------------------------------------

/// An adapter fell back to another provider (spec: `provider_degraded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDegraded {
    /// Provider that failed.
    pub provider_id: ProviderId,
    /// Why it failed.
    pub failure_class: FailureClass,
    /// Provider used instead, if a fallback existed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
}

/// Classification of a provider failure that triggered degradation (BR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The provider produced malformed or unusable tool calls.
    ToolCallFailure,
    /// The call timed out.
    Timeout,
    /// The provider rate-limited the call.
    RateLimited,
    /// The connection failed.
    ConnectionError,
    /// The response could not be parsed.
    InvalidResponse,
}

// ---------------------------------------------------------------------------
// model_lifecycle (BR-9 — Teton differentiator, no ACP equivalent)
// ---------------------------------------------------------------------------

/// Local-model lifecycle progress (BR-9): probe → download → benchmark →
/// runtime pressure adaptation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelLifecycle {
    /// The model this update concerns (e.g. a GGUF identifier).
    pub model_id: String,
    /// The lifecycle stage reached.
    pub stage: ModelLifecycleStage,
}

/// A stage in the local-model lifecycle.
///
/// Every variant is a claim about something that **actually happened** on this
/// machine. That is load-bearing rather than stylistic: a daemon whose whole
/// pitch is legibility may not announce a `download`, a `benchmark` or a `ready`
/// it did not perform, so [`AwaitingDecision`](Self::AwaitingDecision) exists to
/// give the honest pre-consent state a name of its own instead of borrowing a
/// later stage's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ModelLifecycleStage {
    /// First-run hardware probe result (RAM/disk/GPU class → candidate tier).
    Probed {
        /// Detected system RAM in bytes.
        ram_bytes: u64,
        /// Whether the machine cleared the hardware floor for a local tier.
        above_floor: bool,
    },
    /// A model has been proposed and the daemon is **waiting for an answer**
    /// (REQ-547 BR-1): nothing has been downloaded, benchmarked, or loaded, and
    /// sessions run remote-only until the user decides.
    AwaitingDecision {
        /// User-facing sentence naming what is being waited on.
        reason: String,
    },
    /// Download progress for the selected model.
    Download {
        /// Bytes fetched so far.
        downloaded_bytes: u64,
        /// Total bytes, when the length is known.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        total_bytes: Option<u64>,
    },
    /// The whole artifact is on disk and its SHA-256 is being checked (BR-6).
    ///
    /// A stage of its own rather than a `download` pinned at 100%: verifying an
    /// 18 GiB file is a multi-minute hash, and a client that could not tell it
    /// apart from a wedged transfer would read the honest work as a hang. This
    /// says "the bytes are here; I am confirming they are the catalog's bytes".
    Verifying {
        /// Bytes being hashed.
        total_bytes: u64,
    },
    /// Post-download micro-benchmark result (validates the BR-8 latency duty).
    Benchmark {
        /// Measured time to first token, in milliseconds.
        first_token_ms: u32,
        /// Measured decode throughput in tokens/second.
        tokens_per_sec: f32,
    },
    /// The model is loaded and serving.
    Ready,
    /// The tier auto-stepped down after a failed duty (benchmark or pressure).
    SteppedDown {
        /// Model stepped away from.
        from_model: String,
        /// Model stepped down to.
        to_model: String,
        /// User-facing reason.
        reason: String,
    },
    /// The local tier is cleanly absent (below floor, or under memory pressure).
    Disabled {
        /// User-facing reason.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// model_selection_proposed / model_selection_decided (REQ-547)
// ---------------------------------------------------------------------------
//
// The consent round-trip that gates the local tier. It mirrors
// `permission_request` → `permission/respond` exactly (REQ-547 D-3): the daemon
// *broadcasts* the proposal as an event carrying a `request_id`, and the
// deciding client answers with a typed method
// ([`crate::methods::ModelConfirmParams`]) keyed by that id. Nothing downloads
// until the answer arrives (BR-1).
//
// Every shape here is a *projection*, not the daemon's internal record: no URL,
// no digest, no install path, no credential (BR-11). The types in this section
// are shared with [`crate::methods`], which reads them for the `model/*` results.

/// GPU acceleration class detected by the first-run probe.
///
/// Variant names and the `snake_case` rule mirror
/// `teton_inference::probe::GpuClass` exactly, so projecting the probe onto the
/// wire is a total map with no room for casing drift — the same technique
/// `teton_core::ProviderKind` uses against [`crate::ProviderKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuClass {
    /// Apple Silicon unified memory + Metal (the MVP first-class target).
    AppleSilicon,
    /// An NVIDIA CUDA GPU.
    Cuda,
    /// No supported accelerator; CPU inference only.
    Cpu,
}

/// The hardware band a catalog model targets (REQ-544's OQ-3 table).
///
/// Ordered smallest-to-largest, mirroring `teton_inference::catalog::TierBand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierBand {
    /// 1.5B-3B class, for 8-16 GiB machines.
    Small,
    /// 7B class, for 16-32 GiB machines.
    Mid,
    /// 30B-A3B class, for 32 GiB+ machines (optional).
    Large,
}

/// The band the probe chose for this machine, including "no local tier".
///
/// A distinct type from [`TierBand`] because the *machine's* band has a fourth
/// state the *catalog's* band does not: `none`, for a machine below the RAM
/// floor. Sent as an explicit `"none"` rather than an absent field so a client
/// can never confuse "below the floor" with "an older daemon omitted this".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChosenBand {
    /// The machine is below the hardware floor; sessions run remote-only.
    None,
    /// The small band.
    Small,
    /// The mid band.
    Mid,
    /// The large band.
    Large,
}

impl ChosenBand {
    /// The concrete catalog band, or `None` when the machine has no local tier.
    #[must_use]
    pub fn band(self) -> Option<TierBand> {
        match self {
            ChosenBand::None => Option::None,
            ChosenBand::Small => Some(TierBand::Small),
            ChosenBand::Mid => Some(TierBand::Mid),
            ChosenBand::Large => Some(TierBand::Large),
        }
    }
}

impl From<Option<TierBand>> for ChosenBand {
    fn from(band: Option<TierBand>) -> Self {
        match band {
            Option::None => ChosenBand::None,
            Some(TierBand::Small) => ChosenBand::Small,
            Some(TierBand::Mid) => ChosenBand::Mid,
            Some(TierBand::Large) => ChosenBand::Large,
        }
    }
}

/// The probe's reasoning, rendered to the user before anything is fetched.
///
/// BR-2 is the whole point of this shape: a bare model name is not sufficient,
/// so the detected hardware and a plain-language `reason` travel with every
/// proposal. It carries machine *facts* only — never a path, a credential, or
/// file content (BR-11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReportView {
    /// Total physical RAM in bytes.
    pub total_ram_bytes: u64,
    /// Free disk in bytes, on the volume the weights would land on.
    pub free_disk_bytes: u64,
    /// Detected accelerator class.
    pub gpu_class: GpuClass,
    /// The band the decision table picked for this machine.
    pub chosen_band: ChosenBand,
    /// User-facing sentence explaining the band choice (BR-2 legibility).
    pub reason: String,
}

/// Where a catalog entry's bytes come from, shown at consent (H-2).
///
/// A projection of the pinned artifact's *origin* — publisher/repo, host, and the
/// short commit revision — so the user can see *from whom* and *from where* they
/// are about to download, not merely a model name. These are public provenance
/// facts (a repository id, a hostname, an abbreviated commit), never a credential,
/// a full URL, a local path, or file content, so BR-11 holds: it is exactly the
/// non-sensitive triple a person needs in order to trust the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProvenance {
    /// The publisher/repository, e.g. `Qwen/Qwen2.5-Coder-3B-Instruct-GGUF`.
    pub repo: String,
    /// The host serving the pinned artifact, e.g. `huggingface.co`. This is the
    /// *catalog's* host; a `[local_model] base_url` mirror or an override catalog
    /// redirects the actual fetch, surfaced separately by
    /// [`ModelSelectionProposed::fetch_notice`].
    pub host: String,
    /// The short (7-hex) commit the URL pins, e.g. `f74adce`. Abbreviated for
    /// display; the full 40-hex pin stays daemon-side (BR-11).
    pub revision: String,
}

/// A catalog entry as offered to the user.
///
/// A deliberate projection of the catalog row: the full `url` and the `sha256`
/// are daemon-side download mechanics the user is not choosing between, and no
/// install path ever appears (BR-11). What is left is what a person needs in
/// order to choose — the name, the band it serves, what it costs in disk, what it
/// needs in RAM, and its non-sensitive [`provenance`](Self::provenance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntryView {
    /// Catalog id, e.g. `qwen2.5-coder-3b`.
    pub name: String,
    /// The hardware band this model serves.
    pub band: TierBand,
    /// Download size in bytes.
    pub size_bytes: u64,
    /// Minimum system RAM required to load it. Choosing an entry whose floor
    /// exceeds [`ProbeReportView::total_ram_bytes`] is permitted but needs a
    /// second, explicit confirmation (BR-3).
    pub ram_floor_bytes: u64,
    /// Where the bytes come from (H-2): publisher/repo, host, short revision.
    /// Present on every entry so the consent screen can always show the source,
    /// not only the name.
    pub provenance: CatalogProvenance,
}

/// The entry the daemon proposes, plus what installing it will take.
///
/// The two travel together so a proposal can never carry a disk requirement
/// belonging to no model, or a model with no stated cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedModel {
    /// The proposed catalog entry.
    pub entry: CatalogEntryView,
    /// Free disk the install needs: the download size plus the working margin
    /// the preflight check applies before fetching a byte (BR-7).
    pub required_disk_bytes: u64,
}

/// A notice that the fetch is redirected away from the provenance host each entry
/// shows (H-2).
///
/// Set when a `[local_model] base_url` mirror or a non-bundled catalog
/// (`TETON_CATALOG`) is in force. A redirected fetch the user cannot see from the
/// entry's [`CatalogProvenance`] alone is exactly where consent means least, so
/// when this is present the client MUST surface it before the user answers.
/// `None` means the bytes come from the host on each entry's provenance.
///
/// Carries a bare host, never a full base URL — no scheme, path, or userinfo — so
/// BR-11 holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchNotice {
    /// The mirror host serving the pinned artifact instead of the provenance
    /// host, e.g. `hf-mirror.corp.internal`. `None` when no mirror is configured
    /// (an override catalog is the reason for the notice).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mirror_host: Option<String>,
    /// True when a non-bundled catalog (`TETON_CATALOG`) replaced the shipped
    /// one, so the entries do not come from the catalog this build was released
    /// with.
    pub override_catalog: bool,
}

/// The daemon proposes a local model and waits (spec: `model_selection_proposed`).
///
/// Emitted after the probe and **before any download** (BR-1). The client answers
/// with [`crate::methods::ModelConfirmParams`], keyed by `request_id` — the same
/// correlation [`PermissionRequest`] uses. While the answer is outstanding
/// sessions still work; they simply run remote-only (D-3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectionProposed {
    /// Correlates with the client's later `model/confirm`.
    pub request_id: RequestId,
    /// The hardware reasoning that produced this proposal (BR-2).
    pub probe: ProbeReportView,
    /// The proposal, or `None` when no catalog entry fits this machine — in
    /// which case `probe.chosen_band` is `none` and the user may still override
    /// to an entry from `alternatives` (BR-3).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub proposed: Option<ProposedModel>,
    /// Every other entry the user may choose instead (BR-3). Excludes the
    /// proposed entry; may include entries above this machine's RAM floor, which
    /// the client must flag rather than hide.
    pub alternatives: Vec<CatalogEntryView>,
    /// Present when the fetch is redirected away from the entries' provenance
    /// host — a configured mirror or an override catalog (H-2). The client MUST
    /// surface it; a silent redirect is where consent means least.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fetch_notice: Option<FetchNotice>,
}

/// Where a model-selection decision came from (spec entity `ModelSelection.source`).
///
/// Mirrors `teton_core::entities::SelectionSource` variant-for-variant, so the
/// daemon's persisted record and this wire form cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    /// The probe's proposal, accepted as offered.
    Probe,
    /// The user chose a different catalog entry, or declined (BR-3/BR-4).
    UserOverride,
    /// A `[local_model] pinned` config key decided it, with no prompt (BR-9).
    ConfigPin,
    /// The explicit opt-in auto-accept path took it unattended (BR-5).
    AutoAccept,
}

/// A model-selection decision was recorded (spec: `model_selection_decided`).
///
/// Emitted for *every* decision, including the ones no human answered
/// (`config_pin`, `auto_accept`), so an attached client always learns why the
/// local tier is in the state it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectionDecided {
    /// The proposal this answers; `None` when no prompt was shown (a config pin
    /// or the auto-accept path).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub request_id: Option<RequestId>,
    /// The chosen catalog model name; `None` exactly when `declined_local`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_name: Option<String>,
    /// True when the local tier was declined: run remote-only and do not
    /// re-prompt on later starts (BR-4).
    pub declined_local: bool,
    /// How the decision was reached.
    pub source: SelectionSource,
}

// ---------------------------------------------------------------------------
// permission_request (ACP: session/request_permission)
// ---------------------------------------------------------------------------

/// The harness needs a permission decision (spec: `permission_request`).
///
/// The client replies with [`crate::methods::PermissionRespondParams`], keyed by
/// `request_id`. ACP: `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Correlates with the client's later response.
    pub request_id: RequestId,
    /// Tool the harness wants to run.
    pub tool_name: String,
    /// Human-facing description of the pending action.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// The choices offered to the user.
    pub options: Vec<PermissionOption>,
}

/// One offered permission choice. ACP: `PermissionOption`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// Stable id echoed back in the response's `option_id`.
    pub option_id: String,
    /// User-facing label.
    pub label: String,
    /// Semantic kind (drives default styling/shortcuts).
    pub kind: PermissionOptionKind,
}

/// The `option_id` of REQ-563's persistent-enable choice (BR-4).
///
/// Lives here, in the protocol, rather than as a private constant on each side:
/// the daemon offers it and the client selects it, and a fifth option told apart
/// **by id** is a string two crates have to agree on exactly. The other four ids
/// can stay private to the daemon because the client picks those by
/// [`PermissionOptionKind`]; this one cannot be, and that is the whole reason it
/// is public.
///
/// It carries [`PermissionOptionKind::AllowAlways`] on the wire — the ACP kind
/// enum is closed and none of its four variants means "and write it down". The
/// kind is therefore a *floor* on what the option does (it does at least allow
/// for the session), and the id is what distinguishes it. A client selecting by
/// kind alone can never reach it by accident, which is deliberate: writing
/// config is not a fallback for "allow for this session".
pub const OPTION_ID_ENABLE_PERMANENT: &str = "enable_permanent";

/// Semantic kind of a permission option. ACP: `PermissionOptionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this one time.
    AllowOnce,
    /// Allow and remember for the session.
    AllowAlways,
    /// Reject this one time.
    RejectOnce,
    /// Reject and remember for the session.
    RejectAlways,
}

// ---------------------------------------------------------------------------
// phase_transition
// ---------------------------------------------------------------------------

/// A structured-mode phase gate passed (spec: `phase_transition`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTransition {
    /// Phase left, or `None` when entering the first phase.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_phase: Option<Phase>,
    /// Phase entered.
    pub to_phase: Phase,
    /// ADLC artifacts carried across the gate.
    pub artifacts: Vec<TaskArtifactRef>,
}

/// A reference to an ADLC artifact (spec entity `TaskArtifact`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactRef {
    /// Owning requirement id.
    pub req_id: String,
    /// Phase that produced the artifact.
    pub phase: Phase,
    /// Repo-relative path to the artifact.
    pub path: String,
}

// ---------------------------------------------------------------------------
// daemon_client_attach
// ---------------------------------------------------------------------------

/// A client attached to the daemon (spec: `daemon_client_attach`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonClientAttach {
    /// The kind of client that attached.
    pub client_kind: ClientKind,
    /// Protocol version negotiated with that client.
    pub protocol_version: ProtocolVersion,
}

// ---------------------------------------------------------------------------
// daemon_lifetime (REQ-565)
// ---------------------------------------------------------------------------
//
// REQ-565's Events table names five events — client_connected,
// client_disconnected, daemon_shutdown_armed, daemon_shutdown_deferred,
// daemon_shutdown. They are realized as one variant carrying a
// [`DaemonLifetimeStage`], the same fold REQ-563's D-8 applied to the
// web-lookup vocabulary: five near-identical top-level variants would give
// every client five match arms for what is one story about one daemon.
//
// What is deliberately NOT on the wire: `conn_id`. The spec requires it to be
// unique per live connection, and the daemon does keep one, but broadcasting it
// would tell every attached client about the existence and identity of the
// others for no consumer benefit. The counts are what the acceptance criteria
// assert on, so the counts are what ships.

/// Work that must finish before the daemon may exit (REQ-565 BR-2).
///
/// Lives here rather than in `teton-core` because it is wire vocabulary — it is
/// the payload of `daemon_shutdown_deferred` — and one definition shared by the
/// decision logic and the event beats two definitions plus a drift test.
///
/// Ordering is load-bearing: `teton_core::lifetime` reports the lowest live
/// activity as *the* blocker, so declaration order decides which one a given
/// set names, and an event payload that reshuffles between runs is one nobody
/// can assert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingActivity {
    /// A prompt turn is executing.
    Turn,
    /// Model weights are downloading or being verified.
    ModelDownload,
    /// Model weights are being loaded or benchmarked.
    ModelLoad,
    /// Cost-ledger writes are outstanding.
    ///
    /// Declared because the spec's vocabulary names it, but structurally empty
    /// as things stand: the ledger is SQLite in autocommit, so a row is durable
    /// the moment `record` returns and there is no buffer to flush. What
    /// actually threatens ledger integrity is a turn killed before it records —
    /// which is why [`Self::Turn`] defers.
    LedgerFlush,
}

/// Why the daemon exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The last client disconnected — the REQ-565 path.
    LastClient,
    /// No client ever arrived within the startup grace.
    ///
    /// Not in the spec's `reason` enum (`last_client | signal`), and
    /// deliberately distinct from both: a daemon nobody ever talked to did not
    /// lose a last client, and reporting it as `last_client` would make the
    /// commonest orphan — a CLI killed during its own autostart poll — look
    /// like a normal session end in the logs.
    StartupUnclaimed,
    /// A signal asked the daemon to stop.
    Signal,
}

/// Which moment in the daemon's lifetime this event reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum DaemonLifetimeStage {
    /// A client completed its handshake (spec: `client_connected`).
    ClientConnected {
        /// Live connections after this one was admitted.
        live_connection_count: u32,
    },
    /// A client's socket closed, for any reason (spec: `client_disconnected`).
    ClientDisconnected {
        /// Live connections after this one left.
        live_connection_count: u32,
    },
    /// The last client left and a shutdown is pending (spec:
    /// `daemon_shutdown_armed`).
    ShutdownArmed {
        /// The policy that armed it, for diagnostics.
        policy: String,
        /// Seconds until exit under a linger policy; `0` for a strict
        /// exit-on-last-disconnect.
        linger_seconds: u64,
    },
    /// A pending shutdown is waiting on in-flight work (spec:
    /// `daemon_shutdown_deferred`).
    ShutdownDeferred {
        /// What is holding the daemon open.
        blocking_activity: BlockingActivity,
    },
    /// The daemon is exiting (spec: `daemon_shutdown`).
    Shutdown {
        /// Why.
        reason: ExitReason,
        /// How long the daemon ran.
        uptime_seconds: u64,
        /// Sessions closed during teardown.
        sessions_closed: u32,
    },
}

/// A moment in the daemon's lifetime (REQ-565).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonLifetime {
    /// Which moment.
    #[serde(flatten)]
    pub stage: DaemonLifetimeStage,
}

// ---------------------------------------------------------------------------
// web_lookup / web_consent_decided / web_taint_overridden (REQ-563)
// ---------------------------------------------------------------------------
//
// The opt-in web-lookup vocabulary. REQ-563's Events table names ten events;
// architecture D-8 realizes them with three variants, and the fold is written
// down here rather than left to be rediscovered from a diff:
//
//   * every way a lookup can *end* — completed, served from cache, refused by
//     either inspection, refused by the allowlist or by the tier ceiling,
//     restricted by session taint, or unreachable — is one `web_lookup` event
//     carrying a [`WebLookupOutcome`]. They share a subject (which kind, which
//     host, how many bytes came back) and differ only in the ending, which is a
//     field, not a type.
//   * the consent *prompt* gets no event: the web tool authorizes through the
//     existing [`PermissionRequest`] like every other tool (architecture D-5),
//     so only the *decision* needs a name of its own.
//   * `web_lookup_requested` has no wire event at all. A request is observable
//     at Ask-time (as a `permission_request`) or at its outcome; an event
//     between the two would announce an intention the very next inspection may
//     refuse, and a client rendering it would show a lookup that never happened.
//
// Every payload here names the destination **host** and nothing finer (BR-7 of
// this REQ, BR-7 of the charter). No full URL, no path, no query text, no
// credential — the same constraint that keeps [`BlockCause::Redaction`] from
// echoing what it found, applied to the one event family whose entire subject
// is an outgoing utterance.

/// The graded web-lookup capability a grant or a decision concerns (BR-3).
///
/// Mirrors `teton_core::config::WebTier` variant-for-variant — the precedent
/// [`crate::Tier`] and [`SelectionSource`] set — so the daemon's configured
/// ceiling and this wire form cannot drift apart.
///
/// Ordered lowest-to-highest, and the ordering is the rule rather than a
/// presentation detail: each tier includes the ones below it (BR-3), so
/// `granted >= requested` is the whole of the tier check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTier {
    /// No web lookup at all. The default, and the *only* disabled state —
    /// architecture D-9 drops the spec's separate `enabled` flag precisely so
    /// nothing can disagree with this value.
    Off,
    /// Fetch a URL that appeared verbatim in a user message of this session.
    FetchUserUrl,
    /// Fetch a URL the model composed.
    FetchAnyUrl,
    /// Free-text search against the user's configured endpoint.
    Search,
}

impl WebTier {
    /// Every tier, lowest first — so a sweep over the ladder cannot miss one a
    /// later REQ adds.
    pub const ALL: [WebTier; 4] = [
        WebTier::Off,
        WebTier::FetchUserUrl,
        WebTier::FetchAnyUrl,
        WebTier::Search,
    ];
}

/// What a lookup was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebLookupKind {
    /// A single-URL fetch of static content.
    Fetch,
    /// A free-text search against the configured endpoint.
    Search,
}

impl WebLookupKind {
    /// Both kinds, for sweeps.
    pub const ALL: [WebLookupKind; 2] = [WebLookupKind::Fetch, WebLookupKind::Search];
}

/// How a lookup ended — the fold of the spec's separate outcome events (D-8).
///
/// Every variant names the Events-table row it realizes, so the fold stays
/// checkable against the requirement instead of becoming folklore. A variant
/// added here without that sentence is a wire value no reader can trace back to
/// a promise.
///
/// Deliberately **not** split into "ok" and "error" families: a refusal is a
/// normal, expected ending for a capability whose whole design is refusing
/// (BR-9 — a lookup failure never fails the turn), so the endings live on one
/// axis and a consumer decides for itself which ones it draws as a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebLookupOutcome {
    /// The lookup went out and came back (spec: `web_lookup_completed`).
    Completed,
    /// Served from the local cache, so **no egress occurred** (spec:
    /// `web_cache_hit`). Recorded like any other lookup: BR-7 asks for every
    /// lookup in the ledger, including the free ones.
    CacheHit,
    /// The provenance gate refused the outgoing text: it derived from
    /// privacy-boundary content (spec: `web_lookup_blocked`, reason
    /// `privacy_block`). The paired [`PrivacyBlock`] event carries the detail;
    /// this outcome is what makes the *lookup* accountable in the ledger.
    BlockedPrivacy,
    /// The redaction scan refused the outgoing text — including the case where
    /// the scan could not run, which is a block and not a skip (spec:
    /// `web_lookup_blocked`, reason `redact_finding`; BR-14, LESSON-492).
    BlockedRedact,
    /// A model-chosen destination fell outside the configured allowlist (spec:
    /// `web_lookup_refused_domain`). Never reached by a user-pasted URL, which
    /// BR-11 exempts.
    RefusedDomain,
    /// The lookup needed a tier above the granted ceiling and was refused
    /// before any prompt (AC-4).
    ///
    /// The spec's table names no event for this one. A refusal that never
    /// reaches consent is invisible everywhere else — there is no
    /// `permission_request` to observe and no packet to capture — so the fold
    /// gives it a value rather than leaving AC-4's refusal unobservable.
    RefusedTier,
    /// A model-composed lookup was refused because this session has touched
    /// boundary content (spec: `web_taint_restricted`; BR-13). A user-pasted
    /// URL in the same session is unaffected.
    TaintRestricted,
    /// The destination was unreachable — a settled, transient-shaped failure,
    /// never a turn error (BR-9, BUG-152's taxonomy).
    ///
    /// The spec's table names no event for it because it is the failure sibling
    /// of `web_lookup_completed` and belongs on the same row: the same lookup,
    /// the same host, a different ending.
    Offline,
}

impl WebLookupOutcome {
    /// Every outcome, so a sweep (tests, a renderer's match) cannot miss one a
    /// later REQ adds.
    pub const ALL: [WebLookupOutcome; 8] = [
        WebLookupOutcome::Completed,
        WebLookupOutcome::CacheHit,
        WebLookupOutcome::BlockedPrivacy,
        WebLookupOutcome::BlockedRedact,
        WebLookupOutcome::RefusedDomain,
        WebLookupOutcome::RefusedTier,
        WebLookupOutcome::TaintRestricted,
        WebLookupOutcome::Offline,
    ];
}

/// A web lookup reached a terminal outcome (spec: the `web_lookup_*` family).
///
/// One event per lookup attempt, whatever the ending, so the ledger and the
/// stream agree on how many lookups a session performed. The session is named
/// by [`EventEnvelope::session_id`], as for every other session-scoped event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebLookup {
    /// Fetch or search.
    pub kind: WebLookupKind,
    /// The destination **host**, e.g. `docs.rs` — never the scheme, the path,
    /// the query string, or a credential (BR-7).
    ///
    /// A host is what makes a lookup accountable ("this session talked to
    /// `docs.rs`") without reproducing the utterance. For a
    /// [`WebLookupOutcome::CacheHit`] it is still the host the cached document
    /// came from, so a session's destinations read the same whether or not the
    /// bytes were already on disk.
    pub host: String,
    /// How it ended.
    pub outcome: WebLookupOutcome,
    /// Bytes of content the lookup brought back. `0` for every outcome that
    /// transferred nothing — a refusal, a block, or an unreachable host.
    pub bytes_in: u64,
    /// **Which** inspection refused a blocked lookup, in the same vocabulary a
    /// [`PrivacyBlock`] uses (REQ-563 BR-14's honesty half).
    ///
    /// `None` for every outcome that is not a block, and the field is omitted
    /// from the wire then — a client written before this existed reads the same
    /// bytes it always did.
    ///
    /// It exists because [`WebLookupOutcome::BlockedRedact`] folds two facts a
    /// user must act on differently: the scan *ran and refused the text*, and
    /// the scan *could not run at all* (no local model loaded, which is the
    /// ordinary state on a loaderless build). Told the first when the truth is
    /// the second, a user goes looking for a secret in a query that contained
    /// none, while the actual fix — install or load the local model the search
    /// tier depends on — is never named. The wire outcome stays at its fixed
    /// eight values (architecture D-8); this is the finer reading beside it, the
    /// same split [`LookupDetail`](../../tetond/egress/enum.LookupDetail.html)
    /// makes daemon-side.
    ///
    /// It carries no more than a `privacy_block` already does: a cause and, for
    /// a located finding, its kind and byte span. No query, no URL, no text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<BlockCause>,
}

/// How long a consent decision holds (spec BR-4's offered scopes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebConsentScope {
    /// This one lookup.
    Once,
    /// The rest of this session. Never written to config; resets with the
    /// session (BR-4).
    Session,
    /// Written to config — the only scope that outlives the daemon (BR-4), and
    /// therefore the only one that is a configuration change rather than a
    /// session fact.
    Persistent,
}

/// A web-lookup consent decision was recorded (spec: `web_consent_granted` and
/// `web_consent_denied`, folded).
///
/// One event with a `granted` flag rather than two events: both spec rows carry
/// the same subject — which tier, at which scope — and differ only in the
/// answer, so a client handling both has a boolean either way. The prompt that
/// preceded this is a [`PermissionRequest`] (architecture D-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebConsentDecided {
    /// The scope the decision applies to.
    ///
    /// A refusal is recorded at the scope it refuses, so "declined for this
    /// session" and "declined this once" stay distinguishable — the difference
    /// decides whether the user is asked again on the next lookup.
    pub scope: WebConsentScope,
    /// The tier the decision concerns. Never [`WebTier::Off`]: a decision is
    /// always about a capability someone asked for.
    pub tier: WebTier,
    /// Whether the tier was granted at that scope.
    pub granted: bool,
}

/// The user lifted this session's taint restriction (spec:
/// `web_taint_overridden`).
///
/// User-only by construction rather than by check: the override arrives as a
/// client RPC ([`crate::methods::WebOverrideParams`]) and tool dispatch has no
/// path to a client RPC, so a model-issued override is not *rejected* at
/// runtime — it is unrepresentable (architecture D-4, AC-12).
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`SessionTitled`] documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebTaintOverridden {
    /// The tiers model-composed lookups resume at: exactly the tiers this
    /// session had already been granted, ascending.
    ///
    /// The override *restores*; it never grants (BR-13), so a tier absent from
    /// this list stays absent, and [`WebTier::Off`] never appears here —
    /// "restored to nothing" is an empty list.
    pub tiers_restored: Vec<WebTier>,
}

// ---------------------------------------------------------------------------
// web capability state + web_setup_completed / web_setup_rejected /
// capability_dead_end (REQ-572)
// ---------------------------------------------------------------------------

/// What the web capability can actually do right now (REQ-572 BR-3, BR-10).
///
/// Mirrors `teton_core`'s `WebCapabilityState` the way [`WebTier`] mirrors
/// `teton_core::config::WebTier`, and for the same reason: the daemon derives
/// the state once, from the predicate that governs tool exposure, and this is
/// the shape that derivation travels in. Two surfaces describing one capability
/// must not be able to disagree (LESSON-456), and a state a client re-derives
/// from prose is a second derivation.
///
/// **Distinct values, not distinguishing prose** (BR-10): a client branches on
/// the variant — "off but available" is an offer to set it up, "search cannot
/// serve" is a named missing piece — and never on the wording.
///
/// # Why [`Self::SearchUnavailable`]'s reason is a `String`
///
/// It is the daemon's own sentence naming the missing piece, rendered verbatim.
/// The CLI shows it; it never branches on it. A structured gap enum would be a
/// vocabulary both ends have to agree on for a value whose only consumer is a
/// human reading a line — and the branch a client *does* take is already the
/// variant it is attached to.
///
/// # There is no `PartiallyConfigured`
///
/// `tier = "search"` with no `search_endpoint` cannot be observed at runtime:
/// `Config::validate()` refuses that document at load
/// (`WebSearchTierWithoutEndpoint`), so a running daemon never holds one. The
/// partially-configured experience lives at **preview** time, in
/// [`crate::methods::WebSetupPreviewResult::warnings`], where a candidate is
/// checked before it is ever written (architecture, Approach).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WebCapabilityState {
    /// The tier is above [`WebTier::Off`], so the web tool registers and the
    /// capability can serve. Carries the ceiling, because "on" is not one
    /// answer: a `fetch_user_url` session and a `search` session are told
    /// different things about what they can ask for.
    Ready {
        /// The configured ceiling. Never [`WebTier::Off`] — that is
        /// [`Self::OffAvailable`].
        tier: WebTier,
    },
    /// No `[web]` table, or `tier = "off"`: the capability ships in this binary
    /// and is one config table away. **The observed failure this REQ exists
    /// for** — a state that used to be indistinguishable from "impossible".
    OffAvailable,
    /// The tier permits search structurally, but the search leg cannot serve.
    SearchUnavailable {
        /// The daemon's sentence naming the missing piece, for rendering.
        reason: String,
    },
}

/// A guided setup flow committed a `[web]` config change (spec:
/// `setup_completed`; BR-14, AC-11).
///
/// Session-scoped: it is delivered under the committing session's
/// [`EventEnvelope::session_id`], like every other event a user's own command
/// produces. Bystander sessions pick the capability up on their next turn (the
/// daemon rebuilds its tool registry per turn) and read the new state from
/// their status surface rather than from this event — announcing another
/// session's command in this one's transcript is the cross-session noise
/// BUG-161 taught us to keep out.
///
/// It exists as an event and not only as the commit RPC's answer because BR-14
/// asks for the change to be **in front of a human** (LESSON-505): a second
/// client attached to the same session watched the capability change under it
/// and is owed the news.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupCompleted {
    /// The ceiling now written to config. Never [`WebTier::Off`]: a completed
    /// setup enabled something.
    pub tier: WebTier,
    /// The config file the write landed in, so the user can go read what they
    /// just agreed to. A path the user already owns and typed their way to —
    /// not a secret, and never the key that may now sit beside it (BR-6: the
    /// value is in the keychain and the config holds a reference).
    ///
    /// **Kept on the wire deliberately, monitor-scope receivers included**
    /// (BUG-166 residual (b) — a decision, not an oversight). Every
    /// connection on this daemon's socket is same-UID, a monitor additionally
    /// holds a consent-granted scope, and the path itself is derivable by any
    /// such peer (`teton status` serves it; the state-dir convention names
    /// it). Blanking it here would protect nothing from the only parties who
    /// can receive it, while breaking the field's one promise for any client
    /// that renders it. The real exposure — an absolute home path, and
    /// therefore a username, on *somebody else's screen* — is a rendering
    /// concern and is handled at the renderer: the CLI deliberately does not
    /// print it (`format_web_setup_completed`).
    pub config_path: String,
}

/// A setup call was refused because it did not come from the user (spec:
/// `setup_rejected_nonuser`; BR-4, AC-4).
///
/// Defense in depth, announced rather than logged. The gate that produced it
/// answers the caller with `NOT_ATTACHED`; this event is what tells the
/// **session's own user** that something else tried, which a log line an
/// adversary can rotate away does not (LESSON-505).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupRejected {
    /// What the refused caller was, in the daemon's own words — an unattached
    /// connection, a monitor, a tool call. Rendered and never branched on, for
    /// the reason [`WebCapabilityState::SearchUnavailable`]'s `reason` is a
    /// string: the client's only job with it is to show it.
    ///
    /// It names a **kind**, never an identity: no pid, no socket peer, no
    /// credential. A refusal notice that fingerprinted the caller would put
    /// data in a transcript that the refusal itself exists to keep out.
    pub origin: String,
}

/// A guided provider-setup flow committed a registration (spec:
/// `provider_setup_completed`; REQ-579 BR-15).
///
/// Session-scoped through the [`EventEnvelope`], like every other event a
/// user's own command produces — the payload carries no `session_id` of its own
/// for [`WebSetupCompleted`]'s structural reason: the envelope flattens over the
/// payload, so a second `session_id` here would be a duplicate key on the wire.
///
/// It exists as an event and not only as the commit RPC's answer because the
/// interactive surface has to be able to print "registered; `think` now routes
/// to it" without polling (BR-15), and because a second client attached to the
/// same session watched routing change under it and is owed the news
/// (LESSON-505).
///
/// **What it deliberately does not carry**: the key, obviously (BR-2, and there
/// is nowhere here to put one), and also the endpoint — whose authority can
/// carry userinfo a pasted URL smuggled in. A client that wants the address
/// reads it from config; an event that repeated it would put a credential-shaped
/// string into a transcript for no gain. The config path is not carried either:
/// unlike [`WebSetupCompleted`], nothing about a provider registration needs the
/// user sent to the file to understand what changed.
///
/// [`dial_host`](Self::dial_host) is the deliberate exception to that second
/// rule and not a softening of it: a host is not an endpoint. It is read from
/// the parser that dials, so it carries no userinfo, no path and no query by
/// construction (LESSON-529) — which is exactly why the *endpoint* may not
/// travel here and the host may. An explicit `:port` rides with it, being part
/// of the destination rather than part of what a credential could hide in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupCompleted {
    /// The id now registered — the one fact the model legitimately learns from
    /// this flow (BR-2): that a provider with this id exists, and nothing else.
    pub provider_id: ProviderId,
    /// Which adapter it speaks.
    pub kind: ProviderKind,
    /// The model it is pinned to. Present, not optional: a commit that landed
    /// registered a candidate whose model was required (REQ-579 BR-6), so there
    /// is no completed setup with nothing to name here.
    pub model: String,
    /// The tier bindings that landed with it. Empty is a legitimate and stated
    /// outcome — the registered-but-unrouted provider BR-7 permits — so a
    /// renderer must read an empty list as "nothing routes to it yet" rather
    /// than as a missing field.
    #[serde(default)]
    pub bindings: Vec<TierBinding>,
    /// The host this provider will be dialed at — the **dial-time** parser's
    /// reading of the endpoint that was written (BR-5, LESSON-529), and the
    /// same string the confirm step showed.
    ///
    /// The announcement is otherwise silent about where turns will now go: a
    /// second client attached to this session watched routing move under it
    /// (the reason this event exists at all) and is owed the destination, not
    /// only the id. Host, plus `:port` when the endpoint states one explicitly;
    /// never userinfo, path or query — and never the endpoint, for the type's
    /// own reason above. The port is on the destination side of that line: a
    /// registration on `:8443` announced as the bare host names a different
    /// socket in the familiar socket's words.
    ///
    /// `#[serde(default)]` so a client built after this field still reads an
    /// older daemon's frame; empty means "this daemon did not say".
    #[serde(default)]
    pub dial_host: String,
}

/// A provider-setup **commit** was refused because it did not come from the
/// user (spec: `provider_setup_rejected_nonuser`; REQ-579 BR-12).
///
/// Defense in depth, announced rather than logged: the gate answers the caller
/// in the response, and this event is what tells the **session's own user** that
/// something else tried, which a log line an adversary can rotate away does not
/// (LESSON-505).
///
/// **The commit arm only.** `provider/setup_plan` and `provider/setup_preview`
/// refuse in-response and publish nothing (BR-12, LESSON-513): they are
/// read-only and attacker-paced, so publishing on them would hand an
/// unauthorized caller a way to fill a user's transcript at will.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupRejected {
    /// Which method was refused, by its wire name — e.g.
    /// `provider/setup_commit`.
    ///
    /// It names a **method**, never an identity: no pid, no socket peer, no
    /// credential, and not the candidate the caller sent. A refusal notice that
    /// fingerprinted the caller — or echoed what it tried to register — would
    /// put data in a transcript that the refusal itself exists to keep out
    /// ([`WebSetupRejected::origin`]'s rule, applied to this flow rather than
    /// assumed inherited from it: LESSON-525).
    ///
    /// A `String` rather than an enum, for [`CapabilityDeadEnd::capability`]'s
    /// reason: a client built before a method existed must be able to report a
    /// refusal it has never heard of rather than fail to parse the frame.
    pub method: String,
}

/// A turn dead-ended on a capability that is off or unconfigured (spec:
/// `capability_dead_end`).
///
/// Emitted only where the daemon can actually see the dead end (architecture
/// ADR-4): the unserved-turn path when routing wanted a tier nothing is
/// configured for, and the web tool's tier-gap refusals. A prose-only refusal
/// by the model — the fully-off case, where the tool does not exist to be
/// called — emits nothing, because classifying model-authored prose as "that
/// was a capability refusal" would be a second classifier over text
/// (LESSON-456). The per-state prompt clause is the mitigation there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDeadEnd {
    /// The capability catalog id, e.g. `web_search` or `remote_provider`.
    ///
    /// A `String` rather than an enum for the same reason the catalog is
    /// bundled text: the set is small, static, and rendered — and a client
    /// built before a capability existed must be able to report a dead end it
    /// has never heard of rather than fail to parse the frame.
    pub capability: String,
}

impl CapabilityDeadEnd {
    /// A turn needed a remote provider and none is configured (REQ-572 AC-2).
    ///
    /// The **settled** absence only: a provider registered without a model, an
    /// unset `default_provider` and a routing mismatch are all configured
    /// remote tiers whose remedy the turn's own error sentence already names,
    /// and a tier that is merely still warming is not a dead end at all.
    pub const REMOTE_PROVIDER: &'static str = "remote_provider";
    /// A web lookup dead-ended on the `[web]` capability — the tool refused a
    /// tier the configured ceiling does not reach.
    ///
    /// Named here beside [`Self::REMOTE_PROVIDER`] rather than spelled as a
    /// literal at the emission site, so the daemon and the client that renders
    /// the id cannot come to hold two spellings of one capability.
    pub const WEB_SEARCH: &'static str = "web_search";
    /// A lookup dead-ended at the `fetch_user_url` tier — the tool refused a
    /// fetch of a URL the user themselves pasted, because the configured ceiling
    /// does not reach even that.
    ///
    /// Here for [`Self::WEB_SEARCH`]'s reason, and here *now* because the reason
    /// was only half-served while one of the three tiers had a constant and the
    /// other two did not: the emission site derives the id from the tier
    /// (`permission_key_for`), so all three ids are already in the wire
    /// vocabulary — two of them only as strings nothing pins. A rename of either
    /// would have gone silently past every test.
    pub const WEB_FETCH_USER_URL: &'static str = "web_fetch_user_url";
    /// A lookup dead-ended at the `fetch_any_url` tier — a fetch of a URL the
    /// *model* chose, refused by the configured ceiling. See
    /// [`Self::WEB_FETCH_USER_URL`].
    pub const WEB_FETCH_ANY_URL: &'static str = "web_fetch_any_url";
}

// ---------------------------------------------------------------------------
// turn_queued (REQ-580)
// ---------------------------------------------------------------------------

/// A prompt turn is being **held**, not refused (spec: `turn_queued`; REQ-580
/// BR-2).
///
/// Before REQ-580 a turn that resolved to the local tier while that tier was
/// still coming up was refused with `TIER_WARMING` and a sentence ending
/// "Retry in a moment" — a retry the user then had to type. Now the daemon does
/// the waiting: the turn is held until the tier settles, and then run exactly
/// as if it had been sent that moment. This event is the hold being announced,
/// so the client can say so instead of showing a silent gap.
///
/// Emitted **once per held turn**, at the moment the hold begins, and only when
/// there is genuinely something to wait for — the two transient states BUG-152
/// named (see [`TierWarming`]). A settled absence (declined, below the floor,
/// a failed load, an unanswered proposal) still refuses immediately with the
/// sentence that names its remedy; nothing about that changed. There is no
/// paired "released" event: the turn's own progress — its `route_decided`, its
/// streamed reply, or its refusal — is what follows.
///
/// Session-scoped, like every event a user's own turn produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnQueued {
    /// The turn being held — the same id the eventual `session/prompt` result
    /// carries, so a client can pair the notice with the reply it precedes.
    pub turn_id: TurnId,
    /// The model whose tier the turn is waiting on. A catalog name, never a
    /// path (REQ-547 BR-11).
    pub model_id: String,
    /// What the tier is doing while the turn waits.
    pub waiting_on: TierWarming,
}

/// The two states of the local tier that **end on their own** — the only two a
/// turn is ever held for (REQ-580 BR-1; BUG-152's transient pair).
///
/// **Distinct values, not distinguishing prose**: a client branches on the
/// variant to say "finishes installing" or "finishes loading" and never on a
/// sentence. The daemon derives the value from the same classification that
/// codes a refusal `TIER_WARMING`, so the two surfaces cannot disagree about
/// which state the tier is in (LESSON-456).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierWarming {
    /// The model was accepted and its download/install is running.
    Installing,
    /// The weights are installed and verified; the daemon is loading and
    /// benchmarking them.
    Loading,
}

// ---------------------------------------------------------------------------
// provider_tested (REQ-581)
// ---------------------------------------------------------------------------

/// A user's connection test finished (spec: `provider_tested`; REQ-581 BR-3).
///
/// Session-scoped through the [`EventEnvelope`], like every other event a
/// user's own command produces — the payload carries no `session_id` of its own
/// for [`ProviderSetupCompleted`]'s structural reason: the envelope flattens
/// over the payload, so a second one here would be a duplicate key on the wire.
///
/// Published on **every** outcome the call reaches, not only the good one: the
/// test either spent or failed, and either way the health map moved under a
/// second client attached to the same session, which is owed the news
/// (LESSON-505). The refusals that never call announce nothing — a connection
/// that may not drive the session is turned away in the response and publishes
/// no event, because a probe is read-shaped and attacker-paced and publishing
/// on it would hand an unauthorized caller a way to fill a user's transcript
/// (LESSON-513; only commits announce their refusals).
///
/// **What it deliberately does not carry**: the credential, obviously, and the
/// endpoint — whose authority can hide userinfo. It does not repeat the model
/// or the dial host either, unlike [`ProviderSetupCompleted`], and the
/// difference is what the two events are for. A setup event announces a *config
/// change*, so the destination that just became live is news; a test changes no
/// config, so the model and host are whatever config already said and the
/// invoking client has them in its [`crate::methods::ProviderTestResult`]. What
/// is new here is the outcome and where health landed.
///
/// The [`outcome`](Self::outcome)'s `reason` is safe to carry for the reason it
/// exists: it is the daemon's own sentence, built from the status, the dial
/// host, the configured model and the credential *reference* — never a vendor's
/// response body, and never a key (architecture ADR-3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTested {
    /// The provider that was tested.
    pub provider_id: ProviderId,
    /// What came back, typed (BR-3).
    ///
    /// Nested rather than `#[serde(flatten)]`ed — the shape
    /// [`PrefixCache::outcome`] took — so it is byte-identical to
    /// [`crate::methods::ProviderTestResult::outcome`]: the client renders the
    /// event and the RPC answer with one function, and a flattened copy would
    /// be a second shape of one value for a renderer to get subtly different.
    pub outcome: ProviderTestOutcome,
    /// Where the test left the provider's health — the same map the router
    /// reads at decision time (BR-4), so a listening client learns what the
    /// next turn will do and not only what this call did.
    pub health_after: ProviderHealth,
}

// ---------------------------------------------------------------------------
// prefix_cache
// ---------------------------------------------------------------------------

/// A local agent turn's prefix-cache outcome (REQ-564).
///
/// One variant with an outcome enum, following [`WebLookup`]'s precedent rather
/// than minting three near-identical events: hit, miss and eviction are three
/// ways one thing ends, and a client that renders the event renders all three
/// or none.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`SessionTitled`] and [`WebTaintOverridden`] document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixCache {
    /// The local model whose context holds (or held) the prefix.
    pub model: String,
    /// How this turn's interaction with the cache ended.
    ///
    /// Flattened, so the outcome tag and its fields sit beside `model` on the
    /// wire rather than nesting under an `outcome` object — the same flat shape
    /// [`EventEnvelope`] gives every other event, and what a client reading
    /// `wire["outcome"]` expects.
    #[serde(flatten)]
    pub outcome: PrefixCacheOutcome,
}

/// How a turn's prefix-cache interaction ended (REQ-564 Events).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PrefixCacheOutcome {
    /// The turn shared a non-empty prefix with the resident KV and prefilled
    /// only what changed.
    Hit {
        /// Prompt tokens whose KV was reused.
        cached_tokens: u64,
        /// Prompt tokens actually prefilled this turn.
        new_tokens: u64,
        /// Whether reuse was capped by a token disagreement rather than by
        /// prompt length (BR-2 as amended 2026-08-10) — history was rewritten
        /// past the reuse point (compaction, a BUG-147 fabrication cut) and
        /// this turn re-prefilled the rewritten tail.
        ///
        /// `default` so events recorded before the amendment still
        /// deserialize; absent means `false`, which is what those events
        /// meant — the old rule never produced a divergent hit.
        #[serde(default)]
        divergent: bool,
    },
    /// The turn prefilled from position zero.
    Miss {
        /// Why the cache did not serve — never an error, and never a guess
        /// (BR-8).
        reason: PrefixCacheMiss,
        /// Prompt tokens prefilled, i.e. the whole prompt.
        processed_tokens: u64,
    },
    /// The resident prefix was dropped.
    ///
    /// Reported rather than silent: a cache that vanishes without a word is
    /// indistinguishable from one that was never warm, and BR-4 asks for
    /// silent *degradation*, not silent *eviction*.
    Evicted {
        /// What took the memory back.
        reason: EvictionReason,
    },
}

/// Why a turn could not reuse the resident prefix (REQ-564 BR-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefixCacheMiss {
    /// Nothing was resident.
    Cold,
    /// The resident prefix belonged to another session (BR-3's single slot).
    SessionSwitch,
    /// Same session, but nothing was reusable — the streams disagreed at the
    /// very first token, or the prompt was too short to reuse anything (BR-2,
    /// as amended: a *mid-stream* disagreement is a hit carrying
    /// `divergent: true`, not a miss).
    Divergent,
    /// The prefix had been dropped before this turn.
    Evicted,
}

/// Why a resident prefix was dropped (REQ-564 BR-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionReason {
    /// The runtime asked for the memory back.
    MemoryPressure,
    /// The engine is being unloaded or swapped for another model.
    EngineUnload,
    /// A generation failed partway, so the resident KV no longer provably
    /// matches the recorded prefix.
    GenerationFailed,
}

// ---------------------------------------------------------------------------
// context_cleared
// ---------------------------------------------------------------------------

/// A session's retained conversation was cleared (REQ-567 BR-8).
///
/// Published on every accepted `session/clear`, including one that dropped
/// nothing — deliberately *not* [`WebTaintOverridden`]'s transition-only rule.
/// That event announces a state change with consequences, so a re-lift
/// announces nothing; this one announces the **user's action**, and every
/// attached client has to stop describing a conversation the next prompt will
/// not carry. A clear of an already-empty session ends in exactly that state,
/// and `blocks_dropped` is what tells the two apart.
///
/// ## What it does not mean
///
/// **The conversation only** (REQ-567 OQ-4, resolved). The session's privacy
/// taint, its user-pasted-URL set, and its remembered permission grants all
/// survive a clear, so a client must never render this as a consent or egress
/// reset: a routinely-typed clear that silently widened either would be
/// LESSON-495's harm, and a grant is only as narrow as its key.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`SessionTitled`], [`WebTaintOverridden`] and [`PrefixCache`] document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCleared {
    /// How many retained blocks went, `0` when there was nothing to drop.
    ///
    /// Blocks rather than tokens: the conversation is stored as blocks, so this
    /// is the one count the daemon can state exactly rather than estimate.
    pub blocks_dropped: u64,
}

// ---------------------------------------------------------------------------
// attach_consent_requested / attach_refused (REQ-569)
// ---------------------------------------------------------------------------

/// What a consent request — and the grant it may mint — is *for* (REQ-569
/// BR-2, ADR-D).
///
/// The wire half of the daemon's grant scope. Kept as its own enum rather than
/// a boolean because the two are never interchangeable: an attach grant opens
/// one named session, a monitor grant is sight of every session there is and
/// every session there will be. A client that rendered both prompts with one
/// sentence would be asking the user to approve the wrong thing half the time
/// (LESSON-495 — the key encodes the whole question, and so must the prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentScope {
    /// `session/attach` against the session named by
    /// [`EventEnvelope::session_id`].
    Attach,
    /// The `monitor` declaration — every session's events, present and future.
    Monitor,
}

/// The daemon is asking a user whether to let a connection in (REQ-569 BR-6,
/// ADR-E).
///
/// Raised when a connection that neither created the session nor holds a grant
/// asks to attach, and when a connection asks to `monitor` without a
/// monitor-scope grant. Answered with `attach/consent` by `request_id`, exactly
/// as a `permission_request` is answered by `permission/respond`. An unanswered
/// request defaults **closed** after the daemon's bounded window (BR-7), so a
/// client that renders this and never replies costs the requester a refusal,
/// never a grant.
///
/// The session is named by [`EventEnvelope::session_id`] rather than by a field
/// here — the flatten rule [`ContextCleared`] documents — and is absent
/// entirely for [`ConsentScope::Monitor`], which names no single session
/// because it asks for all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachConsentRequested {
    /// Correlates this prompt with the `attach/consent` that answers it.
    pub request_id: RequestId,
    /// What is being asked for.
    pub scope: ConsentScope,
    /// A short, non-sensitive description of who is asking — the client kind
    /// and the name it gave at the handshake.
    ///
    /// **Deliberately not identity and deliberately not a path.** It carries no
    /// pid, no executable path, no environment and no command line: a consent
    /// prompt is rendered to a user, and everything in it is a string an
    /// unprivileged same-UID peer chose. The daemon bounds its length and strips
    /// control characters before it is published (REQ-568's monitor-log
    /// precedent), so a requester cannot forge extra lines in whatever surface
    /// renders it — but a client must still treat it as untrusted text and never
    /// as an authorization fact. The authorization fact is the ancestry gate the
    /// daemon already applied.
    pub requester: String,
}

/// Why an attach or monitor request was refused (REQ-569 BR-5).
///
/// Stable wire names so a client renders from the code rather than from prose
/// (BUG-152). Deliberately three, not one: they have three different remedies —
/// ask a user who is looking at another client, ask again because the user said
/// no, or ask again because nobody answered in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachRefusedReason {
    /// No grant, and no consent could be raised — for `monitor`, that means no
    /// connection was attached anywhere to ask (the daemon never approves a
    /// monitor to the requester's own face).
    NoGrant,
    /// A user was asked and said no.
    ConsentDenied,
    /// A user was asked and the bounded window elapsed unanswered. Resolves to
    /// denied, and mints nothing (BR-7).
    ConsentTimeout,
    /// The connection that asked went away before anyone answered (REQ-569
    /// verify, F3).
    ///
    /// Its own error response reaches nobody — there is no longer a socket to
    /// write it to — so this exists entirely for the *other* end: the surface
    /// that rendered the prompt has a security dialog on screen asking about a
    /// connection that no longer exists, and without this it stays there until a
    /// user answers a question about nobody.
    ///
    /// Deliberately not folded into [`Self::ConsentTimeout`]. A timeout says a
    /// user was asked and did not answer in time, which is a fact about the
    /// user; this says the asker left, which is a fact about the peer — and a
    /// client that reported "you were too slow" for a request nobody was still
    /// waiting on would be telling the user something false about their own
    /// behaviour.
    RequesterGone,
}

/// An attach or monitor request ended in a refusal (REQ-569 BR-5).
///
/// The *observability* half of the refusal: the requester learns the outcome
/// from its own error response, and this is what tells the surface that
/// rendered the prompt how it ended, so a consent prompt does not sit on a
/// user's screen after the window closed.
///
/// Not published for an ancestry refusal ([`crate::jsonrpc::error_code::ATTACH_FORBIDDEN`]).
/// That one is terminal and raises no prompt, so there is no prompt to retire —
/// and announcing it would let a daemon-spawned child make itself heard on
/// every attached client's stream by probing.
///
/// The session, when there is one, is named by [`EventEnvelope::session_id`]
/// (the flatten rule again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRefused {
    /// The request this ends, when a request was raised at all.
    ///
    /// The correlation key, and the reason this field exists rather than the
    /// session alone: two connections can have a prompt outstanding for the
    /// same session at once, and a surface rendering both has to know *which*
    /// one to retire. `None` for [`AttachRefusedReason::NoGrant`], where the
    /// daemon refused without ever raising one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub request_id: Option<RequestId>,
    /// What was being asked for.
    pub scope: ConsentScope,
    /// Which refusal this is.
    pub reason: AttachRefusedReason,
}

/// The daemon minted a session grant (REQ-569 verify, F6).
///
/// # Why this is on the wire and not only in the log
///
/// Minting a grant is the one act on this seam that widens who can see and
/// drive a session, and until now the only record of the riskiest way it
/// happens — a connection approving its own request, because nobody was
/// attached to ask — was a sentence on the daemon's stderr. That stream is read
/// on startup failure and almost never otherwise, is truncated by the CLI's
/// spawn path, and is same-uid writable, so the process that self-approved can
/// erase the evidence. This event is in-perimeter, unsuppressable by the
/// requester, and delivered to a human who is looking at a screen now.
///
/// **Daemon-scoped**: [`EventEnvelope::session_id`] is `None`, so REQ-568's
/// delivery rule broadcasts it to every handshaked connection rather than to
/// the session's attachees. That is deliberate — the point is that somebody
/// *else* sees it — and it is also why no session id appears anywhere in the
/// payload: an announcement that reaches every connection must not carry an id
/// BR-10 keeps from most of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrantMinted {
    /// What the grant opens.
    pub scope: ConsentScope,
    /// Who it was minted for — the same untrusted, daemon-bounded descriptor
    /// [`AttachConsentRequested::requester`] carries, and to be treated the same
    /// way: a hint, never an identity.
    pub requester: String,
    /// Who approved it — the answering connection's descriptor, bounded and
    /// stripped exactly like [`Self::requester`], and exactly as untrusted
    /// (REQ-569 re-verify, R1).
    ///
    /// **This is the field that shows self-dealing**, and it exists because
    /// [`Self::self_approved`] does not. One actor holding two connections has
    /// X approve Y's attach: two different connection ids, so the flag is
    /// `false` and the announcement reads as an ordinary peer approval. What
    /// gives that away is the *relation* between the two parties, so the
    /// announcement carries both parties rather than a verdict about them — a
    /// reader who sees the same name asked and answered has something to act on.
    ///
    /// Matching descriptors are evidence, never proof: the string is peer-chosen
    /// and two honest clients may well spell themselves the same way. A reader
    /// is being handed the relation, not a decision.
    pub approver: String,
    /// Whether the connection that asked is the *same connection* that approved
    /// (REQ-569 BR-6's second arm).
    ///
    /// `true` is one accepted residual made visible: nobody was attached to the
    /// target session, so the prompt was rendered at the requester and the
    /// requester answered it. For a person resuming their own session that is
    /// the intended flow; for a headless same-UID process it means no human was
    /// involved at all, and the daemon cannot tell the two apart.
    ///
    /// **`false` is not a clean bill of health**, and reading it as one is the
    /// blindness R1 records: it is a fact about connection ids, so an attacker
    /// who holds two connections and answers its own request with the second one
    /// is announced with `self_approved: false`. Use [`Self::approver`] for the
    /// question "did somebody else really decide this".
    pub self_approved: bool,
    /// How many announcements the daemon's per-connection bound dropped since
    /// the last one that got through (REQ-569 re-verify, R3). `0` in the
    /// ordinary case.
    ///
    /// Minting a grant is attacker-triggerable — a peer loops `session/attach`
    /// and self-approves — and this event is daemon-scoped, so every unbounded
    /// announcement is a line on every connected client's screen. The daemon
    /// rate-limits it per requesting connection and reports the arrears here, so
    /// the bound never costs a reader the knowledge that something was
    /// suppressed: a burst is one notice that says how much it stands for
    /// instead of a thousand notices that scroll the real one away.
    pub suppressed: u32,
    /// What verified a human's presence behind this grant (REQ-570 BR-9, AC-9).
    ///
    /// `"os_biometric"`, `"os_credential"`, or `"none"`. This is the field that
    /// makes [`Self::self_approved`]'s blindness recoverable: R1 records that a
    /// `false` there is not a clean bill of health, because an attacker holding
    /// two connections is announced as an ordinary peer approval. A human had to
    /// be at the machine for anything other than `"none"` to appear here, so an
    /// operator can tell an **attested** grant from one that merely had two
    /// connection ids involved.
    ///
    /// `"none"` is carried explicitly rather than omitted, and the distinction
    /// matters: a missing field and "no attestation" must not be the same wire
    /// shape, or a client reading an older daemon's event would silently read
    /// unattested grants as attested ones. Since REQ-570 the only grants that
    /// reach `"none"` are ones minted on a path that requires no attestation at
    /// all.
    pub attestation: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
    }

    /// A representative first-run proposal: a 32 GiB Apple Silicon machine, a
    /// mid-band pick, and one smaller alternative.
    fn sample_proposal() -> ModelSelectionProposed {
        ModelSelectionProposed {
            request_id: RequestId::from("m1"),
            probe: ProbeReportView {
                total_ram_bytes: 32 * 1024 * 1024 * 1024,
                free_disk_bytes: 200 * 1024 * 1024 * 1024,
                gpu_class: GpuClass::AppleSilicon,
                chosen_band: ChosenBand::Mid,
                reason: "32 GB of RAM clears the 7B band's floor with headroom to spare".to_owned(),
            },
            proposed: Some(ProposedModel {
                entry: CatalogEntryView {
                    name: "qwen2.5-coder-7b".to_owned(),
                    band: TierBand::Mid,
                    size_bytes: 4_700_000_000,
                    ram_floor_bytes: 12_884_901_888,
                    provenance: CatalogProvenance {
                        repo: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF".to_owned(),
                        host: "huggingface.co".to_owned(),
                        revision: "13fb94b".to_owned(),
                    },
                },
                required_disk_bytes: 5_700_000_000,
            }),
            alternatives: vec![CatalogEntryView {
                name: "qwen2.5-coder-3b".to_owned(),
                band: TierBand::Small,
                size_bytes: 2_000_000_000,
                ram_floor_bytes: 5_368_709_120,
                provenance: CatalogProvenance {
                    repo: "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF".to_owned(),
                    host: "huggingface.co".to_owned(),
                    revision: "f74adce".to_owned(),
                },
            }],
            fetch_notice: None,
        }
    }

    /// Wraps an event, round-trips the envelope, and returns the wire object so
    /// callers can assert on the `event` tag.
    fn envelope_wire(event: Event) -> serde_json::Value {
        let env = EventEnvelope::new(1, Some(SessionId::from("s1")), event);
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn envelope_is_flat_and_tagged_by_event_name() {
        let wire = envelope_wire(Event::RouteDecided(RouteDecided {
            category: Some(Category::Design),
            tier: Some(Tier::Think),
            phase: Some(Phase::Architect),
            provider_id: ProviderId::from("anthropic"),
            model: Some("opus".to_owned()),
            reason: "architecture phase routes to the frontier tier".to_owned(),
            effort: Some(ResolvedEffort::effort(crate::effort::EffortLevel::Xhigh)),
        }));
        // Flattened: envelope metadata and the payload share one object.
        assert_eq!(wire["event"], "route_decided");
        assert_eq!(wire["seq"], 1);
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["provider_id"], "anthropic");
        assert_eq!(wire["category"], "design");
        assert_eq!(wire["tier"], "think");
        // REQ-559 AC-4: the event names the effective effort.
        assert_eq!(wire["effort"]["kind"], "effort");
        assert_eq!(wire["effort"]["level"], "xhigh");
    }

    #[test]
    fn event_names_match_the_spec_events_table() {
        // The six names fixed by REQ-544's Events table, plus the three
        // streaming/lifecycle events. `name()` must equal the serialized tag.
        let cases: Vec<(Event, &str)> = vec![
            (
                Event::RouteDecided(RouteDecided {
                    category: None,
                    tier: None,
                    phase: None,
                    provider_id: ProviderId::from("p"),
                    model: None,
                    reason: "r".to_owned(),
                    effort: None,
                }),
                "route_decided",
            ),
            (
                Event::PrivacyBlock(PrivacyBlock {
                    path: "secret.txt".to_owned(),
                    provider_id: ProviderId::from("p"),
                    action: PrivacyAction::Stripped,
                    cause: BlockCause::Boundary,
                }),
                "privacy_block",
            ),
            (
                Event::PhaseTransition(PhaseTransition {
                    from_phase: Some(Phase::Spec),
                    to_phase: Phase::Architect,
                    artifacts: vec![],
                }),
                "phase_transition",
            ),
            (
                Event::CostRecorded(CostRecorded {
                    record: CostRecord {
                        session_id: SessionId::from("s"),
                        phase: None,
                        category: None,
                        provider_id: ProviderId::from("p"),
                        model: "m".to_owned(),
                        input_tokens: 1,
                        output_tokens: 2,
                        usd_micros: 1234,
                        cached_tokens: None,
                        reasoning_tokens: None,
                        probe: false,
                    },
                }),
                "cost_recorded",
            ),
            (
                Event::ProviderDegraded(ProviderDegraded {
                    provider_id: ProviderId::from("p"),
                    failure_class: FailureClass::Timeout,
                    fallback_id: Some(ProviderId::from("q")),
                }),
                "provider_degraded",
            ),
            (
                Event::DaemonClientAttach(DaemonClientAttach {
                    client_kind: ClientKind::Cli,
                    protocol_version: crate::PROTOCOL_VERSION,
                }),
                "daemon_client_attach",
            ),
            (
                Event::SessionUpdate(SessionUpdate {
                    update: SessionUpdatePayload::AgentMessageChunk {
                        text: "hi".to_owned(),
                    },
                }),
                "session_update",
            ),
            (
                Event::SessionTitled(SessionTitled {
                    title: "wire the unreached categories".to_owned(),
                }),
                "session_titled",
            ),
            (
                Event::PermissionRequest(PermissionRequest {
                    request_id: RequestId::from("r"),
                    tool_name: "shell".to_owned(),
                    description: None,
                    options: vec![],
                }),
                "permission_request",
            ),
            (
                Event::ModelLifecycle(ModelLifecycle {
                    model_id: "qwen".to_owned(),
                    stage: ModelLifecycleStage::Ready,
                }),
                "model_lifecycle",
            ),
            (
                Event::ModelSelectionProposed(sample_proposal()),
                "model_selection_proposed",
            ),
            (
                Event::ModelSelectionDecided(ModelSelectionDecided {
                    request_id: Some(RequestId::from("m1")),
                    model_name: Some("qwen2.5-coder-3b".to_owned()),
                    declined_local: false,
                    source: SelectionSource::Probe,
                }),
                "model_selection_decided",
            ),
            (
                Event::WebLookup(WebLookup {
                    kind: WebLookupKind::Fetch,
                    host: "docs.rs".to_owned(),
                    outcome: WebLookupOutcome::Completed,
                    bytes_in: 4096,
                    cause: None,
                }),
                "web_lookup",
            ),
            (
                Event::WebConsentDecided(WebConsentDecided {
                    scope: WebConsentScope::Session,
                    tier: WebTier::FetchAnyUrl,
                    granted: true,
                }),
                "web_consent_decided",
            ),
            (
                Event::WebTaintOverridden(WebTaintOverridden {
                    tiers_restored: vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl],
                }),
                "web_taint_overridden",
            ),
            (
                Event::ContextCleared(ContextCleared { blocks_dropped: 6 }),
                "context_cleared",
            ),
            (
                Event::ProviderSetupCompleted(ProviderSetupCompleted {
                    provider_id: ProviderId::from("p"),
                    kind: ProviderKind::OpenaiCompatible,
                    model: "m".to_owned(),
                    bindings: vec![],
                    dial_host: "api.example".to_owned(),
                }),
                "provider_setup_completed",
            ),
            (
                // REQ-579's one variant whose wire name is *not* its derived
                // snake_case spelling — the row exists so the `#[serde(rename)]`
                // and `name()` are checked against the same literal.
                Event::ProviderSetupRejected(ProviderSetupRejected {
                    method: "provider/setup_commit".to_owned(),
                }),
                "provider_setup_rejected_nonuser",
            ),
            (
                Event::TurnQueued(TurnQueued {
                    turn_id: TurnId::from("turn-1"),
                    model_id: "qwen".to_owned(),
                    waiting_on: TierWarming::Loading,
                }),
                "turn_queued",
            ),
            (
                Event::ProviderTested(ProviderTested {
                    provider_id: ProviderId::from("p"),
                    outcome: ProviderTestOutcome::Reached {
                        latency_ms: 412,
                        input_tokens: 11,
                        output_tokens: 1,
                        usd_micros: Some(37),
                    },
                    health_after: ProviderHealth::Healthy,
                }),
                "provider_tested",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(event.name(), expected, "name() mismatch");
            let env = EventEnvelope::new(0, None, event);
            let wire: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
            assert_eq!(wire["event"], expected, "wire tag mismatch");
            assert_eq!(env.event_name(), expected);
        }
    }

    /// AC-15's wire half (BR-9a): the title reaches a client as a flat
    /// `session_titled` object naming its session, and survives the round trip
    /// unchanged.
    ///
    /// `session_id` is asserted on the wire object rather than on the payload
    /// because the envelope is what carries it — the assertion is about what a
    /// client receives, which is the only level at which "the event names its
    /// session" is a claim worth making. `envelope_wire` round-trips before
    /// returning, so re-adding `session_id` to [`SessionTitled`] fails here on
    /// the resulting duplicate key rather than reaching a client.
    #[test]
    fn session_titled_round_trips_under_its_wire_name() {
        let titled = SessionTitled {
            title: "wire the unreached categories".to_owned(),
        };
        round_trip(&titled);

        let wire = envelope_wire(Event::SessionTitled(titled.clone()));
        assert_eq!(wire["event"], "session_titled");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["title"], "wire the unreached categories");

        assert_eq!(Event::SessionTitled(titled).name(), "session_titled");
    }

    /// **REQ-567 BR-8's wire half.** A clear reaches a client as a flat
    /// `context_cleared` object naming its session and how much went, and
    /// survives the round trip unchanged.
    ///
    /// `session_id` is asserted on the wire object rather than on the payload
    /// for [`SessionTitled`]'s reason: the envelope is what carries it, and
    /// `envelope_wire` round-trips before returning, so re-adding `session_id`
    /// to [`ContextCleared`] fails here on the duplicate key rather than
    /// reaching a client.
    ///
    /// The zero case is asserted beside the populated one because it is a real
    /// event and not a degenerate one — clearing an already-empty session is
    /// idempotent and still announced, so `0` has to survive the wire as a
    /// number rather than be skipped as a default.
    #[test]
    fn context_cleared_round_trips_under_its_wire_name() {
        let cleared = ContextCleared { blocks_dropped: 6 };
        round_trip(&cleared);

        let wire = envelope_wire(Event::ContextCleared(cleared));
        assert_eq!(wire["event"], "context_cleared");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["blocks_dropped"], 6);

        assert_eq!(Event::ContextCleared(cleared).name(), "context_cleared");

        let empty = ContextCleared { blocks_dropped: 0 };
        round_trip(&empty);
        let wire = envelope_wire(Event::ContextCleared(empty));
        assert_eq!(
            wire["blocks_dropped"], 0,
            "a clear that dropped nothing must still say so on the wire"
        );
    }

    #[test]
    fn session_update_variants_round_trip() {
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::AgentMessageChunk {
                text: "chunk".to_owned(),
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::ToolCall {
                tool_call_id: "c1".to_owned(),
                title: "read file".to_owned(),
                status: ToolCallStatus::Pending,
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::ToolCallUpdate {
                tool_call_id: "c1".to_owned(),
                status: ToolCallStatus::Completed,
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::Diff {
                path: "src/a.rs".to_owned(),
                old_text: None,
                new_text: "fn a() {}".to_owned(),
            },
        });
        round_trip(&SessionUpdate {
            update: SessionUpdatePayload::Plan {
                entries: vec![PlanEntry {
                    content: "write tests".to_owned(),
                    status: PlanEntryStatus::InProgress,
                }],
            },
        });
    }

    #[test]
    fn route_decided_round_trips() {
        round_trip(&RouteDecided {
            category: Some(Category::Edit),
            tier: Some(Tier::Build),
            phase: Some(Phase::Implement),
            provider_id: ProviderId::from("deepseek"),
            model: Some("deepseek-coder".to_owned()),
            reason: "implement phase routes to the configured cheap tier".to_owned(),
            effort: Some(ResolvedEffort::effort(crate::effort::EffortLevel::High)),
        });
    }

    /// REQ-558 AC-8: the four things a decision must name travel together, and
    /// they survive the wire. Asserted on the JSON rather than only on the
    /// round-tripped struct, because a client reads the JSON.
    #[test]
    fn route_decided_names_its_category_tier_provider_and_reason() {
        let decided = RouteDecided {
            category: Some(Category::Digest),
            tier: Some(Tier::Scan),
            phase: None,
            provider_id: ProviderId::from("on-device"),
            model: Some("qwen2.5-coder-3b".to_owned()),
            reason: "Routing the 'digest' category to 'on-device' through its 'scan' tier binding."
                .to_owned(),
            // The local tier: a declared no-op, reported as one (BR-6).
            effort: Some(ResolvedEffort::omit(
                crate::effort::EffortOmission::ShapeNone,
            )),
        };
        round_trip(&decided);
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&decided).unwrap()).unwrap();
        assert_eq!(wire["category"], "digest");
        assert_eq!(wire["tier"], "scan");
        assert_eq!(wire["provider_id"], "on-device");
        assert!(!wire["reason"].as_str().unwrap().is_empty());
        // BR-11: a freeform turn has no phase, and the absent phase does not
        // take the category with it — the two are independent facts now.
        assert!(wire.get("phase").is_none());
    }

    /// A pre-REQ-558 client (or a replayed pre-REQ event) omits the new keys
    /// entirely. They must deserialize as absent rather than fail the envelope —
    /// the same `default` posture every other optional payload field takes.
    #[test]
    fn route_decided_and_cost_record_accept_a_payload_with_no_category() {
        let decided: RouteDecided = serde_json::from_str(
            r#"{"phase":"review","provider_id":"anthropic","reason":"because"}"#,
        )
        .unwrap();
        assert_eq!(decided.category, None);
        assert_eq!(decided.tier, None);
        assert_eq!(decided.phase, Some(Phase::Review));

        let record: CostRecord = serde_json::from_str(
            r#"{"session_id":"s1","provider_id":"anthropic","model":"m",
                "input_tokens":1,"output_tokens":2,"usd_micros":3}"#,
        )
        .unwrap();
        assert_eq!(record.category, None);
        assert_eq!(record.phase, None);
    }

    #[test]
    fn privacy_block_round_trips() {
        round_trip(&PrivacyBlock {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
            cause: BlockCause::Boundary,
        });
    }

    /// A frame emitted before REQ-562 carries no `cause` key at all. It must
    /// read as the block it actually was — a boundary block — rather than
    /// failing the envelope, which is the whole claim `#[serde(default)]` makes
    /// and therefore the thing to assert rather than comment (LESSON-486).
    #[test]
    fn a_privacy_block_with_no_cause_key_reads_as_a_boundary_block() {
        let block: PrivacyBlock = serde_json::from_str(
            r#"{"path":"secrets/prod.env","provider_id":"anthropic",
                "action":"rerouted_to_local"}"#,
        )
        .unwrap();
        assert_eq!(block.cause, BlockCause::Boundary);
        assert_eq!(block.path, "secrets/prod.env");
        assert_eq!(block.action, PrivacyAction::ReroutedToLocal);

        // Non-vacuity: the default is not swallowing a `cause` that *is* present.
        let scanned: PrivacyBlock = serde_json::from_str(
            r#"{"path":"outbound payload","provider_id":"anthropic",
                "action":"rerouted_to_local","cause":{"cause":"scan_unavailable"}}"#,
        )
        .unwrap();
        assert_eq!(scanned.cause, BlockCause::ScanUnavailable);
    }

    /// The other direction of the same compatibility claim: a build that has
    /// never heard of `cause` still reads a frame that carries one. Serde
    /// ignores unknown fields by default and no type here opts out, but the
    /// posture is what keeps `PROTOCOL_VERSION` still, so it is asserted rather
    /// than assumed — modelled by the pre-REQ-562 shape of the struct.
    #[test]
    fn a_client_predating_the_cause_field_still_reads_a_frame_that_carries_one() {
        #[derive(Deserialize)]
        struct PreCausePrivacyBlock {
            path: String,
            action: PrivacyAction,
        }

        let wire = serde_json::to_string(&PrivacyBlock {
            path: "outbound payload, bytes 1400-1436".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
            cause: BlockCause::Redaction {
                kind: FindingKind::Credential,
                span: ByteSpan {
                    start: 1400,
                    end: 1436,
                },
            },
        })
        .unwrap();

        let old: PreCausePrivacyBlock = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.path, "outbound payload, bytes 1400-1436");
        assert_eq!(old.action, PrivacyAction::ReroutedToLocal);
    }

    #[test]
    fn the_three_block_causes_round_trip_and_serialize_distinctly() {
        let causes = [
            BlockCause::Boundary,
            BlockCause::Redaction {
                kind: FindingKind::Secret,
                span: ByteSpan {
                    start: 1400,
                    end: 1436,
                },
            },
            BlockCause::ScanUnavailable,
        ];
        for cause in causes {
            round_trip(&PrivacyBlock {
                path: "secrets/prod.env".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause,
            });
        }

        let tags: Vec<String> = causes
            .iter()
            .map(|cause| {
                let wire: serde_json::Value =
                    serde_json::from_str(&serde_json::to_string(cause).unwrap()).unwrap();
                wire["cause"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(tags, ["boundary", "redaction", "scan_unavailable"]);
    }

    #[test]
    fn provenance_rejected_round_trips_with_and_without_a_tool() {
        for tool in [Some("mcp__fs__read_file".to_owned()), None] {
            round_trip(&ProvenanceRejected {
                source: "/etc/passwd".to_owned(),
                tool,
                reason: ProvenanceRejection::Absolute,
            });
        }
    }

    /// The four refusals are four different problems, so they must not collapse
    /// into one wire value — a consumer that cannot tell an absolute claim from
    /// a traversal cannot say which one to fix.
    #[test]
    fn every_provenance_rejection_reason_serializes_distinctly() {
        let reasons = [
            ProvenanceRejection::Absolute,
            ProvenanceRejection::ParentTraversal,
            ProvenanceRejection::NotCanonical,
            ProvenanceRejection::Empty,
        ];
        let wire: Vec<String> = reasons
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        assert_eq!(
            wire,
            [
                "\"absolute\"",
                "\"parent_traversal\"",
                "\"not_canonical\"",
                "\"empty\""
            ]
        );
        for reason in reasons {
            round_trip(&ProvenanceRejected {
                source: "sub/../secrets/prod.env".to_owned(),
                tool: None,
                reason,
            });
        }
    }

    /// The wire-compatibility claim that keeps [`crate::PROTOCOL_VERSION`] at 2:
    /// the new variant is a new tag value on an internally-tagged enum, so the
    /// frame carries the same `event` discriminator every other event does and
    /// no existing frame's shape moved. Asserted rather than commented, because
    /// the version note is only as good as the encoding it describes.
    #[test]
    fn provenance_rejected_is_an_additive_tag_on_the_event_envelope() {
        let env = EventEnvelope::new(
            7,
            Some(SessionId::from("s1")),
            Event::ProvenanceRejected(ProvenanceRejected {
                source: "/etc/passwd".to_owned(),
                tool: Some("mcp__fs__read_file".to_owned()),
                reason: ProvenanceRejection::Absolute,
            }),
        );
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        assert_eq!(wire["event"], "provenance_rejected");
        assert_eq!(wire["source"], "/etc/passwd");
        assert_eq!(wire["reason"], "absolute");
        assert_eq!(env.event_name(), "provenance_rejected");
        round_trip(&env);

        // `tool` is omitted rather than null when the refusal names no tool.
        let guard = EventEnvelope::new(
            8,
            None,
            Event::ProvenanceRejected(ProvenanceRejected {
                source: "../outside".to_owned(),
                tool: None,
                reason: ProvenanceRejection::ParentTraversal,
            }),
        );
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&guard).unwrap()).unwrap();
        assert!(
            wire.as_object().unwrap().get("tool").is_none(),
            "an absent tool must not serialize as a key: {wire}"
        );
        round_trip(&guard);
    }

    /// BR-6 at the protocol layer: a redaction cause can carry a locator and
    /// nothing else. The key set is asserted exhaustively, so a later field able
    /// to hold the matched text turns this red instead of shipping quietly.
    #[test]
    fn a_redaction_cause_carries_only_a_kind_and_a_span() {
        let wire: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&BlockCause::Redaction {
                kind: FindingKind::Pii,
                span: ByteSpan { start: 0, end: 12 },
            })
            .unwrap(),
        )
        .unwrap();

        let mut keys: Vec<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["cause", "kind", "span"]);
        assert_eq!(wire["kind"], "pii");
        assert_eq!(wire["span"]["start"], 0);
        assert_eq!(wire["span"]["end"], 12);
    }

    /// **One label, two renderers.** The daemon's typed-error sentence and the
    /// CLI's `privacy_block` line both compose from this, and they used to hold
    /// byte-identical private copies — two spellings of one fact, on the one
    /// surface that explains a privacy decision.
    ///
    /// What is pinned: each kind gets its own phrase (a collapsed label would
    /// make two different findings read the same), the phrases are content-free
    /// nouns, and they are *not* the wire names — the wire value is frozen for
    /// compatibility and the prose is free to change.
    #[test]
    fn every_finding_kind_has_its_own_content_free_user_label() {
        use std::collections::BTreeSet;

        let kinds = [
            FindingKind::Secret,
            FindingKind::Credential,
            FindingKind::Pii,
            FindingKind::Unknown,
        ];
        let labels: Vec<&str> = kinds.iter().map(|k| k.user_label()).collect();
        assert_eq!(
            labels.iter().collect::<BTreeSet<_>>().len(),
            kinds.len(),
            "two kinds sharing a label read as the same finding: {labels:?}"
        );
        for label in &labels {
            assert!(!label.is_empty());
            assert!(
                label.starts_with("a ") || label.starts_with("personal "),
                "a label is a noun phrase a sentence can be built around: {label}"
            );
        }
        assert_eq!(FindingKind::Credential.user_label(), "a credential");
        // The wire name is a different value with a different contract.
        assert_eq!(
            serde_json::to_value(FindingKind::Credential).unwrap(),
            "credential"
        );
    }

    #[test]
    fn every_finding_kind_round_trips_through_its_wire_name() {
        for (kind, name) in [
            (FindingKind::Secret, "secret"),
            (FindingKind::Credential, "credential"),
            (FindingKind::Pii, "pii"),
            (FindingKind::Unknown, "unknown"),
        ] {
            round_trip(&kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
        }
    }

    #[test]
    fn cost_recorded_round_trips() {
        round_trip(&CostRecorded {
            record: CostRecord {
                session_id: SessionId::from("s1"),
                phase: Some(Phase::Review),
                category: Some(Category::Review),
                provider_id: ProviderId::from("anthropic"),
                model: "claude-opus".to_owned(),
                input_tokens: 1000,
                output_tokens: 500,
                usd_micros: 45_000,
                cached_tokens: None,
                reasoning_tokens: None,
                probe: false,
            },
        });
    }

    /// REQ-581 BR-5's ledger half on the wire: a probe row says so, an ordinary
    /// turn's row says nothing at all, and a record from a daemon that predates
    /// the flag reads as a turn.
    ///
    /// The absent-when-false shape is asserted on the serialized keys rather
    /// than on a reading of the struct, because it is the compatibility claim:
    /// a client built against the older `cost_recorded` reads exactly the bytes
    /// it always did, which is why this field moves no protocol version.
    #[test]
    fn a_probe_row_is_marked_and_every_other_row_is_unchanged() {
        let turn = CostRecord {
            session_id: SessionId::from("s1"),
            phase: None,
            category: Some(Category::Edit),
            provider_id: ProviderId::from("kimi"),
            model: "kimi-k2-turbo-preview".to_owned(),
            input_tokens: 1000,
            output_tokens: 500,
            usd_micros: 45_000,
            cached_tokens: None,
            reasoning_tokens: None,
            probe: false,
        };
        round_trip(&turn);
        let wire = serde_json::to_value(&turn).unwrap();
        assert!(
            wire.get("probe").is_none(),
            "a turn's row carries no probe key at all: {wire}"
        );

        // The probe's own row: the same call shape, billed the same way, and
        // attributed to no routing category — nothing routed it, the user asked
        // for it by name.
        let probe = CostRecord {
            category: None,
            probe: true,
            ..turn.clone()
        };
        round_trip(&probe);
        let wire = serde_json::to_value(&probe).unwrap();
        assert_eq!(wire["probe"], true, "{wire}");
        assert_eq!(
            wire["usd_micros"], 45_000,
            "a probe is billed like any other call — the flag counts it apart, \
             it does not price it apart: {wire}"
        );

        // A row from a daemon built before REQ-581 has no `probe` key, and the
        // ledger keeps `NULL` there rather than a backfilled guess. Both read
        // as `false`: that daemon made no probes, so "not a probe" is the
        // honest reading of its silence.
        let pre_581 = serde_json::json!({
            "session_id": "s1",
            "provider_id": "kimi",
            "model": "kimi-k2-turbo-preview",
            "input_tokens": 1000,
            "output_tokens": 500,
            "usd_micros": 45_000,
        });
        let decoded: CostRecord =
            serde_json::from_value(pre_581).expect("an older daemon's row must still parse");
        assert!(!decoded.probe);
        assert_eq!(decoded.usd_micros, 45_000);
    }

    #[test]
    fn provider_degraded_round_trips() {
        round_trip(&ProviderDegraded {
            provider_id: ProviderId::from("flaky"),
            failure_class: FailureClass::ToolCallFailure,
            fallback_id: Some(ProviderId::from("anthropic")),
        });
    }

    #[test]
    fn model_lifecycle_stages_round_trip() {
        for stage in [
            ModelLifecycleStage::Probed {
                ram_bytes: 16 * 1024 * 1024 * 1024,
                above_floor: true,
            },
            ModelLifecycleStage::AwaitingDecision {
                reason: "awaiting your answer to the local-model proposal".to_owned(),
            },
            ModelLifecycleStage::Download {
                downloaded_bytes: 100,
                total_bytes: Some(1000),
            },
            ModelLifecycleStage::Benchmark {
                first_token_ms: 250,
                tokens_per_sec: 42.5,
            },
            ModelLifecycleStage::Ready,
            ModelLifecycleStage::SteppedDown {
                from_model: "7b".to_owned(),
                to_model: "3b".to_owned(),
                reason: "benchmark exceeded the 1s latency duty".to_owned(),
            },
            ModelLifecycleStage::Disabled {
                reason: "machine below the 8GB floor; running remote-only".to_owned(),
            },
        ] {
            round_trip(&ModelLifecycle {
                model_id: "qwen2.5-coder-3b".to_owned(),
                stage,
            });
        }
    }

    #[test]
    fn model_selection_proposed_round_trips() {
        round_trip(&sample_proposal());
    }

    #[test]
    fn model_selection_proposed_below_the_floor_omits_the_proposal() {
        // A machine under the RAM floor still gets a proposal event — with no
        // pick, band `none`, and the full alternatives list so the user can
        // still override (BR-3). The absent `proposed` must not become `null`.
        let below_floor = ModelSelectionProposed {
            request_id: RequestId::from("m2"),
            probe: ProbeReportView {
                total_ram_bytes: 4 * 1024 * 1024 * 1024,
                free_disk_bytes: 10 * 1024 * 1024 * 1024,
                gpu_class: GpuClass::Cpu,
                chosen_band: ChosenBand::None,
                reason: "4 GB of RAM is below the 8 GB floor; sessions run remote-only".to_owned(),
            },
            proposed: None,
            alternatives: vec![CatalogEntryView {
                name: "qwen2.5-coder-1.5b".to_owned(),
                band: TierBand::Small,
                size_bytes: 1_100_000_000,
                ram_floor_bytes: 3_221_225_472,
                provenance: CatalogProvenance {
                    repo: "Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF".to_owned(),
                    host: "huggingface.co".to_owned(),
                    revision: "f86cb2c".to_owned(),
                },
            }],
            fetch_notice: None,
        };
        round_trip(&below_floor);

        let json = serde_json::to_string(&below_floor).unwrap();
        assert!(!json.contains("proposed"), "wire: {json}");
        let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(wire["probe"]["chosen_band"], "none");
    }

    #[test]
    fn model_selection_decided_round_trips_every_source() {
        for source in [
            SelectionSource::Probe,
            SelectionSource::UserOverride,
            SelectionSource::ConfigPin,
            SelectionSource::AutoAccept,
        ] {
            round_trip(&ModelSelectionDecided {
                request_id: Some(RequestId::from("m1")),
                model_name: Some("qwen2.5-coder-3b".to_owned()),
                declined_local: false,
                source,
            });
        }
        // A decline carries no model name (BR-4)…
        round_trip(&ModelSelectionDecided {
            request_id: Some(RequestId::from("m1")),
            model_name: None,
            declined_local: true,
            source: SelectionSource::UserOverride,
        });
        // …and an unprompted decision carries no request id (BR-5 auto-accept).
        round_trip(&ModelSelectionDecided {
            request_id: None,
            model_name: Some("qwen2.5-coder-7b".to_owned()),
            declined_local: false,
            source: SelectionSource::AutoAccept,
        });
    }

    #[test]
    fn selection_source_uses_the_spec_wire_names() {
        for (source, expected) in [
            (SelectionSource::Probe, "\"probe\""),
            (SelectionSource::UserOverride, "\"user_override\""),
            (SelectionSource::ConfigPin, "\"config_pin\""),
            (SelectionSource::AutoAccept, "\"auto_accept\""),
        ] {
            assert_eq!(serde_json::to_string(&source).unwrap(), expected);
        }
    }

    #[test]
    fn chosen_band_round_trips_through_the_optional_catalog_band() {
        // The `Option<TierBand>` ↔ `ChosenBand` map is total in both directions,
        // so no caller has to hand-roll the "below the floor" case.
        for band in [
            None,
            Some(TierBand::Small),
            Some(TierBand::Mid),
            Some(TierBand::Large),
        ] {
            assert_eq!(ChosenBand::from(band).band(), band);
        }
        assert_eq!(
            serde_json::to_string(&ChosenBand::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&TierBand::Large).unwrap(),
            "\"large\""
        );
    }

    #[test]
    fn gpu_class_mirrors_the_probe_wire_names() {
        // Same strings `teton_inference::probe::GpuClass` emits, so the daemon's
        // projection can never drift in casing.
        for (class, expected) in [
            (GpuClass::AppleSilicon, "\"apple_silicon\""),
            (GpuClass::Cuda, "\"cuda\""),
            (GpuClass::Cpu, "\"cpu\""),
        ] {
            assert_eq!(serde_json::to_string(&class).unwrap(), expected);
        }
    }

    #[test]
    fn a_proposal_never_carries_a_url_digest_or_path() {
        // BR-11: the leak surface is whatever rides the outbound structure, so it
        // is constrained at the payload definition. `CatalogEntryView` is a
        // projection precisely so a catalog `url`/`sha256` and the daemon's
        // install path cannot ride along.
        let json = serde_json::to_string(&sample_proposal()).unwrap();
        for forbidden in ["url", "sha256", "path", "http", "/Users/", "auth"] {
            assert!(
                !json.contains(forbidden),
                "proposal payload leaked `{forbidden}`: {json}"
            );
        }
    }

    #[test]
    fn a_proposal_carries_the_provenance_triple_without_a_full_url() {
        // H-2: the user must see *from whom* and *from where* the bytes come.
        // Publisher/repo, host, and the short revision are non-sensitive (BR-11):
        // none is a credential, a full URL, a path, or file content.
        let json = serde_json::to_string(&sample_proposal()).unwrap();
        assert!(json.contains("provenance"), "no provenance: {json}");
        assert!(
            json.contains("Qwen/Qwen2.5-Coder-7B-Instruct-GGUF"),
            "{json}"
        );
        assert!(json.contains("huggingface.co"), "no host: {json}");
        assert!(json.contains("13fb94b"), "no short revision: {json}");
        // The host is a bare hostname, not a scheme+URL.
        assert!(!json.contains("://"), "a full URL rode the wire: {json}");
    }

    #[test]
    fn a_mirror_fetch_notice_round_trips_and_carries_only_a_bare_host() {
        // A redirected fetch must be legible (H-2) but still leak nothing (BR-11):
        // a bare mirror host, never the base URL's scheme, path, or userinfo.
        let mut proposal = sample_proposal();
        proposal.fetch_notice = Some(FetchNotice {
            mirror_host: Some("hf-mirror.corp.internal".to_owned()),
            override_catalog: false,
        });
        round_trip(&proposal);
        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("hf-mirror.corp.internal"), "{json}");
        for forbidden in ["url", "sha256", "http", "/Users/", "auth", "://"] {
            assert!(
                !json.contains(forbidden),
                "mirror notice leaked `{forbidden}`: {json}"
            );
        }
    }

    #[test]
    fn an_absent_fetch_notice_does_not_ride_the_wire() {
        // The common case (bundled catalog, no mirror) must not add a null field.
        let json = serde_json::to_string(&sample_proposal()).unwrap();
        assert!(!json.contains("fetch_notice"), "{json}");
    }

    #[test]
    fn permission_request_round_trips() {
        round_trip(&PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: "shell".to_owned(),
            description: Some("run `cargo test`".to_owned()),
            options: vec![
                PermissionOption {
                    option_id: "allow_once".to_owned(),
                    label: "Allow once".to_owned(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "reject_always".to_owned(),
                    label: "Reject for session".to_owned(),
                    kind: PermissionOptionKind::RejectAlways,
                },
            ],
        });
    }

    #[test]
    fn phase_transition_round_trips() {
        round_trip(&PhaseTransition {
            from_phase: Some(Phase::Architect),
            to_phase: Phase::Implement,
            artifacts: vec![TaskArtifactRef {
                req_id: "REQ-544".to_owned(),
                phase: Phase::Architect,
                path: ".adlc/specs/REQ-544/architecture.md".to_owned(),
            }],
        });
    }

    #[test]
    fn daemon_client_attach_round_trips() {
        round_trip(&DaemonClientAttach {
            client_kind: ClientKind::Extension,
            protocol_version: crate::PROTOCOL_VERSION,
        });
    }

    #[test]
    fn envelope_omits_session_id_when_daemon_scoped() {
        let env = EventEnvelope::new(
            9,
            None,
            Event::ModelLifecycle(ModelLifecycle {
                model_id: "qwen".to_owned(),
                stage: ModelLifecycleStage::Ready,
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("session_id"));
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn unknown_fields_in_event_payloads_are_tolerated() {
        // Forward compatibility: an extra field the daemon added later must not
        // break an older client parsing the flattened envelope.
        let json = r#"{
            "session_id": "s1",
            "seq": 4,
            "event": "route_decided",
            "provider_id": "anthropic",
            "reason": "spec phase routes to the frontier tier",
            "future_field": {"weight": 0.9}
        }"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.event_name(), "route_decided");
        match env.event {
            Event::RouteDecided(rd) => assert_eq!(rd.provider_id, ProviderId::from("anthropic")),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_in_a_model_proposal_are_tolerated() {
        // Forward compatibility for the consent payloads specifically: a newer
        // daemon that adds a field to the probe report or a catalog entry must
        // not break a client built against this shape.
        let json = r#"{
            "seq": 7,
            "event": "model_selection_proposed",
            "request_id": "m1",
            "probe": {
                "total_ram_bytes": 34359738368,
                "free_disk_bytes": 214748364800,
                "gpu_class": "apple_silicon",
                "chosen_band": "mid",
                "reason": "32 GB clears the 7B band",
                "future_probe_field": {"thermal_headroom": 0.8}
            },
            "proposed": {
                "entry": {
                    "name": "qwen2.5-coder-7b",
                    "band": "mid",
                    "size_bytes": 4700000000,
                    "ram_floor_bytes": 12884901888,
                    "provenance": {
                        "repo": "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
                        "host": "huggingface.co",
                        "revision": "13fb94b"
                    },
                    "future_entry_field": "quant"
                },
                "required_disk_bytes": 5700000000
            },
            "alternatives": [],
            "future_top_level_field": true
        }"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.event_name(), "model_selection_proposed");
        match env.event {
            Event::ModelSelectionProposed(p) => {
                assert_eq!(p.probe.chosen_band, ChosenBand::Mid);
                assert_eq!(
                    p.proposed.expect("proposal present").entry.name,
                    "qwen2.5-coder-7b"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The sorted key set of a payload's wire object.
    fn wire_keys(value: &impl Serialize) -> Vec<String> {
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap();
        let mut keys: Vec<String> = wire
            .as_object()
            .expect("payload is an object")
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        keys
    }

    /// Every folded outcome survives the wire on both kinds of lookup, swept
    /// from [`WebLookupOutcome::ALL`] rather than a hand-kept list — an outcome
    /// added by a later REQ reaches this test without anyone remembering to
    /// extend it (the `Category::ALL` sweep in the ledger sets the precedent).
    #[test]
    fn every_web_lookup_outcome_round_trips_on_every_kind() {
        for kind in WebLookupKind::ALL {
            for outcome in WebLookupOutcome::ALL {
                let lookup = WebLookup {
                    kind,
                    host: "docs.rs".to_owned(),
                    outcome,
                    // Only a completed transfer carries bytes; the rest are the
                    // endings that moved nothing.
                    bytes_in: match outcome {
                        WebLookupOutcome::Completed | WebLookupOutcome::CacheHit => 4096,
                        _ => 0,
                    },
                    // The finer reading of a block, carried only by the two
                    // blocking outcomes — and swept on both, so the optional
                    // field round-trips beside every outcome rather than only
                    // beside its absence.
                    cause: match outcome {
                        WebLookupOutcome::BlockedRedact => Some(BlockCause::ScanUnavailable),
                        WebLookupOutcome::BlockedPrivacy => Some(BlockCause::Boundary),
                        _ => None,
                    },
                };
                round_trip(&lookup);
                let wire = envelope_wire(Event::WebLookup(lookup));
                assert_eq!(wire["event"], "web_lookup");
                assert_eq!(wire["session_id"], "s1");
                assert_eq!(wire["host"], "docs.rs");
            }
        }
    }

    /// The wire names are the contract with every client, so they are pinned
    /// literally rather than derived from the variant spelling.
    #[test]
    fn the_web_vocabulary_uses_its_architecture_wire_names() {
        for (outcome, expected) in [
            (WebLookupOutcome::Completed, "completed"),
            (WebLookupOutcome::CacheHit, "cache_hit"),
            (WebLookupOutcome::BlockedPrivacy, "blocked_privacy"),
            (WebLookupOutcome::BlockedRedact, "blocked_redact"),
            (WebLookupOutcome::RefusedDomain, "refused_domain"),
            (WebLookupOutcome::RefusedTier, "refused_tier"),
            (WebLookupOutcome::TaintRestricted, "taint_restricted"),
            (WebLookupOutcome::Offline, "offline"),
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), expected);
        }
        // Non-vacuity for the sweep above: the list here and `ALL` are the same
        // eight values, so neither can quietly fall behind the other.
        assert_eq!(WebLookupOutcome::ALL.len(), 8);

        for (kind, expected) in [
            (WebLookupKind::Fetch, "fetch"),
            (WebLookupKind::Search, "search"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), expected);
        }
        for (tier, expected) in [
            (WebTier::Off, "off"),
            (WebTier::FetchUserUrl, "fetch_user_url"),
            (WebTier::FetchAnyUrl, "fetch_any_url"),
            (WebTier::Search, "search"),
        ] {
            assert_eq!(serde_json::to_value(tier).unwrap(), expected);
        }
        for (scope, expected) in [
            (WebConsentScope::Once, "once"),
            (WebConsentScope::Session, "session"),
            (WebConsentScope::Persistent, "persistent"),
        ] {
            assert_eq!(serde_json::to_value(scope).unwrap(), expected);
        }
    }

    /// BR-3's ladder as an ordering: each tier includes the ones below it, so a
    /// tier check is a comparison and never a table someone has to keep in sync.
    #[test]
    fn web_tiers_are_ordered_lowest_first() {
        let mut sorted = WebTier::ALL;
        sorted.sort_unstable();
        assert_eq!(sorted, WebTier::ALL, "ALL must already be in tier order");
        assert!(WebTier::Off < WebTier::FetchUserUrl);
        assert!(WebTier::FetchUserUrl < WebTier::FetchAnyUrl);
        assert!(WebTier::FetchAnyUrl < WebTier::Search);
    }

    /// **BR-7 at the protocol layer: a lookup event names a host and nothing
    /// finer.**
    ///
    /// The leak surface is whatever rides the outbound structure, so it is
    /// constrained at the payload definition rather than at each emitter — the
    /// technique [`BlockCause::Redaction`] uses against the text it found and
    /// [`CatalogEntryView`] uses against a catalog URL. The key sets are
    /// asserted **exhaustively**, so a field later added that could hold the
    /// full URL, the query the model composed, or the search key turns this red
    /// instead of shipping quietly.
    #[test]
    fn the_web_event_family_carries_a_host_and_never_a_url_query_or_credential() {
        assert_eq!(
            wire_keys(&WebLookup {
                kind: WebLookupKind::Search,
                host: "search.example.com".to_owned(),
                outcome: WebLookupOutcome::Completed,
                bytes_in: 2048,
                cause: None,
            }),
            ["bytes_in", "host", "kind", "outcome"]
        );
        assert_eq!(
            wire_keys(&WebConsentDecided {
                scope: WebConsentScope::Once,
                tier: WebTier::Search,
                granted: true,
            }),
            ["granted", "scope", "tier"]
        );
        assert_eq!(
            wire_keys(&WebTaintOverridden {
                tiers_restored: vec![WebTier::FetchUserUrl],
            }),
            ["tiers_restored"]
        );

        // And the values, not only the field names: a search — the kind whose
        // spec row carried the "verbatim query" — serializes with nothing that
        // could be one. Scanned on the `web_lookup` payload alone because the
        // tier *names* legitimately contain `url` (`fetch_any_url` is a
        // capability, not a destination), and a substring sweep cannot tell the
        // two apart.
        let json = serde_json::to_string(&WebLookup {
            kind: WebLookupKind::Search,
            host: "search.example.com".to_owned(),
            outcome: WebLookupOutcome::Completed,
            bytes_in: 2048,
            cause: None,
        })
        .unwrap();
        for forbidden in ["://", "url", "query", "token", "secret", "auth", "key", "?"] {
            assert!(
                !json.contains(forbidden),
                "a lookup event leaked `{forbidden}`: {json}"
            );
        }
    }

    /// The consent decision is one event with an answer, not two events (D-8).
    /// Both answers must survive the wire, and a denial must stay a denial —
    /// `granted` has no `default`, so a payload that omits it is an error
    /// rather than a silent "no".
    #[test]
    fn a_web_consent_decision_round_trips_both_answers_at_every_scope() {
        for scope in [
            WebConsentScope::Once,
            WebConsentScope::Session,
            WebConsentScope::Persistent,
        ] {
            for granted in [true, false] {
                round_trip(&WebConsentDecided {
                    scope,
                    tier: WebTier::FetchAnyUrl,
                    granted,
                });
            }
        }

        let wire = envelope_wire(Event::WebConsentDecided(WebConsentDecided {
            scope: WebConsentScope::Persistent,
            tier: WebTier::Search,
            granted: false,
        }));
        assert_eq!(wire["event"], "web_consent_decided");
        assert_eq!(wire["scope"], "persistent");
        assert_eq!(wire["tier"], "search");
        assert_eq!(wire["granted"], false);

        assert!(
            serde_json::from_str::<WebConsentDecided>(r#"{"scope":"once","tier":"search"}"#)
                .is_err(),
            "an answer this build cannot read must fail loudly, never default to one"
        );
    }

    /// AC-12's wire half: the override names the tiers it restored and the
    /// session it restored them for — the session through the envelope, like
    /// every other session-scoped event. Re-adding `session_id` to the payload
    /// fails here on the duplicate key rather than reaching a client.
    #[test]
    fn web_taint_overridden_names_its_tiers_and_its_session() {
        let overridden = WebTaintOverridden {
            tiers_restored: vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl],
        };
        round_trip(&overridden);

        let wire = envelope_wire(Event::WebTaintOverridden(overridden));
        assert_eq!(wire["event"], "web_taint_overridden");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["tiers_restored"][0], "fetch_user_url");
        assert_eq!(wire["tiers_restored"][1], "fetch_any_url");

        // BR-13: an override that restores nothing says so with an empty list,
        // not with `off` — "no tiers" is not a tier.
        let none_restored = WebTaintOverridden {
            tiers_restored: vec![],
        };
        round_trip(&none_restored);
        let wire = envelope_wire(Event::WebTaintOverridden(none_restored));
        assert_eq!(wire["tiers_restored"].as_array().unwrap().len(), 0);
    }

    /// REQ-572 BR-14 / AC-11's wire half: all three new events reach a client
    /// as flat objects naming the session they belong to, through the envelope
    /// like every other session-scoped event.
    ///
    /// The scoping is asserted on the wire object rather than on the payloads
    /// because the envelope is what carries it — and `envelope_wire`
    /// round-trips before returning, so a `session_id` field added to any of
    /// these payloads fails here on the duplicate key rather than reaching a
    /// client (the shape [`WebTaintOverridden`] documents).
    #[test]
    fn the_setup_events_are_session_scoped_under_their_wire_names() {
        let completed = WebSetupCompleted {
            tier: WebTier::Search,
            config_path: "/Users/dev/.config/teton/config.toml".to_owned(),
        };
        round_trip(&completed);
        let wire = envelope_wire(Event::WebSetupCompleted(completed));
        assert_eq!(wire["event"], "web_setup_completed");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["tier"], "search");
        assert_eq!(wire["config_path"], "/Users/dev/.config/teton/config.toml");

        let rejected = WebSetupRejected {
            origin: "a connection not attached to this session".to_owned(),
        };
        round_trip(&rejected);
        let wire = envelope_wire(Event::WebSetupRejected(rejected));
        assert_eq!(wire["event"], "web_setup_rejected");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["origin"], "a connection not attached to this session");

        let dead_end = CapabilityDeadEnd {
            capability: "web_search".to_owned(),
        };
        round_trip(&dead_end);
        let wire = envelope_wire(Event::CapabilityDeadEnd(dead_end));
        assert_eq!(wire["event"], "capability_dead_end");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["capability"], "web_search");

        // Non-vacuity: the key above comes from the envelope's scope and not
        // from a payload that always emits one. Scoped to nothing, it is
        // absent — which is why publishing one of these without a session is a
        // bug the daemon can commit, not a shape this type prevents.
        let wire = serde_json::to_value(EventEnvelope::new(
            2,
            None,
            Event::CapabilityDeadEnd(CapabilityDeadEnd {
                capability: "remote_provider".to_owned(),
            }),
        ))
        .unwrap();
        assert!(wire.get("session_id").is_none(), "{wire}");
    }

    /// REQ-579 BR-15 / BR-12's wire half: both provider-setup events reach a
    /// client as flat objects naming the session they belong to, and the
    /// rejection lands under the `_nonuser` spelling the spec names rather than
    /// the snake_case one the derive would have produced.
    ///
    /// The scoping is asserted on the wire object rather than on the payloads
    /// because the envelope is what carries it — and `envelope_wire`
    /// round-trips before returning, so a `session_id` field added to either
    /// payload fails here on the duplicate key rather than reaching a client.
    #[test]
    fn the_provider_setup_events_are_session_scoped_under_their_wire_names() {
        let completed = ProviderSetupCompleted {
            provider_id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            model: "kimi-k2-turbo-preview".to_owned(),
            bindings: vec![TierBinding {
                tier: Tier::Think,
                provider_id: ProviderId::from("kimi"),
            }],
            dial_host: "api.moonshot.ai".to_owned(),
        };
        round_trip(&completed);
        let wire = envelope_wire(Event::ProviderSetupCompleted(completed));
        assert_eq!(wire["event"], "provider_setup_completed");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["provider_id"], "kimi");
        assert_eq!(wire["kind"], "openai-compatible");
        assert_eq!(wire["model"], "kimi-k2-turbo-preview");
        assert_eq!(wire["bindings"][0]["tier"], "think");
        assert_eq!(wire["bindings"][0]["provider_id"], "kimi");
        assert_eq!(
            wire["dial_host"], "api.moonshot.ai",
            "the announcement names where turns will now go — a host, and only a \
             host: {wire}"
        );

        // BR-7's unrouted outcome is a distinct, legible answer — an empty list
        // rather than an absent key, so a renderer says "nothing routes to it
        // yet" instead of reading a missing field as an error.
        let unrouted = ProviderSetupCompleted {
            provider_id: ProviderId::from("kimi"),
            kind: ProviderKind::Anthropic,
            model: "claude-x".to_owned(),
            bindings: vec![],
            dial_host: "api.anthropic.com".to_owned(),
        };
        round_trip(&unrouted);
        let wire = envelope_wire(Event::ProviderSetupCompleted(unrouted));
        assert_eq!(wire["bindings"].as_array().unwrap().len(), 0);

        let rejected = ProviderSetupRejected {
            method: "provider/setup_commit".to_owned(),
        };
        round_trip(&rejected);
        let wire = envelope_wire(Event::ProviderSetupRejected(rejected));
        assert_eq!(wire["event"], "provider_setup_rejected_nonuser");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["method"], "provider/setup_commit");
        // The refusal names the method and nothing else: no caller identity,
        // and no echo of the candidate it tried to register (BR-12, BR-2).
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["event", "method", "seq", "session_id"]
        );
    }

    /// REQ-580 BR-2's wire half: a held turn reaches a client as a flat
    /// `turn_queued` object naming its session, its turn, the model it waits
    /// on, and — as a **value**, not a sentence — which of the two transient
    /// states the tier is in. Both `TierWarming` variants ride the wire under
    /// their snake_case spellings, so a renderer that branches on them is
    /// branching on the same literals the daemon wrote.
    ///
    /// The key set is asserted whole: the payload names a catalog model and
    /// nothing else about the install — no path, no URL, no digest (REQ-547
    /// BR-11) — and a field added later turns this red rather than riding into
    /// a transcript.
    #[test]
    fn a_held_turn_is_announced_session_scoped_with_a_typed_cause() {
        for (waiting_on, spelled) in [
            (TierWarming::Installing, "installing"),
            (TierWarming::Loading, "loading"),
        ] {
            let queued = TurnQueued {
                turn_id: TurnId::from("turn-7"),
                model_id: "qwen3-coder-30b-a3b".to_owned(),
                waiting_on,
            };
            round_trip(&queued);
            let wire = envelope_wire(Event::TurnQueued(queued));
            assert_eq!(wire["event"], "turn_queued");
            assert_eq!(wire["session_id"], "s1");
            assert_eq!(wire["turn_id"], "turn-7");
            assert_eq!(wire["model_id"], "qwen3-coder-30b-a3b");
            assert_eq!(wire["waiting_on"], spelled, "{wire}");
            assert_eq!(
                wire.as_object().unwrap().keys().collect::<Vec<_>>(),
                [
                    "event",
                    "model_id",
                    "seq",
                    "session_id",
                    "turn_id",
                    "waiting_on"
                ]
            );
        }
    }

    /// REQ-581 BR-3/BR-4's wire half: a finished connection test reaches a
    /// client as a flat `provider_tested` object naming its session, the
    /// provider, what came back **as a value**, and where health landed.
    ///
    /// The scoping is asserted on the wire object rather than on the payload,
    /// for [`ProviderSetupCompleted`]'s reason — the envelope is what carries
    /// it, and `envelope_wire` round-trips before returning, so a `session_id`
    /// added to the payload fails here on the duplicate key rather than
    /// reaching a client.
    ///
    /// The key set is asserted whole, and a failing outcome is put through the
    /// same assertion as a reaching one: the announcement names an id, an
    /// outcome and a health word, and never the endpoint, the key, or the
    /// vendor's own prose (architecture ADR-3). A field added later turns this
    /// red rather than riding into a transcript.
    #[test]
    fn a_finished_connection_test_is_announced_session_scoped_with_a_typed_outcome() {
        let reached = ProviderTested {
            provider_id: ProviderId::from("kimi"),
            outcome: ProviderTestOutcome::Reached {
                latency_ms: 412,
                input_tokens: 11,
                output_tokens: 1,
                usd_micros: Some(37),
            },
            health_after: ProviderHealth::Healthy,
        };
        round_trip(&reached);
        let wire = envelope_wire(Event::ProviderTested(reached));
        assert_eq!(wire["event"], "provider_tested");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["provider_id"], "kimi");
        assert_eq!(wire["outcome"]["outcome"], "reached");
        assert_eq!(wire["outcome"]["latency_ms"], 412);
        assert_eq!(wire["outcome"]["usd_micros"], 37);
        assert_eq!(
            wire["health_after"], "healthy",
            "a reached test says the provider is routable again, as a value the \
             client branches on: {wire}"
        );
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "event",
                "health_after",
                "outcome",
                "provider_id",
                "seq",
                "session_id"
            ],
            "{wire}"
        );

        // A refusal is announced too — the call was made and it failed, which
        // is exactly the news a second client attached to this session needs.
        // Its `reason` names the credential *reference* and never a key value
        // (AC-2), and the payload's key set does not grow to hold one.
        let refused = ProviderTested {
            provider_id: ProviderId::from("kimi"),
            outcome: ProviderTestOutcome::Refused {
                status: 401,
                reason: "HTTP 401 from api.moonshot.ai — the vendor did not accept the \
                         credential at keychain://teton/kimi"
                    .to_owned(),
            },
            health_after: ProviderHealth::Unavailable,
        };
        round_trip(&refused);
        let wire = envelope_wire(Event::ProviderTested(refused));
        assert_eq!(wire["outcome"]["outcome"], "refused");
        assert_eq!(wire["outcome"]["status"], 401);
        assert_eq!(wire["health_after"], "unavailable");
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "event",
                "health_after",
                "outcome",
                "provider_id",
                "seq",
                "session_id"
            ],
            "{wire}"
        );
        let rendered = serde_json::to_string(&wire).unwrap();
        assert!(
            rendered.contains("keychain://teton/kimi") && !rendered.contains("sk-"),
            "the refusal names the reference the request authenticated with, \
             never the value behind it: {rendered}"
        );
    }

    /// BR-2's event half: **neither provider-setup event has anywhere to put a
    /// key**, and neither repeats the endpoint whose authority could carry one.
    ///
    /// Asserted on the serialized key sets rather than on a reading of the
    /// structs, so a `key_ref` or `endpoint` field added later turns this red
    /// instead of riding along into a transcript. The rule is re-applied at this
    /// second surface rather than assumed inherited from the web family
    /// (LESSON-525).
    #[test]
    fn no_provider_setup_event_can_carry_the_key_or_the_endpoint() {
        let planted = "sk-live-do-not-log-me";
        let wire = serde_json::to_string(&ProviderSetupCompleted {
            provider_id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            model: "kimi-k2-turbo-preview".to_owned(),
            bindings: vec![TierBinding {
                tier: Tier::Think,
                provider_id: ProviderId::from("kimi"),
            }],
            // A host, which is what `dial_host` is — the userinfo, path and
            // query a whole endpoint would carry are exactly what the field is
            // defined to have already dropped.
            dial_host: "api.moonshot.ai".to_owned(),
        })
        .unwrap();
        assert!(!wire.contains(planted), "{wire}");
        assert!(
            !wire.contains("://") && !wire.contains('@'),
            "the completion carries a host, never an endpoint that could hide \
             userinfo in its authority: {wire}"
        );

        // Every field name the completion can carry, spelled out (sorted, which
        // is how `serde_json` hands back an object's keys): a key- or
        // URL-carrying field would have to be added to this list to be added to
        // the type.
        let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&wire)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            ["bindings", "dial_host", "kind", "model", "provider_id"],
            "{wire}"
        );
    }

    /// BR-10: the three states are distinguished on the wire by a **tag**, not
    /// by prose a client would have to re-parse. Each one round-trips, and the
    /// tag spellings are pinned literally because they are the contract.
    #[test]
    fn every_web_capability_state_round_trips_under_its_own_tag() {
        for (state, expected_tag) in [
            (
                WebCapabilityState::Ready {
                    tier: WebTier::FetchUserUrl,
                },
                "ready",
            ),
            (WebCapabilityState::OffAvailable, "off_available"),
            (
                WebCapabilityState::SearchUnavailable {
                    reason: "search needs the local model, which is not loaded".to_owned(),
                },
                "search_unavailable",
            ),
        ] {
            round_trip(&state);
            let wire = serde_json::to_value(&state).unwrap();
            assert_eq!(wire["state"], expected_tag, "{wire}");
        }

        // The payloads ride beside the tag rather than inside a nested object,
        // so a renderer reads one flat shape per state.
        let wire = serde_json::to_value(WebCapabilityState::Ready {
            tier: WebTier::Search,
        })
        .unwrap();
        assert_eq!(wire["tier"], "search");

        // A state this build has never heard of is an error, not a silent
        // reading of one of these three: guessing "ready" for an unknown tag
        // would tell a user a capability is live when the daemon said
        // something else entirely.
        assert!(serde_json::from_str::<WebCapabilityState>(r#"{"state":"someday"}"#).is_err());
    }

    /// Forward compatibility for the web family specifically: a newer daemon
    /// that adds a field to a lookup event must not break a client built
    /// against this shape (the posture that keeps `PROTOCOL_VERSION` still).
    #[test]
    fn unknown_fields_in_a_web_lookup_are_tolerated() {
        let json = r#"{
            "session_id": "s1",
            "seq": 11,
            "event": "web_lookup",
            "kind": "fetch",
            "host": "docs.rs",
            "outcome": "cache_hit",
            "bytes_in": 4096,
            "future_field": {"age_secs": 12}
        }"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.event_name(), "web_lookup");
        match env.event {
            Event::WebLookup(lookup) => {
                assert_eq!(lookup.outcome, WebLookupOutcome::CacheHit);
                assert_eq!(lookup.host, "docs.rs");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// A hit reports both halves of the split — what was reused and what still
    /// had to be prefilled. One number alone cannot answer "was this turn
    /// fast", which is the entire question the event exists for.
    #[test]
    fn a_prefix_cache_hit_reports_reused_and_prefilled_counts() {
        let hit = PrefixCache {
            model: "qwen2.5-coder-3b".to_owned(),
            outcome: PrefixCacheOutcome::Hit {
                cached_tokens: 15_000,
                new_tokens: 84,
                divergent: true,
            },
        };
        round_trip(&hit);

        let wire = envelope_wire(Event::PrefixCache(hit));
        assert_eq!(wire["event"], "prefix_cache");
        // The session rides the envelope, never the payload — a `session_id`
        // field on the struct would emit the key twice and fail to parse.
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["outcome"], "hit");
        assert_eq!(wire["cached_tokens"], 15_000);
        assert_eq!(wire["new_tokens"], 84);
        assert_eq!(wire["divergent"], true);
    }

    /// A hit recorded before the BR-2 amendment carries no `divergent` key;
    /// it must still deserialize, and to `false` — which is what it meant, as
    /// the old rule never produced a divergent hit.
    #[test]
    fn a_pre_amendment_hit_without_divergent_deserializes_to_false() {
        let json = r#"{
            "seq": 7,
            "ts_ms": 1,
            "session_id": "s1",
            "event": "prefix_cache",
            "model": "qwen2.5-coder-3b",
            "outcome": "hit",
            "cached_tokens": 15000,
            "new_tokens": 84
        }"#;
        let env: EventEnvelope = serde_json::from_str(json).unwrap();
        match env.event {
            Event::PrefixCache(cache) => assert_eq!(
                cache.outcome,
                PrefixCacheOutcome::Hit {
                    cached_tokens: 15_000,
                    new_tokens: 84,
                    divergent: false,
                }
            ),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// BR-8: every miss carries its actual reason. A reader must be able to
    /// tell "history was rewritten" from "another session took the slot" —
    /// folding them into one `miss` is the failure this asserts against.
    #[test]
    fn every_miss_reason_has_its_own_wire_spelling() {
        for (reason, spelling) in [
            (PrefixCacheMiss::Cold, "cold"),
            (PrefixCacheMiss::SessionSwitch, "session_switch"),
            (PrefixCacheMiss::Divergent, "divergent"),
            (PrefixCacheMiss::Evicted, "evicted"),
        ] {
            let miss = PrefixCache {
                model: "qwen2.5-coder-3b".to_owned(),
                outcome: PrefixCacheOutcome::Miss {
                    reason,
                    processed_tokens: 2_048,
                },
            };
            round_trip(&miss);

            let wire = envelope_wire(Event::PrefixCache(miss));
            assert_eq!(wire["outcome"], "miss");
            assert_eq!(wire["reason"], spelling);
            assert_eq!(wire["processed_tokens"], 2_048);
        }
    }

    #[test]
    fn every_eviction_reason_has_its_own_wire_spelling() {
        for (reason, spelling) in [
            (EvictionReason::MemoryPressure, "memory_pressure"),
            (EvictionReason::EngineUnload, "engine_unload"),
            (EvictionReason::GenerationFailed, "generation_failed"),
        ] {
            let evicted = PrefixCache {
                model: "qwen2.5-coder-3b".to_owned(),
                outcome: PrefixCacheOutcome::Evicted { reason },
            };
            round_trip(&evicted);

            let wire = envelope_wire(Event::PrefixCache(evicted));
            assert_eq!(wire["outcome"], "evicted");
            assert_eq!(wire["reason"], spelling);
        }
    }

    /// An outcome this build cannot read must fail loudly rather than default
    /// to one — the same posture `web_consent_decided` holds. Silently reading
    /// an unknown outcome as `hit` would report reuse that never happened.
    #[test]
    fn an_unknown_prefix_cache_outcome_is_rejected() {
        assert!(
            serde_json::from_str::<PrefixCacheOutcome>(r#"{"outcome":"partial"}"#).is_err(),
            "an unknown outcome must not silently become a known one"
        );
        assert!(
            serde_json::from_str::<PrefixCacheMiss>(r#""stale""#).is_err(),
            "an unknown miss reason must not silently become a known one"
        );
    }

    // -----------------------------------------------------------------------
    // daemon_lifetime — REQ-565
    // -----------------------------------------------------------------------

    /// All five of the spec's lifetime events ride one variant, and each stage
    /// round-trips with the spelling the acceptance suite greps for.
    #[test]
    fn every_lifetime_stage_round_trips_under_one_event_name() {
        let stages = vec![
            (
                DaemonLifetimeStage::ClientConnected {
                    live_connection_count: 1,
                },
                "client_connected",
            ),
            (
                DaemonLifetimeStage::ClientDisconnected {
                    live_connection_count: 0,
                },
                "client_disconnected",
            ),
            (
                DaemonLifetimeStage::ShutdownArmed {
                    policy: "on-last-disconnect".to_owned(),
                    linger_seconds: 0,
                },
                "shutdown_armed",
            ),
            (
                DaemonLifetimeStage::ShutdownDeferred {
                    blocking_activity: BlockingActivity::Turn,
                },
                "shutdown_deferred",
            ),
            (
                DaemonLifetimeStage::Shutdown {
                    reason: ExitReason::LastClient,
                    uptime_seconds: 42,
                    sessions_closed: 2,
                },
                "shutdown",
            ),
        ];

        for (stage, tag) in stages {
            let event = Event::DaemonLifetime(DaemonLifetime {
                stage: stage.clone(),
            });
            assert_eq!(event.name(), "daemon_lifetime");

            let json = serde_json::to_string(&event).expect("serialize");
            assert!(
                json.contains(&format!("\"stage\":\"{tag}\"")),
                "stage tag `{tag}` missing from {json}"
            );

            let back: Event = serde_json::from_str(&json).expect("deserialize");
            match back {
                Event::DaemonLifetime(lifetime) => assert_eq!(lifetime.stage, stage),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    /// The payload spellings AC-3 asserts on. A rename here silently breaks the
    /// acceptance suite's log grep, so they are pinned.
    #[test]
    fn blocking_activity_and_exit_reason_use_the_specs_spellings() {
        let pairs = [
            (BlockingActivity::Turn, "\"turn\""),
            (BlockingActivity::ModelDownload, "\"model_download\""),
            (BlockingActivity::ModelLoad, "\"model_load\""),
            (BlockingActivity::LedgerFlush, "\"ledger_flush\""),
        ];
        for (activity, expected) in pairs {
            assert_eq!(serde_json::to_string(&activity).unwrap(), expected);
        }

        let reasons = [
            (ExitReason::LastClient, "\"last_client\""),
            (ExitReason::StartupUnclaimed, "\"startup_unclaimed\""),
            (ExitReason::Signal, "\"signal\""),
        ];
        for (reason, expected) in reasons {
            assert_eq!(serde_json::to_string(&reason).unwrap(), expected);
        }
    }

    /// Declaration order decides which blocker a mixed set reports
    /// (`teton_core::lifetime` takes the lowest), so it is a contract, not an
    /// accident of how the variants happened to be typed.
    #[test]
    fn blocking_activity_ordering_is_a_contract() {
        assert!(BlockingActivity::Turn < BlockingActivity::ModelDownload);
        assert!(BlockingActivity::ModelDownload < BlockingActivity::ModelLoad);
        assert!(BlockingActivity::ModelLoad < BlockingActivity::LedgerFlush);
    }

    /// REQ-569's two events on the wire: the names the spec's Events table
    /// fixes, and the four payload keys `attach_consent_requested` carries —
    /// one of which (`session_id`) comes from the envelope rather than from the
    /// struct, because the flatten would emit it twice otherwise.
    #[test]
    fn the_attach_consent_events_carry_the_spec_payload_under_their_spec_names() {
        let requested = Event::AttachConsentRequested(AttachConsentRequested {
            request_id: RequestId::from("consent-0"),
            scope: ConsentScope::Attach,
            requester: "cli client \"teton\"".to_owned(),
        });
        assert_eq!(requested.name(), "attach_consent_requested");
        let wire = envelope_wire(requested);
        assert_eq!(wire["event"], "attach_consent_requested");
        assert_eq!(wire["request_id"], "consent-0");
        assert_eq!(wire["scope"], "attach");
        assert_eq!(wire["requester"], "cli client \"teton\"");
        assert_eq!(
            wire["session_id"], "s1",
            "the session comes off the envelope — a field here would collide \
             with it under the flatten"
        );

        let refused = Event::AttachRefused(AttachRefused {
            request_id: Some(RequestId::from("consent-0")),
            scope: ConsentScope::Attach,
            reason: AttachRefusedReason::ConsentTimeout,
        });
        assert_eq!(refused.name(), "attach_refused");
        let wire = envelope_wire(refused);
        assert_eq!(wire["event"], "attach_refused");
        assert_eq!(wire["reason"], "consent_timeout");
        assert_eq!(
            wire["request_id"], "consent-0",
            "a refusal names the request it ends, so a surface rendering two \
             prompts for one session retires the right one"
        );

        // The one refusal that ends no request carries no id — and omits the
        // key rather than sending a null a client would have to special-case.
        let no_prompt = envelope_wire(Event::AttachRefused(AttachRefused {
            request_id: None,
            scope: ConsentScope::Monitor,
            reason: AttachRefusedReason::NoGrant,
        }));
        assert!(no_prompt.get("request_id").is_none(), "{no_prompt}");
        assert_eq!(no_prompt["reason"], "no_grant");
    }

    /// REQ-569 verify (F6): the grant announcement is **daemon-scoped** and
    /// says out loud whether it was self-approved.
    ///
    /// The absent `session_id` is the load-bearing assertion. It is what makes
    /// REQ-568's delivery rule broadcast the frame to every handshaked
    /// connection rather than to the target session's attachees — an
    /// announcement only the beneficiary can see is not an announcement — and it
    /// is simultaneously what keeps the frame from leaking an id BR-10 withholds
    /// from those same connections.
    #[test]
    fn a_minted_grant_is_announced_daemon_wide_and_names_its_approver_arm() {
        let minted = Event::SessionGrantMinted(SessionGrantMinted {
            scope: ConsentScope::Attach,
            requester: "cli client \"teton\"".to_owned(),
            approver: "cli client \"teton\"".to_owned(),
            self_approved: true,
            suppressed: 0,
            attestation: "os_biometric".to_owned(),
        });
        assert_eq!(minted.name(), "session_grant_minted");

        let env = EventEnvelope::new(7, None, minted);
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        assert_eq!(wire["event"], "session_grant_minted");
        assert_eq!(wire["scope"], "attach");
        assert_eq!(wire["requester"], "cli client \"teton\"");
        assert_eq!(wire["self_approved"], true);
        assert!(
            wire.get("session_id").is_none(),
            "a grant announcement goes to every connection, so it names no \
             session: {wire}"
        );

        // R1: the announcement names **both** parties. The peer-approved shape
        // is the one that needs it — `self_approved` is `false` there whether a
        // real second user answered or an attacker's second connection did, so
        // the descriptors are the only thing on the wire that tells them apart.
        let peer = envelope_wire(Event::SessionGrantMinted(SessionGrantMinted {
            scope: ConsentScope::Attach,
            requester: "cli client \"attacker\"".to_owned(),
            approver: "cli client \"attacker\"".to_owned(),
            self_approved: false,
            suppressed: 12,
            attestation: "none".to_owned(),
        }));
        assert_eq!(peer["self_approved"], false);
        assert_eq!(
            peer["requester"], peer["approver"],
            "two connections, one name: the relation is what a reader acts on, \
             and it has to survive the wire: {peer}"
        );
        assert_eq!(peer["suppressed"], 12);

        // REQ-570 AC-9: the attestation method rides along, and it is what
        // rescues the pair above. Same requester and approver, `self_approved`
        // false — indistinguishable from an honest peer approval on the R1
        // evidence alone. `attestation: "none"` is what tells a reader no human
        // was ever verified.
        assert_eq!(wire["attestation"], "os_biometric");
        assert_eq!(peer["attestation"], "none");
    }

    /// BR-5: each refusal reason has its own spelling, and a monitor request
    /// names no session.
    ///
    /// The spellings are what a client renders from (BUG-152), so a rename is a
    /// wire break rather than a refactor. The daemon-scoped case is asserted
    /// alongside them because "monitor" and "attach to the session that is
    /// null" must not become the same frame.
    #[test]
    fn every_refusal_reason_has_its_own_spelling_and_monitor_names_no_session() {
        for (reason, expected) in [
            (AttachRefusedReason::NoGrant, "\"no_grant\""),
            (AttachRefusedReason::ConsentDenied, "\"consent_denied\""),
            (AttachRefusedReason::ConsentTimeout, "\"consent_timeout\""),
            (AttachRefusedReason::RequesterGone, "\"requester_gone\""),
        ] {
            assert_eq!(serde_json::to_string(&reason).unwrap(), expected);
        }
        for (scope, expected) in [
            (ConsentScope::Attach, "\"attach\""),
            (ConsentScope::Monitor, "\"monitor\""),
        ] {
            assert_eq!(serde_json::to_string(&scope).unwrap(), expected);
        }

        let env = EventEnvelope::new(
            7,
            None,
            Event::AttachConsentRequested(AttachConsentRequested {
                request_id: RequestId::from("consent-1"),
                scope: ConsentScope::Monitor,
                requester: "cli client \"watcher\"".to_owned(),
            }),
        );
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        assert!(
            wire.get("session_id").is_none(),
            "a monitor request asks for every session, so it names none: {wire}"
        );
        assert_eq!(wire["scope"], "monitor");
    }
}
