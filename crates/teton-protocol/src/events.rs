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
//! makes against a registered provider. REQ-583 adds `session_root_changed`
//! (BR-7), which announces that a live session's root moved on the user's
//! `/cd` — always beside a `context_cleared`, since the move clears the
//! conversation. REQ-586 adds `context_pressure` (BR-7), which announces that
//! the context gate dropped blocks, elided one in place, or re-fitted the
//! conversation to a new route's budget — nothing is clamped in silence.
//! REQ-585 adds `skill_invoked` (BR-12), which announces that a user-typed
//! `/name` expanded into a prompt turn, and carries what the echo line and
//! `/verbose` render — never the body, which stays in the file.
//! REQ-589 adds the over-budget offer's three announcements —
//! `skill_over_budget_offered`, `skill_over_budget_accepted` and
//! `skill_over_budget_remedy_applied` (BR-13). They record that an expansion
//! too large for the route's budget was put to the user as a *question*
//! rather than refused outright, what was answered, and what durable write
//! the answer caused — the three facts that tell "nobody was asked" from
//! "somebody was asked and said no".
//! REQ-611 adds `transcript_state` (BR-15), the **only** thing the daemon-side
//! transcript puts on the bus: whether a session is recording and why that
//! changed. Everything the transcript writes to its file — the prompt, the tool
//! input and result, the permission decision, the file's own open and close —
//! is a `tetond` record type and deliberately **not** an [`Event`]
//! (REQ-611 architecture ADR-2, BR-4): widening the bus would change who learns
//! a session's content, which is the one thing that REQ must not do.
//! REQ-612 adds `repo_context_state` (BR-1/BR-2), the **one** event the
//! repository-notes block puts on the bus: whether the file at the session root
//! is riding this session's system prompt, how many bytes of it are, and — when
//! it is not — which of the three reasons applies. The spec's Events table first
//! named two (`repo_context_loaded`, `repo_context_withheld`); architecture
//! ADR-6 folded them into this one event's `state`. It carries **no file name**,
//! for the reason `transcript_state` carries no path: which file the notes came
//! out of is [`crate::methods::SessionContextResult`]'s routed answer to the
//! connection that asked.
//!
//! This list is an index, not decoration: a new variant of [`Event`] that is not
//! named here makes the paragraph above wrong.

use serde::{Deserialize, Serialize};

use crate::effort::ResolvedEffort;
use crate::methods::{
    ProviderHealth, ProviderTestOutcome, RepoContextSource, RepoContextStateKind, RootKind,
    SessionRoot, SkillSource, TierBinding,
};
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
    /// The user moved a live session's root with `/cd` (REQ-583 BR-7).
    SessionRootChanged(SessionRootChanged),
    /// The context gate dropped, elided, or re-fitted conversation to a
    /// turn's budget (REQ-586 BR-7).
    ContextPressure(ContextPressure),
    /// A user-typed `/name` expanded into a prompt turn (REQ-585 BR-12).
    SkillInvoked(SkillInvoked),
    /// A skill call was refused before any file was resolved (BUG-189).
    SkillRefused(SkillRefused),
    /// The `projects` tool found a project this turn (REQ-584 BR-11).
    ProjectMatch(ProjectMatch),
    /// An over-budget skill expansion was put to a human as a question
    /// instead of being refused (REQ-589 BR-3).
    SkillOverBudgetOffered(SkillOverBudgetOffered),
    /// A human answered an over-budget offer with "send it" (REQ-589 BR-1).
    SkillOverBudgetAccepted(SkillOverBudgetAccepted),
    /// An over-budget offer's going-forward remedy was written through
    /// `config/set` (REQ-589 BR-7, BR-8).
    SkillOverBudgetRemedyApplied(SkillOverBudgetRemedyApplied),
    /// A session started at a root worth warning about with **no** boundaries
    /// in force (REQ-597 BR-5).
    UnboundedRootWarning(UnboundedRootWarning),
    /// The shipped default boundary set contributed rows to a starting
    /// session's effective set (REQ-597 System Model).
    BoundaryDefaultsApplied(BoundaryDefaultsApplied),
    /// A session's transcript started or stopped recording (REQ-611 BR-15).
    TranscriptState(TranscriptState),
    /// A session's repository notes were loaded, or exist and were not made
    /// resident (REQ-612 BR-1/BR-2).
    RepoContextState(RepoContextState),
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
            Event::SessionRootChanged(_) => "session_root_changed",
            Event::ContextPressure(_) => "context_pressure",
            Event::SkillInvoked(_) => "skill_invoked",
            Event::SkillRefused(_) => "skill_refused",
            Event::ProjectMatch(_) => "project_match",
            Event::SkillOverBudgetOffered(_) => "skill_over_budget_offered",
            Event::SkillOverBudgetAccepted(_) => "skill_over_budget_accepted",
            Event::SkillOverBudgetRemedyApplied(_) => "skill_over_budget_remedy_applied",
            Event::UnboundedRootWarning(_) => "unbounded_root_warning",
            Event::BoundaryDefaultsApplied(_) => "boundary_defaults_applied",
            Event::TranscriptState(_) => "transcript_state",
            Event::RepoContextState(_) => "repo_context_state",
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
    /// The word budget this route attempt's context was fitted to (REQ-586
    /// BR-8) — the `HarnessConfig` pair's token half, as the router derived it
    /// for this attempt, never recomputed by a surface.
    ///
    /// `Option` is for **wire additivity only**, exactly as [`Self::effort`]:
    /// a daemon that has this field always populates it, a frame from a daemon
    /// predating it carries no key and reads `None`, and a client predating it
    /// ignores a key serde does not require it to know — so this moves neither
    /// [`crate::PROTOCOL_VERSION`] nor [`crate::PROTOCOL_VERSION_MIN`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub budget_tokens: Option<u64>,
    /// The byte budget of the same attempt — the pair's other half. Both
    /// currencies ride the event because on a remote route the byte guard is
    /// what binds for prose and code, and the word figure alone would overstate
    /// what fits (architecture "Derivation"). Same additivity as
    /// [`Self::budget_tokens`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub budget_bytes: Option<u64>,
    /// Which constraint bound the budget (REQ-586 BR-8) — computed once, where
    /// the route is decided, and what `/verbose`, `/doctor`, `context_pressure`
    /// and every refusal read. Same additivity as [`Self::budget_tokens`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bound: Option<BudgetBound>,
    /// The per-prompt **spend** ceiling in force, in micro-cents (REQ-588 BR-2).
    ///
    /// The money twin of [`Self::bound`], and additive in the same way: absent
    /// on every turn where the user has set no ceiling, which is what makes an
    /// un-opted-in turn serialize byte-identically to before this REQ. The
    /// figure rides rather than the rendered dollars so the surface can format
    /// it once, through the same composer the refusal uses — two surfaces
    /// describing one ceiling must not be able to disagree (BR-2).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub spend_ceiling_micro_cents: Option<u64>,
    /// Whether the derivation had to **raise** this attempt's pair to its floor
    /// — so [`Self::bound`] names what the user declared, and the budget above
    /// is larger than that declaration asked for (REQ-586 TASK-194 2b).
    ///
    /// The floor is the smallest budget that can still hold the harness's own
    /// system prompt. A `context_budget_cap` of 500 on a 200k provider derives
    /// *below* it, so the pair is raised and the turn gets more room than the
    /// cap asked for — a bound reported as `user cap` with nothing beside it
    /// would be a surface claiming a ceiling that is not in force. Rendered as
    /// a clause on the `/verbose` route line and on every pressure line; the
    /// remedy (`/doctor`'s advisory) reads the same fact off the snapshot.
    ///
    /// Same additivity as [`Self::budget_tokens`]: a daemon that has this field
    /// always populates it, and `None` means a daemon that predates it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bound_floored: Option<bool>,
    /// How many bytes of the repository's own notes this route may carry
    /// (REQ-612 BR-3, BR-7): `min(8192, budget_bytes / 4)`.
    ///
    /// The **ceiling** the route stamps, not what is resident — the resident
    /// figure is a property of the file and rides
    /// [`RepoContextState::resident_bytes`]. Both are on the wire because
    /// `/verbose` renders them as a pair (`notes 2,310 B / cap 4,096 B`) and a
    /// user weighing a file against a budget needs to know whether the number
    /// they are looking at is against the cap or well under it.
    ///
    /// Derived where the budget is, so a floored route's quarter and the cap the
    /// truncation marker names are one number asked twice rather than two that
    /// can drift.
    ///
    /// Same additivity as [`Self::budget_tokens`]: a daemon that has this field
    /// always populates it, and `None` means a daemon that predates it — which
    /// renders the pre-REQ-612 clause byte for byte.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo_context_cap: Option<u64>,
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
    /// What is being consented to, as a **structure** rather than a sentence
    /// (REQ-585 BR-11, architecture ADR-7).
    ///
    /// Absent on every request a pre-REQ-585 daemon raises and on every
    /// ordinary tool prompt, so this is additive and moves no version. Present
    /// when the client must be able to recognize the request **without parsing
    /// the permission key**: BR-11 says so outright, because the key's
    /// `skill:<source>:<name>` shape is an implementation detail and a client
    /// sniffing an unstable string would mis-fire in the one direction that
    /// costs a swallowed stdin line.
    ///
    /// It is a structure and not a [`Self::description`] string for a
    /// mechanical reason as well: `Surface::line` destroys newlines, so "every
    /// command of the invocation, listed verbatim" cannot ride a one-line
    /// description — the client has to render one line per command, from a
    /// list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<PermissionSubject>,
}

/// What a [`PermissionRequest`] is about, in a form a client selects on by
/// **kind** rather than by string (REQ-585 ADR-7).
///
/// [`OPTION_ID_ENABLE_PERMANENT`] is the shipped precedent for the one value a
/// client may match by string; everything else is matched by a typed
/// discriminant, and this is that discriminant for the *subject* of a request.
///
/// **Fail-closed by construction.** [`Self::Unrecognized`] exists so that a
/// subject a client has never heard of maps to a variant it *can* see: the
/// client refuses (with
/// [`crate::methods::RefusalReason::UnrecognizedSubject`]) instead of falling
/// through to `prompter.ask`, which on a pipe would read the user's next line
/// and turn a pasted `y` into consent for shell commands. A `kind` this build
/// does not know must therefore **deserialize**, not error — that is the whole
/// property, and it is pinned by a test rather than left to serde's defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionSubject {
    /// A skill's dynamic-context commands, asked about **once per invocation**
    /// with every command shown (REQ-585 BR-6).
    SkillDynamicContext {
        /// The skill's dispatchable name.
        skill: String,
        /// Which root it came from — half of the grant key, and the half that
        /// decides whether the grant survives a `/cd` (ADR-6).
        source: SkillSource,
        /// Every command of this invocation, in document order, **already
        /// substituted**: BR-4 puts `$ARGUMENTS`/`$N` substitution before
        /// execution precisely so the consent shows what will actually run.
        ///
        /// Each entry is file-supplied text, bounded and rendered on one line
        /// by the daemon and defused again by the client's `Surface`.
        commands: Vec<String>,
        /// Who invoked the skill whose commands these are (REQ-587 BR-5).
        ///
        /// BR-5 requires the consent to say **who asked**: "you asked for
        /// `deploy`" and "the model decided to run `deploy`" are different
        /// questions that carry the same command list, and the human at
        /// `guarded` is entitled to know which one is on the screen.
        ///
        /// Additive, absent means [`InvokedBy::User`] — a request from a daemon
        /// predating REQ-587 could only ever have been the user's, and a client
        /// predating the field renders REQ-585's prompt unchanged. Note what
        /// that costs and what it does not: the prompt still lists every
        /// command verbatim under the skill's own key, so the *decision* the
        /// user makes is the same one; only the attribution is missing.
        #[serde(default, skip_serializing_if = "InvokedBy::is_user")]
        invoked_by: InvokedBy,
    },
    /// May the model run **this repository's** skills as instructions at all
    /// (REQ-587 BR-4, architecture ADR-7)?
    ///
    /// Asked once per session per root, before any expansion, under
    /// [`crate::methods::project_skill_trust_key`] — never a skill's own key
    /// and never a tool's name (LESSON-495: the key encodes the question).
    /// Nothing here grants an *effect*: `shell`, `edit` and each skill's
    /// dynamic-context key gate effects exactly as they did. What it guards is
    /// the one channel by which repository text reaches the model labelled
    /// *instructions* rather than *data* with no human typing its name.
    ///
    /// # A new variant is not additive the way a new field is
    ///
    /// A field a client has never heard of is ignored; a *variant* it has never
    /// heard of lands on [`Self::Unrecognized`], and that arm is a **refusal**,
    /// not an ignore — the client answers
    /// [`crate::methods::RefusalReason::UnrecognizedSubject`] without asking
    /// anyone, and the daemon tells the model `project_not_acknowledged`.
    ///
    /// So on a REQ-585-vintage client a project skill is **never**
    /// model-invocable. That is a shipped consequence rather than a bug, and it
    /// is worth stating plainly:
    ///
    /// - it is fail-closed, which is the entire purpose of `Unrecognized`;
    /// - it is announced, not silent — the client prints a refusal line naming
    ///   the request's key, and the model is given a typed refusal;
    /// - the next step it names is one that client can actually perform:
    ///   `/permissions full` ([`crate::permissions::PermissionLevel::Full`]),
    ///   at which BR-4 allows a project skill with no acknowledgment at all.
    ///   The exception is a project skill that *shadows* a user skill, which
    ///   asks even at `full`; that one stays refused until the client
    ///   understands this subject, and the only fix is upgrading the client.
    ///
    /// Pinned by `project_skill_trust_is_a_variant_an_older_client_refuses`,
    /// because the field-additivity test does not cover a variant and would
    /// pass while this held or failed.
    ProjectSkillTrust {
        /// The session root the acknowledgment is scoped to, **home-relative**
        /// — never an absolute path carrying a username into a transcript
        /// (REQ-585 BR-1's entity table).
        ///
        /// It is the same spelling [`crate::methods::project_skill_trust_key`]
        /// builds the grant key from, so what the user sees and what the answer
        /// is remembered under cannot name two different repositories.
        ///
        /// # Repository-authored: unbounded, not control-stripped, and the
        /// client defuses (REQ-591 BR-11)
        ///
        /// **This paragraph is a correction.** The contract used to say this
        /// field was `session_root::display_for`-minted and *"bounded"*, and
        /// both halves were false. The minter is the daemon's
        /// `tools::skill::trust_root_name` — `display_for` was replaced because
        /// it renders every non-UTF-8 byte as `U+FFFD`, which is not injective
        /// and so let two repositories mint one grant key — and nothing anywhere
        /// truncates or filters the result. A client that read the old sentence
        /// at face value would render this string raw, and a **directory name**
        /// is repository-authored input: a newline or an ESC in it is valid
        /// UTF-8 and arrives here exactly as it sits on disk.
        ///
        /// The daemon does not bound or strip it, and that is a decision rather
        /// than an omission:
        ///
        /// - **Truncating would re-open the collision the minter closed.** This
        ///   string is the grant key's source, and
        ///   [`crate::methods::project_skill_trust_key`]'s own doc refuses
        ///   truncation for exactly that reason — two roots cut to one prefix
        ///   are one key, so an acknowledgment given about one repository would
        ///   answer for another.
        /// - **Stripping is not injective either**, and it would break the
        ///   guarantee two paragraphs up: the prompt would name one string and
        ///   the answer be remembered under a different one, which is the
        ///   LESSON-495 failure in miniature.
        ///
        /// So the contract is the one `skills` already carries, for the same
        /// reason: **the client defuses at render, as it does every other
        /// file-derived string.** Teton's own CLI writes it through
        /// `render::Surface::line`, which neutralizes every control character —
        /// so there is no exploit on the shipped client — but a third-party
        /// client that writes this field straight to a terminal lets a directory
        /// name move the cursor and rewrite the row that asked the question.
        root: String,
        /// The project's model-invocable skills, so the user is answering about
        /// a named set rather than a category (BR-4).
        ///
        /// **Bounded by the daemon**, at most twenty entries: an unbounded
        /// prompt is LESSON-517's shape, and the tail rides as `more`'s count
        /// instead. Each `name` is file-supplied and already matched
        /// REQ-585's `^[a-z0-9][a-z0-9_-]{0,63}$` to be registered at all; the
        /// client defuses again at render, as it does every other file-derived
        /// string.
        skills: Vec<ProjectSkillTrustEntry>,
        /// How many model-invocable project skills `skills` does not list —
        /// the `+N more` tail, `0` when the list is complete.
        ///
        /// A count rather than a truncation flag: "and 5 more" and "and some
        /// more" are different facts, and the user is being asked to trust the
        /// whole set.
        more: u32,
        /// Who reached for a skill from this repository (REQ-589 BR-6, REQ-587
        /// BR-5).
        ///
        /// **The same field [`Self::SkillDynamicContext`] carries, for the same
        /// reason, and it arrived here late.** REQ-587 minted this question when
        /// the model's tool was its only caller, so the prompt could name the
        /// model outright. REQ-589 ADR-10 gave the typed `/name` path the same
        /// door — and on that path no model asked for anything, which left a
        /// security prompt making a false statement about who is acting, on the
        /// one question whose whole job is letting a human decide whether to
        /// trust a repository.
        ///
        /// The **answer** is invoker-independent and stays so: one key per root
        /// (`crate::methods::project_skill_trust_key`), one answer per session,
        /// and a grant the user gave at their own prompt still frees the model's
        /// later reach. This field changes what the question *says*, never what
        /// it is remembered under.
        ///
        /// Additive, and **absent means [`InvokedBy::Model`]** — the one
        /// `invoked_by` on this wire whose default is not `User`, which is a
        /// decision and not an oversight.
        ///
        /// Every other `invoked_by` defaults to `User` because a daemon
        /// predating it could only ever have reported a typed invocation. Here
        /// the history runs the other way: this subject was minted by REQ-587
        /// with the model's tool as its *only* caller, so a request with no
        /// `invoked_by` came from a daemon on which the model was the only thing
        /// that could ask. Defaulting to `User` would make such a request render
        /// as "you asked" when the model asked — the very false statement this
        /// field exists to remove, told in the more dangerous direction, on a
        /// prompt a human answers about trusting a repository. The default is
        /// therefore the conservative reading, and the skip predicate follows
        /// it: the model path writes no key, so its wire stays byte-identical to
        /// REQ-587's and neither [`crate::PROTOCOL_VERSION`] nor
        /// [`crate::PROTOCOL_VERSION_MIN`] moves.
        ///
        /// A client predating the field ignores the typed path's key and renders
        /// REQ-587's model wording — this defect, unfixed, on that client only.
        #[serde(
            default = "InvokedBy::model",
            skip_serializing_if = "InvokedBy::is_model"
        )]
        invoked_by: InvokedBy,
    },
    /// A skill expansion measured **larger than the route's context budget**,
    /// put to a human as a question instead of refused (REQ-589 BR-3,
    /// architecture ADR-2).
    ///
    /// Raised on the **user-typed path only**. A model-invoked expansion keeps
    /// today's refusal and is never offered a choice (BR-2): there is no human
    /// inside a mid-loop tool call to answer per-invocation, and a consent
    /// nobody could give is not one to ask for.
    ///
    /// # Facts the daemon already has, never a second measurement
    ///
    /// Every figure here is **read off the measurement that already
    /// happened** — `skill_fit`'s pair and the router's stamped
    /// [`BudgetBound`] — and none of it is re-derived to be shown. A second
    /// estimator beside the one that refused is LESSON-456's shape, and
    /// REQ-586's own verify pass caught exactly that once already.
    ///
    /// The daemon says what it expects and asks anyway. There is no overrun
    /// ceiling above which it stops asking, because a prediction of failure is
    /// a thing to *say*, not a reason to withhold the choice (BR-3);
    /// [`Self::window_verdict`] is what selects which true sentence is said.
    ///
    /// # What is deliberately absent
    ///
    /// **No provider response body, and no field one could ride in.** The
    /// daemon-side invariant `a_skill_refusal_carries_no_provider_response_body`
    /// exists because a provider's error text is remote-supplied prose with no
    /// business on a consent prompt — it is the one string on this path that
    /// something upstream of the user controls. What travels instead is
    /// [`WindowVerdict`]: a typed verdict computed from integers, never a
    /// quotation.
    ///
    /// **No overrun pair.** `measured − budget` is a `saturating_sub` at the
    /// surface that renders it. Carrying it as well would be two ways to say
    /// one fact — LESSON-545's shape — and the two could then disagree.
    ///
    /// **No remedy.** What a durable fix would be rides the request's
    /// [`PermissionOption`] list instead (ADR-1): the remedy-bearing option ids
    /// appear only where BR-7 grants this bound a remedy, and each label names
    /// the concrete write. A `remedy` field here could say a remedy exists
    /// while the options offered none, which is the disagreement the single
    /// representation rules out.
    ///
    /// # A new variant is a refusal on an older client, and that is the point
    ///
    /// [`Self::ProjectSkillTrust`]'s note applies verbatim. A client that
    /// predates this `kind` lands on [`Self::Unrecognized`] and answers
    /// [`crate::methods::RefusalReason::UnrecognizedSubject`] without asking
    /// anyone; the turn then refuses under today's sentence, which is precisely
    /// what BR-4 requires — a declined or unanswerable offer *is* today's
    /// refusal, and silence is never consent. So the catch-all arm below must
    /// stay a refusal rather than be softened; an old client that guessed here
    /// would send an oversized turn nobody approved.
    SkillOverBudget {
        /// The skill's dispatchable name, already matched
        /// `^[a-z0-9][a-z0-9_-]{0,63}$` to be registered at all.
        skill: String,
        /// Which root it came from.
        ///
        /// Carried for the reason ASSUME-018 states: the name in a
        /// project-sourced offer is **repository-authored** text, and it has to
        /// render under the same distinguishing treatment project skills
        /// already get rather than as bare harness vocabulary. The client
        /// cannot apply that treatment from a name alone, so the source travels
        /// with it — the same pairing [`Self::SkillDynamicContext`] carries.
        source: SkillSource,
        /// Which of BR-8's two budget checks measured this expansion.
        ///
        /// It changes what the user can *do* about the answer, which is why it
        /// is on the prompt: a Stage A refusal is about the body, while a
        /// Stage B one has to say that the dynamic context output is what spent
        /// the room.
        stage: SkillStage,
        /// The word figure `skill_fit` measured, verbatim.
        ///
        /// Spelled `tokens` because that is what [`ContextPressure`] and
        /// [`RouteDecided`] already call this figure on the wire. A third
        /// spelling for one number is LESSON-528's shape — identical today,
        /// and identical only until one of them is edited.
        measured_tokens: u64,
        /// The byte figure `skill_fit` measured, verbatim.
        measured_bytes: u64,
        /// The word budget the router stamped for this route, verbatim — the
        /// same value [`RouteDecided::budget_tokens`] carried.
        budget_tokens: u64,
        /// The byte budget the router stamped for this route, verbatim.
        budget_bytes: u64,
        /// Which constraint bound that budget, read off the stamped
        /// [`RouteDecided::bound`] and never re-derived here.
        bound: BudgetBound,
        /// What the route's declared window says about this expansion (BR-3).
        window_verdict: WindowVerdict,
        /// **The question, worded by the daemon** — rendered verbatim, never
        /// re-composed (REQ-589 ADR-16, BR-5).
        ///
        /// # Why a sentence rides a structured subject at all
        ///
        /// Everything else on this variant is a *fact*, and this crate's
        /// standing rule is that the daemon states facts while the client
        /// writes the line ([`Self::ProjectSkillTrust`]'s entries are the
        /// exemplar). This field is the deliberate exception, and TASK-243 is
        /// what forced it: BR-5 requires the offer question, the decline
        /// refusal and the acceptance record to be **one** composer's three
        /// arms, because the decline refusal has to be byte-identical to the
        /// `-32023` sentence this route already produced (AC-3) and the three
        /// must quote one measurement. That composer is `tetond`'s
        /// `skill_refusal`. Of the three sentences it writes, only the option
        /// **labels** had a surface: the four `PermissionOption`s. The verdict
        /// clause (BR-3), BR-7b's "this bound has no durable fix" and BR-14.2's
        /// observed-rejection lead reached no reader at all — a producer with
        /// no consumer, invisible to a green suite (LESSON-544).
        ///
        /// A client that re-worded those three from `stage`, `bound` and
        /// [`Self::window_verdict`] would be the **second composer** BR-5
        /// forbids, and the two would drift the first time either was edited.
        /// So the words have one home and travel finished.
        ///
        /// # The structure is not redundant beside it
        ///
        /// The client still reads every field around this one: for layout and
        /// emphasis, for deciding which option rows to draw, and for the
        /// [`WindowVerdict::Unknown`] hedge, which is a statement about *this
        /// build's* vocabulary rather than about the route and therefore cannot
        /// come out of a sentence the daemon wrote.
        ///
        /// # What can be in it
        ///
        /// Exactly what the composer admits: integers this daemon measured, two
        /// literal config key names, the skill's name, and a sanitized provider
        /// id. **No provider response body** — none is in scope on this path,
        /// which is the whole difference between `-32023` and `-32022`, and is
        /// what `a_skill_refusal_carries_no_provider_response_body` pins on the
        /// daemon side. A project-sourced skill's name is repository-authored
        /// text (ASSUME-018); the composer marks it, and a client defuses it at
        /// render as it does every other file-derived string.
        ///
        /// Required rather than `#[serde(default)]`, unlike the tolerant arms on
        /// [`SkillStage`] and [`WindowVerdict`]: those exist for a *value* a
        /// later build might mint inside a known kind, whereas no daemon that
        /// can emit this `kind` at all predates this field — the whole variant
        /// is REQ-589's. A default would only hide a daemon that stopped
        /// wording its own question.
        sentence: String,
        /// The provider whose window or cap is in question, when the route has
        /// one to name.
        ///
        /// Absent for a route whose bound names no provider — and absent is a
        /// *fact* rather than a gap, since a remedy the offer cannot address to
        /// a provider is one the user cannot act on. Sanitized and bounded by
        /// the daemon like every other identifier that reaches a prompt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_id: Option<ProviderId>,
    },
    /// A subject this build does not know. Never constructed by a daemon —
    /// serde produces it when the `kind` is one this build has never heard of,
    /// which is exactly the case the client must refuse rather than guess at.
    #[serde(other)]
    Unrecognized,
}

