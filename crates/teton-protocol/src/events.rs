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
//!
//! This list is an index, not decoration: a new variant of [`Event`] that is not
//! named here makes the paragraph above wrong.

use serde::{Deserialize, Serialize};

use crate::{Category, ClientKind, Phase, ProtocolVersion, ProviderId, RequestId, SessionId, Tier};

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
    /// A web lookup reached a terminal outcome (REQ-563 BR-7). Every way a
    /// lookup can end — including the ones the spec names as separate events —
    /// is a [`WebLookupOutcome`] on this one variant.
    WebLookup(WebLookup),
    /// A web-lookup consent decision was recorded (REQ-563 BR-4). The *prompt*
    /// that preceded it is a [`PermissionRequest`], not an event of its own.
    WebConsentDecided(WebConsentDecided),
    /// The user lifted this session's web taint restriction (REQ-563 BR-13).
    WebTaintOverridden(WebTaintOverridden),
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
            Event::CostRecorded(_) => "cost_recorded",
            Event::ProviderDegraded(_) => "provider_degraded",
            Event::ModelLifecycle(_) => "model_lifecycle",
            Event::ModelSelectionProposed(_) => "model_selection_proposed",
            Event::ModelSelectionDecided(_) => "model_selection_decided",
            Event::PermissionRequest(_) => "permission_request",
            Event::PhaseTransition(_) => "phase_transition",
            Event::DaemonClientAttach(_) => "daemon_client_attach",
            Event::WebLookup(_) => "web_lookup",
            Event::WebConsentDecided(_) => "web_consent_decided",
            Event::WebTaintOverridden(_) => "web_taint_overridden",
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
        }));
        // Flattened: envelope metadata and the payload share one object.
        assert_eq!(wire["event"], "route_decided");
        assert_eq!(wire["seq"], 1);
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["provider_id"], "anthropic");
        assert_eq!(wire["category"], "design");
        assert_eq!(wire["tier"], "think");
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
            },
        });
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
}
