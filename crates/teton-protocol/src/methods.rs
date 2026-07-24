//! Typed client→daemon methods.
//!
//! Each request type implements [`RpcMethod`], binding it to its wire method
//! name and its result type, so a caller cannot pair the wrong params, result,
//! or method string. Method names are slash-namespaced in the ACP style; where
//! ACP already names an equivalent call, an `ACP:` comment records it so the
//! future compatibility shim is a rename.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::events::{CatalogEntryView, ModelSelectionProposed, ProbeReportView, SelectionSource};
use crate::jsonrpc::{Id, Request};
use crate::{
    Phase, PrivacyMode, ProviderId, ProviderKind, RequestId, SessionId, SessionMode, TurnId,
};

/// Binds a request-parameter type to its wire method name and result type.
pub trait RpcMethod: Serialize + DeserializeOwned {
    /// The JSON-RPC `method` string this params type is sent under.
    const METHOD: &'static str;
    /// The result type expected in the matching response.
    type Result: Serialize + DeserializeOwned;
}

/// Builds a typed [`Request`] whose `method` is filled from `P::METHOD`.
pub fn request<P: RpcMethod>(id: Id, params: P) -> Request<P> {
    Request::new(id, P::METHOD, params)
}

// ---------------------------------------------------------------------------
// session lifecycle
// ---------------------------------------------------------------------------

/// Create a new session. ACP equivalent: `session/new`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreateParams {
    /// Freeform (default) or structured (ADLC) mode.
    pub mode: SessionMode,
    /// Starting phase; required in structured mode, `None` in freeform.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
}

/// Result of [`SessionCreateParams`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreateResult {
    /// The id assigned to the new session.
    pub session_id: SessionId,
}

impl RpcMethod for SessionCreateParams {
    const METHOD: &'static str = "session/create";
    type Result = SessionCreateResult;
}

/// A one-line description of a session, used in listings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session id. ACP: `sessionId`.
    pub session_id: SessionId,
    /// Interaction mode.
    pub mode: SessionMode,
    /// Current phase, or `None` in freeform mode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<Phase>,
    /// Optional human-facing title.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
}

/// List every session the daemon holds (surface-parity rule, BR-4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionListParams {}

/// Result of [`SessionListParams`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResult {
    /// Every live session, newest first.
    pub sessions: Vec<SessionSummary>,
}

impl RpcMethod for SessionListParams {
    const METHOD: &'static str = "session/list";
    type Result = SessionListResult;
}

/// Attach to an existing session. ACP equivalent: `session/load`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAttachParams {
    /// The session to attach to.
    pub session_id: SessionId,
}

/// Result of [`SessionAttachParams`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAttachResult {
    /// Snapshot of the attached session.
    pub session: SessionSummary,
}

impl RpcMethod for SessionAttachParams {
    const METHOD: &'static str = "session/attach";
    type Result = SessionAttachResult;
}

// ---------------------------------------------------------------------------
// prompt turn
// ---------------------------------------------------------------------------

/// One block of prompt content. ACP: a prompt content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptBlock {
    /// Plain text. ACP: `text`.
    Text {
        /// The text content.
        text: String,
    },
    /// A reference to a resource by URI. ACP: `resource_link`.
    ResourceLink {
        /// The resource URI.
        uri: String,
        /// Optional display name.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        name: Option<String>,
    },
}

/// Submit a prompt turn to a session. ACP equivalent: `session/prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTurnParams {
    /// Target session.
    pub session_id: SessionId,
    /// The prompt, as an ordered list of content blocks.
    pub prompt: Vec<PromptBlock>,
}

/// Why a prompt turn ended. ACP: `stopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The turn completed normally.
    EndTurn,
    /// The model hit its output-token ceiling.
    MaxTokens,
    /// The turn hit the harness request/loop ceiling.
    MaxTurnRequests,
    /// The model refused.
    Refusal,
    /// The client cancelled the turn.
    Cancelled,
}

/// Result of [`PromptTurnParams`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTurnResult {
    /// Id of the completed turn.
    pub turn_id: TurnId,
    /// Why the turn ended.
    pub stop_reason: StopReason,
}

