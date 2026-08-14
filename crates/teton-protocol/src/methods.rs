//! Typed client→daemon methods.
//!
//! Each request type implements [`RpcMethod`], binding it to its wire method
//! name and its result type, so a caller cannot pair the wrong params, result,
//! or method string. Method names are slash-namespaced in the ACP style; where
//! ACP already names an equivalent call, an `ACP:` comment records it so the
//! future compatibility shim is a rename.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::effort::{EffortLevel, ResolvedEffort};
use crate::events::{
    CatalogEntryView, ModelSelectionProposed, ProbeReportView, SelectionSource, WebCapabilityState,
    WebTier,
};
use crate::jsonrpc::{Id, Request};
use crate::permissions::PermissionLevel;
use crate::{
    BindingSource, Category, ConfigurableCategory, Phase, PrivacyMode, ProviderId, ProviderKind,
    RequestId, SessionId, SessionMode, Tier, TierBindingSource, TurnId,
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
    /// The client's working directory — the repo this session's tools are
    /// jailed to (BUG-147). The daemon runs under launchd with cwd `/`, so
    /// without this every tool call ran against the filesystem root. Absolute
    /// path; when absent the daemon falls back to its own (env-derived) root.
    /// ACP equivalent: `session/new`'s `cwd`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<std::path::PathBuf>,
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
    /// The working directory this session's tools are jailed to (BUG-147);
    /// `None` on sessions created by clients that did not send one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cwd: Option<std::path::PathBuf>,
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

/// A user's answer to an `attach_consent_requested` event (REQ-569 BR-6,
/// ADR-E).
///
/// The counterpart of [`PermissionRespondParams`], and deliberately the same
/// shape: the daemon raises a prompt carrying a `request_id` and the deciding
/// client answers by that id while the daemon's reader loop stays free.
///
/// **Its own method, not a reuse of `permission/respond`** (ADR-E). Two
/// reasons: the permission registry is session-scoped by construction and an
/// attach request has no attachment yet, and `permission/respond` is a gated
/// method (REQ-569 BR-9) — routing consent through it would put the gate in
/// front of the thing that opens the gate.
///
/// **Who may send it is not "whoever received it".** The daemon offers the
/// prompt to a surface a user already owns — a connection attached to the
/// target session, or (only when nothing is attached to it) the requester
/// itself — and enforces that same rule when the answer comes back. A `monitor`
/// receives every session's events and may answer none of these: seeing a
/// prompt and deciding it are the two things this REQ separates
/// (LESSON-502).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachConsentParams {
    /// Correlates with the `attach_consent_requested` event's `request_id`.
    pub request_id: RequestId,
    /// The decision.
    pub outcome: AttachConsentOutcome,
}

/// The two answers to a consent prompt.
///
/// A **closed** enum with no catch-all and no `Default`, for
/// [`ModelConfirmOutcome`]'s reason and one stronger: an `outcome` this build
/// cannot read is a deserialization error the daemon returns as
/// [`crate::jsonrpc::error_code::INVALID_PARAMS`], never a silent fallback.
/// There is no safe default to fall back *to* — one direction mints a
/// credential and the other refuses one — so the only correct answer to an
/// unreadable decision is to refuse to read it. Timeout is not a variant here
/// because it is not something a client says: it is what the daemon does when
/// nobody says anything (BR-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttachConsentOutcome {
    /// Mint the grant that was asked for — and only that one, at only that
    /// scope.
    Granted,
    /// Refuse it. Mints nothing.
    Denied,
}

/// Result of [`AttachConsentParams`].
///
/// Carries no decision, like [`PermissionRespondResult`] — the authoritative
/// outcome is what the *requester* is told, and echoing it here would put the
/// record in two places that could disagree. It carries the one fact the
/// answering client cannot otherwise learn: whether its answer arrived in time
/// to be the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachConsentResult {
    /// Whether a request was still waiting on this `request_id`.
    ///
    /// `false` means the window had already closed (or the request was already
    /// answered): the requester has been — or is about to be — refused, and a
    /// client that reported success regardless would tell a user they let
    /// someone in when the daemon let nobody in.
    pub resolved: bool,
}

impl RpcMethod for AttachConsentParams {
    const METHOD: &'static str = "attach/consent";
    type Result = AttachConsentResult;
}

/// Empty a session's retained conversation (REQ-567 BR-8, architecture D-2).
///
/// No ACP equivalent — ACP has no clear, and a bespoke addition is ADR-002's
/// expected shape. It exists because carry makes the conversation *daemon*
/// state: a client-local `/clear` would be a lie, since the next
/// `session/prompt` would still be seeded from the daemon's copy.
///
/// **User-only, by construction** (BR-8). The method is reachable from client
/// RPC dispatch and from nowhere else: no tool in the registry wraps it, so a
/// model that emits a tool call named `session/clear` finds no such tool — the
/// same channel argument [`WebOverrideParams`] rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClearParams {
    /// The session whose conversation is dropped.
    pub session_id: SessionId,
}

/// Result of [`SessionClearParams`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClearResult {
    /// How many retained blocks the clear dropped; `0` when the session had
    /// nothing to say yet.
    ///
    /// Reported rather than acknowledged silently: without it a user cannot
    /// tell "cleared a long conversation" from "cleared one that was already
    /// empty", and those are the two things they are most likely to want to
    /// know. It counts *retained blocks*, never tokens — the conversation's
    /// unit of storage, and the only one the daemon can state exactly.
    pub blocks_dropped: u64,
}

impl RpcMethod for SessionClearParams {
    const METHOD: &'static str = "session/clear";
    type Result = SessionClearResult;
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
/// Carries no *decision*, like [`PermissionRespondResult`]: the authoritative
/// outcome reaches *every* attached client as a `model_selection_decided` event,
/// so echoing it here would duplicate the record in two places that could
/// disagree. It carries one fact the event cannot: whether **this** answer is
/// what decided anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfirmResult {
    /// Whether this answer reached a proposal that was still awaiting one.
    ///
    /// `false` means no waiter held this `request_id`: the proposal had already
    /// been answered, or an explicit `model/set` superseded and cancelled it. A
    /// client that reported success regardless would tell a user their answer
    /// landed when a different decision is on record — so the daemon says which
    /// it was and the client can point at the `model_selection_decided` event
    /// that names the cancelled `request_id`.
    ///
    /// Defaults to `true` when absent, so a payload from a daemon that predates
    /// this field reads as the success it used to mean unconditionally.
    #[serde(default = "delivered_by_default")]
    pub delivered: bool,
}

/// A `model/confirm` result with no `delivered` field means "delivered".
fn delivered_by_default() -> bool {
    true
}

impl Default for ModelConfirmResult {
    fn default() -> Self {
        Self { delivered: true }
    }
}

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
    /// The model this provider calls (REQ-557 BR-1) — the declared routing
    /// identity, never derived from the price table or the provider id.
    ///
    /// `Option` because a client attached to a daemon mid-migration may
    /// legitimately see a provider whose model is not resolved yet (REQ-557
    /// ADR-C/ADR-E), and because local providers carry none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Reference to an OS-keychain entry. NEVER a raw key or token (BR-7); the
    /// wire and config only carry the reference, the daemon resolves it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub auth_ref: Option<String>,
}

// `RoutingRule` and `ConfigUpdate::SetRoutingRule` are **gone** (REQ-558 AC-9).
// The protocol carries no phase-keyed routing type at all now: a category is the
// dispatch key, lifecycle phase is a cost-attribution fact, and the two are not
// the same axis. `TierBindingConfig` and `CategoryBindingConfig` below are what
// replaced them.

/// Bind a routing tier to a provider — the primary configuration surface
/// (REQ-558 ADR-H, `teton policy set-tier`).
///
/// Four of these configure every category, because a category inherits its
/// tier's binding unless a [`CategoryBindingConfig`] overrides it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierBindingConfig {
    /// The tier this row binds.
    pub tier: Tier,
    /// Provider the tier routes to.
    pub provider_id: ProviderId,
    /// Provider used when the primary is unavailable or not routable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
}

/// Override one category's binding (`teton policy set-category`).
///
/// `name` is a [`ConfigurableCategory`], so a binding for `route` or `redact` is
/// not a request the daemon rejects — it is a request that cannot be encoded
/// (ADR-B).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryBindingConfig {
    /// The category this row binds.
    pub name: ConfigurableCategory,
    /// Provider this category routes to, ahead of its tier's binding.
    pub provider_id: ProviderId,
    /// Provider used when the primary is unavailable or not routable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
}

/// One tier row of `teton policy show`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierRouteView {
    /// The tier.
    pub tier: Tier,
    /// The provider it routes to, or `None` when it has none and inherits none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider_id: Option<ProviderId>,
    /// Its configured fallback, when the row carries one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
    /// Whether the provider is the configured one or an inherited fill.
    pub source: TierBindingSource,
}

/// What kind of content a category sends to its model (REQ-561 BR-11, AC-16).
///
/// A fixed descriptor per category, not a runtime choice: what a category is
/// *for* determines what its call must carry, so the answer is the same on every
/// machine and under every binding. [`ContentClass::for_category`] is the whole
/// definition, and it is total over all eleven categories — a category with no
/// call site is described, not omitted, because the question a reader is asking
/// ("what would leave this machine if I bound that tier remotely?") has an
/// answer before the call site exists.
///
/// The point (REQ-561 OQ-4) is that the `scan` tier carries both
/// [`Category::Triage`] and [`Category::Compact`], and those disclose
/// **different** classes: a user who binds `scan` to a remote provider for cheap
/// long-context summarisation also moves conversation history off the machine.
/// Re-splitting the binding is REQ-558's decision and out of scope, so
/// legibility is the mitigation.
///
/// **Disclosure, not enforcement.** Nothing here refuses anything. BR-7's
/// per-content egress scoping is what keeps boundary content local, and a
/// `local-only` source is refused whatever this says. A reader who takes a class
/// named here as a control has read it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    /// The user's own prompt text.
    UserPrompt,
    /// Contents of files in the workspace — the text around a search hit —
    /// **together with the request and the search terms that produced them**.
    ///
    /// The second half is not decoration. `triage`'s prompt carries the user's
    /// own request and the search description alongside the match lines, so a
    /// row disclosing only "file content" understates what leaves the machine
    /// by the one thing a user is most likely to care about. BR-11's whole
    /// mitigation is that this line is accurate.
    FileContent,
    /// Blocks of the session's own conversation history.
    ConversationHistory,
    /// Output a tool produced and the harness took into context.
    ToolOutput,
    /// **The shell command the harness ran** and the stdout and stderr it
    /// produced.
    ///
    /// The command string travels with the output — `shell`'s prompt cannot ask
    /// what a result means without saying what was run — and a command line is
    /// routinely the more revealing of the two (paths, hostnames, branch and
    /// ticket names). A row disclosing only "command output" understates it.
    CommandOutput,
    /// The assembled turn — the prompt, the conversation, and whatever file and
    /// tool content was gathered into it.
    TurnContext,
    /// A payload already assembled for an outbound call, inspected before it
    /// leaves.
    OutboundPayload,
}

