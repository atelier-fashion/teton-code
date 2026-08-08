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
    use crate::ParseCategoryError;

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
            },
        });
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