/// One model-invocable project skill, as the acknowledgment prompt lists it
/// (REQ-587 BR-4).
///
/// A structure and not a pre-marked sentence, on [`PermissionSubject`]'s own
/// rule: the daemon states the facts and the client renders the line. The
/// shadowing mark in particular (`validate (project — shadows your user
/// skill)`) is rendered on both sides of the wire — the prompt here and the
/// expansion's source line in the daemon's frame — and a pre-marked string
/// would make the client's copy a re-parse of the daemon's prose (LESSON-529).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSkillTrustEntry {
    /// The skill's dispatchable name.
    pub name: String,
    /// Whether a **user** skill of the same name exists, which this project
    /// skill takes the name from (REQ-585 BR-2).
    ///
    /// The one case a `full` session can be surprised by, and therefore the one
    /// case BR-4 asks about even at `full`: the model invokes `validate`
    /// meaning the skill the user installed, and gets a body the repository
    /// substituted. Marked in the prompt so the swap is acknowledged rather
    /// than discovered.
    ///
    /// Omitted from the wire when `false` — the ordinary entry's shape, and the
    /// `probe` rule in a second place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadows_user_skill: bool,
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

// The four `option_id`s of REQ-589's over-budget offer (BR-7, ADR-1).
//
// BR-7's answer is two independent booleans — send *this* turn's expansion, and
// write the going-forward fix — while [`crate::methods::PermissionOutcome`] is
// single-choice. Rather than widen that outcome for one caller, or ask two
// sequential questions whose second is unanswerable once the first is declined,
// the four combinations ship as four named ids on the wire that already exists.
// The remedy-bearing pair is appended to the option list **only** where BR-7
// grants that bound a remedy, exactly as the daemon appends
// [`OPTION_ID_ENABLE_PERMANENT`] only when a web tier is in hand.
//
// They are told apart **by id** for that constant's reason: all four share the
// same handful of [`PermissionOptionKind`] values, so a client selecting by kind
// alone could not tell "send it once" from "send it and write the fix". They
// live here beside it because this is where the option-id vocabulary lives — a
// second home for the same class of string is what makes two crates agree only
// until one of them is edited.
//
// The **labels** are the daemon's, and ADR-1 binds them: an option that writes
// config names the concrete write (`capabilities.max_context = 1000000` for
// `kimi`), never "raise the limit". `enable_permanent` carries a comment
// recording that an earlier version promised a write that was silently a no-op,
// which is the failure that rule exists to prevent.

/// Send this turn's expansion whole and write nothing.
///
/// Per-invocation and never persisted (BR-10): the next oversized expansion
/// asks again, because a stored consent could send something nobody approved.
pub const OPTION_ID_OVER_BUDGET_PROCEED_ONCE: &str = "over_budget_proceed_once";

/// Send this turn's expansion whole **and** write the going-forward remedy.
///
/// Offered only where BR-7 gives this bound a remedy, so its presence in an
/// option list is itself the statement that a durable fix exists.
pub const OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY: &str = "over_budget_proceed_and_remedy";

/// Write the going-forward remedy and **do not** send this turn.
///
/// A legitimate answer, not a degenerate one: it is the right choice for a user
/// who wants the limit fixed but does not want this particular oversized turn to
/// run. The turn refuses exactly as BR-4 describes, with the fix already made.
pub const OPTION_ID_OVER_BUDGET_REMEDY_ONLY: &str = "over_budget_remedy_only";

/// Send nothing and write nothing — today's refusal, chosen rather than imposed.
pub const OPTION_ID_OVER_BUDGET_DECLINE: &str = "over_budget_decline";

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
// session_root_changed (REQ-583)
// ---------------------------------------------------------------------------

/// A live session's root moved (REQ-583 BR-7, architecture ADR-4).
///
/// Published on every accepted `session/set_cwd`, right beside the
/// [`ContextCleared`] the move implies — the two are one user action, and a
/// client that renders only the clear would report a reset without its cause.
/// The client that typed `/cd` has the RPC's own answer; this event exists so
/// every *other* attached client learns the root moved (BR-8's re-announce
/// keys off it) and so the issuing client's cached root is refreshed from a
/// daemon fact rather than from what it asked for.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`ContextCleared`], [`SessionTitled`] and [`PrefixCache`] document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRootChanged {
    /// The display of the root the session had before the move — the same
    /// spelling its banner and refusals used.
    pub previous_display: String,
    /// The root the session has now, as the daemon derived it.
    pub root: SessionRoot,
}

// ---------------------------------------------------------------------------
// context_pressure (REQ-586)
// ---------------------------------------------------------------------------

/// Which constraint bound a route attempt's context budget (REQ-586 BR-8).
///
/// One fact with one source: the router computes this where it decides the
/// route, stamps it on [`RouteDecided`], and every surface — `/verbose`,
/// `/doctor`, [`ContextPressure`], the refusal texts — reads that value rather
/// than re-deriving it (LESSON-456: one classifier per fact). The precedence
/// among the variants is the router's to state and test; this enum only names
/// the outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetBound {
    /// The provider's declared context window, less the generation
    /// reservation, is what bound the budget — the ordinary remote case.
    Window,
    /// The provider declared no window (`capabilities.max_context = 0`), so the
    /// default pair applies — stated, never silent (BR-3).
    DefaultUnknown,
    /// `[privacy] redact = true`, and the bytes the redact scan can cover bound
    /// the budget below what the window would have allowed (BR-4).
    RedactScan,
    /// The user's `capabilities.context_budget_cap` sat below the window and is
    /// what bound the budget (BR-5).
    UserCap,
    /// The route is the local engine; its budget is the local pair regardless
    /// of any declared window.
    LocalEngine,
    /// A bound this build does not know (REQ-588 BR-4).
    ///
    /// Same direction and same reasoning as [`ContextPressureKind::Unknown`]:
    /// a future constraint must cost the *name* of the bound, never the event
    /// that carries it.
    #[serde(other)]
    Unknown,
}

impl BudgetBound {
    /// The wire spelling, identical to the serialized tag — [`Event::name`]'s
    /// arrangement, one level down.
    ///
    /// Snake_case, because that is what the field carries; the words a person
    /// is shown are [`BudgetBound::words`]. Kept as its own accessor so a
    /// surface that needs the token — a log line, a machine-read `/doctor`
    /// row — asks for it rather than reaching for `serde_json` to get a
    /// string out of a five-way enum.
    #[must_use]
    pub const fn wire_name(&self) -> &'static str {
        match self {
            BudgetBound::Window => "window",
            BudgetBound::DefaultUnknown => "default_unknown",
            BudgetBound::RedactScan => "redact_scan",
            BudgetBound::UserCap => "user_cap",
            BudgetBound::LocalEngine => "local_engine",
            // Round-trips as itself: a client that re-emits what it read must
            // not silently relabel an unknown bound as a known one.
            BudgetBound::Unknown => "unknown",
        }
    }

    /// The words the bound is **said** in: `unknown window`, not
    /// `default_unknown`.
    ///
    /// One table, and it lives here rather than in the CLI because both sides
    /// need it. The client words the `/verbose` route line and every
    /// `context_pressure` line; the daemon words the refusals that name the
    /// bound — REQ-585 BR-8's oversized-skill refusal is the first, and it
    /// runs in `tetond`, which cannot reach a `teton` helper. A second table
    /// over there would be the mirrored-predicate shape LESSON-528 is about:
    /// identical today, and identical only until one of them is edited.
    ///
    /// Each spelling names the thing a user would go and change — a bound of
    /// `unknown window` is a `capabilities.max_context` that was never set,
    /// which is why the wire's `default_unknown` is not what is printed. The
    /// phrases are lower-case fragments, so a caller may set them in a
    /// sentence (`bound: user cap`) or after a colon without re-casing them.
    #[must_use]
    pub const fn words(&self) -> &'static str {
        match self {
            BudgetBound::Window => "window",
            BudgetBound::DefaultUnknown => "unknown window",
            BudgetBound::RedactScan => "redact scan",
            BudgetBound::UserCap => "user cap",
            BudgetBound::LocalEngine => "local engine",
            // Deliberately vague, because it is: this build cannot say WHICH
            // constraint bound the pair, and inventing a plausible-sounding
            // one would name a setting the user could go and change for no
            // reason. Every other phrase here names a real knob.
            BudgetBound::Unknown => "a bound this build does not know",
        }
    }
}

/// A count with thousands separators: `4096` → `4,096` — **the one home** of
/// how a budget's word figure is spelled (REQ-586 BR-8, LESSON-456).
///
/// Budgets are five- and six-digit numbers a reader compares at a glance ("did
/// that turn really only get 4k?"), and an ungrouped `132650` is the one shape
/// that cannot be read at a glance.
///
/// It lives beside [`BudgetBound::words`] and for the same reason: **both ends
/// spell these figures**. The client words the `/verbose` route line and every
/// `context_pressure` line; the daemon words the big-window notice its provider
/// registration surfaces carry (`harness::budget::big_window_notice`), and it
/// runs in `tetond`, which cannot reach a `teton` helper. Two private
/// formatters for one figure is the mirrored-predicate shape LESSON-528 is
/// about — identical today, and identical only until one of them is edited.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let first = digits.len() % 3;
    for (at, ch) in digits.chars().enumerate() {
        if at > 0 && at % 3 == first {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A byte figure for a budget line: `900 B`, `33 KB`, `4.2 MB` — **the one
/// home** of how a budget's byte figure is spelled (REQ-586 BR-8).
///
/// Named for what it *is* rather than for its first caller: `budget_bytes` is
/// the wire field's name (and one call site hands it `elided_bytes`, which is
/// not a budget at all), so a formatter wearing that name read as an accessor.
///
/// **Decimal** units, and labelled as such. The CLI's `firstrun::format_bytes`
/// is the other byte formatter in the workspace and stays where it is: it
/// renders an *exact* download size in the binary units the daemon's own
/// sentences use, where the tenth of a GiB is a fact about a file. A budget is
/// an approximation with a safety ratio already baked into it, so it is rounded
/// to whole KB and never claims a precision the number has not got — and
/// rounding a 1024-based number under a `KB` label is the exact confusion that
/// formatter's doc warns about, which is why this one divides by 1000.
///
/// Shared for [`thousands`]' reason: the daemon composes the same figures.
#[must_use]
pub fn bytes_figure(bytes: u64) -> String {
    if bytes < 1_000 {
        return format!("{bytes} B");
    }
    if bytes < 1_000_000 {
        return format!("{} KB", (bytes + 500) / 1_000);
    }
    let tenths = (bytes + 50_000) / 100_000;
    match tenths % 10 {
        0 => format!("{} MB", tenths / 10),
        frac => format!("{}.{frac} MB", tenths / 10),
    }
}

/// What the context gate did to earn a [`ContextPressure`] event (REQ-586
/// BR-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPressureKind {
    /// Older blocks were dropped from the assembled context to fit the budget.
    BlocksDropped,
    /// A single block was middle-elided in place because it alone exceeded the
    /// budget.
    BlockElided,
    /// The conversation was re-fitted after a reroute or fallback moved the
    /// turn to a route with a different budget (BR-1).
    RefitOnReroute,
    /// The gate ran and the context **still does not fit** either budget — the
    /// turn is being sent over budget (REQ-586 verify m1, TASK-194 2a).
    ///
    /// Two arms of `truncate_to_budget` deliberately stop short: the in-place
    /// clamp floors the last block's room at 1 KiB, and the drop loop stops at
    /// one block whatever the word estimate says. Both are the right
    /// degradation — a turn that cannot fit beats a turn with no content — and
    /// neither may be silent.
    ///
    /// It has its own name because it used to ride as [`Self::BlockElided`]
    /// with `elided_bytes: 0`, which is worse than silence: BR-7's whole claim
    /// is that nothing is clamped without being said, and an event announced
    /// under the wrong name says something that did not happen. A client
    /// predating this variant drops the frame rather than mis-rendering it —
    /// the same fail-closed choice the enum's snake_case tag makes everywhere
    /// else — so no reader is ever told the wrong story about a context that
    /// did not fit.
    DidNotFit,
    /// A kind this build does not know (REQ-588 BR-4).
    ///
    /// Tolerant for BUG-186's reason, applied one enum over: this travels
    /// **daemon → client only** and the surface it feeds is a notice, so
    /// failing closed buys nothing — without this arm a future kind takes the
    /// whole `context_pressure` frame down at `serde_json::from_value` and
    /// BR-7's "nothing is clamped in silence" quietly becomes false.
    ///
    /// The doc on [`Self::DidNotFit`] above notes that a client predating that
    /// variant drops the frame. That is precisely the defect this closes, and
    /// it is why the arm is worth adding before the next kind rather than
    /// after.
    #[serde(other)]
    Unknown,
}

/// The context gate dropped, elided, or re-fitted conversation to a turn's
/// budget (REQ-586 BR-7).
///
/// Emitted whenever `truncate_to_budget` dropped at least one block, elided a
/// block in place, or re-fitted the context after a reroute — nothing is
/// clamped in silence, on any tier. The CLI renders one line, never gated by
/// `/verbose` (the [`ContextCleared`] precedent: the reset is the news), and an
/// elision of the **newest user block** is additionally a turn notice, because
/// that is the case where the model would answer a prompt the user did not
/// send.
///
/// `budget_tokens`, `budget_bytes` and `bound` repeat the route's figures from
/// [`RouteDecided`] so a client can render the line from this event alone,
/// without correlating it back to the route frame.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`ContextCleared`], [`SessionTitled`] and [`PrefixCache`] document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPressure {
    /// What the gate did.
    pub kind: ContextPressureKind,
    /// How many blocks were dropped; `0` for an elision or a refit that dropped
    /// none.
    pub dropped_blocks: u64,
    /// How many bytes an in-place elision removed; `0` when nothing was elided.
    pub elided_bytes: u64,
    /// Whether the block elided was the newest user block — the case the CLI
    /// additionally reports as a turn notice (BR-7).
    pub newest_user_elided: bool,
    /// The word budget the context was fitted to.
    pub budget_tokens: u64,
    /// The byte budget the context was fitted to.
    pub budget_bytes: u64,
    /// Which constraint bound that budget — the route's, read off
    /// [`RouteDecided::bound`], never re-derived here.
    pub bound: BudgetBound,
    /// Whether the derivation had to **raise** the pair to its floor, so the
    /// bound above could not be honored as declared (REQ-586 TASK-194 2b).
    ///
    /// See [`RouteDecided::bound_floored`] for what the floor is and why a
    /// bound that says `user cap` beside a budget larger than that cap is the
    /// untruth this field exists to close.
    ///
    /// `#[serde(default)]` rather than `Option`: this event's other fields are
    /// all required, and `false` is exactly what a frame from a daemon
    /// predating the field means — that daemon floored nothing it could report.
    #[serde(default)]
    pub bound_floored: bool,
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

// ---------------------------------------------------------------------------
/// The best project a turn's `projects` call matched (REQ-584 BR-11).
///
/// **The hand-off is a surface line, not prose the model is asked to say**
/// (REQ-579 ADR-9, LESSON-532). A small model told "tell the user to run
/// `/cd teton-code`" often does not, or garbles the command; the session prints
/// it from this record instead, so the recipe reaches the user whatever the
/// model said. Published only when there was a match — a turn that called the
/// tool and found nothing publishes none, and a turn that never called it
/// publishes none either.
///
/// **What an older client loses, stated rather than assumed.** [`Event`] is a
/// closed enum with no `#[serde(other)]`, so a client built before this variant
/// drops the whole frame at `serde_json::from_value` — it does not render a
/// degraded line, it renders nothing. The cost is one convenience line on a
/// turn whose *answer* arrived normally in the tool result, which is why this
/// ships as an ordinary additive event the way `skill_invoked` and
/// `skill_refused` did before it.
///
/// The general gap — that `Event` itself is not tolerant, where BUG-186 made
/// its inner enums so — is real and outlives this REQ. It is recorded as a
/// follow-up rather than widened here: adding `#[serde(other)] Unknown` to
/// `Event` touches every match on it and deserves its own change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMatch {
    /// The best match's name, bounded and neutralised by the daemon.
    pub name: String,
    /// Its display path, bounded the same way.
    pub display: String,
}

/// A skill call the daemon refused **before resolving it to a file** (BUG-189).
///
/// BR-9 says a refusal is never silent: one line per invocation, one line per
/// typed refusal. Five of the daemon's seven refusal reasons ride
/// [`SkillInvoked`] with `refused` set, because a registry row was in hand to
/// describe. Two never resolve one:
///
/// - `unknown_skill` — no row carries that name;
/// - `invalid_arguments` — the parse is what failed, so there is not even a
///   name to trust (a capped *listing* call is the same shape: it named
///   nothing).
///
/// Those two used to reach the model as a typed result and the human not at
/// all. They could not be forced onto `SkillInvoked`, whose subject is a
/// **file**: it carries a `source`, a `path_display` and a `body_bytes`, and
/// publishing one here would have meant choosing a root the file was never
/// found under and inventing every identifying field but the model's own
/// spelling — a hollow record that reads like a real one, which on a session
/// surface is worse than saying nothing.
///
/// So this is a record whose subject is a **name**, and it carries only what is
/// actually known. `name` is optional because in the `invalid_arguments` case
/// nothing reliable was parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRefused {
    /// The name the call asked for, when one could be read.
    ///
    /// **Untrusted and model-supplied** — it matched no registry row, which is
    /// the whole point of this record. Bounded by the daemon before it is sent
    /// and defused again by the client's `Surface`. Absent when the arguments
    /// did not parse into a usable name at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The daemon's stable refusal id — the same token the model is given, so
    /// both audiences are told the same fact. The client words it.
    pub reason: String,
}

// skill_invoked (REQ-585)
// ---------------------------------------------------------------------------

/// Who issued a skill invocation (REQ-587 BR-9).
///
/// Two doors, named: REQ-585 shipped the user typing `/name`, and REQ-587 adds
/// the model calling the `skill` tool. It is what BR-9's echo line renders
/// (`skill validate (user, 4.6 KB, 2 dynamic commands) — invoked by the
/// model`), what BR-5's consent text says out loud, and the fact BR-4's
/// acknowledgment turns on — a model-typed name has no human behind it at the
/// moment of invocation, and that difference is the whole reason the
/// acknowledgment exists.
///
/// **Closed**, like [`SkillSource`] and [`NotRunReason`] beside it. An
/// externally tagged enum cannot carry serde's `other` arm, so a third invoker
/// would be a deliberate wire change rather than a value silently read as one
/// of these two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokedBy {
    /// The user typed `/name` (REQ-585 BR-4).
    ///
    /// **The default**, because it is what every invocation a daemon predating
    /// REQ-587 could report was.
    #[default]
    User,
    /// The model called the `skill` tool (REQ-587 BR-1).
    Model,
}