impl RpcMethod for PromptTurnParams {
    const METHOD: &'static str = "session/prompt";
    type Result = PromptTurnResult;
}

// ---------------------------------------------------------------------------
// permission response
// ---------------------------------------------------------------------------

/// The client's answer to a `permission_request` event.
///
/// ACP: the response to `session/request_permission`. In Teton the daemon
/// *broadcasts* the request as an event (multiple clients may be attached) and
/// the deciding client replies with this method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRespondParams {
    /// Correlates with the `permission_request` event's `request_id`.
    pub request_id: RequestId,
    /// The chosen outcome.
    pub outcome: PermissionOutcome,
}

/// Outcome of a permission prompt. ACP: `RequestPermissionOutcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// The user picked one of the offered options.
    Selected {
        /// The chosen option's id (see `PermissionOption`).
        option_id: String,
    },
    /// The user dismissed the prompt without choosing.
    Cancelled,
}

/// Result of [`PermissionRespondParams`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionRespondResult {}

impl RpcMethod for PermissionRespondParams {
    const METHOD: &'static str = "permission/respond";
    type Result = PermissionRespondResult;
}

// ---------------------------------------------------------------------------
// local model selection (REQ-547)
// ---------------------------------------------------------------------------
//
// `model/confirm` is to `model_selection_proposed` what `permission/respond` is
// to `permission_request` (D-3): the daemon broadcasts, the deciding client
// answers by `request_id`. `model/list` / `model/set` / `model/status` are the
// post-first-run surface behind `teton model …` (AC-9).
//
// The payload projections these results carry ([`CatalogEntryView`],
// [`ProbeReportView`], [`SelectionSource`]) are defined in [`crate::events`]
// alongside the proposal that introduces them, so the event and the method
// results are literally the same types and cannot drift.

/// The client's answer to a `model_selection_proposed` event.
///
/// The daemon *broadcasts* the proposal as an event (multiple clients may be
/// attached) and the deciding client replies with this method, keyed by
/// `request_id` — deliberately the same shape as [`PermissionRespondParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfirmParams {
    /// Correlates with the `model_selection_proposed` event's `request_id`.
    pub request_id: RequestId,
    /// The chosen outcome.
    pub outcome: ModelConfirmOutcome,
}

/// The three — and only three — answers to a model proposal.
///
/// A **closed** enum with no `#[serde(other)]` catch-all and no `Default`: an
/// `outcome` this build does not know is a deserialization *error* (which the
/// daemon returns as [`crate::jsonrpc::error_code::INVALID_PARAMS`]), never a
/// silent fallback. That is load-bearing rather than stylistic — BR-1 says
/// nothing downloads without an explicit decision, so an answer that cannot be
/// understood must fail loudly instead of being read as "accept".
///
/// Note the asymmetry with the rest of this crate: unknown *fields* are
/// tolerated for forward compatibility, but an unknown *variant tag* is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ModelConfirmOutcome {
    /// Install the proposed model as offered.
    Accept,
    /// Install a different catalog entry instead (BR-3).
    Choose {
        /// The catalog name to install; must name an entry the daemon offered.
        name: String,
        /// Set only after the user answered a *second*, explicit confirmation
        /// that this entry's RAM floor exceeds the machine's RAM (BR-3). The
        /// daemon refuses such a choice while this is false, so an over-sized
        /// pick can never happen by accident — and the guard lives here, in the
        /// protocol, rather than as a convention each client re-implements.
        #[serde(default)]
        confirmed_above_ram_floor: bool,
    },
    /// Decline the local tier; the machine runs remote-only and is not
    /// re-prompted (BR-4).
    Decline,
}

/// Result of [`ModelConfirmParams`].
///
/// Deliberately empty, like [`PermissionRespondResult`]: the authoritative
/// outcome reaches *every* attached client as a `model_selection_decided` event,
/// so echoing it here would duplicate the record in two places that could
/// disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfirmResult {}

impl RpcMethod for ModelConfirmParams {
    const METHOD: &'static str = "model/confirm";
    type Result = ModelConfirmResult;
}