impl ContentClass {
    /// The class of content `category` sends to its model.
    ///
    /// Exhaustive on purpose, in the same spirit as `has_call_site`: a twelfth
    /// category cannot be added without stating what it transmits.
    ///
    /// For a category with no call site the class describes what it *would*
    /// carry once built — [`Category::Redact`]'s is REQ-562's — which is a
    /// description of intent, not evidence of a call site. What a category
    /// transmits *today* is [`CategoryRouteView::reached`]'s answer, and the two
    /// are read together.
    #[must_use]
    pub const fn for_category(category: Category) -> Self {
        match category {
            // `route` classifies the prompt just typed; `title` names the
            // session from its first one. Both carry prompt text and nothing
            // else, which is why they share a class despite sharing no purpose.
            Category::Route | Category::Title => ContentClass::UserPrompt,
            // Screens an outbound payload for secrets before it leaves. Since
            // REQ-562 that scan is a real model call at the egress choke point,
            // and this class is what it sees: the exact bytes on their way out.
            Category::Redact => ContentClass::OutboundPayload,
            // Summarises a tool result on its way into context.
            Category::Digest => ContentClass::ToolOutput,
            // Decides which conversation blocks to forget, so it reads them.
            Category::Compact => ContentClass::ConversationHistory,
            // Ranks grep and glob hits, which are file text — and cannot rank
            // them without being told what the user asked for and what was
            // searched for, both of which ride in the same prompt.
            Category::Triage => ContentClass::FileContent,
            // Interprets what a command printed, which means it is also told
            // what the command was.
            Category::Shell => ContentClass::CommandOutput,
            // Turn completion: all four arrive at the same call and carry the
            // same thing — everything the turn has assembled.
            Category::Edit | Category::Design | Category::Debug | Category::Review => {
                ContentClass::TurnContext
            }
        }
    }

    /// The lowercase wire name — identical to the serde form, as with
    /// [`Category::as_str`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ContentClass::UserPrompt => "user_prompt",
            ContentClass::FileContent => "file_content",
            ContentClass::ConversationHistory => "conversation_history",
            ContentClass::ToolOutput => "tool_output",
            ContentClass::CommandOutput => "command_output",
            ContentClass::TurnContext => "turn_context",
            ContentClass::OutboundPayload => "outbound_payload",
        }
    }

    /// The phrase a human reads in `teton policy show` (AC-16).
    ///
    /// Separate from [`Self::as_str`] because the two answer different
    /// questions: `as_str` is the wire spelling a client parses, this is the
    /// sentence fragment it prints. It lives here so the daemon and every client
    /// disclose one wording rather than each inventing its own.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            ContentClass::UserPrompt => "your prompt",
            ContentClass::FileContent => "file content and your request",
            ContentClass::ConversationHistory => "conversation history",
            ContentClass::ToolOutput => "tool output",
            ContentClass::CommandOutput => "the command and its output",
            ContentClass::TurnContext => "the whole turn",
            ContentClass::OutboundPayload => "outbound payloads",
        }
    }
}

impl std::fmt::Display for ContentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One category row of `teton policy show` — the effective routing state of a
/// single category, **as the daemon's own resolver answers it**.
///
/// Every routing field here is read off
/// `teton_core::category::CategoryResolution`, the same value `route_decided` is
/// built from (BR-6, AC-11). Nothing in this struct is recomputed by the surface
/// that renders it, which is the point: the table a human reads and the event a
/// turn emits describe one routing state, so they must not be able to disagree.
///
/// Two fields are not routing state and say so where they are declared:
/// [`Self::reached`] is a fact about the daemon's call sites, and
/// [`Self::content_class`] is a fact about what the category is for. Both are
/// still answered by the daemon rather than by the renderer, for the same reason
/// the routing fields are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryRouteView {
    /// The category.
    pub category: Category,
    /// The tier it inherits from — a compile-time property, populated even when
    /// nothing is bound.
    pub tier: Tier,
    /// The provider it resolves to, or `None` when it cannot be routed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider_id: Option<ProviderId>,
    /// The fallback that would serve a mid-turn failure, already screened.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_id: Option<ProviderId>,
    /// Override, tier inheritance, pinned, or unbound.
    pub source: BindingSource,
    /// Whether any model call site in the harness dispatches on this category
    /// yet (REQ-558 ADR-A).
    ///
    /// `false` is rendered as `declared, no call site yet`. The schema is
    /// complete for all eleven categories so the remaining call sites can be
    /// tagged without another config migration — but a knob that silently does
    /// nothing invites a user to tune it, so the table says which ones those
    /// are. The daemon derives this from its own call sites; it is not a
    /// configured value.
    ///
    /// Defaulted rather than required, and `false` is the *accurate* default
    /// rather than a placeholder: a daemon old enough to omit this field
    /// predates REQ-558, and before REQ-558 no model call site dispatched on a
    /// category at all. See [`Self::content_class`] for why an additive field on
    /// this struct carries a default in the first place.
    #[serde(default)]
    pub reached: bool,
    /// What kind of content this category sends to its model (BR-11, AC-16).
    ///
    /// Populated for all eleven categories from [`ContentClass::for_category`],
    /// including the ones with no call site: a blank cell would read as "this
    /// one is safe", which is the opposite of what an unbuilt call site means.
    ///
    /// Read it with [`Self::reached`]. The class says what kind of content the
    /// call carries; `reached` says whether any call is made today. A category
    /// that transmits nothing today is the pair `reached: false` plus its
    /// declared class — it says so, rather than going absent from the table.
    ///
    /// It is on the wire rather than derived by each renderer for the reason the
    /// struct's own doc gives: the daemon answers, the surface prints. The
    /// TypeScript mirror (ADR-002) gets the same answer without re-deriving a
    /// table that could drift from this one.
    ///
    /// ## Why it has a default (mixed-version skew)
    ///
    /// The socket and lock filenames are stable by ADR-007, so a newly-installed
    /// CLI can find an already-running older daemon, and the handshake accepts it
    /// whenever the protocol ranges overlap. A *required* field therefore turns a
    /// merely-older daemon into a raw `missing field` serde error out of
    /// `Connection::call` — a failure mode with no sentence in it. Every other
    /// field added to this wire since has carried a `default` for the same
    /// reason; this one was the exception.
    ///
    /// The default is the **widest** class rather than a guess at the right one:
    /// a serde field default cannot read the sibling `category`, and for a
    /// disclosure the only safe direction to be wrong in is "more". A reader who
    /// sees every row claiming the whole turn is looking at a daemon that
    /// predates the field, not at a routing change — the daemon's silence, not
    /// its answer.
    ///
    /// This fixes the field, and only the field. It does **not** make
    /// `config/get` work against the last released daemon (v0.1.10): that
    /// snapshot's `routing` is a phase-keyed table of a different shape
    /// entirely, so it fails on `category` long before reaching here. That break
    /// is REQ-558's and wants a protocol-version decision, not a serde default.
    #[serde(default = "undisclosed_content_class")]
    pub content_class: ContentClass,
    /// The resolver's sentence naming the signal that fired, verbatim.
    pub reason: String,
}

/// The class assumed for a [`CategoryRouteView`] whose daemon did not send one.
///
/// The widest of the seven, deliberately. A missing disclosure is a daemon that
/// predates the field, and the failure a reader can be hurt by is a row that
/// claims *less* content leaves than does — so an absent answer reads as the
/// most, never the least. It is not a claim about the category; it is the
/// absence of one.
fn undisclosed_content_class() -> ContentClass {
    ContentClass::TurnContext
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
    /// The four tier rows — the primary configuration surface.
    pub tiers: Vec<TierRouteView>,
    /// The effective routing state of every category, resolver-answered.
    ///
    /// Named `routing` because that is what it is; it replaced a phase-keyed
    /// table of the same name (AC-9). One row per [`Category`], all eleven,
    /// including the two that are pinned and the ones with no call site yet.
    pub routing: Vec<CategoryRouteView>,
    /// The category a freeform turn takes when classification is bypassed or
    /// fails (BR-9, AC-12).
    ///
    /// `Option` only so the field is additive on the wire: a daemon that sends
    /// it always sends one of the four judgment categories. It is here, and not
    /// only in `teton policy show`, because AC-12 asks for the declared default
    /// to be *configuration-visible* — a value a client can read is the
    /// difference between a declared default and a hidden constant.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub judgment_default: Option<Category>,
    /// Privacy boundaries.
    pub privacy: Vec<PrivacyBoundaryConfig>,
    /// Whether the REQ-562 redaction scan runs inside the egress choke point —
    /// the `[privacy] redact` opt-in, on the wire so a client can *report* it.
    ///
    /// ## Why it is not called `privacy`
    ///
    /// That name is taken, by the boundary list directly above, and the two are
    /// different things: a set of path globs, and a single switch over outbound
    /// payloads. Folding the switch into the boundary table's name is the
    /// collision REQ-562 TASK-067 recorded when it deliberately left the opt-in
    /// off the wire; this is that note being acted on rather than re-discovered.
    ///
    /// ## Visibility, not control
    ///
    /// There is no [`ConfigUpdate`] variant for it and none is coming from this
    /// REQ (the spec's *"no new RPCs"*): the switch is set in the config file
    /// and merely read here. `teton policy show` renders it, because a `redact`
    /// row that says what the category *would* send leaves a user with no way
    /// to tell whether anything is scanning today.
    ///
    /// ## Additive with a default, like every field added to this wire since
    ///
    /// A snapshot from a daemon predating this field carries no key, and reads
    /// `false` — the historical fact rather than a filler value, since no
    /// **released** daemon predating it ran the scan. (The claim is narrowed to
    /// released builds on purpose: within REQ-562's own branch there were
    /// intermediate builds that ran the scan before the field was added to this
    /// wire, so "no such daemon ran the scan" is false of them. They shipped to
    /// nobody, no client will ever read a snapshot one of them wrote, and the
    /// default is the safe direction anyway — but a claim that is true of the
    /// world and false of the repository's own history is the kind that gets
    /// quoted back at a later reader.) A client predating the field ignores a
    /// key it does not know. So the addition moves neither
    /// [`crate::PROTOCOL_VERSION`] nor
    /// [`crate::PROTOCOL_VERSION_MIN`], exactly as `PrivacyBlock::cause` did
    /// not (REQ-562 ADR-7); both directions are asserted against literal JSON
    /// in this module's tests rather than left as a claim.
    ///
    /// `false` is also the only safe direction for an absent answer to fall in:
    /// a reader that assumed *enabled* from a daemon's silence would tell a
    /// user their outbound payloads are scanned when nothing is scanning them.
    #[serde(default)]
    pub redact_enabled: bool,
    /// The global reasoning-effort setting and what it resolves to per provider
    /// (REQ-559 BR-9).
    ///
    /// Carried on the existing snapshot rather than behind a new RPC — the spec
    /// adds none — so `teton effort`, `/effort` and (REQ-560) the status line
    /// all read one answer the daemon computed with the **same** function the
    /// router calls. Two surfaces describing one setting must not be able to
    /// drift (LESSON-456, REQ-555 BR-4).
    ///
    /// `Option` for wire additivity only: a daemon that has this field always
    /// populates it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub effort: Option<EffortView>,
    /// What the web capability can actually do (REQ-572 BR-3, BR-10).
    ///
    /// On the existing snapshot rather than behind a new RPC, for
    /// [`Self::effort`]'s reason: the status surface needs to say "web lookup
    /// is available but off" in the same breath it says everything else, and a
    /// second round-trip is a second answer that can arrive from a different
    /// moment than the first.
    ///
    /// A **typed** state, not a sentence (BR-10): a client renders guidance by
    /// branching on the variant, and prose it would have to re-parse is the
    /// second classifier LESSON-456 warns about.
    ///
    /// `Option` for wire additivity only, like the two fields above — a daemon
    /// that has this field always populates it, and `None` therefore reads as
    /// "this daemon predates the field", never as a state. That is why the
    /// absent case is not folded into [`WebCapabilityState::OffAvailable`]:
    /// "off, and one table away" is a claim about the user's config, and an
    /// older daemon's silence is not evidence for it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub web_capability: Option<WebCapabilityState>,
}