impl InvokedBy {
    /// True for [`Self::User`].
    ///
    /// The `skip_serializing_if` predicate every `invoked_by` field uses: a
    /// user invocation writes no key, so its wire stays byte-identical to the
    /// one REQ-585 wrote and neither [`crate::PROTOCOL_VERSION`] nor
    /// [`crate::PROTOCOL_VERSION_MIN`] moves.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }

    /// True for [`Self::Model`].
    ///
    /// The `skip_serializing_if` predicate for the one field whose default is
    /// `Model` rather than `User` — see
    /// [`PermissionSubject::ProjectSkillTrust::invoked_by`], where the history
    /// that inverts it is written down.
    #[must_use]
    pub fn is_model(&self) -> bool {
        matches!(self, Self::Model)
    }

    /// [`Self::Model`], as a `serde` `default` a field attribute can name.
    ///
    /// `#[derive(Default)]` picks [`Self::User`] and that stays right for every
    /// other `invoked_by`; this exists so the one field that must default the
    /// other way can say so at its own declaration instead of the enum
    /// changing meaning underneath the rest.
    #[must_use]
    pub fn model() -> Self {
        Self::Model
    }
}

/// A skill expanded into a turn — a user-typed `/name` (REQ-585 BR-12,
/// architecture ADR-15) or, since REQ-587, a model-issued `skill` call
/// ([`Self::invoked_by`] says which).
///
/// **Typed, not pre-rendered.** The CLI already knows the name and source from
/// its `skills/list` snapshot but not the size, the ignored keys or the
/// outcomes, so *some* event is required; making it a structure rather than a
/// finished sentence is LESSON-544's rule — a test that builds the wire value
/// by hand leaves the producer unguarded, and the assertion has to run against
/// what the daemon actually emitted.
///
/// **Published before the second budget check, never after** (ADR-11's Stage
/// B). A turn where the user approved four commands, watched them run, and was
/// then refused is precisely the turn whose record matters most; emitting
/// after a refusal would leave it with no echo line and no `/verbose`
/// outcomes, while BR-12 says *every* invocation echoes one.
///
/// The **body is never here**. BR-12 says it is not printed — it is in the
/// file — and this event is what the printing is driven from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvoked {
    /// The skill's dispatchable name.
    pub name: String,
    /// Which root it came from.
    pub source: SkillSource,
    /// The file it was read from, **relative and bounded** — never an absolute
    /// path carrying a username, or the location of the user's working tree,
    /// into a transcript (BR-1's entity table).
    ///
    /// Which base it is relative to follows the skill's [`SkillSource`]: a
    /// `project` skill is spelled from the session root
    /// (`.claude/skills/x/SKILL.md`), a `user` skill from the home folder
    /// (`~/.claude/skills/x/SKILL.md`). The daemon derives it at discovery,
    /// where the source, the root and `HOME` are all in hand, and bounds it
    /// with `bounded_field` at the wire. Before BUG-187 only the home half
    /// existed, so a project skill in a checkout outside `$HOME` — a CI
    /// workspace, an external volume — reached this field absolute.
    pub path_display: String,
    /// The body's size in bytes, which is what BR-12's echo line renders
    /// (`/status → skill status (user, 5.3 KB, 4 dynamic commands)`).
    pub body_bytes: u64,
    /// The frontmatter keys Teton does not honor, listed so `/verbose` can say
    /// what was inert rather than leaving a user to infer it from behaviour
    /// (BR-5: `allowed-tools`, `model`, `effort`, … are read and ignored).
    pub ignored_keys: Vec<String>,
    /// A frontmatter `name` that disagrees with the file's own name, said out
    /// loud rather than silently ignored (BR-2).
    ///
    /// The spelling that dispatches is the directory or the file stem, always —
    /// one spelling reaches one handler, REQ-555's rule. A file that declares a
    /// different one is not wrong, it is *misleading*, and the author is the
    /// person best placed to fix it. Additive: absent for every skill whose
    /// declaration agrees, which is nearly all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_note: Option<String>,
    /// One entry per `` !`command` `` in the body, in document order.
    ///
    /// Empty for a skill with no dynamic context — which is a real state, not
    /// a missing one, and is why BR-12's echo line can honestly say "0 dynamic
    /// commands".
    pub outcomes: Vec<DynamicOutcomeView>,
    /// Who issued this invocation (REQ-587 BR-9).
    ///
    /// Additive, and **absent means [`InvokedBy::User`]**: a daemon predating
    /// REQ-587 has no `skill` tool, so every invocation it could report was a
    /// user typing `/name`, and the bytes it wrote are the bytes this build
    /// writes for a user invocation — the key appears only for a model one.
    ///
    /// What a client built before the field loses is the `— invoked by the
    /// model` suffix on one echo line. That is a missing adjective, not a
    /// missing guard: the guard is BR-4's acknowledgment, which the daemon
    /// settles before an expansion exists, and an old client cannot reach a
    /// model-invoked *project* skill at all (see
    /// [`PermissionSubject::ProjectSkillTrust`]).
    #[serde(default, skip_serializing_if = "InvokedBy::is_user")]
    pub invoked_by: InvokedBy,
    /// Whether a **user** skill of this name lost its spelling to this one
    /// (REQ-587 BR-9).
    ///
    /// BR-9's echo line names it — `skill validate (project — shadows your user
    /// skill, …)` — and `/verbose` repeats it. Carried rather than derived,
    /// because the only surface that could derive it is the client, and the
    /// registry snapshot lives on its `UiContext` while `render_event` sees
    /// only `SessionState`. A renderer that reached for the snapshot would be
    /// answering a question about *this* invocation from a value that may have
    /// moved under a `/cd` since.
    ///
    /// Additive, and **absent means `false`**: a daemon predating the field
    /// wrote nothing, and "nothing is shadowed" is the state nearly every
    /// invocation is in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadows_user_skill: bool,
    /// Whether the model may reach this skill through the `skill` tool — the
    /// frontmatter flag as the file wrote it (REQ-587 BR-3, BR-9's `/verbose`).
    ///
    /// Additive in the one direction that keeps the wire byte-identical:
    /// absent means `true`, which is what every skill a pre-REQ-587 daemon
    /// could report was, because that daemon had no flag to read.
    #[serde(default = "invocable_by_default", skip_serializing_if = "is_invocable")]
    pub model_invocable: bool,
    /// Whether the user may dispatch this skill by typing `/name` — the other
    /// frontmatter flag, on the same additive terms as
    /// [`Self::model_invocable`].
    #[serde(default = "invocable_by_default", skip_serializing_if = "is_invocable")]
    pub user_invocable: bool,
    /// How many `skill` calls this turn has spent, and the ceiling
    /// (REQ-587 BR-6a, BR-9's `/verbose`).
    ///
    /// `None` for a user-typed `/name`, and that is a *fact* rather than an
    /// omission: the per-turn cap bounds the **model's** invocations inside one
    /// prompt turn, and a human typing a slash command spends none of it. A
    /// renderer showing "1 of 12" there would be inventing a budget the user
    /// is not drawing on.
    ///
    /// The cap travels with the count because it is a daemon constant: a client
    /// that hardcoded 12 would print a stale ceiling the day it moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_invocations: Option<TurnInvocations>,
    /// Why this invocation was **refused**, as the stable reason id the model
    /// was given — or `None` when it ran (REQ-587 BR-9).
    ///
    /// # A refused call and a command-free skill are otherwise the same bytes
    ///
    /// Every other field on this event describes a skill that *expanded*: the
    /// name, the file, the body's size, the commands' outcomes. A refusal
    /// carries all of them and an empty `outcomes` — which is byte-identical to
    /// a skill with no dynamic context that ran perfectly. Without this field a
    /// client renders the two the same, so BR-9's "a refusal is never silent"
    /// is met by a line that reports the opposite of what happened, which is
    /// worse than silence.
    ///
    /// # The reason, not a bool
    ///
    /// BR-9 asks for one line per typed refusal **naming the reason**. A bare
    /// flag would make every client re-derive one, and there is nothing on the
    /// event to derive it from. The value is the same stable id the model
    /// reads at the head of its refusal sentence — `over_budget`,
    /// `per_turn_cap`, `repeated`, `not_model_invocable` — so the human and the
    /// model are told the same word, and a suite asserts an id rather than a
    /// phrase that reads differently next month.
    ///
    /// `unknown_skill` is deliberately **not** among those examples: it is one
    /// of the two ids this field can never carry. A refusal record describes a
    /// registry row — a source, a path, a size — and `unknown_skill` names no
    /// row, so the daemon publishes nothing rather than a hollow record that
    /// would have to invent one. `invalid_arguments` is the other, for the same
    /// reason from the other end: the call whose parse failed named no skill.
    ///
    /// **Daemon-authored, never file-authored**, which is what makes a `String`
    /// safe here on `name_note`'s precedent: the ids come from the daemon's own
    /// typed refusal set, and the publish site bounds the value like every
    /// other string on this event. It is a `String` rather than a closed enum
    /// because the refusal set is still growing — the daemon raises its reasons
    /// from several layers (the tool, the turn's bookkeeping, the loop's budget
    /// stages), and a variant per reason would be a wire commitment made ahead
    /// of a set that is not settled. Two of today's ids (`unknown_skill`,
    /// `invalid_arguments`) are not published on this event at all, for the
    /// reason above; the rest are, and the next one added will be too, without
    /// a wire change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// The serde default for an invocation flag: absent means permitted.
///
/// A free function rather than an inline literal because `serde(default = …)`
/// takes a path, and the same one is named by both flags — one reading of "the
/// pre-REQ-587 world allowed both".
fn invocable_by_default() -> bool {
    true
}

/// The `skip_serializing_if` predicate both invocation flags use: a permitted
/// flag writes no key, so an ordinary skill's wire stays byte-identical to the
/// one REQ-585 wrote.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_invocable(value: &bool) -> bool {
    *value
}

/// A turn's `skill` invocation count against the per-turn cap (REQ-587 BR-6a).
///
/// Two numbers rather than two optional fields, so a skew that carried the
/// count without the ceiling — or the reverse — is not representable. `/verbose`
/// renders them as one phrase, and they are only ever true together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInvocations {
    /// How many `skill` calls this turn has made, **including this one**.
    ///
    /// Every call counts — expansion, listing or typed refusal — because a
    /// refusal that cost nothing would make a loop of refusals unbounded.
    pub count: u32,
    /// The most this turn may make.
    pub cap: u32,
}

/// What became of one `` !`command` `` (REQ-585 BR-6, BR-12).
///
/// A **typed** outcome, never prose a renderer parses (the spec's System Model
/// says so in as many words): the daemon composes the placeholder that goes
/// into the turn, and this is the parallel record the surface renders. The two
/// are deliberately not the same string — a client that had to re-parse
/// `[dynamic context not run: … — declined]` to count what ran would be a
/// second parser of the daemon's own sentence (LESSON-529).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicOutcomeView {
    /// The command as it was considered, **after** `$ARGUMENTS`/`$N`
    /// substitution (BR-4 precedes BR-6, so this is what the consent showed).
    ///
    /// File-supplied bytes: bounded and rendered on one line by the daemon,
    /// defused again by the client's `Surface`. It is echoed into the not-run
    /// placeholder too, which is why the expander neutralizes envelope tags in
    /// it — at `plan`, where no command runs, the raw command text is the part
    /// that reaches the model (ADR-10).
    pub command: String,
    /// How it ended.
    pub outcome: DynamicOutcome,
}

/// The four ways a dynamic-context command can end (REQ-585 BR-6).
///
/// Only the first one puts anything in the turn; the other three leave an
/// explicit placeholder, because BR-6's rule is that the model is **told** what
/// it does not have rather than left to read a gap. None of them fails the
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DynamicOutcome {
    /// Ran to completion and its stdout was inlined.
    Ran {
        /// How many bytes of stdout were inlined, after the cap.
        output_bytes: u64,
        /// Whether the `shell` tool's `MAX_OUTPUT_CHARS` cut it short — the
        /// model is reading a prefix, and the surface says so.
        truncated: bool,
    },
    /// Never started, and why.
    NotRun {
        /// Which door was closed.
        reason: NotRunReason,
    },
    /// Ran and exited non-zero.
    Failed {
        /// The exit status, or `None` when the process was killed by a signal
        /// and there is none to report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_status: Option<i32>,
    },
    /// Killed at the `shell` tool's timeout.
    TimedOut,
    /// A `kind` this build does not know (BUG-186).
    ///
    /// This travels **daemon → client only** and both surfaces it feeds are
    /// cosmetic, so failing closed buys nothing: without this arm a future
    /// fifth `kind` fails the whole `skill_invoked` frame at
    /// `serde_json::from_value`, and BR-12's "every invocation echoes one"
    /// quietly becomes false with nothing said. Degrading one outcome line is
    /// strictly better than dropping the event that carries it.
    ///
    /// Contrast [`crate::methods::PermissionSubject`], which is deliberately
    /// **not** tolerant: its unrecognized arm is load-bearing and must stay a
    /// refusal, because guessing there would run a command nobody approved.
    #[serde(other)]
    Unknown,
}

/// Why a dynamic-context command was never started (REQ-585 BR-6, BR-11).
///
/// Four closed doors rather than one, because the placeholder the model reads
/// says which: "the user declined" and "no human could be asked" are different
/// facts about the same missing output, and AC-9 requires the pipe case to say
/// so explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunReason {
    /// The user was asked and said no.
    Declined,
    /// The session's permission level does not run them at all (`plan`).
    Level,
    /// There was no terminal to ask at — piped stdin at a level that would ask
    /// (BR-11). The client refused **without reading stdin**, so the user's
    /// next line stayed their next prompt.
    NoTerminal,
    /// The client did not recognize the request's [`PermissionSubject`] and
    /// refused rather than guessing (ADR-7).
    UnrecognizedSubject,
    /// Consent was given and the command still never started — the shell was
    /// missing, the jail root could not be resolved, the spawn failed.
    ///
    /// The one arm that is **not** a closed door. It exists because the other
    /// four are all answers to "who said no", and reporting a command that
    /// never ran as [`DynamicOutcome::Failed`] instead would tell a reader it
    /// was attempted and exited — a false statement on `/verbose`, and one that
    /// points at the wrong fix. What went wrong here is on this machine, not in
    /// anybody's answer.
    CouldNotStart,
    /// A reason this build does not know (BUG-186).
    ///
    /// Rendered as a bare "not run" — the fact survives even when the reason
    /// does not. See [`DynamicOutcome::Unknown`] for why this direction is
    /// tolerant where `PermissionSubject` stays closed.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// skill_over_budget_offered / _accepted / _remedy_applied (REQ-589)
// ---------------------------------------------------------------------------
//
// Three announcements around one question. They exist because
// `SkillInvoked::refused` keeps `over_budget` as the single reason token for
// every not-sent outcome (REQ-585 AC-9's rule, kept where it already lives), so
// the *record* — not a second refusal token — is what tells "nobody was asked"
// from "somebody was asked and said no" from "somebody said yes".
//
// A turn refused without ever reaching a human publishes none of them. A turn
// that was offered and declined publishes `skill_over_budget_offered` alone. A
// turn that was offered and accepted publishes that and
// `skill_over_budget_accepted`. Only a durable write publishes
// `skill_over_budget_remedy_applied`, and it is the one of the three that may
// appear without an accept, because remedy-only is a legitimate answer.

/// Which of BR-8's two budget checks measured an expansion (REQ-585 ADR-11).
///
/// The wire half of the daemon's `harness::budget::SkillStage`. That type is
/// not re-exported here because it carries the refusal *clause* it words, and
/// `tetond` composing a sentence a client re-parses is LESSON-529's shape: what
/// crosses the wire is the fact of which stage spoke, and the sentence is
/// composed at the surface that renders it — this crate's standing rule for
/// [`PermissionSubject`], and the reason [`BudgetBound::words`] lives here
/// rather than in either binary.
///
/// The distinction is what a user can *do* about the answer, which is why it is
/// carried at all: a body that will not fit is refused before consent is asked,
/// so nobody approves four commands, watches them run and is then told the turn
/// was refused — and when the refusal does land after the commands, the message
/// has to say their output is what spent the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStage {
    /// **Stage A** — the body with a `[dynamic context pending]` placeholder
    /// standing in each `` !`command` `` slot, measured before consent.
    Body,
    /// **Stage B** — the same expansion with the dynamic-context outcomes
    /// folded in, measured after they ran.
    WithDynamicContext,
    /// A stage this build does not know.
    ///
    /// Tolerant, and the direction is chosen rather than inherited. This rides
    /// [`PermissionSubject::SkillOverBudget`], where the closed, fail-closed
    /// decision lives one level up at the `kind` tag: an unknown *kind* is a
    /// refusal, which is the guard. An unknown *stage* changes no authority —
    /// the question is still "send this over-budget expansion" — so failing
    /// closed here buys nothing and costs a great deal. Without this arm a
    /// future stage fails the whole `permission_request` frame at
    /// `serde_json::from_value`: nothing renders on any screen, and the
    /// daemon's waiter parks with no timeout of its own, so BR-4's "a declined
    /// or unanswerable offer is exactly today's refusal" never fires because
    /// nobody ever refuses. Degrading one adjective is strictly better than
    /// dropping the question that carries it (BUG-186, applied where it counts).
    #[serde(other)]
    Unknown,
}

/// What the route's declared window says about an over-budget expansion
/// (REQ-589 BR-3).
///
/// Over-budget and over-window are **not the same event**: over-budget is this
/// daemon's own policy refusing, over-window is the provider refusing. The
/// daemon knows which one it is looking at and BR-3 requires it to say so — and
/// then to ask anyway, rather than deciding on the user's behalf.
///
/// This is what selects which true sentence the client renders, and it is a
/// typed verdict computed from integers rather than anything a provider said.
/// That is deliberate: see [`PermissionSubject::SkillOverBudget`] on why no
/// provider response body may reach a consent prompt.
///
/// **The verdict and the [`BudgetBound`] are not independent axes.** A verdict
/// exists only where a window was declared, so most of the 5 × 3 cross product
/// is unreachable: `LocalEngine` and `DefaultUnknown` reach [`Self::WindowUnknown`]
/// alone, while `Window`, `UserCap` and `RedactScan` reach [`Self::FitsWindow`]
/// and [`Self::ExceedsWindow`]. A test written for any other cell passes
/// vacuously (LESSON-520).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowVerdict {
    /// Over budget, but inside the window the route declares.
    ///
    /// **Not a promise that the send will serve** (ADR-15). On a window- or
    /// cap-bound route the band between the budget and the declared window *is*
    /// the generation reservation, so an expansion that clears the window while
    /// overflowing the budget is eating the room held back for the reply — the
    /// offer says the prompt fits the declared window and may leave the response
    /// very little to work with, and claims nothing further. On the byte-clamped
    /// `RedactScan` bound the band is the egress scanner's ceiling instead, and
    /// the offer says only that this daemon's own budget is what refused
    /// (ADR-17). A test in `harness::budget` pins that neither sentence says the
    /// send is expected to serve.
    FitsWindow,
    /// The route declares a window and the expansion exceeds it. Proceeding
    /// without raising it will very likely be rejected by the provider — which
    /// the offer states plainly, while still leaving both choices open.
    ExceedsWindow,
    /// No window fact exists — the local tier, or a remote provider with
    /// `capabilities.max_context = 0`. The daemon **cannot promise** the send
    /// will fit and says exactly that, rather than implying either of the
    /// above; the typed `context_length_exceeded` outcome is the backstop.
    WindowUnknown,
    /// A verdict this build does not know.
    ///
    /// Tolerant for the reason spelled out on [`SkillStage::Unknown`], and with
    /// one addition: this arm must be rendered as a *hedge*, never silently
    /// relabelled [`Self::WindowUnknown`]. "No window fact exists" and "this
    /// build cannot read the verdict" are different statements, and only the
    /// second is true here.
    #[serde(other)]
    Unknown,
}

/// What a **durable** fix for an over-budget route would be (REQ-589 BR-7).
///
/// The *record* half of ADR-1's decision. The offer itself expresses the remedy
/// as named [`PermissionOption`] ids whose labels state the concrete write; this
/// enum is what the events carry, so a reader can tell which fix was proposed
/// and which was taken without re-parsing an option label (LESSON-529).
///
/// **One representation**, and absence is [`Self::NotOffered`] rather than an
/// `Option<RemedyKind>` — two ways to say "no remedy" is LESSON-545's shape.
/// `NotOffered` is reachable and not an oversight: `RedactScan` has no durable
/// fix (BR-7b), and that offer must present the one-time override alone rather
/// than imply a fix exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemedyKind {
    /// The provider declares no window, so the default pair bound the budget:
    /// write `capabilities.max_context`.
    DeclareWindow,
    /// A user cap sat below the window and is what bound the budget: raise
    /// `capabilities.context_budget_cap`.
    RaiseCap,
    /// The declared window is what bound the budget: raise
    /// `capabilities.max_context`.
    RaiseWindow,
    /// The route is the local engine: register a remote provider carrying a
    /// declared window, then bind the tier to it. Two writes, made safe by
    /// **ordering** rather than by atomicity (ADR-5) — the reverse order is the
    /// only one that can leave a newly-bound remote tier with no window.
    BindTierRemote,
    /// This bound has no durable fix (BR-7b).
    NotOffered,
    /// A remedy this build does not know.
    ///
    /// Tolerant on [`SkillStage::Unknown`]'s reasoning; distinct from
    /// [`Self::NotOffered`], which is the daemon stating that no fix exists.
    /// "There is no remedy" and "this build cannot name the remedy" must not
    /// collapse into one line on a record someone reads later.
    #[serde(other)]
    Unknown,
}