/// A wire projection of the recorded decision (spec entity `ModelSelection`).
///
/// Mirrors `teton_core::entities::ModelSelection` field-for-field **except** the
/// install path, which never appears in a protocol payload (BR-11); a client
/// that wants to show the path resolves it locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectionView {
    /// The chosen catalog model name; `None` exactly when `declined_local`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_name: Option<String>,
    /// How the decision was reached.
    pub source: SelectionSource,
    /// True when the local tier was declined (BR-4).
    pub declined_local: bool,
    /// When the decision was recorded, in Unix epoch milliseconds.
    pub decided_at_ms: u64,
}

/// Install state of a model's weights (spec entity `InstallState.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallStatus {
    /// Nothing on disk.
    Absent,
    /// A partial download exists; resumable, never loadable (BR-9).
    Partial,
    /// Present and verified against the catalog digest (BR-6).
    Verified,
    /// Present but failed verification; must be discarded, never installed.
    Corrupt,
}

/// Install state of the selected model (spec entity `InstallState`).
///
/// Carries no `path`: BR-11 keeps absolute filesystem paths out of every
/// protocol payload, and the daemon's state directory is a convention the client
/// already knows, so `teton model status` can render a path without one ever
/// crossing the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallStateView {
    /// The model these weights belong to.
    pub model_name: String,
    /// Current state of the weights on disk.
    pub status: InstallStatus,
}

/// List the catalog with each entry's fit for this machine (AC-9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListParams {}

/// One row of [`ModelListResult`]: a catalog entry plus its fit.
///
/// `fits_ram` / `fits_disk` are computed daemon-side against the probe so every
/// client renders the same verdict, rather than each re-deriving it (and
/// disagreeing about the working margin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListEntry {
    /// The catalog entry.
    pub entry: CatalogEntryView,
    /// Whether this machine clears the entry's RAM floor. `false` entries are
    /// still selectable, with the BR-3 second confirmation.
    pub fits_ram: bool,
    /// Whether there is enough free disk to install it right now (BR-7).
    pub fits_disk: bool,
}

/// Result of [`ModelListParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListResult {
    /// The machine the fits were computed against (BR-2 legibility).
    pub probe: ProbeReportView,
    /// Every catalog entry, in catalog order.
    pub models: Vec<ModelListEntry>,
    /// The current selection, or `None` when no decision has been recorded yet.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub selection: Option<ModelSelectionView>,
}

impl RpcMethod for ModelListParams {
    const METHOD: &'static str = "model/list";
    type Result = ModelListResult;
}

/// Change the selected model after first run (AC-9: `teton model set <name>`).
///
/// A user-only action, like every config mutation (spec Permissions table) —
/// never inferable from model output or file content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSetParams {
    /// The catalog name to switch to.
    pub name: String,
    /// The BR-3 second confirmation, exactly as on
    /// [`ModelConfirmOutcome::Choose`]: required before an entry above this
    /// machine's RAM floor is accepted.
    #[serde(default)]
    pub confirmed_above_ram_floor: bool,
}

/// Result of [`ModelSetParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSetResult {
    /// The selection now in force.
    pub selection: ModelSelectionView,
}

impl RpcMethod for ModelSetParams {
    const METHOD: &'static str = "model/set";
    type Result = ModelSetResult;
}

/// Report the current selection and install state (AC-9: `teton model status`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatusParams {}

/// Result of [`ModelStatusParams`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatusResult {
    /// The recorded decision, or `None` when none has been made.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub selection: Option<ModelSelectionView>,
    /// Install state of the selected weights, or `None` when nothing is selected.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub install: Option<InstallStateView>,
    /// The proposal awaiting an answer, if one is outstanding — **the whole
    /// payload**, byte-for-byte what the [`ModelSelectionProposed`] event
    /// carried.
    ///
    /// This is what makes delivery independent of attach timing (REQ-547). The
    /// daemon publishes the proposal on its own task, possibly before it accepts
    /// its first connection, so an event-only design leaves a client that
    /// attached a moment later with no way to learn *which* entry was proposed —
    /// and BR-2 requires naming it, with its download size and RAM floor. A bare
    /// `request_id` would let such a client *answer* a prompt it could not
    /// *render*, which is consent in name only.
    ///
    /// It carries the `request_id` itself rather than duplicating it in a
    /// sibling field, so the id a client answers with and the proposal it
    /// rendered cannot disagree. A client that sees both this and the live event
    /// de-duplicates on that id and prompts exactly once.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pending_proposal: Option<ModelSelectionProposed>,
}