/// The global effort setting, plus what it resolves to for each registered
/// provider (REQ-559 BR-9, AC-8).
///
/// Every row is produced by `teton_core::effort::resolve_effort` — the same
/// function the router calls per model call — so a row here cannot describe a
/// provider differently from the request that actually goes to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortView {
    /// The level the user set (or the declared default). This is the
    /// **pre-clamp** request; each row below says what that becomes.
    pub level: EffortLevel,
    /// One row per registered provider, in configuration order.
    pub providers: Vec<ProviderEffortView>,
}

/// What the global effort resolves to for one provider (REQ-559 BR-9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEffortView {
    /// The provider this row describes.
    pub provider_id: ProviderId,
    /// What its requests actually carry — already clamped, and carrying the
    /// reason when nothing is sent (BR-6).
    pub resolved: ResolvedEffort,
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
    /// Bind a routing tier to a provider (`teton policy set-tier`).
    SetTierBinding(TierBindingConfig),
    /// Override one category's binding (`teton policy set-category`).
    SetCategoryBinding(CategoryBindingConfig),
    /// Add or replace a privacy boundary.
    SetPrivacyBoundary(PrivacyBoundaryConfig),
    /// Set the global reasoning-effort level (REQ-559 BR-8, `teton effort <level>`
    /// / `/effort <level>`).
    ///
    /// **Persisted**, unlike REQ-560's session-scoped permission level. The
    /// asymmetry is deliberate: an effort level that survives a restart costs
    /// money predictably, while a permission level that survives one removes a
    /// guardrail invisibly.
    ///
    /// A new `ConfigUpdate` variant, not a new RPC — `config/set` already
    /// carries every configuration mutation.
    SetEffort(EffortLevel),
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
    /// Reasoning tokens summed over the calls that **reported** a split, or
    /// `None` when none did (REQ-559 BR-11).
    ///
    /// A **subset** of the output tokens, never added to them. `None` renders
    /// as "unreported": a `0` standing in for "the provider didn't tell us" is
    /// displaying an estimate as an actual, which REQ-544 BR-2 forbids.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_tokens: Option<u64>,
    /// How many calls reported a reasoning split, so a partial figure can say
    /// so rather than reading as a whole-ledger total.
    #[serde(default)]
    pub calls_reporting_reasoning: u64,
    /// The models behind [`Self::unpriced_calls`], by name, deduplicated and
    /// ordered (REQ-557 BR-9 / AC-7b).
    ///
    /// A client can name what needs a price entry instead of only reporting that
    /// something went uncosted. Empty whenever `unpriced_calls` is 0.
    #[serde(default)]
    pub unpriced_models: Vec<String>,
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
    /// Per-session web-lookup roll-up, ordered by session id (REQ-563 BR-7 /
    /// AC-6).
    ///
    /// A separate list rather than columns on [`CostGroupView`], mirroring the
    /// separate ledger table it comes from: a lookup has no tokens and no cost
    /// to add to a call's, and "calls" and "lookups" are different counts a
    /// reader must not see summed.
    ///
    /// `#[serde(default)]`, like [`Self::unpriced_models`]: a daemon built
    /// before REQ-563 sends no such key, and a client reading one must get an
    /// empty roll-up rather than a deserialization failure.
    #[serde(default)]
    pub web_per_session: Vec<WebTotalsView>,
}

/// One session's web-lookup totals inside a [`CostReportView`] (REQ-563 AC-6).
///
/// Carries no host and no URL. The per-lookup destination is on the
/// [`crate::events::WebLookup`] event and in the ledger row; a roll-up's job is
/// how many and how much, and adding a destination list here would put an
/// outgoing-utterance trace in the one surface a user is most likely to paste
/// into a bug report (BR-7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebTotalsView {
    /// The session id.
    pub key: String,
    /// Lookups this session performed, whatever their outcome — blocked,
    /// refused, and cache-served ones included (BR-7: every lookup is counted,
    /// including the free ones).
    pub lookups: u64,
    /// Bytes those lookups brought back. `0` from every ending that transferred
    /// nothing, so this is content received and not traffic attempted.
    pub bytes_in: u64,
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

// ---------------------------------------------------------------------------
// web lookup control (REQ-563)
// ---------------------------------------------------------------------------
//
// Two user-only actions on a session's web capability. Both are **client** RPCs
// rather than harness tools, and that placement is the enforcement rather than a
// convention: tool dispatch and the client socket are structurally distinct
// channels, so a model that emits a tool call named `web/override` reaches
// nothing at all (architecture D-4, AC-12). There is no check to forget.
//
// Types only — the handlers, the session flag, and the cache eviction they drive
// land with the daemon's web module.

/// Lift a session's web taint restriction (BR-13 / AC-12).
///
/// Restores model-composed lookups at the tiers this session was **already**
/// granted: it grants nothing new, is never written to config, and resets with
/// the session. Surfaced as a command the user types, never as a tool.
///
/// It carries a `session_id` even though the restriction is "this session's":
/// the flag is session-scoped state and the daemon holds many sessions, so the
/// call has to name the one it means. Architecture D-4 calls the RPC
/// parameterless in the sense that there is nothing to *choose* — no scope, no
/// tier, no degree — only a session to name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebOverrideParams {
    /// The session whose restriction is lifted.
    pub session_id: SessionId,
}

/// Result of [`WebOverrideParams`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebOverrideResult {
    /// Whether a taint restriction was actually in force when the override
    /// arrived.
    ///
    /// Not redundant with an empty [`Self::tiers_restored`], and the difference
    /// is user-visible: a restricted session holding no grants restores nothing,
    /// and a session that was never restricted also restores nothing. A client
    /// that could not tell those apart would confirm a lift that never happened,
    /// so the daemon says which it was and the CLI can answer "nothing was
    /// restricted" instead of a false confirmation.
    pub was_restricted: bool,
    /// The tiers model-composed lookups resume at — the same list the
    /// [`crate::events::WebTaintOverridden`] event carries, ascending, and never
    /// including [`WebTier::Off`].
    pub tiers_restored: Vec<WebTier>,
}

impl RpcMethod for WebOverrideParams {
    const METHOD: &'static str = "web/override";
    type Result = WebOverrideResult;
}

/// Read or set a session's permission level (REQ-560, ADR-D).
///
/// One method for both because they are one question asked two ways, and because
/// a set that did not return the resulting level would let a client's rendered
/// status row drift from the daemon's actual posture. `level: None` reads;
/// `Some(l)` sets and reads back.
///
/// Like [`WebOverrideParams`], this is a **client** RPC and never a harness
/// tool, and that placement is the enforcement rather than a convention: tool
/// dispatch and the client socket are structurally distinct channels, so a model
/// that emits a tool call named `session/permissions` — or tool output
/// containing the text `/permissions full` — reaches nothing at all. Permission
/// posture is not inferable from model output, tool output, or file content;
/// only the session user can change it, by typing.
///
/// It carries a `session_id` because the level is session-scoped state and the
/// daemon holds many sessions. A second client attached to the same session sees
/// a level set by the first — the level lives on the daemon's per-session gate,
/// which is the surface-parity rule (REQ-544 BR-4) working as intended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionsParams {
    /// The session whose level is being read or set.
    pub session_id: SessionId,
    /// The level to set, or `None` to read the current one without changing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<PermissionLevel>,
}

/// Result of [`SessionPermissionsParams`] — always the level now in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionsResult {
    /// The session's permission level after this call.
    pub level: PermissionLevel,
    /// Whether this call changed it.
    ///
    /// A read is never a change, and setting the level a session already holds
    /// is not one either. The distinction keeps a confirmation honest — the same
    /// reason [`WebOverrideResult::was_restricted`] exists — so the CLI can say
    /// "already at full" instead of announcing a change that did not happen.
    pub changed: bool,
}