/// An over-budget skill expansion was put to a human as a question (REQ-589
/// BR-3).
///
/// Published when the offer is **raised**, not when it is answered, so a turn
/// that was asked about and declined is distinguishable from one where no human
/// could be reached — the distinction REQ-585 AC-9 draws, and the reason
/// `OVER_BUDGET_REASON` does not need a second refusal token to carry it.
///
/// **Typed, not pre-rendered** (LESSON-544): the figures ride as integers and
/// the sentence is composed at the surface. A test that built this value by
/// hand would leave the producer unguarded, which is why the acceptance leg
/// drives it from a real turn instead.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the shape
/// [`ContextCleared`], [`SessionTitled`] and [`PrefixCache`] document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOverBudgetOffered {
    /// The skill whose expansion was measured.
    pub skill: String,
    /// Which root it came from (ASSUME-018: a project name is
    /// repository-authored text and renders as such).
    pub source: SkillSource,
    /// Which of BR-8's two checks measured it.
    pub stage: SkillStage,
    /// The word figure `skill_fit` measured, verbatim.
    pub measured_tokens: u64,
    /// The byte figure `skill_fit` measured, verbatim.
    pub measured_bytes: u64,
    /// The word budget the router stamped, verbatim.
    pub budget_tokens: u64,
    /// The byte budget the router stamped, verbatim.
    pub budget_bytes: u64,
    /// Which constraint bound that budget — the route's, never re-derived.
    pub bound: BudgetBound,
    /// What the declared window says about this expansion.
    pub window_verdict: WindowVerdict,
    /// Which durable fix the offer named, or [`RemedyKind::NotOffered`].
    ///
    /// Recorded because it is the fact a later reader cannot recover: the
    /// option list is gone by then, and "this bound had no remedy" is what
    /// explains an offer that presented the one-time override alone.
    pub remedy_kind: RemedyKind,
}

/// A human answered an over-budget offer with "send it" (REQ-589 BR-1).
///
/// The expansion goes out **whole** — the same bytes `skill_fit` measured,
/// unshortened. No path this REQ introduces may middle-elide, truncate or
/// summarize it, so the figures below are the figures that were actually sent
/// and this event is the record of that promise being kept.
///
/// Published for an accept, and only an accept. `over_budget_remedy_only` is
/// not one: it writes the fix and refuses the turn, so it publishes a
/// [`SkillOverBudgetRemedyApplied`] and no accept — which is what makes "was
/// this oversized turn actually sent?" answerable from the record alone.
///
/// The bound is **not** repeated here: [`SkillOverBudgetOffered`] carries it
/// and the two events are correlated by session and sequence. What is repeated
/// is what BR-1's promise is about — the measured pair, the budget it exceeded,
/// and the verdict the user was told before they answered.
///
/// The session is named by [`EventEnvelope::session_id`] — see
/// [`SkillOverBudgetOffered`] on why that is not a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOverBudgetAccepted {
    /// The skill whose expansion was sent.
    pub skill: String,
    /// Which root it came from.
    pub source: SkillSource,
    /// Which of BR-8's two checks had measured it.
    pub stage: SkillStage,
    /// The word figure that was sent, verbatim — BR-1's "whole" is this
    /// number, not a shortened one.
    pub measured_tokens: u64,
    /// The byte figure that was sent, verbatim.
    pub measured_bytes: u64,
    /// The word budget it exceeded.
    pub budget_tokens: u64,
    /// The byte budget it exceeded.
    pub budget_bytes: u64,
    /// What the user was told the window would do, before they answered.
    pub window_verdict: WindowVerdict,
}

/// An over-budget offer's going-forward remedy was written (REQ-589 BR-7,
/// BR-8).
///
/// The write itself goes through `config/set` and nowhere else (ADR-4),
/// inheriting that method's posture verbatim rather than minting a second
/// durable-write path for the same class of fact. This event is the
/// announcement, not the authority: no new authority is minted here, and a user
/// `config/set` would already refuse is refused identically.
///
/// **Both values, always.** A record that named only the new one would leave a
/// reader unable to tell a raise from a first declaration, which is the
/// difference between [`RemedyKind::RaiseWindow`] and
/// [`RemedyKind::DeclareWindow`] and the difference between a fix and a
/// surprise.
///
/// The session is named by [`EventEnvelope::session_id`] — see
/// [`SkillOverBudgetOffered`] on why that is not a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOverBudgetRemedyApplied {
    /// Which fix was written. Never [`RemedyKind::NotOffered`] on a published
    /// event — that value means no fix existed to take.
    pub remedy_kind: RemedyKind,
    /// The provider the write addressed, when the remedy names one.
    ///
    /// Absent for a remedy that addresses no single provider. Sanitized and
    /// bounded by the daemon, like every identifier that reaches a surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<ProviderId>,
    /// What the setting read before the write, spelled by the daemon.
    ///
    /// A **string** rather than a number because the four remedies do not write
    /// one type: a window and a cap are integers, a tier binding is a name. A
    /// client that had to know which is which per [`RemedyKind`] would be a
    /// second classifier of the daemon's own decision (LESSON-456), and the
    /// only consumer of these two is a line a person reads.
    pub previous_value: String,
    /// What the setting reads after the write, spelled the same way.
    pub new_value: String,
}

// ---------------------------------------------------------------------------
// transcript_state (REQ-611)
// ---------------------------------------------------------------------------

/// Why a session's transcript state changed (REQ-611 System Model, BR-15).
///
/// A **closed** enum with no catch-all and no `Default`, for
/// [`crate::methods::AttachConsentOutcome`]'s reason: a reason this build
/// cannot read is a deserialization error, never a silent reading of one of
/// these four. There is no safe value to fall back *to* — `config_default` says
/// the user asked for this, `session_command` says they just typed it,
/// `write_failure` and `dir_refused` say the daemon stopped recording without
/// being asked — so a client that guessed would tell somebody their session is
/// being recorded for a reason that is not the true one, or that a failure was
/// a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStateReason {
    /// The session opened with `[transcript] enabled = true` in the config file
    /// — the durable switch, in force before anybody typed anything.
    ConfigDefault,
    /// The user switched it for this session with `/transcript on` / `off`
    /// (`session/transcript`, architecture ADR-6). Nothing durable was written.
    SessionCommand,
    /// A write failed, so the sink closed this session's file and stopped
    /// (BR-6). The turn in flight was **not** failed; only the recording was.
    WriteFailure,
    /// The transcript directory could not be created, or existed wider than
    /// owner-only, and was refused at open (BR-9). The session runs normally
    /// with no transcript.
    DirRefused,
}

/// A session's effective transcript state changed (REQ-611 BR-15).
///
/// Published session-scoped on every effective-state change, so every attached
/// client and every declared monitor learns that recording started or stopped
/// — including the two changes nobody asked for
/// ([`TranscriptStateReason::WriteFailure`],
/// [`TranscriptStateReason::DirRefused`]), which is the half a user could not
/// otherwise find out about.
///
/// **There is no `path` field, and that is BR-15 rather than an omission.**
/// The event is *news*; the file's location is *boundary content* — a
/// transcript path names the user's home, the same class REQ-569 BR-10 gives
/// `cwd` — so it is answered on the asking connection as
/// [`crate::methods::SessionTranscriptResult`]'s routed reply and broadcast to
/// nobody. A monitor is told a session is recording and is not told where to
/// read it. Adding a path here would hand every declared monitor the filename
/// of the session's full content, which is precisely the split this struct
/// exists to hold.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`ContextCleared`], [`SessionTitled`] and [`PrefixCache`] document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptState {
    /// Whether the session is recording **after** this change.
    ///
    /// The effective state, not the config default: a session whose config says
    /// `true` and which then failed a write reports `false` here, because what
    /// a client renders has to be what is actually happening to the file.
    pub enabled: bool,
    /// What changed it.
    pub reason: TranscriptStateReason,
}

// ---------------------------------------------------------------------------
// repo_context_state (REQ-612)
// ---------------------------------------------------------------------------