impl RpcMethod for ModelStatusParams {
    const METHOD: &'static str = "model/status";
    type Result = ModelStatusResult;
}

// ---------------------------------------------------------------------------
// config operations
// ---------------------------------------------------------------------------

/// A configured model provider (spec entity `ModelProvider`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider id.
    pub id: ProviderId,
    /// Provider family.
    pub kind: ProviderKind,
    /// Endpoint URL; required for remote kinds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub endpoint: Option<String>,
    /// Reference to an OS-keychain entry. NEVER a raw key or token (BR-7); the
    /// wire and config only carry the reference, the daemon resolves it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub auth_ref: Option<String>,
}

/// A single phase→provider routing rule (spec entity `RoutingPolicy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingRule {
    /// The phase this rule governs.
    pub phase: Phase,
    /// Provider selected for the phase.
    pub provider_id: ProviderId,
    /// Provider used on error/timeout of `provider_id`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
}

/// A privacy boundary over a path glob (spec entity `PrivacyBoundary`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyBoundaryConfig {
    /// Repo-relative glob the boundary applies to.
    pub path_glob: String,
    /// Enforcement mode.
    pub mode: PrivacyMode,
}

/// Read the daemon's current configuration snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigGetParams {}

/// The full, current configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// Registered providers.
    pub providers: Vec<ProviderConfig>,
    /// Routing policy table.
    pub routing: Vec<RoutingRule>,
    /// Privacy boundaries.
    pub privacy: Vec<PrivacyBoundaryConfig>,
}

/// Result of [`ConfigGetParams`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigGetResult {
    /// Current configuration.
    pub snapshot: ConfigSnapshot,
}

impl RpcMethod for ConfigGetParams {
    const METHOD: &'static str = "config/get";
    type Result = ConfigGetResult;
}

/// A single configuration mutation.
///
/// Applying any of these is a user-only action (interactive confirmation) and
/// is never driven by model output or file content (spec Permissions table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConfigUpdate {
    /// Register or replace a provider.
    RegisterProvider(ProviderConfig),
    /// Set the routing rule for a phase.
    SetRoutingRule(RoutingRule),
    /// Add or replace a privacy boundary.
    SetPrivacyBoundary(PrivacyBoundaryConfig),
}

/// Apply a configuration mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSetParams {
    /// The mutation to apply.
    pub update: ConfigUpdate,
}

/// Result of [`ConfigSetParams`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSetResult {
    /// True when the mutation was accepted and persisted.
    pub applied: bool,
}

impl RpcMethod for ConfigSetParams {
    const METHOD: &'static str = "config/set";
    type Result = ConfigSetResult;
}

// ---------------------------------------------------------------------------
// cost query
// ---------------------------------------------------------------------------

/// Query the daemon's authoritative cost ledger (BR-2).
///
/// The cost meter is derived only from recorded model calls; this method reads
/// the persisted ledger so a client (`teton cost`) can report authoritative
/// history rather than only what it happened to observe on the live event
/// stream. Teton differentiator — no ACP equivalent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostQueryParams {}

/// One roll-up group in a [`CostReportView`] (per phase or per provider).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostGroupView {
    /// Grouping key (phase wire-name, or provider id, or `none`/`unpriced`).
    pub key: String,
    /// Calls attributed to this group.
    pub calls: u64,
    /// Input tokens summed over the group.
    pub input_tokens: u64,
    /// Output tokens summed over the group.
    pub output_tokens: u64,
    /// Recorded spend for the group, in integer micro-USD (priced calls only).
    pub usd_micros: i64,
}