impl RpcMethod for SessionPermissionsParams {
    const METHOD: &'static str = "session/permissions";
    type Result = SessionPermissionsResult;
}

/// Evict a cached document so the next lookup of that URL re-fetches (BR-12's
/// explicit-refresh clause, AC-10).
///
/// This type holds a full `url` where the events and the ledger may hold only a
/// host (BR-7), and the asymmetry is the rule working rather than an exception
/// to it: BR-7 constrains what the daemon **records and broadcasts**, and this
/// is the user's own typed argument travelling client→daemon on the way in. It
/// is never echoed back — [`WebRefreshResult`] answers with an outcome alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebRefreshParams {
    /// The URL whose cached document is evicted.
    pub url: String,
}

/// What a refresh found in the cache.
///
/// A **closed** enum with no catch-all, like [`ModelConfirmOutcome`]: an outcome
/// this build does not know is a deserialization error rather than a silent
/// reading of one of these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRefreshOutcome {
    /// A cached document was present and has been removed.
    Evicted,
    /// Nothing was cached for that URL.
    ///
    /// A fact, not a failure: an uncached URL is already going to be fetched
    /// fresh, so the user got what they asked for. The two are still separate
    /// values because "there was a stale copy and it is gone" and "there was
    /// never a copy" are different answers to *why* the next fetch is live.
    Absent,
}

/// Result of [`WebRefreshParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebRefreshResult {
    /// What the refresh found. Carries no URL back: the client already knows
    /// what it asked about, and the daemon has no reason to repeat it (BR-7).
    pub outcome: WebRefreshOutcome,
}

impl RpcMethod for WebRefreshParams {
    const METHOD: &'static str = "web/refresh";
    type Result = WebRefreshResult;
}

// ---------------------------------------------------------------------------
// guided web setup (REQ-572)
// ---------------------------------------------------------------------------
//
// Three stateless endpoints — plan, preview, commit — and **no flow state
// anywhere on this wire** (architecture ADR-1). The client collects answers
// locally, which is what every client already does with a prompt line, and the
// daemon stays the sole authority on validation, on what the preview says, and
// on the commit. There is no flow id here because there is no flow to name: the
// pending-prompt registries we have shipped each grew a cross-session or
// bystander-answer bug (BUG-161, BUG-162), and the cheapest such surface is the
// one that does not exist.
//
// Like `web/override`, these are **client** RPCs and never harness tools, and
// that placement is BR-4's enforcement rather than a convention: tool dispatch
// and the client socket are structurally distinct channels, so a model emitting
// a tool call named `web/setup_commit` reaches nothing at all. The connection
// gate (attachment) is the second leg, and a caller that fails it is answered
// with `NOT_ATTACHED` and announced with a `web_setup_rejected` event.
//
// Types only — the derivation, the validator, the atomic write and the config
// swap land with the daemon (TASK-129/130), and the walkthrough with the CLI
// (TASK-132).

/// What the `[web]` table says today, for the flow to show before it changes it
/// (REQ-572 AC-7).
///
/// Deliberately **not** the whole table: this is the summary a user needs to
/// answer "what am I about to replace", and every field here is already
/// non-secret by construction — a tier, a host, and two references. The search
/// endpoint appears as a **host** (BR-7 of REQ-563, the rule the whole web
/// event family follows) and the key appears as the reference the config holds,
/// never the value the keychain holds (BR-6).
///
/// No `Default`, deliberately: [`WebTier`] on this wire has none either, and a
/// summary that could be built out of nothing is one a daemon could send in
/// place of the `None` that means "there is no `[web]` table" — the exact
/// distinction [`WebSetupPlanResult::current_web`] exists to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebTableSummary {
    /// The configured ceiling. [`WebTier::Off`] is a legitimate value here —
    /// a `[web]` table that names `off` is the state
    /// [`WebCapabilityState::OffAvailable`] describes, written down.
    pub tier: WebTier,
    /// The configured search backend's **host**, from the executor's own parse
    /// of the endpoint (BR-9, LESSON-494) — never the full endpoint, its path,
    /// or its query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_host: Option<String>,
    /// The configured key **reference**, e.g. `keychain://teton/web-search`.
    /// A name, not a secret: the value it points at never crosses this socket
    /// in either direction (BR-6, ADR-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_key_ref: Option<String>,
    /// The configured auth-header template, e.g. `X-Subscription-Token: {key}`
    /// (BUG-165). Carries no credential by construction — `{key}` is where one
    /// would go, and the substitution happens only when the request is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_auth: Option<String>,
}

/// One search backend the setup flow can suggest, as **data**: the shapes a
/// user would otherwise have to know by heart, and never a secret (REQ-573
/// BR-6).
///
/// The daemon owns the list (REQ-573 ADR-A) so a backend's endpoint and header
/// shape are written down in exactly one place; a client renders what it was
/// handed rather than keeping a second copy that drifts from the first (BR-1).
///
/// No `Default`, for [`WebTableSummary`]'s reason: a suggestion built out of
/// nothing is one a client could show as if a daemon had sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebBackendSuggestion {
    /// Stable identifier, e.g. `searxng` — what callers and tests key on, and
    /// deliberately not display text, which may be reworded.
    pub id: String,
    /// The name to show a user.
    pub label: String,
    /// An absolute example endpoint including whatever query the backend
    /// requires — the string a user can paste as typed.
    pub endpoint: String,
    /// The host this suggestion answers for, so a typed endpoint can be
    /// matched back to it (BR-8). `None` for a self-hosted backend, whose host
    /// is wherever the user runs it, and the field is then absent from the
    /// wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The auth-header template this backend wants, e.g.
    /// `X-Subscription-Token: {key}` (BUG-165). Present exactly when
    /// [`Self::needs_key`] is set, absent from the wire otherwise, and
    /// carrying no credential by construction — `{key}` is where one would go,
    /// and the substitution happens only when the request is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_template: Option<String>,
    /// Whether this backend needs a key at all — the answer the flow defaults
    /// to, rather than a client's guess re-derived from the template's
    /// presence.
    pub needs_key: bool,
    /// A sentence to show beside the suggestion, when there is one worth
    /// showing. `None` is the common case and absent from the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// The backends this daemon suggests, plus the header shape to offer for the
/// ones it does not know (REQ-573 BR-1).
///
/// Sent whole rather than piecemeal so the fallback travels with the list it
/// falls back from: a client that matched no [`WebBackendSuggestion::host`]
/// still has a template to offer without declaring one of its own.
///
/// No `Default`: an empty catalog manufactured client-side would be
/// indistinguishable from one a daemon sent, and the `None` on
/// [`WebSetupPlanResult::suggestion_catalog`] is the "this daemon predates the
/// catalog" fact the degraded path keys off (BR-3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupCatalog {
    /// The generic header shape, offered when a typed endpoint matches no
    /// suggestion — [`crate::GENERIC_SEARCH_AUTH_TEMPLATE`], carried as data so
    /// a client never has to hold its own copy.
    pub default_auth_template: String,
    /// The suggestions, in the order a client should show them.
    pub backends: Vec<WebBackendSuggestion>,
}

/// Ask what enabling web lookup would involve (REQ-572 BR-1, BR-3).
///
/// Read-only, and the flow's first step: it answers with the capability state
/// the **exposure predicate** produced — not a second derivation the client
/// could disagree with — plus what the search leg needs and what is configured
/// today. A client can render all of it as instructions and stop there, which
/// is BR-12's degradation path: the walkthrough is an enhancement over this
/// answer, never the only way to reach it.
///
/// It carries a `session_id` for [`WebOverrideParams`]'s reason: the gate that
/// admits it is session attachment, so the call has to name the session it
/// claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupPlanParams {
    /// The session asking.
    pub session_id: SessionId,
}

/// Result of [`WebSetupPlanParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupPlanResult {
    /// What the capability can do right now — the typed state (BR-10), from
    /// the one classifier that also governs tool exposure (BR-3).
    pub state: WebCapabilityState,
    /// Whether the `search` tier is worth **offering** in the tier menu.
    ///
    /// Not derivable from [`Self::state`], which describes the capability as
    /// configured: a machine sitting at `off_available` may still be unable to
    /// serve search (REQ-563 BR-14 couples search egress to the redaction
    /// scan, which needs the local model), and a menu that offered it anyway
    /// would walk a user through configuring a tier that refuses every query.
    pub search_available: bool,
    /// When [`Self::search_available`] is false, the missing piece named — the
    /// sentence the client shows beside the greyed-out menu entry (AC-7).
    /// `None` when search is offerable, and the field is then absent from the
    /// wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_gap: Option<String>,
    /// The `[web]` table as it stands, or `None` when the config has none —
    /// the fresh-install case this REQ exists for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_web: Option<WebTableSummary>,
    /// The backends to suggest and the header shape to fall back on (REQ-573
    /// BR-1), or `None` from a daemon that predates the catalog — the field is
    /// then absent from the wire and the client names no backend at all rather
    /// than inventing a list (BR-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_catalog: Option<WebSetupCatalog>,
}

impl RpcMethod for WebSetupPlanParams {
    const METHOD: &'static str = "web/setup_plan";
    type Result = WebSetupPlanResult;
}

/// Show exactly what a candidate `[web]` table would write, without writing it
/// (REQ-572 BR-7).
///
/// The daemon builds the candidate config, runs the **same** `Config::validate`
/// startup runs, and serializes the result — so the preview is the bytes, not a
/// description of them. Nothing is written and nothing is remembered: a preview
/// the user abandons leaves the daemon exactly as it found it (BR-11).
///
/// The answer's own fields, not this call, are what a user confirms; the same
/// parameters are then sent to [`WebSetupCommitParams`], which re-derives from
/// them rather than trusting a preview it kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupPreviewParams {
    /// The session previewing.
    pub session_id: SessionId,
    /// The ceiling the candidate would set.
    pub tier: WebTier,
    /// The search backend's endpoint as the user typed it. Absent below the
    /// `search` tier, where there is no backend to name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_endpoint: Option<String>,
    /// A **reference** to the key the client has already written to the OS
    /// keychain, e.g. `keychain://teton/web-search`.
    ///
    /// The secret itself never appears in these params — the CLI collects it
    /// echo-off, stores it, and sends the name (ADR-3). A raw key here is
    /// refused by the same predicate that refuses one in a provider's
    /// `auth_ref`, so the rule is enforced rather than requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_key_ref: Option<String>,
    /// The auth-header template, `{key}` marking where the credential goes
    /// (BUG-165). Absent means `Authorization: Bearer {key}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_auth: Option<String>,
}