/// A session's repository notes were loaded, or exist and were not made
/// resident (REQ-612 System Model, BR-1/BR-2).
///
/// Published session-scoped whenever the state is established or changes: at
/// `session/create`, in `/cd`'s rebuild before `session_root_changed` reaches a
/// second client, on BR-6's staleness re-read, and on `/context on|off`. Every
/// attached client and declared monitor learns that a repository file is riding
/// this session's system prompt — including the three ways it can fail to
/// (`withheld_boundary`, `withheld_off`, `unreadable`), which are the half a
/// user could not otherwise find out about.
///
/// **One event, not the two the spec's Events table first named.** Architecture
/// ADR-6 folded `repo_context_loaded` and `repo_context_withheld` into
/// [`Self::state`]: a client renders one line either way, and one event is one
/// `name()` arm, one spec-table row and one thing for a monitor to subscribe
/// to. The two spellings survive as two of the six values of
/// [`RepoContextStateKind`].
///
/// **There is no file name here, and that is BR-2's split rather than an
/// omission**, exactly as for [`TranscriptState`]. The event is *news*; which
/// file the notes came out of is answered on the asking connection as
/// [`crate::methods::SessionContextResult`]'s routed reply. [`Self::source`]
/// says which of the two *names* was read, which is a closed two-value enum
/// this build wrote and not a path — a monitor learns a repository has notes
/// and does not learn where the user's working tree is.
///
/// The session is named by [`EventEnvelope::session_id`], not by a field here:
/// [`Event`] is internally tagged and flattened, so a `session_id` on this
/// struct would emit the key twice and fail to deserialize — the same shape
/// [`ContextCleared`], [`SessionTitled`], [`PrefixCache`] and [`TranscriptState`]
/// document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContextState {
    /// What the notes are doing **after** this change.
    pub state: RepoContextStateKind,
    /// Which name was read, when one was.
    ///
    /// Additive and absent-means-nothing-was-opened: `absent` and
    /// `withheld_off` carry no source, because `off` never opened a file and so
    /// does not know which of the two names is on disk (BR-2's "off means
    /// unopened").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RepoContextSource>,
    /// The file's size on disk in bytes — **absent when the daemon does not
    /// know one**.
    ///
    /// An `Option` for [`crate::methods::SessionContextResult::bytes_on_disk`]'s
    /// reason, and it is the same defect on this half of the split: `0` is a
    /// measurement meaning "the file is empty", while a symlinked entry, a
    /// directory wearing the name and a refused `stat` have no measurement to
    /// give. Absent means not known; a client renders no size for it.
    ///
    /// Additive, so a monitor built before this reads it as `None` and a daemon
    /// that never populates it emits no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_on_disk: Option<u64>,
    /// How many of those bytes are in the system prompt — `0` for every state
    /// but `loaded` and `truncated`.
    ///
    /// Published beside [`Self::bytes_on_disk`] rather than instead of it
    /// because the pair is what BR-7 makes visible: resident bytes are spent on
    /// every model call of every iteration, and a user weighing that trade needs
    /// both the file's size and the part of it they are paying for.
    pub resident_bytes: u64,
    /// Whether the file was cut to fit the route's effective cap (BR-3).
    ///
    /// Redundant with `state == `[`RepoContextStateKind::Truncated`] on purpose,
    /// for [`crate::methods::SessionContextResult::truncated`]'s reason: a
    /// client renders the byte figures above beside a flag rather than by
    /// matching on an enum whose future values it may not know.
    pub truncated: bool,
    /// Why, in the daemon's own words, when the state has words to give.
    ///
    /// **Only `unreadable` has any.** The reason names the filesystem verdict —
    /// a symlinked entry, a permission, bytes that are not text — from a closed
    /// set of harness-authored sentences. A `withheld_boundary` carries none and
    /// never will: the glob that covered the file is configuration, not a fact
    /// about the file, and the state word already names the remedy.
    ///
    /// **Bounded and neutralised by the daemon before it reaches the wire**
    /// (`session_root::bounded_field`, the [`crate::methods::SkillView`] rule).
    /// An `io::Error`'s text is repository-adjacent content — it can carry a
    /// path the repository chose — so it is treated as file bytes at both ends:
    /// bounded where the frame is written, and defused again at render through
    /// `Surface::line` (REQ-591 BR-11, ADR-009's two layers).
    ///
    /// Absent for every state that has nothing to explain, so `unreadable` with
    /// no reason is "we could not say why" and `unreadable` with one is a
    /// remedy the user can act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::methods::RootKind;
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
            budget_tokens: Some(132_650),
            budget_bytes: Some(397_952),
            bound: Some(BudgetBound::Window),
            bound_floored: None,
            spend_ceiling_micro_cents: None,
            repo_context_cap: None,
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
        // REQ-586 BR-8: the event names the budget and its bound, in the flat
        // object, under the spec's snake_case spellings.
        assert_eq!(wire["budget_tokens"], 132_650);
        assert_eq!(wire["budget_bytes"], 397_952);
        assert_eq!(wire["bound"], "window");
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
                    budget_tokens: None,
                    budget_bytes: None,
                    bound: None,
                    bound_floored: None,
                    spend_ceiling_micro_cents: None,
                    repo_context_cap: None,
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
                    subject: None,
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
            (
                Event::SessionRootChanged(SessionRootChanged {
                    previous_display: "~/repo".to_owned(),
                    root: SessionRoot {
                        display: "~".to_owned(),
                        kind: RootKind::Home,
                        project_name: None,
                        vcs_branch: None,
                    },
                }),
                "session_root_changed",
            ),
            (
                Event::ContextPressure(ContextPressure {
                    kind: ContextPressureKind::BlocksDropped,
                    dropped_blocks: 3,
                    elided_bytes: 0,
                    newest_user_elided: false,
                    budget_tokens: 4_096,
                    budget_bytes: 32_768,
                    bound: BudgetBound::LocalEngine,
                    bound_floored: false,
                }),
                "context_pressure",
            ),
            (
                Event::SkillInvoked(SkillInvoked {
                    name: "status".to_owned(),
                    source: SkillSource::User,
                    path_display: "~/.claude/skills/status/SKILL.md".to_owned(),
                    body_bytes: 5_432,
                    ignored_keys: vec!["allowed-tools".to_owned()],
                    name_note: None,
                    outcomes: vec![DynamicOutcomeView {
                        command: "git branch --show-current".to_owned(),
                        outcome: DynamicOutcome::Ran {
                            output_bytes: 19,
                            truncated: false,
                        },
                    }],
                    invoked_by: InvokedBy::User,
                    shadows_user_skill: false,
                    model_invocable: true,
                    user_invocable: true,
                    turn_invocations: None,
                    refused: None,
                }),
                "skill_invoked",
            ),
            (
                Event::TranscriptState(TranscriptState {
                    enabled: true,
                    reason: TranscriptStateReason::ConfigDefault,
                }),
                "transcript_state",
            ),
            (
                // REQ-612 ADR-6: the one event, whose `state` carries the
                // distinction the spec's two names drew.
                Event::RepoContextState(RepoContextState {
                    state: RepoContextStateKind::Loaded,
                    source: Some(RepoContextSource::TetonMd),
                    bytes_on_disk: Some(3_120),
                    resident_bytes: 3_120,
                    truncated: false,
                    reason: None,
                }),
                "repo_context_state",
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

    /// **REQ-583 BR-7's wire half.** A moved root reaches a client as a flat
    /// `session_root_changed` object naming its session, where the root was,
    /// and where it is now, and survives the round trip unchanged.
    ///
    /// `session_id` is asserted on the wire object rather than on the payload
    /// for [`ContextCleared`]'s reason: the envelope is what carries it, and
    /// `envelope_wire` round-trips before returning, so re-adding `session_id`
    /// to [`SessionRootChanged`] fails here on the duplicate key rather than
    /// reaching a client. The payload's own keys are asserted absent of a
    /// `session_id` for the same reason, from the other side.
    ///
    /// Both ends of the optionals ride along: a project root with a name and a
    /// branch, and a home root with neither — the second emits no key for
    /// either, which is what BR-8's re-announce reads.
    #[test]
    fn session_root_changed_round_trips_under_its_wire_name() {
        let changed = SessionRootChanged {
            previous_display: "~".to_owned(),
            root: SessionRoot {
                display: "~/Documents/GitHub/teton-code".to_owned(),
                kind: RootKind::Project,
                project_name: Some("teton-code".to_owned()),
                vcs_branch: Some("main".to_owned()),
            },
        };
        round_trip(&changed);

        let wire = envelope_wire(Event::SessionRootChanged(changed.clone()));
        assert_eq!(wire["event"], "session_root_changed");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["previous_display"], "~");
        assert_eq!(wire["root"]["display"], "~/Documents/GitHub/teton-code");
        assert_eq!(wire["root"]["kind"], "project");
        assert_eq!(wire["root"]["project_name"], "teton-code");
        assert_eq!(wire["root"]["vcs_branch"], "main");
        let payload = serde_json::to_value(&changed).unwrap();
        assert!(
            payload.get("session_id").is_none(),
            "the envelope names the session, the payload must not: {payload}"
        );

        assert_eq!(
            Event::SessionRootChanged(changed).name(),
            "session_root_changed"
        );

        let to_home = SessionRootChanged {
            previous_display: "~/Documents/GitHub/teton-code".to_owned(),
            root: SessionRoot {
                display: "~".to_owned(),
                kind: RootKind::Home,
                project_name: None,
                vcs_branch: None,
            },
        };
        round_trip(&to_home);
        let wire = envelope_wire(Event::SessionRootChanged(to_home));
        assert_eq!(wire["root"]["kind"], "home");
        assert!(wire["root"].get("project_name").is_none(), "{wire}");
        assert!(wire["root"].get("vcs_branch").is_none(), "{wire}");
    }

    /// **REQ-611 BR-15's wire half.** A transcript state change reaches a
    /// client as a flat `transcript_state` object naming its session, whether
    /// the session is recording now, and why that changed — and carrying **no
    /// path**.
    ///
    /// The absent path is what this test is for, and it is asserted from both
    /// sides. Outbound: the payload's key set is exactly `{enabled, reason}`,
    /// asserted whole rather than by probing for `path`, so any *other* field
    /// added later fails here too — the transcript's location is boundary
    /// content and this event goes to every declared monitor. Inbound: a frame
    /// that arrives carrying a stray `path` parses with the key dropped, which
    /// is this crate's posture for unknown fields (serde's default, which no
    /// type here opts out of), so a daemon that grew one could not smuggle a
    /// location into a client through this event.
    ///
    /// All four reasons ride along spelled out, so a fifth has to be added here
    /// by hand, and the closed-enum half is asserted from the other side: an
    /// unknown reason is a deserialization error rather than a default, because
    /// [`TranscriptStateReason`] has no safe value to fall back to — see its
    /// own doc comment.
    ///
    /// `session_id` is asserted on the wire object rather than on the payload
    /// for [`ContextCleared`]'s reason: the envelope is what carries it, and
    /// `envelope_wire` round-trips before returning, so re-adding `session_id`
    /// to [`TranscriptState`] fails here on the duplicate key rather than
    /// reaching a client.
    ///
    /// **Shown to fail** (conventions: show the test can fail before trusting
    /// that it passed). The mutation was the regression BR-15 forbids, not a
    /// proxy for it: `pub path: Option<String>` added to [`TranscriptState`]
    /// (which costs the struct its `Copy`, hence a few `clone()`s in the
    /// fixtures) and populated with a plausible transcript path. This test went
    /// red on the key-set assertion — `the event must carry no path (BR-15):
    /// left ["enabled", "path", "reason"]` — and
    /// `session_transcript_round_trips_each_action` stayed green, which is the
    /// right split: the path belongs on the routed result and nowhere else.
    /// Restored after observing.
    #[test]
    fn transcript_state_carries_enabled_and_reason_and_no_path() {
        let opened = TranscriptState {
            enabled: true,
            reason: TranscriptStateReason::ConfigDefault,
        };
        round_trip(&opened);

        let wire = envelope_wire(Event::TranscriptState(opened));
        assert_eq!(wire["event"], "transcript_state");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["enabled"], true);
        assert_eq!(wire["reason"], "config_default");

        assert_eq!(Event::TranscriptState(opened).name(), "transcript_state");

        // BR-15 outbound: the news says *that* it is recording, never *where*.
        let payload = serde_json::to_value(opened).unwrap();
        let keys: Vec<&str> = payload
            .as_object()
            .expect("the payload is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["enabled", "reason"],
            "the event must carry no path (BR-15): {payload}"
        );

        // Every reason, under the spec table's spelling, and both states of the
        // flag — a stop is the half a client most needs to render.
        for (reason, spelling) in [
            (TranscriptStateReason::ConfigDefault, "config_default"),
            (TranscriptStateReason::SessionCommand, "session_command"),
            (TranscriptStateReason::WriteFailure, "write_failure"),
            (TranscriptStateReason::DirRefused, "dir_refused"),
        ] {
            let stopped = TranscriptState {
                enabled: false,
                reason,
            };
            round_trip(&stopped);
            let wire = envelope_wire(Event::TranscriptState(stopped));
            assert_eq!(wire["reason"], spelling, "{wire}");
            assert_eq!(
                wire["enabled"], false,
                "a transcript that stopped must say so on the wire: {wire}"
            );
        }

        // BR-15 inbound: a frame carrying a path — a future daemon, or
        // something hand-rolled — is read without one, and re-emits without
        // one.
        let stray = r#"{"enabled":true,"reason":"session_command","path":"/Users/dev/.local/share/teton/transcripts/s1.jsonl"}"#;
        let parsed: TranscriptState =
            serde_json::from_str(stray).expect("an unknown key is dropped, not refused");
        assert_eq!(
            parsed,
            TranscriptState {
                enabled: true,
                reason: TranscriptStateReason::SessionCommand,
            }
        );
        let back = serde_json::to_value(parsed).unwrap();
        assert!(
            back.get("path").is_none(),
            "a stray path must not survive into what a client is handed: {back}"
        );

        // Closed: an unreadable reason is an error, never one of these four.
        assert!(
            serde_json::from_str::<TranscriptState>(r#"{"enabled":false,"reason":"vibes"}"#)
                .is_err(),
            "a reason this build cannot read must not deserialize to one it acts on"
        );
    }

    /// **REQ-612 AC-2's wire half, both directions of the additive rule
    /// (REQ-573).** `repo_context_state` reaches a client as a flat object
    /// naming its session, what the notes are doing, and what they cost — and
    /// **no file name**. The two directions are asserted rather than assumed,
    /// and like [`a_context_that_did_not_fit_has_its_own_kind_and_an_unknown_one_degrades`]
    /// they are **not symmetric**, which is worth saying out loud:
    ///
    /// * **a daemon that populates only the required keys → this build**:
    ///   additive and lossless. `source` and `reason` are absent, not `null`,
    ///   and read as `None`.
    /// * **this daemon → a client predating REQ-612**: the frame is *dropped*,
    ///   in silence, which is exactly what "older clients ignore unknown
    ///   events" means in this codebase. [`Event`] is closed with no
    ///   `#[serde(other)]`, so an unknown `event` tag is a deserialization
    ///   error, and the CLI's reader spells that
    ///   `serde_json::from_value(params).ok()?` — a `None` that skips the
    ///   notification. The cost is one status line about a file the *turn* is
    ///   carrying regardless, which is why this ships as an ordinary additive
    ///   event rather than a protocol-version bump.
    ///
    /// The absent file name is asserted the way [`TranscriptState`]'s absent
    /// path is: the payload's key set is checked **whole** rather than by
    /// probing for `file`, so any other field added later fails here too. This
    /// event goes to every declared monitor, and a monitor is told a session has
    /// repository notes without being told where the user's working tree is.
    ///
    /// **Shown to fail** (conventions: show the test can fail before trusting
    /// that it passed). Three mutations. `pub file: Option<String>` added to
    /// [`RepoContextState`] and populated — red on the key-set assertion
    /// (`left ["bytes_on_disk", "file", "resident_bytes", "source", "state",
    /// "truncated"]`), with `session_context_params_and_result_round_trip_and_do_not_end_a_turn`
    /// still green, which is the right split: the file name belongs on the
    /// routed result and nowhere else. Dropping `#[serde(default,
    /// skip_serializing_if = "Option::is_none")]` from `source` — red where the
    /// first direction starts, on `a frame carrying only the required keys must
    /// parse`, which is where the test stops. Renaming the `name()` arm to
    /// `repo_context_loaded` — red on the tag assertion here **and** in
    /// `event_names_match_the_spec_events_table`, which is the pairing that
    /// makes ADR-6's "one event, one name, one row" checkable. Restored after
    /// observing.
    ///
    /// **Verify (MAJOR 2), shown to fail.** Flattening `bytes_on_disk` back to
    /// a bare `u64` — the shape this replaced — is red on the `None` legs of
    /// the state table: `absent`, `withheld_off` and a `stat`-refused
    /// `unreadable` emit `"bytes_on_disk":0`, and a client renders `0 bytes on
    /// disk` for a file it never measured. Dropping only the
    /// `skip_serializing_if` emits `null` and is red on the same assertion.
    #[test]
    fn repo_context_state_is_additive_in_both_directions() {
        let loaded = RepoContextState {
            state: RepoContextStateKind::Loaded,
            source: Some(RepoContextSource::TetonMd),
            bytes_on_disk: Some(3_120),
            resident_bytes: 3_120,
            truncated: false,
            reason: None,
        };
        round_trip(&loaded);

        let wire = envelope_wire(Event::RepoContextState(loaded.clone()));
        assert_eq!(wire["event"], "repo_context_state");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["state"], "loaded");
        assert_eq!(wire["source"], "teton_md");
        assert_eq!(wire["bytes_on_disk"], 3_120);
        assert_eq!(wire["resident_bytes"], 3_120);
        assert_eq!(wire["truncated"], false);
        assert_eq!(
            Event::RepoContextState(loaded.clone()).name(),
            "repo_context_state"
        );

        // BR-2's split: the news says *that* the notes are resident and what
        // they cost, never *which file on disk* they came out of.
        let payload = serde_json::to_value(&loaded).unwrap();
        let keys: Vec<&str> = payload
            .as_object()
            .expect("the payload is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            // `serde_json`'s map is ordered, so this is the set spelled
            // alphabetically rather than in declaration order.
            vec![
                "bytes_on_disk",
                "resident_bytes",
                "source",
                "state",
                "truncated"
            ],
            "the event must carry no file name (BR-2): {payload}"
        );

        // Every state, spelled as the System Model spells it, with the payload
        // each one actually carries. A truncated file is resident *and* cut.
        // `withheld_boundary` and an `unreadable` the daemon `stat`ed carry the
        // file's size with **none** of it resident — the pair is the point: a
        // user is told a file is there, how big it is, and that none of it is in
        // the prompt. `absent` and `withheld_off` opened nothing, so they name
        // no source and **no size at all**: `off` never `stat`ed, and `absent`
        // has nothing to `stat`. A size is a measurement, and those two have
        // none to give — which is why the field is an `Option` and not a `0`
        // (verify MAJOR 2). The `unreadable` leg below is the one whose `stat`
        // itself failed, so it is `None` beside a source that is `Some`.
        for (state, spelling, source, on_disk, resident) in [
            (
                RepoContextStateKind::Truncated,
                "truncated",
                Some(RepoContextSource::AgentsMd),
                Some(40_000_u64),
                8_100_u64,
            ),
            (RepoContextStateKind::Absent, "absent", None, None, 0),
            (
                RepoContextStateKind::WithheldOff,
                "withheld_off",
                None,
                None,
                0,
            ),
            (
                RepoContextStateKind::WithheldBoundary,
                "withheld_boundary",
                Some(RepoContextSource::TetonMd),
                Some(2_048),
                0,
            ),
            (
                RepoContextStateKind::Unreadable,
                "unreadable",
                Some(RepoContextSource::TetonMd),
                None,
                0,
            ),
        ] {
            let event = RepoContextState {
                state,
                source,
                bytes_on_disk: on_disk,
                resident_bytes: resident,
                truncated: state == RepoContextStateKind::Truncated,
                reason: None,
            };
            round_trip(&event);
            let wire = envelope_wire(Event::RepoContextState(event));
            assert_eq!(wire["state"], spelling, "{wire}");
            assert_eq!(wire["resident_bytes"], resident, "{wire}");
            if source.is_none() {
                assert!(
                    wire.get("source").is_none(),
                    "a state that opened no file names no source: {wire}"
                );
            }
            match on_disk {
                Some(bytes) => assert_eq!(wire["bytes_on_disk"], bytes, "{wire}"),
                // The whole of MAJOR 2 on this half of the split: a state the
                // daemon has no size for emits no key, so no client can render
                // `0 bytes on disk` for a file it never measured.
                None => assert!(
                    wire.get("bytes_on_disk").is_none(),
                    "a state the daemon never measured reported a size: {wire}"
                ),
            }
        }

        // The bounded reason, which only two states have words for. It is the
        // daemon's own sentence, bounded before it reaches here — a filesystem
        // error's text is repository-adjacent content (REQ-591 BR-11).
        let unreadable = RepoContextState {
            state: RepoContextStateKind::Unreadable,
            source: Some(RepoContextSource::TetonMd),
            bytes_on_disk: None,
            resident_bytes: 0,
            truncated: false,
            reason: Some("Operation not permitted (os error 1)".to_owned()),
        };
        round_trip(&unreadable);
        let wire = envelope_wire(Event::RepoContextState(unreadable));
        assert_eq!(wire["reason"], "Operation not permitted (os error 1)");

        // Direction one — a frame carrying only the required keys reads with
        // all three optionals `None`, and a payload that never populated them
        // emits no key at all rather than `null`, which is the same wire a
        // daemon predating any of the three writes.
        let minimal: RepoContextState =
            serde_json::from_str(r#"{"state":"absent","resident_bytes":0,"truncated":false}"#)
                .expect("a frame carrying only the required keys must parse");
        assert_eq!(minimal.source, None);
        assert_eq!(minimal.reason, None);
        assert_eq!(minimal.bytes_on_disk, None);
        let wire = serde_json::to_value(&minimal).unwrap();
        assert!(wire.get("source").is_none(), "{wire}");
        assert!(wire.get("reason").is_none(), "{wire}");
        assert!(wire.get("bytes_on_disk").is_none(), "{wire}");
        // And a frame from a daemon that *did* send the flattened `0` still
        // reads as the measurement it was — `Some(0)`, an empty file — rather
        // than being confused with the absence above.
        let flattened: RepoContextState = serde_json::from_str(
            r#"{"state":"absent","bytes_on_disk":0,"resident_bytes":0,"truncated":false}"#,
        )
        .expect("the pre-REQ-612-verify spelling must still parse");
        assert_eq!(flattened.bytes_on_disk, Some(0));

        // Direction two — a reader built before REQ-612. This models the shipped
        // `Event` faithfully in the one respect that decides the outcome: it is
        // internally tagged on `event` and closed, so a tag it has never heard
        // of is an error, and the CLI turns that error into a skipped
        // notification.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "event", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum EventAsShippedBeforeReq612 {
            TranscriptState(TranscriptState),
            ContextCleared(ContextCleared),
        }
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct EnvelopeAsShippedBeforeReq612 {
            #[serde(default)]
            session_id: Option<SessionId>,
            seq: u64,
            #[serde(flatten)]
            event: EventAsShippedBeforeReq612,
        }

        let frame = serde_json::to_string(&EventEnvelope::new(
            7,
            Some(SessionId::from("s1")),
            Event::RepoContextState(loaded),
        ))
        .unwrap();
        assert!(
            serde_json::from_str::<EnvelopeAsShippedBeforeReq612>(&frame)
                .ok()
                .is_none(),
            "the older reader must drop the frame — which is how the CLI's \
             `from_value(params).ok()?` ignores an unknown event: {frame}"
        );

        // Non-vacuity: that same reader still reads the events it shipped with,
        // so the drop above is about the new tag and not about the envelope.
        let known = serde_json::to_string(&EventEnvelope::new(
            8,
            Some(SessionId::from("s1")),
            Event::TranscriptState(TranscriptState {
                enabled: true,
                reason: TranscriptStateReason::ConfigDefault,
            }),
        ))
        .unwrap();
        assert!(serde_json::from_str::<EnvelopeAsShippedBeforeReq612>(&known).is_ok());
    }

    /// **REQ-586 BR-7's wire half.** A clamp reaches a client as a flat
    /// `context_pressure` object naming its session, what the gate did, how
    /// much, and the budget it fitted to with its bound — and survives the
    /// round trip unchanged.
    ///
    /// `session_id` is asserted on the wire object rather than on the payload
    /// for [`ContextCleared`]'s reason: the envelope is what carries it, and
    /// `envelope_wire` round-trips before returning, so re-adding `session_id`
    /// to [`ContextPressure`] fails here on the duplicate key rather than
    /// reaching a client. The payload's own keys are asserted absent of a
    /// `session_id` for the same reason, from the other side.
    ///
    /// All three kinds ride along, spelled out, so a fourth has to be added
    /// here by hand — and the zero counts survive as numbers rather than being
    /// skipped as defaults, because "re-fitted and dropped nothing" is a real
    /// report.
    #[test]
    fn context_pressure_round_trips_under_its_wire_name() {
        let pressure = ContextPressure {
            kind: ContextPressureKind::BlocksDropped,
            dropped_blocks: 3,
            elided_bytes: 0,
            newest_user_elided: false,
            budget_tokens: 4_096,
            budget_bytes: 32_768,
            bound: BudgetBound::LocalEngine,
            bound_floored: false,
        };
        round_trip(&pressure);

        let wire = envelope_wire(Event::ContextPressure(pressure));
        assert_eq!(wire["event"], "context_pressure");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["kind"], "blocks_dropped");
        assert_eq!(wire["dropped_blocks"], 3);
        assert_eq!(wire["elided_bytes"], 0);
        assert_eq!(wire["newest_user_elided"], false);
        assert_eq!(wire["budget_tokens"], 4_096);
        assert_eq!(wire["budget_bytes"], 32_768);
        assert_eq!(wire["bound"], "local_engine");
        let payload = serde_json::to_value(pressure).unwrap();
        assert!(
            payload.get("session_id").is_none(),
            "the envelope names the session, the payload must not: {payload}"
        );

        assert_eq!(Event::ContextPressure(pressure).name(), "context_pressure");

        // The elision of the newest user block — the case that is additionally
        // a turn notice — and the refit, which may have dropped nothing.
        for (kind, spelling) in [
            (ContextPressureKind::BlockElided, "block_elided"),
            (ContextPressureKind::RefitOnReroute, "refit_on_reroute"),
        ] {
            let pressure = ContextPressure {
                kind,
                dropped_blocks: 0,
                elided_bytes: 1_024,
                newest_user_elided: true,
                budget_tokens: 84_650,
                budget_bytes: 253_952,
                bound: BudgetBound::RedactScan,
                bound_floored: false,
            };
            round_trip(&pressure);
            let wire = envelope_wire(Event::ContextPressure(pressure));
            assert_eq!(wire["kind"], spelling, "{wire}");
            assert_eq!(
                wire["dropped_blocks"], 0,
                "a refit that dropped nothing must still say so on the wire"
            );
            assert_eq!(wire["elided_bytes"], 1_024);
            assert_eq!(wire["newest_user_elided"], true);
            assert_eq!(wire["bound"], "redact_scan");
        }
    }

    /// **The one home's golden table** (REQ-586 BR-8): both figure formatters,
    /// at every boundary that decides a unit.
    ///
    /// It lives here because [`thousands`] and [`bytes_figure`] live here. When
    /// they were lifted out of the CLI so the daemon and the CLI could not word
    /// the same figure two ways, the table pinning them stayed behind in
    /// `session_ui.rs` — so `cargo test -p teton-protocol` was green with the KB
    /// rounding removed, and the only thing standing between a wrong budget
    /// figure and a release was a test in a different crate. A formatter and the
    /// numbers that pin it belong in one place.
    ///
    /// The boundaries are the point, not the samples: 999/1,000 is where `B`
    /// becomes `KB`, 999,999 is the one that must **not** round up into `MB`
    /// (`1000 KB`, deliberately), 1,000,000 is where `MB` starts, and 32,768 is
    /// the default byte budget — the figure a reader sees most often.
    #[test]
    fn budget_figures_are_grouped_and_scaled() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(4_096), "4,096");
        assert_eq!(thousands(132_650), "132,650");
        assert_eq!(thousands(1_050_000), "1,050,000");

        assert_eq!(bytes_figure(0), "0 B");
        assert_eq!(bytes_figure(999), "999 B");
        assert_eq!(bytes_figure(1_000), "1 KB");
        assert_eq!(bytes_figure(32_768), "33 KB");
        assert_eq!(bytes_figure(999_999), "1000 KB");
        assert_eq!(bytes_figure(1_000_000), "1 MB");
        assert_eq!(bytes_figure(4_200_000), "4.2 MB");
    }

    /// **TASK-194 2a/2b, both directions of the additive rule.**
    ///
    /// `did_not_fit` is a fourth kind with its own wire spelling — the case
    /// that used to ride as `block_elided` with a zero for a tell — and
    /// `bound_floored` is a fifth field with a `false` default. The two
    /// directions are asserted rather than assumed, and they are **not
    /// symmetric**, which the name says out loud:
    ///
    /// * **older daemon → this client**: additive and lossless. A frame with no
    ///   `bound_floored` key reads `false`, so the line renders exactly as it
    ///   did before.
    /// * **newer daemon → older client**: the frame is *dropped*. An
    ///   unrecognized `kind` is a refusal to deserialize, never a silent
    ///   coercion into a neighbouring variant — the fail-closed half of BR-7,
    ///   since a client that cannot name what happened must say nothing rather
    ///   than say the wrong thing, and it is why `did_not_fit` needed a new
    ///   spelling instead of reusing one. It is also a real cost, recorded as
    ///   this REQ's forward-compat residual: a released client predating the
    ///   variant shows a user *nothing* for an over-budget turn. Nothing is a
    ///   worse answer than the old wrong one only if the wrong one was
    ///   actionable, and "a block was elided by 0 bytes" was not.
    #[test]
    fn a_context_that_did_not_fit_has_its_own_kind_and_an_unknown_one_degrades() {
        let pressure = ContextPressure {
            kind: ContextPressureKind::DidNotFit,
            dropped_blocks: 0,
            elided_bytes: 0,
            newest_user_elided: false,
            budget_tokens: 6_250,
            budget_bytes: 50_000,
            bound: BudgetBound::UserCap,
            bound_floored: true,
        };
        round_trip(&pressure);
        let wire = envelope_wire(Event::ContextPressure(pressure));
        assert_eq!(wire["kind"], "did_not_fit");
        assert_eq!(wire["bound_floored"], true);
        // No two kinds share a spelling — the whole point of the new one.
        let spellings: Vec<String> = [
            ContextPressureKind::BlocksDropped,
            ContextPressureKind::BlockElided,
            ContextPressureKind::RefitOnReroute,
            ContextPressureKind::DidNotFit,
        ]
        .into_iter()
        .map(|kind| serde_json::to_value(kind).unwrap().to_string())
        .collect();
        let unique: std::collections::HashSet<&String> = spellings.iter().collect();
        assert_eq!(unique.len(), spellings.len(), "{spellings:?}");

        // Older daemon: no `bound_floored` key at all.
        let older: ContextPressure = serde_json::from_str(
            r#"{"kind":"block_elided","dropped_blocks":0,"elided_bytes":9,
                "newest_user_elided":false,"budget_tokens":4096,"budget_bytes":32768,
                "bound":"window"}"#,
        )
        .expect("a frame predating the field still parses");
        assert!(!older.bound_floored);

        // A kind this build does not know is **never folded into one it does**.
        //
        // REQ-586 wrote this as `is_err()` — the enum was closed, so an unknown
        // kind was refused outright. REQ-588 BR-4 opened it, because refusing
        // the *value* meant dropping the whole *frame*, and BR-7's "nothing is
        // clamped in silence" then quietly became false at the moment something
        // was. The concern behind the original assertion is unchanged and is
        // what is pinned here: the unknown kind lands on its own arm, not on
        // `DidNotFit` or any other real one.
        assert!(serde_json::from_str::<ContextPressureKind>("\"did_not_fit\"").is_ok());
        assert_eq!(
            serde_json::from_str::<ContextPressureKind>("\"a_kind_from_the_future\"").unwrap(),
            ContextPressureKind::Unknown,
            "an unknown kind must degrade to Unknown, never be folded into a known kind"
        );
    }

    /// **TASK-194 2b, the route line's half.** `bound_floored` rides
    /// `route_decided` beside the bound it qualifies, and a frame from a daemon
    /// predating it carries no key and reads `None` — the `effort` rule, one
    /// field over.
    #[test]
    fn route_decided_carries_whether_its_bound_was_floored() {
        let decided = RouteDecided {
            category: None,
            tier: None,
            phase: None,
            provider_id: crate::ProviderId::from("kimi"),
            model: None,
            reason: "r".to_owned(),
            effort: None,
            budget_tokens: Some(6_250),
            budget_bytes: Some(50_000),
            bound: Some(BudgetBound::UserCap),
            bound_floored: Some(true),
            spend_ceiling_micro_cents: None,
            repo_context_cap: None,
        };
        round_trip(&decided);
        let wire = serde_json::to_value(&decided).unwrap();
        assert_eq!(wire["bound_floored"], true);

        let quiet = RouteDecided {
            bound_floored: None,
            ..decided.clone()
        };
        let wire = serde_json::to_value(&quiet).unwrap();
        assert!(
            wire.get("bound_floored").is_none(),
            "an unstated fact writes no key: {wire}"
        );
        let back: RouteDecided = serde_json::from_value(wire).unwrap();
        assert_eq!(back.bound_floored, None);
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

    /// **BR-8's one home, both halves.** Every bound has a wire spelling that
    /// *is* the serde tag and a human spelling that is not it.
    ///
    /// The golden pair is written out again here rather than read off the
    /// accessors, which is the whole point: a table compared against itself
    /// asserts nothing. The `match` is exhaustive and has no wildcard, so a
    /// sixth variant cannot be added without being given both spellings here
    /// — and the assertions below then demand that the production ones agree.
    ///
    /// `words()` is checked to be free of `_` because the failure mode this
    /// guards is a variant whose human spelling was filled in by pasting the
    /// wire token: legal, compiling, and shown to a user as `default_unknown`.
    #[test]
    fn every_budget_bound_carries_its_wire_name_and_its_words() {
        const fn golden(bound: BudgetBound) -> (&'static str, &'static str) {
            match bound {
                BudgetBound::Window => ("window", "window"),
                BudgetBound::DefaultUnknown => ("default_unknown", "unknown window"),
                BudgetBound::RedactScan => ("redact_scan", "redact scan"),
                BudgetBound::UserCap => ("user_cap", "user cap"),
                BudgetBound::LocalEngine => ("local_engine", "local engine"),
                // REQ-588 BR-4. Its words are deliberately a phrase rather
                // than a knob name: this build cannot say which constraint
                // bound the pair, and every other spelling here names
                // something the user could go and change.
                BudgetBound::Unknown => ("unknown", "a bound this build does not know"),
            }
        }

        let all = [
            BudgetBound::Window,
            BudgetBound::DefaultUnknown,
            BudgetBound::RedactScan,
            BudgetBound::UserCap,
            BudgetBound::LocalEngine,
            BudgetBound::Unknown,
        ];
        let mut wire_names = Vec::new();
        let mut said = Vec::new();
        for bound in all {
            let (wire, words) = golden(bound);
            assert_eq!(bound.wire_name(), wire, "{bound:?}");
            assert_eq!(bound.words(), words, "{bound:?}");
            assert!(
                !words.contains('_'),
                "`{words}` reads like a wire token, not like something to say to a \
                 person: {bound:?}"
            );

            // The wire half is the tag, in both directions — an accessor that
            // drifted from the `serde` rename would be a `/doctor` row or a
            // log line naming a bound no client can parse.
            let json = serde_json::to_string(&bound).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: BudgetBound = serde_json::from_str(&json).unwrap();
            assert_eq!(back, bound);
            round_trip(&bound);

            wire_names.push(wire);
            said.push(words);
        }

        wire_names.sort_unstable();
        wire_names.dedup();
        said.sort_unstable();
        said.dedup();
        assert_eq!(wire_names.len(), all.len(), "two bounds share a wire name");
        assert_eq!(
            said.len(),
            all.len(),
            "two bounds are said the same way, so a user cannot tell which knob to reach for"
        );
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
            budget_tokens: None,
            budget_bytes: None,
            bound: None,
            bound_floored: None,
            spend_ceiling_micro_cents: None,
            repo_context_cap: None,
        });

        // REQ-586: the budget pair and its bound ride the same frame, and every
        // bound survives the wire under its own spelling — the list is spelled
        // out so a sixth variant has to be added here by hand.
        for (bound, spelling) in [
            (BudgetBound::Window, "window"),
            (BudgetBound::DefaultUnknown, "default_unknown"),
            (BudgetBound::RedactScan, "redact_scan"),
            (BudgetBound::UserCap, "user_cap"),
            (BudgetBound::LocalEngine, "local_engine"),
        ] {
            let decided = RouteDecided {
                category: Some(Category::Edit),
                tier: Some(Tier::Build),
                phase: None,
                provider_id: ProviderId::from("deepseek"),
                model: Some("deepseek-coder".to_owned()),
                reason: "implement phase routes to the configured cheap tier".to_owned(),
                effort: Some(ResolvedEffort::effort(crate::effort::EffortLevel::High)),
                budget_tokens: Some(84_650),
                budget_bytes: Some(253_952),
                bound: Some(bound),
                bound_floored: None,
                spend_ceiling_micro_cents: None,
                repo_context_cap: None,
            };
            round_trip(&decided);
            let wire: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&decided).unwrap()).unwrap();
            assert_eq!(wire["budget_tokens"], 84_650);
            assert_eq!(wire["budget_bytes"], 253_952);
            assert_eq!(wire["bound"], spelling, "{wire}");
        }
    }

    /// REQ-586's additivity, both directions, for the three new `route_decided`
    /// keys — the `effort` rule re-applied rather than assumed inherited.
    ///
    /// A frame from a daemon predating the budget carries no key and reads
    /// `None`; a build that has never heard of the keys still reads a frame
    /// that carries them (modelled by the pre-REQ-586 shape of the reader).
    /// Serde ignores unknown fields by default and no type here opts out, but
    /// the posture is what keeps [`crate::PROTOCOL_VERSION`] still, so it is
    /// asserted — the same claim `a_client_predating_the_cause_field_…` makes
    /// for `PrivacyBlock`.
    #[test]
    fn route_decided_budget_fields_are_additive_in_both_directions() {
        // A pre-REQ-586 frame: no budget, no bound — absent, not an error.
        let decided: RouteDecided = serde_json::from_str(
            r#"{"phase":"review","provider_id":"anthropic","reason":"because"}"#,
        )
        .unwrap();
        assert_eq!(decided.budget_tokens, None);
        assert_eq!(decided.budget_bytes, None);
        assert_eq!(decided.bound, None);
        // And a frame that never populated them emits no key at all, rather
        // than `null` — the same wire an older daemon writes.
        let wire = serde_json::to_value(&decided).unwrap();
        assert!(wire.get("budget_tokens").is_none(), "{wire}");
        assert!(wire.get("budget_bytes").is_none(), "{wire}");
        assert!(wire.get("bound").is_none(), "{wire}");

        // The other direction: a reader built before the fields.
        #[derive(Deserialize)]
        struct PreBudgetRouteDecided {
            provider_id: ProviderId,
            reason: String,
            effort: Option<ResolvedEffort>,
        }
        let wire = serde_json::to_string(&RouteDecided {
            category: Some(Category::Edit),
            tier: Some(Tier::Build),
            phase: None,
            provider_id: ProviderId::from("kimi"),
            model: Some("kimi-k3".to_owned()),
            reason: "implement routes to the cheap tier".to_owned(),
            effort: Some(ResolvedEffort::effort(crate::effort::EffortLevel::High)),
            budget_tokens: Some(84_650),
            budget_bytes: Some(253_952),
            bound: Some(BudgetBound::UserCap),
            bound_floored: None,
            spend_ceiling_micro_cents: None,
            repo_context_cap: None,
        })
        .unwrap();
        assert!(
            wire.contains(r#""budget_tokens":84650"#)
                && wire.contains(r#""budget_bytes":253952"#)
                && wire.contains(r#""bound":"user_cap""#),
            "the fixture must actually carry the new keys: {wire}"
        );
        let old: PreBudgetRouteDecided = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.provider_id, ProviderId::from("kimi"));
        assert_eq!(old.reason, "implement routes to the cheap tier");
        assert_eq!(
            old.effort,
            Some(ResolvedEffort::effort(crate::effort::EffortLevel::High))
        );
    }

    /// REQ-585's additivity, both directions, for `PermissionRequest.subject`
    /// — the `route_decided` budget rule re-applied rather than assumed
    /// inherited.
    ///
    /// A request from a daemon predating the field carries no key and reads
    /// `None`; a client built before the field still reads a request that
    /// carries it. Serde ignores unknown fields by default and no type here
    /// opts out, but the posture is what keeps [`crate::PROTOCOL_VERSION`]
    /// still, so it is asserted.
    #[test]
    fn permission_request_subject_is_additive_in_both_directions() {
        // A pre-REQ-585 request: no subject — absent, not an error. This is
        // also every ordinary tool prompt from *this* build, which is why the
        // client cannot treat "has a subject" as the only recognizable state.
        let request: PermissionRequest =
            serde_json::from_str(r#"{"request_id":"r1","tool_name":"shell","options":[]}"#)
                .expect("a request from a daemon predating the field must still parse");
        assert_eq!(request.subject, None);
        assert_eq!(request.tool_name, "shell");

        // And a request that never populated it emits no key at all, rather
        // than `null` — the same wire an older daemon writes.
        let wire = serde_json::to_value(&request).unwrap();
        assert!(wire.get("subject").is_none(), "{wire}");

        // The other direction: a reader built before the field.
        #[derive(Deserialize)]
        struct PreSubjectPermissionRequest {
            request_id: RequestId,
            tool_name: String,
            options: Vec<PermissionOption>,
        }
        let wire = serde_json::to_string(&PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: "skill:user:status".to_owned(),
            description: None,
            options: vec![PermissionOption {
                option_id: "allow_once".to_owned(),
                label: "Allow once".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            subject: Some(PermissionSubject::SkillDynamicContext {
                skill: "status".to_owned(),
                source: SkillSource::User,
                commands: vec![
                    "cat ~/.claude/adlc/ETHOS.md".to_owned(),
                    "git branch --show-current".to_owned(),
                ],
                invoked_by: InvokedBy::User,
            }),
        })
        .unwrap();
        assert!(
            wire.contains(r#""subject":{"kind":"skill_dynamic_context""#)
                && wire.contains(r#""source":"user""#)
                && wire.contains(r#""git branch --show-current""#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreSubjectPermissionRequest =
            serde_json::from_str(&wire).expect("a client predating the field still reads it");
        assert_eq!(old.request_id, RequestId::from("r1"));
        assert_eq!(old.tool_name, "skill:user:status");
        assert_eq!(old.options.len(), 1, "the old reader still gets its fields");

        // BR-6: the commands ride as a **list**, one entry per command, not as
        // a joined sentence — `Surface::line` destroys newlines, so a client
        // that had to split a string could not render one line per command.
        let back: PermissionRequest = serde_json::from_str(&wire).unwrap();
        match back.subject.expect("the subject survives the round trip") {
            PermissionSubject::SkillDynamicContext {
                skill,
                source,
                commands,
                invoked_by,
            } => {
                assert_eq!(skill, "status");
                assert_eq!(source, SkillSource::User);
                assert_eq!(commands.len(), 2);
                assert_eq!(commands[1], "git branch --show-current");
                assert_eq!(invoked_by, InvokedBy::User);
            }
            // REQ-587's variant is matched here rather than swept up with
            // `_`: this arm existing is what makes the `Unrecognized` arm
            // below mean "unknown to this build" instead of "anything else".
            PermissionSubject::ProjectSkillTrust { .. } => {
                panic!("a dynamic-context subject must not read as the acknowledgment")
            }
            // REQ-589's variant, matched for REQ-587's reason one line up. It
            // is also the first place ADR-2's forcing function lands: adding a
            // subject reddens every exhaustive match on the enum, which is the
            // property that keeps a client from silently skipping a consent it
            // has never heard of.
            PermissionSubject::SkillOverBudget { .. } => {
                panic!("a dynamic-context subject must not read as the over-budget offer")
            }
            PermissionSubject::Unrecognized => panic!("a known kind must not read as unrecognized"),
        }

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-585 adds only optional fields and one method, so the negotiated version \
             does not move — the capability is proven by a successful `skills/list`"
        );
    }

    /// ADR-7's fail-closed rule, pinned at the layer that has to hold it: a
    /// subject `kind` this build has never heard of **deserializes**, to a
    /// variant the client can see, and does not error.
    ///
    /// This is not a serde nicety. The client's rule is "refuse anything you do
    /// not recognize, without calling `prompter.ask`" — and a subject that
    /// failed to parse would take the whole `PermissionRequest` with it, so the
    /// client would see no request at all and fall back to whatever it does for
    /// a malformed event. On a pipe that difference costs the user's next stdin
    /// line, which becomes a `y` (LESSON-537). `Unrecognized` is what makes the
    /// unknown case *visible* rather than absent.
    #[test]
    fn an_unknown_permission_subject_kind_reads_as_unrecognized_and_does_not_error() {
        let wire = r#"{
            "request_id":"r1",
            "tool_name":"skill:project:deploy",
            "options":[],
            "subject":{"kind":"some_future_thing","fields":"a client cannot know"}
        }"#;
        let request: PermissionRequest =
            serde_json::from_str(wire).expect("an unknown kind must not fail the whole request");
        assert_eq!(
            request.subject,
            Some(PermissionSubject::Unrecognized),
            "the unknown case has to be a value the client can branch on"
        );

        // Non-vacuity: the *known* kind still reaches its own variant, so the
        // assertion above is reached by the tag being unknown rather than by a
        // catch-all that swallows everything.
        let known = serde_json::to_string(&PermissionSubject::SkillDynamicContext {
            skill: "status".to_owned(),
            source: SkillSource::Project,
            commands: vec!["pwd".to_owned()],
            invoked_by: InvokedBy::User,
        })
        .unwrap();
        assert!(
            known.contains(r#""kind":"skill_dynamic_context""#),
            "{known}"
        );
        let back: PermissionSubject = serde_json::from_str(&known).unwrap();
        assert!(
            matches!(back, PermissionSubject::SkillDynamicContext { .. }),
            "a known kind must not be swallowed by the catch-all"
        );
    }

    /// REQ-587 BR-9: `SkillInvoked.invoked_by` is additive in both directions
    /// — `permission_request_subject_is_additive_in_both_directions`'s four
    /// legs re-applied rather than assumed inherited.
    ///
    /// An event from a daemon predating the field carries no key and reads
    /// [`InvokedBy::User`]; a client built before the field still reads an
    /// event that carries it. The non-vacuity leg is the one that earns its
    /// place: a fixture that never wrote `invoked_by` would satisfy the
    /// old-reader leg by writing nothing at all.
    #[test]
    fn skill_invoked_says_who_invoked_it_additively() {
        // A pre-REQ-587 event: no `invoked_by` — absent, not an error, and the
        // absence is a fact rather than a gap, because that daemon has no
        // `skill` tool and every invocation it could report was typed.
        let invoked: SkillInvoked = serde_json::from_str(
            r#"{"name":"status","source":"user","path_display":"~/.claude/skills/status/SKILL.md",
                "body_bytes":118,"ignored_keys":[],"outcomes":[]}"#,
        )
        .expect("an event from a daemon predating the field must still parse");
        assert_eq!(invoked.invoked_by, InvokedBy::User);
        assert_eq!(invoked.name, "status");

        // And a user invocation emits no key at all, rather than `"user"` or
        // `null` — the same wire an older daemon writes. Downgrading the
        // `skip_serializing_if` to a bare `default` fails here.
        let wire = serde_json::to_value(&invoked).unwrap();
        assert!(wire.get("invoked_by").is_none(), "{wire}");

        // The other direction: a reader built before the field.
        #[derive(Deserialize)]
        struct PreInvokerSkillInvoked {
            name: String,
            source: SkillSource,
            body_bytes: u64,
            outcomes: Vec<DynamicOutcomeView>,
        }
        let wire = serde_json::to_string(&SkillInvoked {
            name: "validate".to_owned(),
            source: SkillSource::Project,
            path_display: ".claude/skills/validate/SKILL.md".to_owned(),
            body_bytes: 4_712,
            ignored_keys: vec![],
            name_note: None,
            shadows_user_skill: false,
            model_invocable: true,
            user_invocable: true,
            turn_invocations: None,
            refused: None,
            outcomes: vec![DynamicOutcomeView {
                command: "git status --short".to_owned(),
                outcome: DynamicOutcome::Ran {
                    output_bytes: 41,
                    truncated: false,
                },
            }],
            invoked_by: InvokedBy::Model,
        })
        .unwrap();
        assert!(
            wire.contains(r#""invoked_by":"model""#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreInvokerSkillInvoked =
            serde_json::from_str(&wire).expect("a client predating the field still reads it");
        assert_eq!(old.name, "validate");
        assert_eq!(old.source, SkillSource::Project);
        assert_eq!(old.body_bytes, 4_712);
        assert_eq!(
            old.outcomes.len(),
            1,
            "the old reader still gets its fields"
        );
        // What it loses is one adjective on one echo line — BR-9's `— invoked
        // by the model` suffix. Not a guard: BR-4's acknowledgment is settled
        // daemon-side before an expansion exists.

        // BR-12 still holds with a second invoker: the body is never here.
        let back: SkillInvoked = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.invoked_by, InvokedBy::Model);
        let wire = serde_json::to_value(&back).unwrap();
        assert!(wire.get("body").is_none(), "{wire}");
        // And the path stays home-relative for a model invocation exactly as
        // for a typed one — who asked does not change what may be printed.
        assert!(
            !wire["path_display"]
                .as_str()
                .expect("a string")
                .starts_with('/'),
            "{wire}"
        );

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-587 adds optional fields and one subject variant, so the negotiated \
             version does not move"
        );
    }

    /// REQ-587 BR-9: the three facts BR-9 asks the client to render — the
    /// shadowing fact, the two frontmatter flags, and the turn's invocation
    /// count against the cap — ride the event additively, on
    /// [`skill_invoked_says_who_invoked_it_additively`]'s four legs.
    ///
    /// They are on the event because no reader can derive them. `render_event`
    /// sees only `SessionState`; the registry snapshot the shadowing fact and
    /// the flags would come from lives on the client's `UiContext`, and the
    /// per-turn count lives in the daemon's tool state and exists nowhere else
    /// at all.
    ///
    /// The **non-vacuity leg** is the one that earns its place, exactly as it
    /// does for `invoked_by`: a fixture that never set a flag to its
    /// non-default value would satisfy the old-reader leg by writing no key at
    /// all, and the test would pass against a build that had dropped the field.
    #[test]
    fn skill_invoked_carries_the_shadowing_fact_the_flags_and_the_turn_count_additively() {
        // Leg one — a pre-REQ-587 event. None of the four keys is written, and
        // each reads as the world that daemon was in: nothing shadowed, both
        // invocations permitted (it had no flags to read), and no per-turn cap
        // (it had no `skill` tool to cap).
        let invoked: SkillInvoked = serde_json::from_str(
            r#"{"name":"status","source":"user","path_display":"~/.claude/skills/status/SKILL.md",
                "body_bytes":118,"ignored_keys":[],"outcomes":[]}"#,
        )
        .expect("an event from a daemon predating these fields must still parse");
        assert!(!invoked.shadows_user_skill);
        assert!(invoked.model_invocable, "absent must not read as denied");
        assert!(invoked.user_invocable, "absent must not read as denied");
        assert_eq!(invoked.turn_invocations, None);

        // Leg two — and the ordinary invocation writes none of them either, so
        // the wire REQ-585 wrote is the wire this build writes. Downgrading any
        // of the four `skip_serializing_if`s to a bare `default` fails here.
        let wire = serde_json::to_value(&invoked).unwrap();
        for key in [
            "shadows_user_skill",
            "model_invocable",
            "user_invocable",
            "turn_invocations",
        ] {
            assert!(wire.get(key).is_none(), "{key} was written: {wire}");
        }

        // Leg three — non-vacuity. A model invocation of a shadowing,
        // model-only project skill carries every one of them, so the legs above
        // are reached by the values being defaults rather than by the fields
        // being gone.
        let loud = SkillInvoked {
            name: "validate".to_owned(),
            source: SkillSource::Project,
            path_display: ".claude/skills/validate/SKILL.md".to_owned(),
            body_bytes: 4_712,
            ignored_keys: vec![],
            name_note: None,
            outcomes: vec![],
            invoked_by: InvokedBy::Model,
            shadows_user_skill: true,
            model_invocable: true,
            user_invocable: false,
            turn_invocations: Some(TurnInvocations { count: 3, cap: 12 }),
            refused: None,
        };
        let wire = serde_json::to_string(&loud).unwrap();
        assert!(wire.contains(r#""shadows_user_skill":true"#), "{wire}");
        assert!(wire.contains(r#""user_invocable":false"#), "{wire}");
        assert!(
            !wire.contains(r#""model_invocable""#),
            "a permitted flag still writes no key: {wire}"
        );
        assert!(wire.contains(r#""count":3"#), "{wire}");
        assert!(
            wire.contains(r#""cap":12"#),
            "the cap travels with the count, or a client prints a stale ceiling: {wire}"
        );

        // Leg four — a reader built before the fields still reads that event.
        #[derive(Deserialize)]
        struct PreBr9SkillInvoked {
            name: String,
            body_bytes: u64,
            #[serde(default)]
            invoked_by: InvokedBy,
        }
        let old: PreBr9SkillInvoked =
            serde_json::from_str(&wire).expect("a client predating the fields still reads it");
        assert_eq!(old.name, "validate");
        assert_eq!(old.body_bytes, 4_712);
        assert_eq!(old.invoked_by, InvokedBy::Model);

        // And BR-12 still holds with three more fields on the event: the body
        // is never here.
        let back: SkillInvoked = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, loud);
        assert!(
            serde_json::to_value(&back).unwrap().get("body").is_none(),
            "{wire}"
        );
    }

    /// **BR-9's refusal, additively — and the four legs are why the field is a
    /// field.**
    ///
    /// A refused invocation and a skill with no dynamic context that ran
    /// perfectly carry the *same* bytes on this event: the name, the file, the
    /// body's size, an empty `outcomes`. So "a refusal is never silent" was met
    /// by a line that reported the opposite of what happened, which is worse
    /// than silence — nothing was red, because a client cannot assert a
    /// distinction the wire does not carry.
    ///
    /// The **non-vacuity leg** earns its place exactly as it does for
    /// `invoked_by` and the BR-9 trio: a fixture that never set `refused` to
    /// `Some` satisfies the old-reader and byte-identity legs by writing no key
    /// at all, and would pass against a build that had dropped the field.
    #[test]
    fn skill_invoked_says_it_was_refused_and_why_additively() {
        // Leg one — an event from a daemon predating the field. It ran, because
        // that daemon published a record only for an invocation that expanded.
        let invoked: SkillInvoked = serde_json::from_str(
            r#"{"name":"status","source":"user","path_display":"~/.claude/skills/status/SKILL.md",
                "body_bytes":118,"ignored_keys":[],"outcomes":[]}"#,
        )
        .expect("an event from a daemon predating this field must still parse");
        assert_eq!(
            invoked.refused, None,
            "absent means it ran — the only thing a pre-REQ-587 record could be"
        );

        // Leg two — an invocation that ran writes no key, so REQ-585's wire is
        // byte-for-byte the wire this build writes. Downgrading the
        // `skip_serializing_if` to a bare `default` fails here.
        let wire = serde_json::to_value(&invoked).unwrap();
        assert!(
            wire.get("refused").is_none(),
            "an invocation that ran must write nothing: {wire}"
        );

        // Leg three — non-vacuity. A refused model invocation carries the
        // reason, so the legs above are reached by the value being absent
        // rather than by the field being gone.
        let refused = SkillInvoked {
            name: "architect".to_owned(),
            source: SkillSource::User,
            path_display: "~/.claude/skills/architect/SKILL.md".to_owned(),
            body_bytes: 28_700,
            ignored_keys: vec![],
            name_note: None,
            // Empty — and this is the whole point of the field. Without it
            // these bytes are a command-free skill that ran.
            outcomes: vec![],
            invoked_by: InvokedBy::Model,
            shadows_user_skill: false,
            model_invocable: true,
            user_invocable: true,
            turn_invocations: Some(TurnInvocations { count: 1, cap: 12 }),
            refused: Some("over_budget".to_owned()),
        };
        let wire = serde_json::to_string(&refused).unwrap();
        assert!(wire.contains(r#""refused":"over_budget""#), "{wire}");
        assert!(
            !wire.contains(r#""refused":true"#),
            "the reason travels, not a flag: BR-9's line names why, and there \
             is nothing else on this event to derive it from: {wire}"
        );

        // Leg four — a reader built before the field still reads that event,
        // and still gets everything it knew about.
        #[derive(Deserialize)]
        struct PreRefusalSkillInvoked {
            name: String,
            body_bytes: u64,
            #[serde(default)]
            invoked_by: InvokedBy,
        }
        let old: PreRefusalSkillInvoked =
            serde_json::from_str(&wire).expect("a client predating the field still reads it");
        assert_eq!(old.name, "architect");
        assert_eq!(old.body_bytes, 28_700);
        assert_eq!(old.invoked_by, InvokedBy::Model);

        let back: SkillInvoked = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, refused);
        // BR-12 still holds with one more field: the body is never here — least
        // of all on a record whose whole claim is that nothing was folded.
        assert!(
            serde_json::to_value(&back).unwrap().get("body").is_none(),
            "{wire}"
        );
    }

    /// REQ-587 BR-5: the dynamic-context subject says **who asked**, and that
    /// field is additive in both directions.
    ///
    /// The contrast with `project_skill_trust_is_a_variant_an_older_client_refuses`
    /// is the point of keeping the two tests adjacent: a new *field* on a known
    /// `kind` is ignored by an old client, which still reaches its own variant
    /// and still draws REQ-585's prompt. A new *kind* is not.
    #[test]
    fn a_dynamic_context_subjects_invoker_is_additive_in_both_directions() {
        // A pre-REQ-587 request: the known kind, no `invoked_by`.
        let request: PermissionRequest = serde_json::from_str(
            r#"{"request_id":"r1","tool_name":"skill:project:deploy","options":[],
                "subject":{"kind":"skill_dynamic_context","skill":"deploy",
                           "source":"project","commands":["git status"]}}"#,
        )
        .expect("a request from a daemon predating the field must still parse");
        match request.subject.expect("the subject is present") {
            PermissionSubject::SkillDynamicContext {
                invoked_by,
                commands,
                ..
            } => {
                assert_eq!(invoked_by, InvokedBy::User);
                assert_eq!(commands, vec!["git status".to_owned()]);
            }
            other => panic!("a known kind must reach its own variant: {other:?}"),
        }

        // And a user-invoked subject emits no key, rather than `"user"`.
        let wire = serde_json::to_value(PermissionSubject::SkillDynamicContext {
            skill: "deploy".to_owned(),
            source: SkillSource::Project,
            commands: vec!["git status".to_owned()],
            invoked_by: InvokedBy::User,
        })
        .unwrap();
        assert!(wire.get("invoked_by").is_none(), "{wire}");

        // The other direction: the subject enum exactly as a REQ-585-vintage
        // client compiled it, reading a model-invoked request.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum PreInvokerSubject {
            SkillDynamicContext {
                skill: String,
                source: SkillSource,
                commands: Vec<String>,
            },
            #[serde(other)]
            Unrecognized,
        }
        let wire = serde_json::to_string(&PermissionSubject::SkillDynamicContext {
            skill: "deploy".to_owned(),
            source: SkillSource::Project,
            commands: vec!["gcloud run deploy teton".to_owned()],
            invoked_by: InvokedBy::Model,
        })
        .unwrap();
        assert!(
            wire.contains(r#""invoked_by":"model""#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreInvokerSubject =
            serde_json::from_str(&wire).expect("a client predating the field still reads it");
        match old {
            PreInvokerSubject::SkillDynamicContext {
                skill,
                source,
                commands,
            } => {
                assert_eq!(skill, "deploy");
                assert_eq!(source, SkillSource::Project);
                assert_eq!(commands, vec!["gcloud run deploy teton".to_owned()]);
            }
            PreInvokerSubject::Unrecognized => {
                panic!("a new *field* on a known kind must not read as unrecognized")
            }
        }
        // The consequence, stated: that client draws REQ-585's prompt, listing
        // every command verbatim under the skill's own key. The decision the
        // human makes is the same decision; only the attribution is missing.

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-587 adds optional fields and one subject variant, so the negotiated \
             version does not move"
        );
    }

    /// **REQ-588 BR-4 / AC-3.** A future `ContextPressureKind` or `BudgetBound`
    /// costs the *name* of the thing, never the frame that carries it.
    ///
    /// The same four legs BUG-186 established one enum over, and for the same
    /// reason: the client reads events with `serde_json::from_value(..).ok()?`,
    /// so before `#[serde(other)]` a future variant took the whole
    /// `context_pressure` frame down — and BR-7's "nothing is clamped in
    /// silence" quietly became false at exactly the moment something was.
    #[test]
    fn a_future_pressure_kind_or_bound_degrades_and_keeps_the_frame() {
        // A frame from a newer daemon: a fifth kind and a sixth bound, neither
        // constructible by this build — which is the point of the fixture.
        let wire = serde_json::json!({
            "kind": "compacted_by_summary",
            "bound": "monthly_spend",
            "bound_floored": false,
            "budget_tokens": 4_096,
            "budget_bytes": 32_768,
            "dropped_blocks": 0,
            "elided_bytes": 0,
            "newest_user_elided": false,
        });

        // Leg one and two — each unknown value lands on its own `Unknown`.
        let ev: ContextPressure = serde_json::from_value(wire)
            .expect("an unknown kind or bound must not take the whole frame down");
        assert_eq!(ev.kind, ContextPressureKind::Unknown);
        assert_eq!(ev.bound, BudgetBound::Unknown);

        // Leg three — non-vacuity. A known frame is unaffected, so the two
        // above are not passing because everything collapsed to Unknown.
        let known: ContextPressure = serde_json::from_value(serde_json::json!({
            "kind": "did_not_fit",
            "bound": "redact_scan",
            "bound_floored": true,
            "budget_tokens": 1,
            "budget_bytes": 1,
            "dropped_blocks": 0,
            "elided_bytes": 0,
            "newest_user_elided": false,
        }))
        .expect("a known frame still parses");
        assert_eq!(known.kind, ContextPressureKind::DidNotFit);
        assert_eq!(known.bound, BudgetBound::RedactScan);

        // …and a known bound's words are untouched, so the new arm did not
        // disturb the table every surface reads.
        assert_eq!(BudgetBound::RedactScan.words(), "redact scan");
        assert_eq!(BudgetBound::RedactScan.wire_name(), "redact_scan");

        // The unknown bound is deliberately vague rather than plausible: every
        // other phrase names a knob the user could change, and inventing one
        // here would send them to a setting that has nothing to do with it.
        assert!(
            BudgetBound::Unknown.words().contains("does not know"),
            "{}",
            BudgetBound::Unknown.words()
        );

        // Leg four — the contrast. `PermissionSubject` stays CLOSED: its
        // unrecognized arm is a refusal that keeps an unapproved command from
        // running, and tolerance there would be a security change, not a
        // rendering one.
        let subject: PermissionSubject =
            serde_json::from_value(serde_json::json!({ "kind": "some_future_consent" }))
                .expect("PermissionSubject parses, to its refusal arm");
        assert!(
            matches!(subject, PermissionSubject::Unrecognized),
            "the fail-closed sibling stays fail-closed: {subject:?}"
        );
    }

    /// BUG-186: a future outcome `kind` or `NotRunReason` must cost one *line*,
    /// never the whole `skill_invoked` event.
    ///
    /// The failure this pins is not cosmetic drift — it is total for the event.
    /// The client's reader is `serde_json::from_value(params).ok()?`, so before
    /// `#[serde(other)]` a fifth variant took the entire frame down: no echo
    /// line, no `/verbose` outcomes, and BR-12's "every invocation echoes one"
    /// quietly false with nothing said.
    ///
    /// The tolerant direction is chosen deliberately and only here. Both
    /// surfaces this feeds are cosmetic and it travels daemon → client only, so
    /// failing closed buys nothing. Leg four states the contrast: the sibling
    /// [`PermissionSubject`] stays closed, because *its* unrecognized arm is a
    /// refusal that keeps an unapproved command from running.
    #[test]
    fn a_future_outcome_or_reason_degrades_one_line_and_keeps_the_event() {
        // A frame from a *newer* daemon: a fifth outcome kind, and a NotRun
        // carrying a fifth reason. Built as raw JSON on purpose — the point is
        // a value this build's enums cannot construct.
        let wire = serde_json::json!({
            "name": "status",
            "source": "user",
            "path_display": "~/.claude/skills/status/SKILL.md",
            "body_bytes": 5_432,
            "ignored_keys": [],
            "outcomes": [
                { "command": "a", "outcome": { "kind": "deferred_to_next_turn" } },
                { "command": "b", "outcome": { "kind": "not_run", "reason": "quota_exhausted" } },
                { "command": "c", "outcome": { "kind": "timed_out" } },
            ],
            "invoked_by": "model",
            "shadows_user_skill": false,
            "model_invocable": true,
            "user_invocable": true,
        });

        // Leg one — the unknown *kind* lands on `Unknown` rather than failing.
        let ev: SkillInvoked = serde_json::from_value(wire.clone())
            .expect("an unknown outcome kind must not take the whole event down");
        assert_eq!(
            ev.outcomes[0].outcome,
            DynamicOutcome::Unknown,
            "a fifth kind degrades to Unknown: {:?}",
            ev.outcomes[0]
        );

        // Leg two — the unknown *reason* degrades inside a kind we do know, so
        // "it did not run" survives even though "why" does not.
        assert_eq!(
            ev.outcomes[1].outcome,
            DynamicOutcome::NotRun {
                reason: NotRunReason::Unknown
            },
            "a fifth reason keeps its NotRun kind: {:?}",
            ev.outcomes[1]
        );

        // Leg three — non-vacuity. The rest of the event is intact, and a known
        // kind in the same payload still parses as itself, so legs one and two
        // are not passing because everything collapsed to Unknown.
        assert_eq!(ev.name, "status");
        assert_eq!(ev.invoked_by, InvokedBy::Model);
        assert_eq!(ev.outcomes.len(), 3);
        assert_eq!(
            ev.outcomes[2].outcome,
            DynamicOutcome::TimedOut,
            "a known kind alongside unknown ones still parses as itself"
        );

        // Leg four — the contrast that makes this a decision and not an
        // oversight. `PermissionSubject` meets an unknown kind and refuses;
        // that arm is load-bearing and must NOT become tolerant.
        let subject: PermissionSubject =
            serde_json::from_value(serde_json::json!({ "kind": "some_future_consent" }))
                .expect("PermissionSubject also parses, but to its refusal arm");
        assert!(
            matches!(subject, PermissionSubject::Unrecognized),
            "the fail-closed sibling stays fail-closed: {subject:?}"
        );
    }

    /// REQ-587 BR-4 / ADR-7: `ProjectSkillTrust` is a new **variant**, and a
    /// variant is not additive the way a field is — so this is its own skew
    /// leg, and what it pins is a **consequence** rather than a compatibility.
    ///
    /// `permission_request_subject_is_additive_in_both_directions` covers the
    /// `subject` *field*: serde ignores an unknown field, and that test would
    /// stay green whether or not this variant existed. It cannot ignore an
    /// unknown *tag* — [`PermissionSubject`] is closed with `#[serde(other)]
    /// Unrecognized`, and that arm is a **refusal**, not an ignore.
    ///
    /// So the honest claim is not "the variant is additive". It is: an old
    /// client refuses the acknowledgment unconditionally, therefore a project
    /// skill is never model-invocable there, and the refusal that says so names
    /// a next step that client can actually perform. That is shipped behaviour,
    /// not a bug — the fail-closed direction, announced rather than silent.
    #[test]
    fn project_skill_trust_is_a_variant_an_older_client_refuses() {
        use crate::methods::{PermissionOutcome, PermissionRespondParams, RefusalReason};
        use crate::permissions::PermissionLevel;

        let subject = PermissionSubject::ProjectSkillTrust {
            root: "~/dev/teton".to_owned(),
            skills: vec![
                ProjectSkillTrustEntry {
                    name: "deploy".to_owned(),
                    shadows_user_skill: false,
                },
                ProjectSkillTrustEntry {
                    name: "validate".to_owned(),
                    shadows_user_skill: true,
                },
            ],
            more: 5,
            invoked_by: InvokedBy::Model,
        };
        round_trip(&subject);

        let value = serde_json::to_value(&subject).unwrap();
        assert_eq!(value["kind"], "project_skill_trust", "{value}");
        // BR-1's entity table: the root the prompt names is home-relative, and
        // it is the same spelling the grant key is built from.
        assert_eq!(value["root"], "~/dev/teton", "{value}");
        assert!(!value.to_string().contains("/Users/"), "{value}");
        // The shadowing fact rides as a bool the client renders, not as prose
        // it would re-parse (LESSON-529) — and the ordinary entry emits no key.
        assert_eq!(value["skills"][1]["shadows_user_skill"], true, "{value}");
        assert!(
            value["skills"][0].get("shadows_user_skill").is_none(),
            "{value}"
        );
        // LESSON-517: the list is bounded and the tail is a **count**, so "and
        // 5 more" is available rather than "and some more".
        assert_eq!(value["more"], 5, "{value}");

        // Leg one — the skew itself. The whole request, read by a
        // REQ-585-vintage client: the subject enum exactly as that client
        // compiled it, inside the request struct exactly as it had it.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum PreTrustSubject {
            SkillDynamicContext {
                skill: String,
                source: SkillSource,
                commands: Vec<String>,
            },
            #[serde(other)]
            Unrecognized,
        }
        #[derive(Debug, Deserialize)]
        struct PreTrustRequest {
            request_id: RequestId,
            tool_name: String,
            subject: Option<PreTrustSubject>,
        }
        let wire = serde_json::to_string(&PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: crate::methods::project_skill_trust_key(InvokedBy::User, "~/dev/teton"),
            description: None,
            options: vec![],
            subject: Some(subject),
        })
        .unwrap();
        assert!(
            wire.contains(r#""kind":"project_skill_trust""#),
            "the fixture must actually carry the new kind: {wire}"
        );
        let old: PreTrustRequest = serde_json::from_str(&wire)
            .expect("an unknown kind must not take the whole request down with it");
        assert!(
            matches!(old.subject, Some(PreTrustSubject::Unrecognized)),
            "a new variant lands on the catch-all, which is a refusal: {old:?}"
        );

        // Non-vacuity, both halves. The vintage reader still resolves the kind
        // it *does* know — so the leg above is reached by the tag being new,
        // not by a catch-all that swallows every subject — and this build
        // resolves the new kind to its own variant, so the fixture is not
        // merely a shape nobody can read.
        let known = serde_json::to_string(&PermissionSubject::SkillDynamicContext {
            skill: "deploy".to_owned(),
            source: SkillSource::Project,
            commands: vec!["git status".to_owned()],
            invoked_by: InvokedBy::Model,
        })
        .unwrap();
        let old_known: PreTrustSubject = serde_json::from_str(&known).unwrap();
        match old_known {
            PreTrustSubject::SkillDynamicContext {
                skill,
                source,
                commands,
            } => {
                assert_eq!(skill, "deploy");
                assert_eq!(source, SkillSource::Project);
                assert_eq!(commands, vec!["git status".to_owned()]);
            }
            PreTrustSubject::Unrecognized => {
                panic!(
                    "the vintage reader must still read the kind it knows, or leg one is vacuous"
                )
            }
        }
        let mine: PermissionRequest = serde_json::from_str(&wire).unwrap();
        assert!(
            matches!(
                mine.subject,
                Some(PermissionSubject::ProjectSkillTrust { .. })
            ),
            "this build must reach the variant, or leg one proves nothing"
        );

        // Leg two — what that client *does*. `Unrecognized` is a refusal, and
        // this is the answer it sends, checked on the wire because the daemon
        // reads it: `refused`, not `cancelled`, so the daemon knows nobody was
        // asked and composes `project_not_acknowledged` for the model rather
        // than "the user declined".
        let refusal = serde_json::to_value(PermissionRespondParams {
            request_id: RequestId::from("r1"),
            outcome: PermissionOutcome::Refused {
                reason: RefusalReason::UnrecognizedSubject,
            },
        })
        .unwrap();
        assert_eq!(refusal["outcome"]["outcome"], "refused", "{refusal}");
        assert_eq!(
            refusal["outcome"]["reason"], "unrecognized_subject",
            "{refusal}"
        );

        // Leg three — the next step, and it is one that client can perform.
        // `/permissions full` is REQ-560's, so it predates the vintage being
        // modelled, and BR-4 admits a project skill at `full` with no
        // acknowledgment at all.
        assert_eq!(PermissionLevel::Full.name(), "full");
        // Said with its exception, so the line above is not read as a complete
        // remedy: a project skill that **shadows** a user skill asks even at
        // `full`, so on this client that one stays refused until the client is
        // upgraded. The fixture carries exactly such an entry — `validate`.
        assert!(matches!(
            mine.subject.expect("the subject survives"),
            PermissionSubject::ProjectSkillTrust { ref skills, .. }
                if skills.iter().any(|entry| entry.shadows_user_skill)
        ));

        // And the key the refusal line renders is the acknowledgment's own —
        // the client prints it, and may not parse it (ADR-7). It names the
        // question, is nobody's skill key, and carries no username.
        assert_eq!(old.tool_name, "project_skill_trust:user:~/dev/teton");
        assert!(!crate::methods::is_project_skill_key(&old.tool_name));
        assert!(crate::methods::is_project_acknowledgment_key(
            &old.tool_name
        ));
        assert_eq!(old.request_id, RequestId::from("r1"));

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "a subject variant is not a new method and moves no version — what it \
             costs an older client is a refusal, not a parse failure"
        );
    }

    /// REQ-589 TASK-261: `ProjectSkillTrust.invoked_by` is additive, and its
    /// default runs **the opposite way** from every other `invoked_by`.
    ///
    /// The rule elsewhere is "absent means the user", because a daemon
    /// predating the field could only report a typed invocation. This subject's
    /// history is inverted: REQ-587 minted it with the model's tool as its only
    /// caller, so a request with no `invoked_by` came from a daemon on which the
    /// model was the only thing that could ask. Defaulting to `User` there would
    /// print "you asked" over a question the model raised — the same false
    /// attribution this field exists to remove, told in the direction that
    /// misleads rather than alarms.
    ///
    /// Both halves are pinned, because each is a separate way to get it wrong:
    /// the model path writes **no key** (so REQ-587's wire is byte-identical and
    /// no version moves), and the typed path writes one.
    #[test]
    fn the_trust_subjects_invoker_defaults_to_the_model_that_was_once_its_only_caller() {
        // A pre-TASK-261 request: the known kind, no `invoked_by`. Only the
        // model could have sent it.
        let wire = serde_json::json!({
            "kind": "project_skill_trust",
            "root": "~/dev/teton",
            "skills": [],
            "more": 0,
        });
        let back: PermissionSubject = serde_json::from_value(wire).unwrap();
        match back {
            PermissionSubject::ProjectSkillTrust { invoked_by, .. } => assert_eq!(
                invoked_by,
                InvokedBy::Model,
                "an old daemon's acknowledgment is the model's, and must not be \
                 rendered as the user's"
            ),
            other => panic!("the kind still resolves to its own variant: {other:?}"),
        }

        // The model path adds nothing to the wire, so REQ-587's bytes stand.
        let model = serde_json::to_value(PermissionSubject::ProjectSkillTrust {
            root: "~/dev/teton".to_owned(),
            skills: vec![],
            more: 0,
            invoked_by: InvokedBy::Model,
        })
        .unwrap();
        assert!(model.get("invoked_by").is_none(), "{model}");

        // The typed path says so explicitly — the whole point of the field, and
        // the half a `skip_serializing_if` pointed at the wrong arm would eat
        // with nothing else going red.
        let typed = serde_json::to_value(PermissionSubject::ProjectSkillTrust {
            root: "~/dev/teton".to_owned(),
            skills: vec![],
            more: 0,
            invoked_by: InvokedBy::User,
        })
        .unwrap();
        assert_eq!(typed["invoked_by"], "user", "{typed}");
        let round: PermissionSubject = serde_json::from_value(typed).unwrap();
        assert!(matches!(
            round,
            PermissionSubject::ProjectSkillTrust {
                invoked_by: InvokedBy::User,
                ..
            }
        ));

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "an additive field moves no version"
        );
    }

    /// **REQ-591 BR-11 / AC-14: the corrected contract, asserted rather than
    /// asserted-away.**
    ///
    /// `ProjectSkillTrust::root` used to be documented as
    /// `display_for`-minted and *"bounded"*. It is neither, and the resolution
    /// chosen was to **correct the contract** rather than bound the string —
    /// truncating it would make two repositories share one grant key, which is
    /// the collision the minter exists to prevent, and stripping it would make
    /// the prompt name a repository the answer is not remembered under.
    ///
    /// A doc paragraph is not a guarantee, so this is the guarantee: the wire is
    /// **transparent**. A directory name carrying a newline and an ESC survives
    /// serialization and deserialization byte for byte, so a client implementor
    /// reading the corrected contract can rely on it — the string they receive
    /// is the repository's, and defusing it is theirs to do.
    ///
    /// This test is deliberately the mirror image of a bounding test. If a later
    /// change *does* strip or truncate at this door, this goes red and its
    /// failure message says which paragraph now has to change with it — which is
    /// the only way a contract and a behaviour stay welded once they have come
    /// apart once.
    ///
    /// The other half — that Teton's own client neutralizes it before it reaches
    /// a terminal — is pinned where that rendering lives:
    /// `session_ui::tests::a_repository_named_with_control_bytes_cannot_redraw_the_prompt`.
    #[test]
    fn the_trust_subjects_root_reaches_a_client_exactly_as_the_directory_spelled_it() {
        // A directory name a repository can genuinely have: every byte here is
        // valid UTF-8 and legal in a POSIX path component.
        const HOSTILE: &str = "~/dev/repo\n\x1b[2K\x1b[1Aharmless";

        let wire = serde_json::to_value(PermissionSubject::ProjectSkillTrust {
            root: HOSTILE.to_owned(),
            skills: vec![ProjectSkillTrustEntry {
                name: "deploy".to_owned(),
                shadows_user_skill: false,
            }],
            more: 0,
            invoked_by: InvokedBy::User,
        })
        .unwrap();

        assert_eq!(
            wire["root"], HOSTILE,
            "the daemon neither truncated nor stripped this root, which is what \
             the field's own contract now says — if that changed on purpose, the \
             `Repository-authored` paragraph on `ProjectSkillTrust::root` has to \
             change with it, and the CLI's defusing leg is no longer the only \
             thing standing between a directory name and the user's terminal"
        );

        let back: PermissionSubject = serde_json::from_value(wire).unwrap();
        match back {
            PermissionSubject::ProjectSkillTrust { root, .. } => assert_eq!(
                root, HOSTILE,
                "a client receives the repository's bytes, so the contract's \
                 instruction to defuse at render is addressed to something real"
            ),
            other => panic!("the kind still resolves to its own variant: {other:?}"),
        }
    }

    /// REQ-585 BR-12 / ADR-15: the echo line and `/verbose`'s detail are
    /// rendered from one typed event, and the body is not in it.
    ///
    /// The four outcome shapes are round-tripped together because BR-6's rule
    /// is that a command which did not run **says so** — "declined", "no human
    /// could be asked", "timed out" and "exit 1" are four different sentences
    /// the daemon composes, and folding them into one string here is what would
    /// make the surface re-parse the daemon's own prose (LESSON-529).
    #[test]
    fn skill_invoked_carries_the_echo_line_without_carrying_the_body() {
        let invoked = SkillInvoked {
            name: "status".to_owned(),
            source: SkillSource::User,
            path_display: "~/.claude/skills/status/SKILL.md".to_owned(),
            body_bytes: 5_432,
            ignored_keys: vec!["allowed-tools".to_owned(), "model".to_owned()],
            name_note: None,
            shadows_user_skill: false,
            model_invocable: true,
            user_invocable: true,
            turn_invocations: None,
            refused: None,
            outcomes: vec![
                DynamicOutcomeView {
                    command: "cat ~/.claude/adlc/ETHOS.md".to_owned(),
                    outcome: DynamicOutcome::Ran {
                        output_bytes: 3_812,
                        truncated: false,
                    },
                },
                DynamicOutcomeView {
                    command: "grep -rn TODO .".to_owned(),
                    outcome: DynamicOutcome::Ran {
                        output_bytes: 8_000,
                        truncated: true,
                    },
                },
                DynamicOutcomeView {
                    command: "gcloud run services list".to_owned(),
                    outcome: DynamicOutcome::NotRun {
                        reason: NotRunReason::NoTerminal,
                    },
                },
                DynamicOutcomeView {
                    command: "test -f .adlc/context/architecture.md".to_owned(),
                    outcome: DynamicOutcome::Failed {
                        exit_status: Some(1),
                    },
                },
                DynamicOutcomeView {
                    command: "sleep 600".to_owned(),
                    outcome: DynamicOutcome::TimedOut,
                },
            ],
            invoked_by: InvokedBy::User,
        };
        round_trip(&invoked);

        let wire = envelope_wire(Event::SkillInvoked(invoked));
        assert_eq!(wire["event"], "skill_invoked");
        assert_eq!(wire["name"], "status");
        assert_eq!(wire["source"], "user");
        assert_eq!(wire["body_bytes"], 5_432);
        // BR-1's entity table: relative, never an absolute path carrying a
        // username into a transcript. `~/…` here because this row is a **user**
        // skill; the `project` row below carries the other half of the rule,
        // spelled from the session root (BUG-187).
        assert_eq!(wire["path_display"], "~/.claude/skills/status/SKILL.md");
        assert!(
            !wire["path_display"]
                .as_str()
                .expect("a string")
                .starts_with('/'),
            "{wire}"
        );
        // BR-12: the body is never printed, so it is never sent either. The
        // size is the only thing about it that crosses.
        assert!(wire.get("body").is_none(), "{wire}");
        assert_eq!(
            wire["outcomes"][0]["command"],
            "cat ~/.claude/adlc/ETHOS.md"
        );
        assert_eq!(wire["outcomes"][0]["outcome"]["kind"], "ran");
        assert_eq!(wire["outcomes"][0]["outcome"]["output_bytes"], 3_812);
        assert_eq!(wire["outcomes"][1]["outcome"]["truncated"], true);
        assert_eq!(wire["outcomes"][2]["outcome"]["kind"], "not_run");
        assert_eq!(wire["outcomes"][2]["outcome"]["reason"], "no_terminal");
        assert_eq!(wire["outcomes"][3]["outcome"]["kind"], "failed");
        assert_eq!(wire["outcomes"][3]["outcome"]["exit_status"], 1);
        assert_eq!(wire["outcomes"][4]["outcome"]["kind"], "timed_out");

        // A skill with no dynamic context is a real state, not a missing one:
        // BR-12's echo line says "0 dynamic commands" from an empty list.
        let plain = SkillInvoked {
            name: "beta".to_owned(),
            source: SkillSource::Project,
            path_display: ".claude/commands/beta.md".to_owned(),
            body_bytes: 118,
            ignored_keys: vec![],
            name_note: None,
            outcomes: vec![],
            invoked_by: InvokedBy::User,
            shadows_user_skill: false,
            model_invocable: true,
            user_invocable: true,
            turn_invocations: None,
            refused: None,
        };
        round_trip(&plain);
        let wire = serde_json::to_value(&plain).unwrap();
        assert_eq!(wire["outcomes"], serde_json::json!([]), "{wire}");
        assert_eq!(wire["ignored_keys"], serde_json::json!([]), "{wire}");

        // A signal-killed command has no status to report, and the absent case
        // emits no key rather than `null`.
        let signalled = serde_json::to_value(DynamicOutcome::Failed { exit_status: None }).unwrap();
        assert_eq!(signalled["kind"], "failed");
        assert!(signalled.get("exit_status").is_none(), "{signalled}");
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
            budget_tokens: None,
            budget_bytes: None,
            bound: None,
            bound_floored: None,
            spend_ceiling_micro_cents: None,
            repo_context_cap: None,
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
            subject: None,
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

    /// A representative over-budget subject: a project skill's body measured
    /// past a window-bound remote budget, with a provider to address a fix to.
    fn sample_over_budget_subject() -> PermissionSubject {
        PermissionSubject::SkillOverBudget {
            skill: "analyze".to_owned(),
            source: SkillSource::Project,
            stage: SkillStage::Body,
            measured_tokens: 41_200,
            measured_bytes: 164_800,
            budget_tokens: 32_768,
            budget_bytes: 131_072,
            bound: BudgetBound::Window,
            window_verdict: WindowVerdict::ExceedsWindow,
            // ADR-16: the daemon's own words, carried finished. Spelled here as
            // a stand-in for `skill_refusal`'s output, which lives in `tetond`
            // and cannot be reached from this crate — the *shape* is what this
            // crate owns, and `tetond`'s own suite is where the real sentence is
            // driven from a turn.
            sentence: "`/analyze` does not fit this route's context budget.".to_owned(),
            provider_id: Some(ProviderId::from("kimi")),
        }
    }

    /// ADR-2: the offer's subject carries measured integers, the stamped bound,
    /// the verdict, the skill and a provider id — **and nothing else**.
    ///
    /// The key-set assertion is the point of the test, not decoration. The
    /// daemon-side invariant `a_skill_refusal_carries_no_provider_response_body`
    /// exists because a provider's error text is remote-supplied prose that must
    /// never reach a consent prompt, and the way that invariant is broken here
    /// is by somebody adding one more helpful field. An exact key set makes that
    /// addition redden rather than ship; a `assert!(!wire.contains("body"))`
    /// would not, because the next field will not be called `body`.
    #[test]
    fn the_over_budget_subject_carries_figures_and_no_provider_response_body() {
        let subject = sample_over_budget_subject();
        round_trip(&subject);

        let wire = serde_json::to_value(&subject).unwrap();
        assert_eq!(wire["kind"], "skill_over_budget", "{wire}");

        let mut keys: Vec<&str> = wire
            .as_object()
            .expect("the subject is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bound",
                "budget_bytes",
                "budget_tokens",
                "kind",
                "measured_bytes",
                "measured_tokens",
                "provider_id",
                "sentence",
                "skill",
                "source",
                "stage",
                "window_verdict",
            ],
            "a new key on a consent subject is a decision, not a detail — and the one \
             thing it must never be is anything a provider said: {wire}"
        );

        // ADR-16's one exception to "the daemon states facts, the client writes
        // the line", and it is in the key set rather than beside it: the words
        // travel finished so BR-5's single composer stays single. What the key
        // set still forbids is the *provider's* words arriving under any name.
        assert!(
            wire["sentence"].as_str().is_some_and(|s| !s.is_empty()),
            "the offer's question is worded by the daemon and carried, not \
             re-composed by whoever renders it: {wire}"
        );

        // The figures ride verbatim, under the spellings `route_decided` and
        // `context_pressure` already use for the same two numbers.
        assert_eq!(wire["measured_tokens"], 41_200, "{wire}");
        assert_eq!(wire["budget_tokens"], 32_768, "{wire}");
        assert_eq!(wire["bound"], "window", "{wire}");
        assert_eq!(wire["window_verdict"], "exceeds_window", "{wire}");
        assert_eq!(wire["stage"], "body", "{wire}");
        assert_eq!(wire["provider_id"], "kimi", "{wire}");

        // The overrun is derived where it is rendered, so it is deliberately
        // absent: two ways to state one fact could disagree (LESSON-545).
        assert!(wire.get("overrun_tokens").is_none(), "{wire}");
        assert!(wire.get("overrun_words").is_none(), "{wire}");
        assert!(wire.get("overrun_bytes").is_none(), "{wire}");

        // A route whose bound names no provider omits the key rather than
        // sending an empty string — absent is the fact, not a gap.
        let local = PermissionSubject::SkillOverBudget {
            skill: "analyze".to_owned(),
            source: SkillSource::User,
            stage: SkillStage::WithDynamicContext,
            measured_tokens: 9_000,
            measured_bytes: 36_000,
            budget_tokens: 4_096,
            budget_bytes: 32_768,
            bound: BudgetBound::LocalEngine,
            window_verdict: WindowVerdict::WindowUnknown,
            sentence: "`/analyze` does not fit this route's context budget.".to_owned(),
            provider_id: None,
        };
        round_trip(&local);
        let wire = serde_json::to_value(&local).unwrap();
        assert!(wire.get("provider_id").is_none(), "{wire}");
        assert_eq!(wire["stage"], "with_dynamic_context", "{wire}");
        assert_eq!(wire["window_verdict"], "window_unknown", "{wire}");
    }

    /// REQ-589 ADR-2 / BR-4: `SkillOverBudget` is a new **variant**, and a
    /// client that predates it refuses rather than mis-renders.
    ///
    /// `project_skill_trust_is_a_variant_an_older_client_refuses` pins the same
    /// property for REQ-587's subject; this is its own leg because a test over
    /// *that* variant would stay green whether or not this one existed. What is
    /// pinned is a consequence rather than a compatibility: an old client
    /// answers [`crate::methods::RefusalReason::UnrecognizedSubject`] without
    /// asking anyone, the turn refuses under today's sentence, and BR-4 is
    /// satisfied — an unanswerable offer *is* today's refusal, and silence is
    /// never consent.
    #[test]
    fn skill_over_budget_is_a_variant_an_older_client_refuses() {
        let subject = sample_over_budget_subject();

        // The subject enum exactly as a REQ-587-vintage client compiled it.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum PreOfferSubject {
            SkillDynamicContext {
                skill: String,
                source: SkillSource,
                commands: Vec<String>,
            },
            ProjectSkillTrust {
                root: String,
                skills: Vec<ProjectSkillTrustEntry>,
                more: u32,
            },
            #[serde(other)]
            Unrecognized,
        }
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct PreOfferRequest {
            request_id: RequestId,
            tool_name: String,
            subject: Option<PreOfferSubject>,
        }

        let wire = serde_json::to_string(&PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: "skill:project:analyze".to_owned(),
            description: None,
            options: vec![PermissionOption {
                option_id: OPTION_ID_OVER_BUDGET_PROCEED_ONCE.to_owned(),
                label: "send it this once".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            subject: Some(subject),
        })
        .unwrap();
        assert!(
            wire.contains(r#""kind":"skill_over_budget""#),
            "the fixture must actually carry the new kind: {wire}"
        );
        let old: PreOfferRequest = serde_json::from_str(&wire)
            .expect("an unknown kind must not take the whole request down with it");
        assert!(
            matches!(old.subject, Some(PreOfferSubject::Unrecognized)),
            "a new variant lands on the catch-all, which is a refusal: {old:?}"
        );

        // Non-vacuity, both halves: the vintage reader still resolves a kind it
        // *does* know, so the leg above is reached by the tag being new rather
        // than by a catch-all that swallows everything — and this build resolves
        // the new kind to its own variant.
        let known = serde_json::to_string(&PermissionSubject::ProjectSkillTrust {
            root: "~/dev/teton".to_owned(),
            skills: vec![],
            more: 0,
            invoked_by: InvokedBy::Model,
        })
        .unwrap();
        let old: PreOfferSubject = serde_json::from_str(&known).unwrap();
        assert!(matches!(old, PreOfferSubject::ProjectSkillTrust { .. }));
        let mine: PermissionSubject =
            serde_json::from_str(&serde_json::to_string(&sample_over_budget_subject()).unwrap())
                .unwrap();
        assert!(matches!(mine, PermissionSubject::SkillOverBudget { .. }));
    }

    /// The subject's inner vocabularies degrade rather than take the question
    /// down with them.
    ///
    /// The fail-closed decision lives one level up, at the `kind` tag: an
    /// unknown kind is a refusal, and that is the guard. An unknown *stage*,
    /// *verdict* or *bound* changes no authority — the question is still "send
    /// this over-budget expansion" — so the alternative to tolerance is the
    /// whole `permission_request` frame failing to deserialize, which renders
    /// nothing on any screen and parks the daemon's waiter with no timeout of
    /// its own. That is not fail-closed; it is a question nobody is ever asked.
    #[test]
    fn an_unknown_stage_or_verdict_degrades_instead_of_dropping_the_offer() {
        let wire = r#"{
            "request_id":"r1",
            "tool_name":"skill:project:analyze",
            "options":[],
            "subject":{
                "kind":"skill_over_budget",
                "skill":"analyze",
                "source":"project",
                "stage":"some_future_stage",
                "measured_tokens":41200,
                "measured_bytes":164800,
                "budget_tokens":32768,
                "budget_bytes":131072,
                "bound":"some_future_bound",
                "window_verdict":"some_future_verdict",
                "sentence":"`/analyze` does not fit this route's context budget.",
                "provider_id":"kimi"
            }
        }"#;
        let request: PermissionRequest = serde_json::from_str(wire)
            .expect("a future stage must not cost the user the whole question");
        let PermissionSubject::SkillOverBudget {
            stage,
            bound,
            window_verdict,
            measured_tokens,
            ..
        } = request.subject.expect("the subject survives")
        else {
            panic!("a known kind must reach its own variant");
        };
        assert_eq!(stage, SkillStage::Unknown);
        assert_eq!(bound, BudgetBound::Unknown);
        assert_eq!(window_verdict, WindowVerdict::Unknown);
        // The figures — which are what the user actually decides on — survive
        // the words this build cannot read.
        assert_eq!(measured_tokens, 41_200);

        // Non-vacuity: the known spellings still reach their own variants, so
        // the assertions above are reached by the values being new rather than
        // by a catch-all that swallows every value.
        for (json, expected) in [
            (r#""body""#, SkillStage::Body),
            (r#""with_dynamic_context""#, SkillStage::WithDynamicContext),
        ] {
            assert_eq!(serde_json::from_str::<SkillStage>(json).unwrap(), expected);
        }
        for (json, expected) in [
            (r#""fits_window""#, WindowVerdict::FitsWindow),
            (r#""exceeds_window""#, WindowVerdict::ExceedsWindow),
            (r#""window_unknown""#, WindowVerdict::WindowUnknown),
        ] {
            assert_eq!(
                serde_json::from_str::<WindowVerdict>(json).unwrap(),
                expected
            );
        }
        for (json, expected) in [
            (r#""declare_window""#, RemedyKind::DeclareWindow),
            (r#""raise_cap""#, RemedyKind::RaiseCap),
            (r#""raise_window""#, RemedyKind::RaiseWindow),
            (r#""bind_tier_remote""#, RemedyKind::BindTierRemote),
            (r#""not_offered""#, RemedyKind::NotOffered),
        ] {
            assert_eq!(serde_json::from_str::<RemedyKind>(json).unwrap(), expected);
        }
        assert_eq!(
            serde_json::from_str::<RemedyKind>(r#""a_fifth_fix""#).unwrap(),
            RemedyKind::Unknown,
            "an unreadable remedy must not be silently reported as `not_offered` — \
             \"there is no fix\" and \"this build cannot name the fix\" are different facts"
        );
    }

    /// ADR-1: the four combinations of BR-7's two independent answers ship as
    /// four **option ids** on the wire that already exists.
    ///
    /// The spellings are pinned because they are the contract: the daemon
    /// offers them and the client selects on them by string, exactly as
    /// [`OPTION_ID_ENABLE_PERMANENT`] is. Renaming one silently turns an accept
    /// into an unrecognized answer, which on this path means an oversized turn
    /// that was approved and never sent.
    #[test]
    fn the_four_over_budget_option_ids_are_the_wire_contract() {
        let ids = [
            OPTION_ID_OVER_BUDGET_PROCEED_ONCE,
            OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY,
            OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
            OPTION_ID_OVER_BUDGET_DECLINE,
        ];
        assert_eq!(
            ids,
            [
                "over_budget_proceed_once",
                "over_budget_proceed_and_remedy",
                "over_budget_remedy_only",
                "over_budget_decline",
            ]
        );

        // Four distinct answers, and none of them is the web flow's id — a
        // collision would make one consent selectable from the other's prompt.
        let mut sorted = ids;
        sorted.sort_unstable();
        let mut deduped = sorted.to_vec();
        deduped.dedup();
        assert_eq!(deduped.len(), 4, "the four answers must be four ids");
        assert!(!ids.contains(&OPTION_ID_ENABLE_PERMANENT));
    }

    /// The three announcements round-trip under the spec's own event names, and
    /// each carries what its Events-table row says it carries.
    ///
    /// They are separate rows rather than one folded variant (the
    /// [`WebLookup`] treatment) because they are not one story with three
    /// endings: an offer that was declined publishes only the first, and a
    /// remedy-only answer publishes the third with no second. Folding them
    /// would make "was this turn actually sent?" a question about a field
    /// instead of about which events exist.
    #[test]
    fn skill_over_budget_events_round_trip_under_their_wire_names() {
        let offered = SkillOverBudgetOffered {
            skill: "analyze".to_owned(),
            source: SkillSource::Project,
            stage: SkillStage::Body,
            measured_tokens: 41_200,
            measured_bytes: 164_800,
            budget_tokens: 32_768,
            budget_bytes: 131_072,
            bound: BudgetBound::Window,
            window_verdict: WindowVerdict::ExceedsWindow,
            remedy_kind: RemedyKind::RaiseWindow,
        };
        round_trip(&offered);
        let wire = envelope_wire(Event::SkillOverBudgetOffered(offered));
        assert_eq!(wire["event"], "skill_over_budget_offered", "{wire}");
        assert_eq!(wire["skill"], "analyze", "{wire}");
        assert_eq!(wire["source"], "project", "{wire}");
        assert_eq!(wire["stage"], "body", "{wire}");
        assert_eq!(wire["measured_tokens"], 41_200, "{wire}");
        assert_eq!(wire["measured_bytes"], 164_800, "{wire}");
        assert_eq!(wire["budget_tokens"], 32_768, "{wire}");
        assert_eq!(wire["bound"], "window", "{wire}");
        assert_eq!(wire["window_verdict"], "exceeds_window", "{wire}");
        assert_eq!(wire["remedy_kind"], "raise_window", "{wire}");

        // BR-7b's reachable cell: a `redact_scan` bound has no durable fix, and
        // the record says so rather than leaving the field off.
        let no_remedy = SkillOverBudgetOffered {
            skill: "analyze".to_owned(),
            source: SkillSource::User,
            stage: SkillStage::WithDynamicContext,
            measured_tokens: 9_000,
            measured_bytes: 36_000,
            budget_tokens: 4_096,
            budget_bytes: 32_768,
            bound: BudgetBound::RedactScan,
            window_verdict: WindowVerdict::FitsWindow,
            remedy_kind: RemedyKind::NotOffered,
        };
        round_trip(&no_remedy);
        let wire = envelope_wire(Event::SkillOverBudgetOffered(no_remedy));
        assert_eq!(wire["remedy_kind"], "not_offered", "{wire}");

        let accepted = SkillOverBudgetAccepted {
            skill: "analyze".to_owned(),
            source: SkillSource::Project,
            stage: SkillStage::Body,
            measured_tokens: 41_200,
            measured_bytes: 164_800,
            budget_tokens: 32_768,
            budget_bytes: 131_072,
            window_verdict: WindowVerdict::ExceedsWindow,
        };
        round_trip(&accepted);
        let wire = envelope_wire(Event::SkillOverBudgetAccepted(accepted));
        assert_eq!(wire["event"], "skill_over_budget_accepted", "{wire}");
        // BR-1: the figure in the record is the figure that was sent, whole.
        assert_eq!(wire["measured_bytes"], 164_800, "{wire}");
        assert_eq!(wire["window_verdict"], "exceeds_window", "{wire}");

        let applied = SkillOverBudgetRemedyApplied {
            remedy_kind: RemedyKind::RaiseWindow,
            provider_id: Some(ProviderId::from("kimi")),
            previous_value: "131072".to_owned(),
            new_value: "1000000".to_owned(),
        };
        round_trip(&applied);
        let wire = envelope_wire(Event::SkillOverBudgetRemedyApplied(applied));
        assert_eq!(wire["event"], "skill_over_budget_remedy_applied", "{wire}");
        assert_eq!(wire["remedy_kind"], "raise_window", "{wire}");
        assert_eq!(wire["provider_id"], "kimi", "{wire}");
        // Both values, always: a record naming only the new one cannot tell a
        // raise from a first declaration.
        assert_eq!(wire["previous_value"], "131072", "{wire}");
        assert_eq!(wire["new_value"], "1000000", "{wire}");

        // A remedy that addresses no single provider omits the key.
        let bound_tier = SkillOverBudgetRemedyApplied {
            remedy_kind: RemedyKind::BindTierRemote,
            provider_id: None,
            previous_value: "local".to_owned(),
            new_value: "kimi".to_owned(),
        };
        round_trip(&bound_tier);
        let wire = envelope_wire(Event::SkillOverBudgetRemedyApplied(bound_tier));
        assert!(wire.get("provider_id").is_none(), "{wire}");

        // `name()` and the serialized tag are the same string, which is the
        // property the dispatch table exists to keep.
        for (event, expected) in [
            (
                Event::SkillOverBudgetOffered(SkillOverBudgetOffered {
                    skill: "s".to_owned(),
                    source: SkillSource::User,
                    stage: SkillStage::Body,
                    measured_tokens: 1,
                    measured_bytes: 2,
                    budget_tokens: 3,
                    budget_bytes: 4,
                    bound: BudgetBound::DefaultUnknown,
                    window_verdict: WindowVerdict::WindowUnknown,
                    remedy_kind: RemedyKind::DeclareWindow,
                }),
                "skill_over_budget_offered",
            ),
            (
                Event::SkillOverBudgetAccepted(SkillOverBudgetAccepted {
                    skill: "s".to_owned(),
                    source: SkillSource::User,
                    stage: SkillStage::Body,
                    measured_tokens: 1,
                    measured_bytes: 2,
                    budget_tokens: 3,
                    budget_bytes: 4,
                    window_verdict: WindowVerdict::WindowUnknown,
                }),
                "skill_over_budget_accepted",
            ),
            (
                Event::SkillOverBudgetRemedyApplied(SkillOverBudgetRemedyApplied {
                    remedy_kind: RemedyKind::DeclareWindow,
                    provider_id: None,
                    previous_value: "0".to_owned(),
                    new_value: "200000".to_owned(),
                }),
                "skill_over_budget_remedy_applied",
            ),
        ] {
            assert_eq!(event.name(), expected, "name() mismatch");
            assert_eq!(
                EventEnvelope::new(0, None, event).event_name(),
                expected,
                "envelope name mismatch"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// unbounded_root_warning / boundary_defaults_applied (REQ-597)
// ---------------------------------------------------------------------------

/// A session started with **no** privacy boundaries in force, at a root broad
/// enough for that to matter (REQ-597 BR-5).
///
/// After REQ-597 the empty effective set is reachable only through
/// `[privacy] disable_default_boundaries = true` (BR-3), so this event does not
/// mean "you have not configured boundaries yet" — it means *you turned the
/// shipped ones off, and this session is rooted at your home directory or the
/// filesystem root*.
///
/// Published on the bus rather than routed to the creating connection, and
/// rendered ungated by the CLI. REQ-571 BR-4 is the reason for both: an audit
/// signal that reaches only the party it indicts can be suppressed by them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnboundedRootWarning {
    /// What kind of place the session's root is — `home` or `filesystem_root`.
    /// The two other [`RootKind`]s never raise this.
    pub root_kind: RootKind,
}

/// The shipped default boundary set contributed rows to a starting session
/// (REQ-597 System Model).
///
/// The quiet counterpart to [`UnboundedRootWarning`]: it reports that the
/// protection is **on**, which is the ordinary case, so the CLI gates it behind
/// verbose. Its value is in a transcript or a bug report, where "was the
/// default set in force?" is otherwise unanswerable after the fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryDefaultsApplied {
    /// How many builtin rows were composed into the effective set.
    pub count: usize,
}