/// A serializable projection of the daemon's cost report (BR-2 / AC-4).
///
/// Mirrors the daemon's internal aggregation over the ledger, flattened to wire
/// types the CLI can render without a daemon dependency. `usd_micros` is integer
/// micro-USD so money never rounds on the wire; the savings figure is always an
/// estimate and carries its `methodology` verbatim.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostReportView {
    /// Total recorded spend across priced calls, in micro-USD.
    pub total_usd_micros: i64,
    /// Total recorded calls (priced and unpriced).
    pub total_calls: u64,
    /// Calls that were priced (had a matching price-table entry).
    pub priced_calls: u64,
    /// Calls with no price-table entry (never guessed a cost).
    pub unpriced_calls: u64,
    /// `baseline − actual`; the estimated saving vs. an all-frontier baseline.
    pub savings_usd_micros: i64,
    /// What the same token volume would cost at the baseline, in micro-USD.
    pub baseline_usd_micros: i64,
    /// The baseline comparator, as `provider/model`.
    pub baseline_model: String,
    /// The savings methodology, verbatim (never presented as a measurement).
    pub methodology: String,
    /// Per-phase roll-up, ordered by phase wire-name.
    pub per_phase: Vec<CostGroupView>,
    /// Per-provider roll-up, ordered by provider id.
    pub per_provider: Vec<CostGroupView>,
}

/// Result of [`CostQueryParams`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostQueryResult {
    /// The authoritative cost report.
    pub report: CostReportView,
}