/// Result of [`WebSetupPreviewParams`] — what the commit would write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupPreviewResult {
    /// The `[web]` table, serialized exactly as it would be written (BR-7).
    /// The user confirms these bytes, and the commit re-derives them from the
    /// same parameters — so what was agreed to is what lands.
    pub toml: String,
    /// The host the endpoint parsed to, from the **executor's** parser rather
    /// than a second one (BR-9, LESSON-494): the string shown at the confirm
    /// step is the destination the request builder would actually reach, not a
    /// separately-parsed lookalike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_host: Option<String>,
    /// Non-fatal notes about the candidate — a tier configured below what the
    /// answers imply, a backend that will need a key it was not given.
    ///
    /// Warnings, never errors: a candidate the validator **refuses** is not a
    /// preview with a note attached, it is a `WEB_SETUP_INVALID` response
    /// carrying the validator's own sentence. Empty for a clean candidate, and
    /// the field is a list rather than an `Option` so "nothing to say" has one
    /// spelling.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// A digest of the **whole document** this preview's candidate would write
    /// — what the client hands back as [`WebSetupCommitParams::expect_digest`]
    /// so the commit can refuse to write bytes the user never saw (REQ-572
    /// verify, BR-7).
    ///
    /// It covers the whole config, not just the `[web]` table, because the whole
    /// config is what the commit writes. The fields the flow does *not* collect
    /// — `permission_allow`, `allowed_domains`, `cache_ttl_secs` — ride along
    /// from whatever the live config held when the candidate was built, and any
    /// other session answering "enable permanently" moves them underneath a
    /// preview the user is still reading. Digesting the rendered `[web]` section
    /// alone would catch that particular race and miss the general one; the
    /// bytes the user confirmed are the bytes that get written, so the bytes are
    /// what is pinned.
    ///
    /// Opaque to the client: it round-trips it and never parses it. Empty from a
    /// daemon that predates this field, which a client must read as "this daemon
    /// cannot check" rather than as a digest that failed to match.
    #[serde(default)]
    pub digest: String,
}

impl RpcMethod for WebSetupPreviewParams {
    const METHOD: &'static str = "web/setup_preview";
    type Result = WebSetupPreviewResult;
}

/// Write the candidate `[web]` table and make the capability live (REQ-572
/// BR-8, AC-3).
///
/// The **single commit point** (BR-11): before this call nothing durable
/// exists, and this call either writes the file and swaps the daemon's config
/// or changes nothing at all.
///
/// Its fields are [`WebSetupPreviewParams`]'s, deliberately — the commit
/// re-derives the candidate from the answers rather than accepting a blob the
/// preview handed back, so a client cannot commit something the daemon never
/// validated (BR-8, LESSON-501). Carrying its own copy is what makes the two
/// calls independent; it is not a duplicated struct so much as the same
/// question asked twice, once for show and once for real.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupCommitParams {
    /// The session committing.
    pub session_id: SessionId,
    /// The ceiling to write.
    pub tier: WebTier,
    /// The search backend's endpoint, as in the preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_endpoint: Option<String>,
    /// The keychain **reference**, as in the preview — never the key (BR-6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_key_ref: Option<String>,
    /// The auth-header template, as in the preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_auth: Option<String>,
    /// [`WebSetupPreviewResult::digest`], handed back — the daemon writes only
    /// if the candidate it rebuilds still digests to this (REQ-572 verify,
    /// BR-7).
    ///
    /// **Not a substitute for re-deriving.** The candidate is still rebuilt from
    /// the answers above and put through the same validator (BR-8,
    /// LESSON-501); this is a *guard on the outcome*, so a client cannot use it
    /// to commit a document the daemon never validated — the worst a forged
    /// digest buys is a write the answers already earned.
    ///
    /// `None` means "do not check", which is what a client that predates the
    /// field sends and what a caller with no preview to compare against sends.
    /// The check is opt-in for that reason, not because it is optional in the
    /// flow: `/web setup` always sends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_digest: Option<String>,
}

/// Result of [`WebSetupCommitParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSetupCommitResult {
    /// Whether the config changed.
    ///
    /// `false` is not a failure and not an error response: it is a commit whose
    /// candidate matched what was already configured, and the distinction keeps
    /// the client's confirmation honest — the same reason
    /// [`WebOverrideResult::was_restricted`] exists. A failed commit is an
    /// error response, never `applied: false`.
    pub applied: bool,
    /// The ceiling now in force. Read back rather than assumed, like
    /// [`SessionPermissionsResult::level`]: a client that rendered the tier it
    /// *sent* could confirm a state the daemon does not hold.
    pub tier: WebTier,
}

