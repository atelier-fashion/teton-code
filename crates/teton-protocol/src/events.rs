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
//! that gates the local tier before any weights are fetched.

use serde::{Deserialize, Serialize};

use crate::{ClientKind, Phase, ProtocolVersion, ProviderId, RequestId, SessionId};

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
}

impl Event {
    /// The wire `event` name, identical to the serialized tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Event::SessionUpdate(_) => "session_update",
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
// route_decided (Teton differentiator)
// ---------------------------------------------------------------------------

/// The router picked a provider for a step (spec Events: `route_decided`).
///
/// Every routing decision emits this with its `reason` — the legibility promise
/// (BR-5). The `session` scoping lives in the [`EventEnvelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecided {
    /// Phase that drove the decision; `None` for heuristic freeform routing.
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
    /// Repo-relative path of the boundary-protected content.
    pub path: String,
    /// Provider the content would have reached.
    pub provider_id: ProviderId,
    /// What the daemon did instead.
    pub action: PrivacyAction,
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
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

/// A catalog entry as offered to the user.
///
/// A deliberate projection of the catalog row: `url` and `sha256` are daemon-side
/// download mechanics the user is not choosing between, and no install path ever
/// appears (BR-11). What is left is what a person needs in order to choose — the
/// name, the band it serves, what it costs in disk, and what it needs in RAM.
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
                },
                required_disk_bytes: 5_700_000_000,
            }),
            alternatives: vec![CatalogEntryView {
                name: "qwen2.5-coder-3b".to_owned(),
                band: TierBand::Small,
                size_bytes: 2_000_000_000,
                ram_floor_bytes: 5_368_709_120,
            }],
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
    }

    #[test]
    fn event_names_match_the_spec_events_table() {
        // The six names fixed by REQ-544's Events table, plus the three
        // streaming/lifecycle events. `name()` must equal the serialized tag.
        let cases: Vec<(Event, &str)> = vec![
            (
                Event::RouteDecided(RouteDecided {
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
            phase: Some(Phase::Implement),
            provider_id: ProviderId::from("deepseek"),
            model: Some("deepseek-coder".to_owned()),
            reason: "implement phase routes to the configured cheap tier".to_owned(),
        });
    }

    #[test]
    fn privacy_block_round_trips() {
        round_trip(&PrivacyBlock {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("anthropic"),
            action: PrivacyAction::ReroutedToLocal,
        });
    }

    #[test]
    fn cost_recorded_round_trips() {
        round_trip(&CostRecorded {
            record: CostRecord {
                session_id: SessionId::from("s1"),
                phase: Some(Phase::Review),
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
            }],
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
}