impl RpcMethod for CostQueryParams {
    const METHOD: &'static str = "cost/query";
    type Result = CostQueryResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ChosenBand, GpuClass, TierBand};

    /// Serializes then deserializes `value`, asserting the round-trip is exact.
    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn session_create_round_trips() {
        round_trip(&SessionCreateParams {
            mode: SessionMode::Structured,
            phase: Some(Phase::Spec),
        });
        round_trip(&SessionCreateResult {
            session_id: SessionId::from("s1"),
        });
    }

    #[test]
    fn session_list_round_trips() {
        round_trip(&SessionListParams::default());
        round_trip(&SessionListResult {
            sessions: vec![SessionSummary {
                session_id: SessionId::from("s1"),
                mode: SessionMode::Freeform,
                phase: None,
                title: Some("hack".to_owned()),
            }],
        });
    }

    #[test]
    fn session_attach_round_trips() {
        round_trip(&SessionAttachParams {
            session_id: SessionId::from("s1"),
        });
        round_trip(&SessionAttachResult {
            session: SessionSummary {
                session_id: SessionId::from("s1"),
                mode: SessionMode::Structured,
                phase: Some(Phase::Implement),
                title: None,
            },
        });
    }

    #[test]
    fn prompt_turn_round_trips() {
        round_trip(&PromptTurnParams {
            session_id: SessionId::from("s1"),
            prompt: vec![
                PromptBlock::Text {
                    text: "hi".to_owned(),
                },
                PromptBlock::ResourceLink {
                    uri: "file:///a.rs".to_owned(),
                    name: None,
                },
            ],
        });
        round_trip(&PromptTurnResult {
            turn_id: TurnId::from("t1"),
            stop_reason: StopReason::EndTurn,
        });
    }

    #[test]
    fn permission_respond_round_trips() {
        round_trip(&PermissionRespondParams {
            request_id: RequestId::from("r1"),
            outcome: PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
            },
        });
        round_trip(&PermissionRespondParams {
            request_id: RequestId::from("r1"),
            outcome: PermissionOutcome::Cancelled,
        });
        round_trip(&PermissionRespondResult::default());
    }

    fn sample_probe() -> ProbeReportView {
        ProbeReportView {
            total_ram_bytes: 32 * 1024 * 1024 * 1024,
            free_disk_bytes: 200 * 1024 * 1024 * 1024,
            gpu_class: GpuClass::AppleSilicon,
            chosen_band: ChosenBand::Mid,
            reason: "32 GB of RAM clears the 7B band".to_owned(),
        }
    }

    fn sample_entry() -> CatalogEntryView {
        CatalogEntryView {
            name: "qwen2.5-coder-7b".to_owned(),
            band: TierBand::Mid,
            size_bytes: 4_700_000_000,
            ram_floor_bytes: 12_884_901_888,
            provenance: crate::events::CatalogProvenance {
                repo: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF".to_owned(),
                host: "huggingface.co".to_owned(),
                revision: "13fb94b".to_owned(),
            },
        }
    }

    fn sample_proposal() -> ModelSelectionProposed {
        ModelSelectionProposed {
            request_id: RequestId::from("m1"),
            probe: sample_probe(),
            proposed: Some(crate::events::ProposedModel {
                entry: sample_entry(),
                required_disk_bytes: 4_700_000_000 + 1_073_741_824,
            }),
            alternatives: vec![CatalogEntryView {
                name: "qwen2.5-coder-3b".to_owned(),
                band: TierBand::Small,
                size_bytes: 2_104_932_800,
                ram_floor_bytes: 8_589_934_592,
                provenance: crate::events::CatalogProvenance {
                    repo: "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF".to_owned(),
                    host: "huggingface.co".to_owned(),
                    revision: "f74adce".to_owned(),
                },
            }],
            fetch_notice: None,
        }
    }

    fn sample_selection() -> ModelSelectionView {
        ModelSelectionView {
            model_name: Some("qwen2.5-coder-7b".to_owned()),
            source: SelectionSource::Probe,
            declined_local: false,
            decided_at_ms: 1_771_200_000_000,
        }
    }

    #[test]
    fn model_confirm_round_trips_every_outcome() {
        for outcome in [
            ModelConfirmOutcome::Accept,
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-3b".to_owned(),
                confirmed_above_ram_floor: false,
            },
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-30b-a3b".to_owned(),
                confirmed_above_ram_floor: true,
            },
            ModelConfirmOutcome::Decline,
        ] {
            round_trip(&ModelConfirmParams {
                request_id: RequestId::from("m1"),
                outcome,
            });
        }
        round_trip(&ModelConfirmResult::default());
    }

    #[test]
    fn model_confirm_outcome_is_a_closed_enum() {
        // BR-1: nothing downloads without an explicit decision, so an outcome
        // this build does not know must be a typed error — never a silent
        // fallback to "accept". No `#[serde(other)]`, no `Default`.
        let json = r#"{"request_id": "m1", "outcome": {"outcome": "install_later"}}"#;
        let err = serde_json::from_str::<ModelConfirmParams>(json)
            .expect_err("an unknown outcome must not deserialize");
        let msg = err.to_string();
        assert!(msg.contains("unknown variant"), "message: {msg}");
        // The error names what *is* accepted, so the failure is actionable.
        for expected in ["accept", "choose", "decline"] {
            assert!(
                msg.contains(expected),
                "message should list `{expected}`: {msg}"
            );
        }

        // A missing outcome is likewise an error, not a default.
        serde_json::from_str::<ModelConfirmParams>(r#"{"request_id": "m1"}"#)
            .expect_err("a missing outcome must not default");
        // …and `choose` without a name cannot degrade into a bare accept.
        serde_json::from_str::<ModelConfirmParams>(
            r#"{"request_id": "m1", "outcome": {"outcome": "choose"}}"#,
        )
        .expect_err("`choose` without a name must not deserialize");
    }

    #[test]
    fn model_confirm_tolerates_unknown_fields_but_not_unknown_outcomes() {
        // The two forward-compat axes are deliberately different: an added field
        // is tolerated, an unrecognized decision is not.
        let json = r#"{
            "request_id": "m1",
            "outcome": {
                "outcome": "choose",
                "name": "qwen2.5-coder-3b",
                "future_knob": {"reason": "user preference"}
            },
            "future_top_level": true
        }"#;
        let parsed: ModelConfirmParams = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.outcome,
            ModelConfirmOutcome::Choose {
                name: "qwen2.5-coder-3b".to_owned(),
                // Absent on the wire ⇒ the *safe* value: not confirmed (BR-3).
                confirmed_above_ram_floor: false,
            }
        );
    }

    #[test]
    fn the_br3_second_confirmation_defaults_to_not_confirmed() {
        // An omitted confirmation must never read as "the user confirmed".
        let choose: ModelConfirmOutcome =
            serde_json::from_str(r#"{"outcome": "choose", "name": "big"}"#).unwrap();
        match choose {
            ModelConfirmOutcome::Choose {
                confirmed_above_ram_floor,
                ..
            } => assert!(!confirmed_above_ram_floor),
            other => panic!("expected choose, got {other:?}"),
        }
        let set: ModelSetParams = serde_json::from_str(r#"{"name":"big"}"#).unwrap();
        assert!(!set.confirmed_above_ram_floor);
    }

    #[test]
    fn model_list_round_trips() {
        round_trip(&ModelListParams::default());
        round_trip(&ModelListResult {
            probe: sample_probe(),
            models: vec![
                ModelListEntry {
                    entry: sample_entry(),
                    fits_ram: true,
                    fits_disk: true,
                },
                ModelListEntry {
                    entry: CatalogEntryView {
                        name: "qwen2.5-coder-30b-a3b".to_owned(),
                        band: TierBand::Large,
                        size_bytes: 18_000_000_000,
                        ram_floor_bytes: 51_539_607_552,
                        provenance: crate::events::CatalogProvenance {
                            repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".to_owned(),
                            host: "huggingface.co".to_owned(),
                            revision: "b17cb02".to_owned(),
                        },
                    },
                    fits_ram: false,
                    fits_disk: true,
                },
            ],
            selection: Some(sample_selection()),
        });
        // A first run has no selection yet; the field must vanish, not go null.
        let unselected = ModelListResult {
            probe: sample_probe(),
            models: vec![],
            selection: None,
        };
        round_trip(&unselected);
        assert!(!serde_json::to_string(&unselected)
            .unwrap()
            .contains("selection"));
    }

    #[test]
    fn model_set_round_trips() {
        round_trip(&ModelSetParams {
            name: "qwen2.5-coder-3b".to_owned(),
            confirmed_above_ram_floor: false,
        });
        round_trip(&ModelSetResult {
            selection: ModelSelectionView {
                model_name: Some("qwen2.5-coder-3b".to_owned()),
                source: SelectionSource::UserOverride,
                declined_local: false,
                decided_at_ms: 1_771_200_000_001,
            },
        });
    }

    #[test]
    fn model_status_round_trips() {
        round_trip(&ModelStatusParams::default());
        for status in [
            InstallStatus::Absent,
            InstallStatus::Partial,
            InstallStatus::Verified,
            InstallStatus::Corrupt,
        ] {
            round_trip(&ModelStatusResult {
                selection: Some(sample_selection()),
                install: Some(InstallStateView {
                    model_name: "qwen2.5-coder-7b".to_owned(),
                    status,
                }),
                pending_proposal: None,
            });
        }
        // A declined machine: a selection with no model and no install.
        round_trip(&ModelStatusResult {
            selection: Some(ModelSelectionView {
                model_name: None,
                source: SelectionSource::UserOverride,
                declined_local: true,
                decided_at_ms: 1_771_200_000_002,
            }),
            install: None,
            pending_proposal: None,
        });
        // A first run with a prompt still outstanding (BR-1): the *whole*
        // proposal rides the status, so a client that missed the event renders
        // the same named pick the event would have shown.
        let outstanding = ModelStatusResult {
            selection: None,
            install: None,
            pending_proposal: Some(sample_proposal()),
        };
        round_trip(&outstanding);
        let json = serde_json::to_string(&outstanding).unwrap();
        for named in [
            "qwen2.5-coder-7b",
            "size_bytes",
            "ram_floor_bytes",
            "required_disk_bytes",
        ] {
            assert!(
                json.contains(named),
                "status must name the proposal: {json}"
            );
        }
        // The empty status must be a bare object, not three nulls.
        assert_eq!(
            serde_json::to_string(&ModelStatusResult::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn model_results_never_carry_an_install_path() {
        // BR-11: no absolute filesystem path in any protocol payload. The install
        // path is CLI-local; `InstallStateView` has no field to smuggle it in.
        let status = ModelStatusResult {
            selection: Some(sample_selection()),
            install: Some(InstallStateView {
                model_name: "qwen2.5-coder-7b".to_owned(),
                status: InstallStatus::Verified,
            }),
            pending_proposal: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        for forbidden in ["path", "/Users/", "/home/", "url", "sha256"] {
            assert!(
                !json.contains(forbidden),
                "status leaked `{forbidden}`: {json}"
            );
        }
    }

    #[test]
    fn config_get_round_trips() {
        round_trip(&ConfigGetParams::default());
        round_trip(&ConfigGetResult {
            snapshot: ConfigSnapshot {
                providers: vec![ProviderConfig {
                    id: ProviderId::from("anthropic"),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com".to_owned()),
                    auth_ref: Some("keychain://teton/anthropic".to_owned()),
                }],
                routing: vec![RoutingRule {
                    phase: Phase::Architect,
                    provider_id: ProviderId::from("anthropic"),
                    fallback_id: Some(ProviderId::from("local")),
                }],
                privacy: vec![PrivacyBoundaryConfig {
                    path_glob: "secrets/**".to_owned(),
                    mode: PrivacyMode::LocalOnly,
                }],
            },
        });
    }

    #[test]
    fn config_set_round_trips_each_update_variant() {
        for update in [
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("deepseek"),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com".to_owned()),
                auth_ref: Some("keychain://teton/deepseek".to_owned()),
            }),
            ConfigUpdate::SetRoutingRule(RoutingRule {
                phase: Phase::Implement,
                provider_id: ProviderId::from("deepseek"),
                fallback_id: None,
            }),
            ConfigUpdate::SetPrivacyBoundary(PrivacyBoundaryConfig {
                path_glob: "*.env".to_owned(),
                mode: PrivacyMode::RedactThenRemote,
            }),
        ] {
            round_trip(&ConfigSetParams { update });
        }
        round_trip(&ConfigSetResult { applied: true });
    }

    #[test]
    fn cost_query_round_trips() {
        round_trip(&CostQueryParams::default());
        round_trip(&CostQueryResult {
            report: CostReportView {
                total_usd_micros: 48_100,
                total_calls: 3,
                priced_calls: 2,
                unpriced_calls: 1,
                savings_usd_micros: 500_000,
                baseline_usd_micros: 548_100,
                baseline_model: "anthropic/claude-opus-4".to_owned(),
                methodology: "Estimate, not a measurement.".to_owned(),
                per_phase: vec![CostGroupView {
                    key: "implement".to_owned(),
                    calls: 1,
                    input_tokens: 4_000,
                    output_tokens: 2_000,
                    usd_micros: 3_000,
                }],
                per_provider: vec![CostGroupView {
                    key: "deepseek".to_owned(),
                    calls: 1,
                    input_tokens: 4_000,
                    output_tokens: 2_000,
                    usd_micros: 3_000,
                }],
            },
        });
    }

    #[test]
    fn request_helper_fills_method_from_trait() {
        let req = request(Id::Number(1), SessionListParams::default());
        assert_eq!(req.method, "session/list");
        assert_eq!(SessionCreateParams::METHOD, "session/create");
        assert_eq!(PromptTurnParams::METHOD, "session/prompt");
        assert_eq!(ConfigSetParams::METHOD, "config/set");
        assert_eq!(CostQueryParams::METHOD, "cost/query");
        assert_eq!(ModelConfirmParams::METHOD, "model/confirm");
        assert_eq!(ModelListParams::METHOD, "model/list");
        assert_eq!(ModelSetParams::METHOD, "model/set");
        assert_eq!(ModelStatusParams::METHOD, "model/status");
        assert_eq!(
            request(Id::Number(2), ModelStatusParams::default()).method,
            "model/status"
        );
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compat() {
        // A future daemon adds fields this build has never seen; deserializing
        // must still succeed (forward compatibility).
        let json = r#"{
            "mode": "structured",
            "phase": "spec",
            "future_knob": true,
            "another": {"nested": 1}
        }"#;
        let parsed: SessionCreateParams = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.mode, SessionMode::Structured);
        assert_eq!(parsed.phase, Some(Phase::Spec));
    }
}