impl RpcMethod for WebSetupCommitParams {
    const METHOD: &'static str = "web/setup_commit";
    type Result = WebSetupCommitResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ChosenBand, GpuClass, TierBand};
    use crate::ParseCategoryError;
    use crate::GENERIC_SEARCH_AUTH_TEMPLATE;

    /// **Mixed-version skew, on the pairing ADR-007 explicitly endorses.**
    ///
    /// The socket and lock filenames are stable so a newly-installed CLI finds
    /// an already-running older daemon, and `PROTOCOL_VERSION_MIN`/`MAX` are
    /// both 1, so that handshake *succeeds*. A required `content_class` would
    /// then surface as a raw serde error out of `Connection::call` — no
    /// sentence, no remedy — on `teton policy show` and `teton config get`.
    ///
    /// The payload below is a `CategoryRouteView` exactly as a daemon built
    /// between REQ-558 and REQ-561 emits one: every field except the two this
    /// test exists for.
    #[test]
    fn a_category_row_from_a_daemon_predating_these_fields_still_deserializes() {
        let pre_561 = serde_json::json!({
            "category": "triage",
            "tier": "scan",
            "provider_id": "cheap",
            "source": "tier_inheritance",
            "reached": true,
            "reason": "The 'triage' category inherits the 'scan' tier binding."
        });
        let row: CategoryRouteView =
            serde_json::from_value(pre_561).expect("an older daemon's row must still parse");
        assert_eq!(row.category, Category::Triage);
        assert_eq!(
            row.content_class,
            ContentClass::TurnContext,
            "an undisclosed class reads as the widest, never as the narrowest"
        );

        // And one older still, from before REQ-558 declared the call-site
        // marker. `false` is not a placeholder here: nothing dispatched on a
        // category in that daemon, so `declared, no call site yet` is what its
        // table would have said.
        let pre_558 = serde_json::json!({
            "category": "triage",
            "tier": "scan",
            "source": "unbound",
            "reason": "nothing is bound to 'scan'."
        });
        let row: CategoryRouteView =
            serde_json::from_value(pre_558).expect("an older daemon's row must still parse");
        assert!(!row.reached);

        // Non-vacuity: a row from *this* build still carries its own answer, so
        // the defaults above are reached by absence rather than by always
        // overwriting what the daemon sent.
        let current = serde_json::to_value(CategoryRouteView {
            category: Category::Triage,
            tier: Tier::Scan,
            provider_id: None,
            fallback_id: None,
            source: BindingSource::Unbound,
            reached: true,
            content_class: ContentClass::FileContent,
            reason: "r".to_owned(),
        })
        .expect("serializes");
        let back: CategoryRouteView = serde_json::from_value(current).expect("round-trips");
        assert_eq!(back.content_class, ContentClass::FileContent);
        assert!(back.reached);
    }

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
            cwd: Some(std::path::PathBuf::from("/Users/dev/repo")),
        });
        round_trip(&SessionCreateResult {
            session_id: SessionId::from("s1"),
        });
    }

    #[test]
    fn session_create_without_a_cwd_still_deserializes() {
        // Wire compatibility (BUG-147): an older client that sends no `cwd`
        // must still create a session — the field defaults to None.
        let params: SessionCreateParams = serde_json::from_str(r#"{"mode":"freeform"}"#).unwrap();
        assert_eq!(params.cwd, None);
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
                cwd: Some(std::path::PathBuf::from("/Users/dev/repo")),
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
                cwd: None,
            },
        });
    }

    /// REQ-567 D-2's one new method, pinned beside its siblings: a params/result
    /// pair whose shape nothing else in this file constrains, on a verb clients
    /// call by hand.
    #[test]
    fn session_clear_round_trips() {
        round_trip(&SessionClearParams {
            session_id: SessionId::from("s1"),
        });
        // Both ends of the count, because they are the two the CLI words
        // differently: "there was nothing retained to drop" and a real number.
        round_trip(&SessionClearResult { blocks_dropped: 0 });
        round_trip(&SessionClearResult { blocks_dropped: 12 });
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
                effort: None,
                providers: vec![ProviderConfig {
                    id: ProviderId::from("anthropic"),
                    kind: ProviderKind::Anthropic,
                    endpoint: Some("https://api.anthropic.com".to_owned()),
                    model: Some("claude-opus-5".to_owned()),
                    auth_ref: Some("keychain://teton/anthropic".to_owned()),
                }],
                tiers: vec![TierRouteView {
                    tier: Tier::Think,
                    provider_id: Some(ProviderId::from("anthropic")),
                    fallback_id: Some(ProviderId::from("local")),
                    source: TierBindingSource::Configured,
                }],
                routing: vec![CategoryRouteView {
                    category: Category::Design,
                    tier: Tier::Think,
                    provider_id: Some(ProviderId::from("anthropic")),
                    fallback_id: Some(ProviderId::from("local")),
                    source: BindingSource::TierInheritance,
                    reached: true,
                    content_class: ContentClass::TurnContext,
                    reason: "Routing the 'design' category to 'anthropic' through its 'think' \
                             tier binding."
                        .to_owned(),
                }],
                judgment_default: Some(Category::Edit),
                privacy: vec![PrivacyBoundaryConfig {
                    path_glob: "secrets/**".to_owned(),
                    mode: PrivacyMode::LocalOnly,
                }],
                redact_enabled: true,
                web_capability: Some(WebCapabilityState::Ready {
                    tier: WebTier::FetchUserUrl,
                }),
            },
        });
    }

    /// A snapshot from a daemon that predates `redact_enabled` reads as **off**
    /// (REQ-562), which is the historical fact about such a daemon rather than
    /// a filler value: none of them ran the scan.
    ///
    /// The fixture is a v2-shaped snapshot, because v2 is the oldest shape this
    /// build can read at all — a genuine v1 body fails on `routing` long before
    /// reaching this key, which the neighbouring
    /// `the_last_releases_snapshot_is_unreadable_which_is_why_the_version_is_pinned`
    /// pins and this test must not be read as contradicting. What is asserted
    /// here is the *field's* compatibility posture: a daemon inside the
    /// supported handshake range that has simply never heard of the key.
    #[test]
    fn a_snapshot_with_no_redact_enabled_key_reads_as_off() {
        let without_the_key = serde_json::json!({
            "providers": [],
            "tiers": [],
            "routing": [],
            "privacy": [{"path_glob": "secrets/**", "mode": "local_only"}]
        });
        let snapshot: ConfigSnapshot = serde_json::from_value(without_the_key.clone()).unwrap();
        assert!(
            !snapshot.redact_enabled,
            "an absent answer must never read as `the scan is running`"
        );
        // The rest of the snapshot still arrives — the default is a default,
        // not a fallback for a body that failed to parse.
        assert_eq!(snapshot.privacy.len(), 1);

        // Non-vacuity: the default is not swallowing a key that *is* present,
        // in either state (LESSON-485 — one leg alone would pass against a
        // field hard-wired to `false`).
        for stated in [true, false] {
            let mut wire = without_the_key.clone();
            wire["redact_enabled"] = serde_json::Value::Bool(stated);
            let snapshot: ConfigSnapshot = serde_json::from_value(wire).unwrap();
            assert_eq!(snapshot.redact_enabled, stated);
        }
    }

    /// REQ-572's addition, same posture: a snapshot from a daemon that never
    /// heard of `web_capability` still deserializes, and reads as **no answer**
    /// rather than as a state.
    ///
    /// The fixture is the shape a v2 daemon shipped before this REQ — the same
    /// pre-REQ-572 body `config/get` returns today — so the claim is about the
    /// field's compatibility posture and not about a hand-trimmed object.
    /// `None` is the honest reading: such a daemon derived no capability state
    /// at all, and a client that turned its silence into `off_available` would
    /// be reporting the user's configuration from a build that never looked.
    #[test]
    fn a_snapshot_with_no_web_capability_key_reads_as_no_answer() {
        let pre_572 = serde_json::json!({
            "providers": [],
            "tiers": [],
            "routing": [],
            "privacy": [{"path_glob": "secrets/**", "mode": "local_only"}],
            "redact_enabled": true
        });
        let snapshot: ConfigSnapshot = serde_json::from_value(pre_572.clone()).unwrap();
        assert_eq!(
            snapshot.web_capability, None,
            "an absent answer must not be read as a capability state"
        );
        // The rest still arrives: the default is a default, not a fallback for
        // a body that failed to parse.
        assert_eq!(snapshot.privacy.len(), 1);
        assert!(snapshot.redact_enabled);

        // Non-vacuity, both directions (LESSON-485): the default is not
        // swallowing a key that *is* present, in any of the three states.
        for state in [
            WebCapabilityState::Ready {
                tier: WebTier::Search,
            },
            WebCapabilityState::OffAvailable,
            WebCapabilityState::SearchUnavailable {
                reason: "search needs the local model, which is not loaded".to_owned(),
            },
        ] {
            let mut wire = pre_572.clone();
            wire["web_capability"] = serde_json::to_value(&state).unwrap();
            let snapshot: ConfigSnapshot = serde_json::from_value(wire).unwrap();
            assert_eq!(snapshot.web_capability, Some(state));
        }

        // And the other direction: a client built before the field reads a
        // snapshot that carries it, which is what keeps
        // `crate::PROTOCOL_VERSION` still across this addition.
        #[derive(Deserialize)]
        struct PreSetupSnapshot {
            privacy: Vec<PrivacyBoundaryConfig>,
        }
        let wire = serde_json::to_string(&ConfigSnapshot {
            privacy: vec![PrivacyBoundaryConfig {
                path_glob: "secrets/**".to_owned(),
                mode: PrivacyMode::LocalOnly,
            }],
            web_capability: Some(WebCapabilityState::OffAvailable),
            ..ConfigSnapshot::default()
        })
        .unwrap();
        assert!(
            wire.contains(r#""web_capability":{"state":"off_available"}"#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreSetupSnapshot = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.privacy.len(), 1, "the old reader still gets its fields");

        // A default snapshot omits the key entirely rather than sending
        // `null` — the same wire an older daemon writes.
        let empty = serde_json::to_value(ConfigSnapshot::default()).unwrap();
        assert!(empty.get("web_capability").is_none(), "{empty}");
    }

    /// The other direction of the same claim: a client that predates the field
    /// still reads a snapshot that carries it.
    ///
    /// Serde ignores unknown fields by default and no type here opts out, but
    /// this posture is what keeps [`crate::PROTOCOL_VERSION`] still across the
    /// addition, so it is asserted rather than assumed — modelled by the
    /// pre-REQ-562 shape of the reader, exactly as `PrivacyBlock::cause`'s
    /// skew test does.
    #[test]
    fn a_client_predating_redact_enabled_still_reads_a_snapshot_that_carries_it() {
        #[derive(Deserialize)]
        struct PreSwitchSnapshot {
            privacy: Vec<PrivacyBoundaryConfig>,
            judgment_default: Option<Category>,
        }

        let wire = serde_json::to_string(&ConfigSnapshot {
            privacy: vec![PrivacyBoundaryConfig {
                path_glob: "secrets/**".to_owned(),
                mode: PrivacyMode::LocalOnly,
            }],
            judgment_default: Some(Category::Edit),
            redact_enabled: true,
            ..ConfigSnapshot::default()
        })
        .unwrap();
        assert!(
            wire.contains("\"redact_enabled\":true"),
            "the fixture must actually carry the new key: {wire}"
        );

        let old: PreSwitchSnapshot = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.privacy.len(), 1, "the old reader still gets its fields");
        assert_eq!(old.judgment_default, Some(Category::Edit));
    }

    /// The eleven categories, spelled out rather than iterated, so a twelfth
    /// has to be added here by hand — the same reason the `Category` tests in
    /// `lib.rs` spell them.
    const ALL_CATEGORIES: [Category; 11] = [
        Category::Route,
        Category::Redact,
        Category::Title,
        Category::Digest,
        Category::Compact,
        Category::Triage,
        Category::Edit,
        Category::Shell,
        Category::Design,
        Category::Debug,
        Category::Review,
    ];

    /// AC-16: the disclosure covers **all eleven** categories, and every class it
    /// can name is one some category actually uses.
    ///
    /// The second half is what keeps the first from being vacuous: a mapping
    /// that answered one constant for everything would satisfy "every category
    /// has a class" and disclose nothing. Asserting that all seven classes are
    /// reachable also means no variant of [`ContentClass`] is decoration.
    #[test]
    fn every_category_declares_a_content_class() {
        assert_eq!(ALL_CATEGORIES.len(), 11);

        let mut seen = std::collections::HashSet::new();
        for category in ALL_CATEGORIES {
            let class = ContentClass::for_category(category);
            // The wire spelling and the display form are one string, as with
            // `Category` — a variant whose `as_str` drifts from its serde
            // rename fails here.
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{}\"", class.as_str()), "{category}");
            assert_eq!(class.to_string(), class.as_str());
            assert_eq!(
                serde_json::from_str::<ContentClass>(&json).unwrap(),
                class,
                "{category} must round-trip"
            );
            assert!(
                !class.describe().is_empty(),
                "{category} must have something to say to a human"
            );
            seen.insert(class);
        }
        assert_eq!(
            seen.len(),
            7,
            "every ContentClass variant should be some category's answer: {seen:?}"
        );
    }

    /// OQ-4, the whole reason BR-11 exists: one tier, two categories, two
    /// different kinds of content.
    ///
    /// A user binds `scan` to a remote provider for cheap long-context
    /// summarisation. `triage` sending file content is what they expected;
    /// `compact` sending the conversation is what surprises them. The two rows
    /// disclosing different classes is the entire mitigation, so it is pinned
    /// rather than left to the mapping's good intentions.
    #[test]
    fn triage_and_compact_disclose_different_content_despite_sharing_a_tier() {
        let triage = ContentClass::for_category(Category::Triage);
        let compact = ContentClass::for_category(Category::Compact);
        assert_ne!(
            triage, compact,
            "the scan tier's two categories must not read as one disclosure"
        );
        assert_eq!(triage, ContentClass::FileContent);
        assert_eq!(compact, ContentClass::ConversationHistory);
    }

    /// AC-16's other half: a category with no call site is described, not
    /// omitted — and the pair of fields says both things at once.
    ///
    /// **The set of unreached categories is empty as of REQ-562**, which wired
    /// `redact` — the last of the eleven. So this row is now hypothetical
    /// rather than a snapshot of any live category, and it is kept deliberately:
    /// the *shape* is the contract. A future category lands declared before it
    /// is called, and what this pins is that such a row still discloses what it
    /// would send (`content_class`) while saying nothing is sent yet
    /// (`reached: false`). Neither field alone is the disclosure, which is why
    /// the row is asserted rather than the mapping.
    #[test]
    fn an_unreached_category_still_says_what_it_would_send() {
        let row = CategoryRouteView {
            category: Category::Redact,
            tier: Tier::Reflex,
            provider_id: Some(ProviderId::from("on-device")),
            fallback_id: None,
            source: BindingSource::PinnedLocal,
            reached: false,
            content_class: ContentClass::for_category(Category::Redact),
            reason: "The 'redact' category is pinned to the local tier.".to_owned(),
        };
        assert_eq!(row.content_class, ContentClass::OutboundPayload);
        assert!(
            !row.reached,
            "the fixture is a hypothetical unreached category; `redact` itself \
             has had a call site since REQ-562"
        );
        round_trip(&row);

        let json = serde_json::to_string(&row).unwrap();
        assert!(
            json.contains("\"content_class\":\"outbound_payload\""),
            "the class must reach a client, not stay a daemon-side fact: {json}"
        );
    }

    /// The concrete reason [`crate::PROTOCOL_VERSION_MIN`] is 2.
    ///
    /// This is a verbatim `config/get` snapshot from the last release (v0.1.10),
    /// which predates REQ-558. Today's type cannot read it: `routing` was a
    /// phase-keyed table and is now category-keyed, so the very first row is
    /// missing `category`. `tiers` is absent entirely.
    ///
    /// Kept as an assertion rather than a comment because the two facts have to
    /// move together. **If you make this parse, lower `PROTOCOL_VERSION_MIN` to
    /// 1 in the same change** — a build that can read v1 should say so, and a
    /// build that cannot must not.
    #[test]
    fn the_last_releases_snapshot_is_unreadable_which_is_why_the_version_is_pinned() {
        let v0_1_10 = serde_json::json!({
            "providers": [{
                "id": "anthropic",
                "kind": "anthropic",
                "endpoint": "https://api.anthropic.com",
                "model": "claude-opus-5",
                "auth_ref": "keychain://teton/anthropic"
            }],
            "routing": [
                {"phase": "implement", "provider_id": "anthropic", "fallback_id": "local"},
                {"phase": "io", "provider_id": "local"}
            ],
            "privacy": [{"path_glob": "secrets/**", "mode": "local_only"}]
        });

        let err = serde_json::from_value::<ConfigSnapshot>(v0_1_10)
            .expect_err("a v1 snapshot must not deserialize into the v2 type");
        assert!(
            err.to_string().contains("category"),
            "expected the category-keyed routing row to be the break; got: {err}"
        );

        // And the absence is mutual: the v2 shape carries a `tiers` array and a
        // `category` key that no v1 client knew to send, so neither direction of
        // the pairing is serviceable and the handshake is the right gate.
        let today = serde_json::to_value(ConfigSnapshot::default()).unwrap();
        assert!(today.get("tiers").is_some());
    }

    #[test]
    fn config_set_round_trips_each_update_variant() {
        for update in [
            ConfigUpdate::RegisterProvider(ProviderConfig {
                id: ProviderId::from("deepseek"),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.deepseek.com".to_owned()),
                model: Some("deepseek-chat".to_owned()),
                auth_ref: Some("keychain://teton/deepseek".to_owned()),
            }),
            ConfigUpdate::SetTierBinding(TierBindingConfig {
                tier: Tier::Build,
                provider_id: ProviderId::from("deepseek"),
                fallback_id: None,
            }),
            ConfigUpdate::SetCategoryBinding(CategoryBindingConfig {
                name: ConfigurableCategory::Review,
                provider_id: ProviderId::from("deepseek"),
                fallback_id: Some(ProviderId::from("anthropic")),
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

    /// REQ-562 AC-4, RPC leg: `config/set` cannot carry a binding for a pinned
    /// category, and REQ-562's `[privacy]` opt-in does not change that.
    ///
    /// This asserts the **protocol** type, which is a different type from the
    /// config-file `FromStr` path with a different rejection mechanism
    /// (LESSON-486 #2): the daemon deserializes [`ConfigSetParams`] straight out
    /// of the request, so the pin here is serde refusing a variant that does not
    /// exist. The [`ParseCategoryError::RedactIsPinned`] *sentence* belongs to
    /// `FromStr`, which is the CLI's path — asserted below so the two legs are
    /// visibly distinct rather than assumed to be one.
    ///
    /// The payload is derived from a valid one by swapping only the category
    /// name, so the test cannot pass because the request was malformed for some
    /// unrelated reason.
    #[test]
    fn a_config_set_payload_naming_a_pinned_category_cannot_be_deserialized() {
        let valid = ConfigSetParams {
            update: ConfigUpdate::SetCategoryBinding(CategoryBindingConfig {
                name: ConfigurableCategory::Review,
                provider_id: ProviderId::from("on-device"),
                fallback_id: None,
            }),
        };
        let mut payload = serde_json::to_value(&valid).expect("serialize");
        assert_eq!(payload["update"]["name"], "review", "payload: {payload}");
        assert!(
            serde_json::from_value::<ConfigSetParams>(payload.clone()).is_ok(),
            "the fixture must be a payload the daemon would otherwise accept"
        );

        for pinned in ["redact", "route"] {
            payload["update"]["name"] = serde_json::Value::String(pinned.to_owned());
            assert!(
                serde_json::from_value::<ConfigSetParams>(payload.clone()).is_err(),
                "config/set accepted a binding for the pinned category {pinned}: {payload}"
            );
            // The FromStr leg — what `teton policy set-category` parses — names
            // the pin rather than reading as a typo.
            assert!(
                matches!(
                    pinned.parse::<ConfigurableCategory>(),
                    Err(ParseCategoryError::RedactIsPinned | ParseCategoryError::RouteIsPinned)
                ),
                "{pinned} lost its pinned-category sentence"
            );
        }
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
                unpriced_models: vec!["llama-3-70b".to_owned()],
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
                web_per_session: vec![WebTotalsView {
                    key: "sess-under-test".to_owned(),
                    lookups: 2,
                    bytes_in: 8_192,
                }],
                reasoning_tokens: None,
                calls_reporting_reasoning: 0,
            },
        });
    }

    /// A `cost/query` answer from a daemon built before REQ-563 carries no
    /// `web_per_session` key at all. It must deserialize into an empty roll-up
    /// rather than fail: the field is additive, and a client refusing the whole
    /// report over a missing web section would break `teton cost` against every
    /// older daemon.
    #[test]
    fn a_cost_report_without_the_web_roll_up_still_deserializes() {
        let without_web = serde_json::json!({
            "total_usd_micros": 0,
            "total_calls": 0,
            "priced_calls": 0,
            "unpriced_calls": 0,
            "savings_usd_micros": 0,
            "baseline_usd_micros": 0,
            "baseline_model": "anthropic/claude-opus-4",
            "methodology": "Estimate, not a measurement.",
            "per_phase": [],
            "per_provider": [],
        });
        let decoded: CostReportView =
            serde_json::from_value(without_web).expect("an older report still decodes");
        assert!(decoded.web_per_session.is_empty());
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
        assert_eq!(WebOverrideParams::METHOD, "web/override");
        assert_eq!(SessionPermissionsParams::METHOD, "session/permissions");
        assert_eq!(WebRefreshParams::METHOD, "web/refresh");
        assert_eq!(WebSetupPlanParams::METHOD, "web/setup_plan");
        assert_eq!(WebSetupPreviewParams::METHOD, "web/setup_preview");
        assert_eq!(WebSetupCommitParams::METHOD, "web/setup_commit");
        assert_eq!(
            request(Id::Number(2), ModelStatusParams::default()).method,
            "model/status"
        );
    }

    /// The two user-only web actions round-trip, and the override answers the
    /// "nothing was restricted" case distinguishably from "restricted, but no
    /// tiers to restore" — the distinction TASK-077's no-op notice rests on.
    #[test]
    fn web_control_methods_round_trip() {
        round_trip(&WebOverrideParams {
            session_id: SessionId::from("s1"),
        });
        round_trip(&WebOverrideResult {
            was_restricted: true,
            tiers_restored: vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl],
        });
        // Restricted, but the session had been granted nothing to restore.
        round_trip(&WebOverrideResult {
            was_restricted: true,
            tiers_restored: vec![],
        });
        // Never restricted — same empty list, different answer.
        round_trip(&WebOverrideResult::default());
        assert!(!WebOverrideResult::default().was_restricted);

        round_trip(&WebRefreshParams {
            url: "https://docs.rs/serde/latest/serde/".to_owned(),
        });
        for outcome in [WebRefreshOutcome::Evicted, WebRefreshOutcome::Absent] {
            round_trip(&WebRefreshResult { outcome });
        }
    }

    /// REQ-572's three setup endpoints round-trip, including the answers whose
    /// *absence* is a distinct fact: no `[web]` table yet, no search gap, no
    /// warnings.
    ///
    /// Each is exercised at both ends of its own question — a fresh machine
    /// with nothing configured, and a fully-specified search backend — because
    /// the empty side is the state this REQ exists for and the one a default
    /// would quietly manufacture.
    #[test]
    fn the_web_setup_methods_round_trip() {
        round_trip(&WebSetupPlanParams {
            session_id: SessionId::from("s1"),
        });

        // The fresh-install answer: nothing configured, search not offerable,
        // and the gap named.
        let fresh = WebSetupPlanResult {
            state: WebCapabilityState::OffAvailable,
            search_available: false,
            search_gap: Some("search needs the local model, which is not loaded".to_owned()),
            current_web: None,
            suggestion_catalog: None,
        };
        round_trip(&fresh);
        let wire = serde_json::to_value(&fresh).unwrap();
        assert_eq!(wire["state"]["state"], "off_available");
        assert!(
            wire.get("current_web").is_none(),
            "no `[web]` table must be an absent key, not an empty summary: {wire}"
        );
        assert!(
            wire.get("suggestion_catalog").is_none(),
            "no catalog must be an absent key, not an empty one: {wire}"
        );

        // The configured answer: a table exists, search is offerable, and the
        // gap key drops off the wire.
        let configured = WebSetupPlanResult {
            state: WebCapabilityState::Ready {
                tier: WebTier::Search,
            },
            search_available: true,
            search_gap: None,
            current_web: Some(WebTableSummary {
                tier: WebTier::Search,
                search_host: Some("search.example.com".to_owned()),
                search_key_ref: Some("keychain://teton/web-search".to_owned()),
                search_auth: Some("X-Subscription-Token: {key}".to_owned()),
            }),
            suggestion_catalog: None,
        };
        round_trip(&configured);
        let wire = serde_json::to_value(&configured).unwrap();
        assert!(wire.get("search_gap").is_none(), "{wire}");
        assert_eq!(wire["current_web"]["search_host"], "search.example.com");

        // A `[web]` table that exists and says `off` — a different fact from
        // no table at all, and the one whose keys all drop off the wire.
        let says_off = WebTableSummary {
            tier: WebTier::Off,
            search_host: None,
            search_key_ref: None,
            search_auth: None,
        };
        round_trip(&says_off);
        let wire = serde_json::to_value(&says_off).unwrap();
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["tier"]
        );

        // The REQ-573 catalog, with every optional field exercised at both
        // ends: an entry that names a host, a template and a key it needs,
        // beside one that is self-hosted and needs none. Sentinel values
        // throughout — what this pins is the shape, not the product list,
        // which the daemon owns (ADR-A).
        let with_catalog = WebSetupPlanResult {
            state: WebCapabilityState::Ready {
                tier: WebTier::Search,
            },
            search_available: true,
            search_gap: None,
            current_web: None,
            suggestion_catalog: Some(WebSetupCatalog {
                default_auth_template: GENERIC_SEARCH_AUTH_TEMPLATE.to_owned(),
                backends: vec![
                    WebBackendSuggestion {
                        id: "sentinel-hosted".to_owned(),
                        label: "Sentinel Hosted".to_owned(),
                        endpoint: "https://sentinel.example.com/api/search".to_owned(),
                        host: Some("sentinel.example.com".to_owned()),
                        auth_template: Some("X-Sentinel-Header: {key}".to_owned()),
                        needs_key: true,
                        notes: Some("a sentinel note".to_owned()),
                    },
                    WebBackendSuggestion {
                        id: "sentinel-local".to_owned(),
                        label: "Sentinel Local".to_owned(),
                        endpoint: "http://localhost:9999/search?format=json".to_owned(),
                        host: None,
                        auth_template: None,
                        needs_key: false,
                        notes: None,
                    },
                ],
            }),
        };
        round_trip(&with_catalog);
        let wire = serde_json::to_value(&with_catalog).unwrap();
        assert_eq!(
            wire["suggestion_catalog"]["default_auth_template"],
            GENERIC_SEARCH_AUTH_TEMPLATE
        );
        assert_eq!(
            wire["suggestion_catalog"]["backends"][0]["auth_template"],
            "X-Sentinel-Header: {key}"
        );
        // The self-hosted entry: three of its four keys are the ones that
        // *aren't* there, which is what tells a client it has no host to match
        // and no header to offer.
        let self_hosted = &wire["suggestion_catalog"]["backends"][1];
        assert_eq!(
            self_hosted.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["endpoint", "id", "label", "needs_key"]
        );

        // Preview: the same params the commit takes, at both ends of the
        // ladder — a keyless tier bump and a full search backend.
        for params in [
            WebSetupPreviewParams {
                session_id: SessionId::from("s1"),
                tier: WebTier::FetchUserUrl,
                search_endpoint: None,
                search_key_ref: None,
                search_auth: None,
            },
            WebSetupPreviewParams {
                session_id: SessionId::from("s1"),
                tier: WebTier::Search,
                search_endpoint: Some("https://search.example.com/search?format=json".to_owned()),
                search_key_ref: Some("keychain://teton/web-search".to_owned()),
                search_auth: Some("X-Subscription-Token: {key}".to_owned()),
            },
        ] {
            round_trip(&params);
        }
        round_trip(&WebSetupPreviewResult {
            toml: "[web]\ntier = \"search\"\n".to_owned(),
            search_host: Some("search.example.com".to_owned()),
            warnings: vec!["this backend usually needs a key".to_owned()],
            digest: "a".repeat(64),
        });
        // A clean candidate: no host to show below the search tier, and an
        // empty warning list rather than an absent one.
        round_trip(&WebSetupPreviewResult {
            toml: "[web]\ntier = \"fetch_user_url\"\n".to_owned(),
            search_host: None,
            warnings: vec![],
            digest: "b".repeat(64),
        });
        assert!(WebSetupPreviewResult::default().warnings.is_empty());
        // The compat reading of an absent digest: a daemon that predates the
        // field answers "cannot check", never "checked and matched nothing".
        assert!(WebSetupPreviewResult::default().digest.is_empty());
        let old_preview: WebSetupPreviewResult =
            serde_json::from_str(r#"{"toml":"[web]\n","warnings":[]}"#).unwrap();
        assert!(old_preview.digest.is_empty());

        round_trip(&WebSetupCommitParams {
            session_id: SessionId::from("s1"),
            tier: WebTier::Search,
            search_endpoint: Some("https://search.example.com/search?format=json".to_owned()),
            search_key_ref: Some("keychain://teton/web-search".to_owned()),
            search_auth: None,
            expect_digest: Some("c".repeat(64)),
        });
        // And the same commit with no digest to check — the shape an old client
        // sends, which stays a legal request rather than a parse failure.
        round_trip(&WebSetupCommitParams {
            session_id: SessionId::from("s1"),
            tier: WebTier::FetchAnyUrl,
            search_endpoint: None,
            search_key_ref: None,
            search_auth: None,
            expect_digest: None,
        });
        let old_commit: WebSetupCommitParams =
            serde_json::from_str(r#"{"session_id":"s1","tier":"fetch_any_url"}"#).unwrap();
        assert_eq!(old_commit.expect_digest, None);
        // Both answers: a commit that changed the config, and one whose
        // candidate matched what was already there. Neither is an error.
        round_trip(&WebSetupCommitResult {
            applied: true,
            tier: WebTier::Search,
        });
        round_trip(&WebSetupCommitResult {
            applied: false,
            tier: WebTier::FetchAnyUrl,
        });
    }

    /// REQ-573 AC-1's absent direction, and BUG-158's additive-skew rule: a
    /// plan answer from a daemon that predates the catalog still parses, and
    /// the missing field reads as "this daemon sent no catalog" rather than
    /// failing or manufacturing an empty one.
    ///
    /// The distinction is the whole point — `None` is what the client's
    /// degraded path (BR-3) keys off, so a catalog with zero backends would be
    /// a *different* answer, and one no daemon has any way to send by accident.
    #[test]
    fn a_plan_result_from_before_the_catalog_reads_as_no_catalog() {
        let older = r#"{
            "state": {"state": "off_available"},
            "search_available": false,
            "search_gap": "search needs the local model, which is not loaded"
        }"#;
        let parsed: WebSetupPlanResult = serde_json::from_str(older).unwrap();
        assert_eq!(parsed.suggestion_catalog, None);
        // Non-vacuity: the rest of the answer really did parse.
        assert_eq!(parsed.state, WebCapabilityState::OffAvailable);
        assert!(!parsed.search_available);

        // And an empty catalog is a legible, distinct answer — not the same
        // fact spelled differently.
        let empty = r#"{
            "state": {"state": "off_available"},
            "search_available": false,
            "suggestion_catalog": {"default_auth_template": "Sentinel {key}", "backends": []}
        }"#;
        let parsed: WebSetupPlanResult = serde_json::from_str(empty).unwrap();
        let catalog = parsed
            .suggestion_catalog
            .expect("an empty catalog is a catalog");
        assert!(catalog.backends.is_empty());
        assert_eq!(catalog.default_auth_template, "Sentinel {key}");
    }

    /// BR-6 / ADR-3's wire half: **no setup type can carry the key**, in either
    /// direction. The secret's whole lifecycle stays in the client process, and
    /// what travels is a reference.
    ///
    /// Asserted on the serialized key sets rather than on a reading of the
    /// structs, so a `search_key` field added later turns this red instead of
    /// riding along with the reference that was supposed to replace it.
    #[test]
    fn no_setup_payload_has_anywhere_to_put_the_key() {
        let planted = "sk-live-do-not-log-me";
        let preview = serde_json::to_string(&WebSetupPreviewParams {
            session_id: SessionId::from("s1"),
            tier: WebTier::Search,
            search_endpoint: Some("https://search.example.com/search".to_owned()),
            search_key_ref: Some("keychain://teton/web-search".to_owned()),
            search_auth: Some("X-Subscription-Token: {key}".to_owned()),
        })
        .unwrap();
        let commit = serde_json::to_string(&WebSetupCommitParams {
            session_id: SessionId::from("s1"),
            tier: WebTier::Search,
            search_endpoint: Some("https://search.example.com/search".to_owned()),
            search_key_ref: Some("keychain://teton/web-search".to_owned()),
            search_auth: Some("X-Subscription-Token: {key}".to_owned()),
            expect_digest: Some("d".repeat(64)),
        })
        .unwrap();

        for wire in [&preview, &commit] {
            assert!(!wire.contains(planted), "{wire}");
            // The only credential-shaped key is the reference, and the header
            // template still says `{key}` — nothing substituted a value in.
            assert!(wire.contains("keychain://teton/web-search"), "{wire}");
            assert!(wire.contains("{key}"), "{wire}");
        }

        // Every field name the commit can carry, spelled out (sorted, which is
        // how `serde_json` hands back an object's keys): a key-carrying field
        // would have to be added to this list to be added to the type.
        let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&commit)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "expect_digest",
                "search_auth",
                "search_endpoint",
                "search_key_ref",
                "session_id",
                "tier"
            ]
        );
    }

    /// BR-7's boundary drawn where it actually is: the request may name a URL
    /// (the user typed it), the **answer** may not echo one back. Asserted on
    /// the result's key set so a later `url` field turns this red.
    #[test]
    fn a_refresh_answers_with_an_outcome_and_never_echoes_the_url() {
        let wire = serde_json::to_value(WebRefreshResult {
            outcome: WebRefreshOutcome::Evicted,
        })
        .unwrap();
        let keys: Vec<&String> = wire.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["outcome"]);
        assert_eq!(wire["outcome"], "evicted");

        // Non-vacuity: the URL really was in the request this answers.
        let asked = serde_json::to_value(WebRefreshParams {
            url: "https://docs.rs/serde/latest/serde/".to_owned(),
        })
        .unwrap();
        assert!(asked["url"].as_str().unwrap().contains("docs.rs"));
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

    /// REQ-569 ADR-E: `attach/consent` round-trips both decisions under its own
    /// method name, and an answer this build cannot read is an error rather
    /// than a default.
    ///
    /// The rejection half is the one that matters. Every other closed enum in
    /// this crate refuses an unknown tag to avoid guessing a user's intent;
    /// here a wrong guess in one direction *mints a credential*, so there is no
    /// safe side to fall back to and the parse must simply fail.
    #[test]
    fn attach_consent_round_trips_both_answers_and_rejects_an_unknown_one() {
        assert_eq!(AttachConsentParams::METHOD, "attach/consent");
        for outcome in [AttachConsentOutcome::Granted, AttachConsentOutcome::Denied] {
            round_trip(&AttachConsentParams {
                request_id: RequestId::from("consent-3"),
                outcome,
            });
        }
        round_trip(&AttachConsentResult { resolved: true });

        let wire = serde_json::to_value(AttachConsentParams {
            request_id: RequestId::from("consent-3"),
            outcome: AttachConsentOutcome::Granted,
        })
        .unwrap();
        assert_eq!(wire["outcome"]["outcome"], "granted");

        let unknown = r#"{"request_id":"consent-3","outcome":{"outcome":"maybe"}}"#;
        assert!(
            serde_json::from_str::<AttachConsentParams>(unknown).is_err(),
            "an unreadable decision must not deserialize to one the daemon acts on"
        );
    }
}
