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
    /// Whether a completed call to this method is the end of an assistant
    /// **turn** (REQ-592 BR-8 / ADR-3).
    ///
    /// Lives here, beside the wire name, because it is a property of the method
    /// rather than of any caller: the client's event pump drops its markdown
    /// fence exactly when a turn ends, and about thirty of its `call` sites —
    /// every slash handler, the setup walkthroughs, the status probes — are not
    /// turns at all. A per-call-site rule is one a handler can get wrong; a
    /// per-method one is answered where the method is declared, so a future
    /// turn-shaped RPC declares itself one in the same place it declares its
    /// name.
    ///
    /// Defaulted to `false`: a method that says nothing about turns is not one.
    const ENDS_TURN: bool = false;
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

/// What kind of place a session's root is (REQ-583 System Model, BR-4).
///
/// Derived by the daemon from the stored path at every use, never stored:
/// `home` when the path is `$HOME`, `filesystem_root` when it is `/`,
/// `project` when the directory holds a name from the project-marker table,
/// `plain` otherwise. Wire spellings are snake_case (`filesystem_root`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// The directory holds a project marker (a VCS directory or a top-level
    /// build manifest).
    Project,
    /// The path is the user's home directory.
    Home,
    /// The path is `/`.
    FilesystemRoot,
    /// None of the above: a directory that is not a project.
    Plain,
}

/// The wire view of a session's root — the directory its tools are jailed to
/// (REQ-583 System Model).
///
/// The daemon derives this from the stored path; the client renders what it
/// was told and never re-derives. `display` is the one spelling every surface
/// uses (banner, launch notice, environment block, jail refusals, `/cd`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRoot {
    /// Home-relative (`~`, `~/Documents/GitHub/teton-code`) or absolute when
    /// not under `$HOME`.
    pub display: String,
    /// What kind of place the root is.
    pub kind: RootKind,
    /// The root directory's basename; present iff `kind == project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// The git branch, when the project is a git checkout and the branch can
    /// be read without invoking git; absent otherwise (never a guessed value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs_branch: Option<String>,
}

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
    /// The session root the daemon settled on (REQ-583 BR-6): what the CLI's
    /// banner `cwd:` line and launch notice render.
    ///
    /// Optional for wire compatibility in both directions — an older daemon
    /// omits it and an older client ignores it (additive; no
    /// `PROTOCOL_VERSION` bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<SessionRoot>,
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

/// Move a live session's root (REQ-583 BR-7, architecture ADR-4) — the `/cd`
/// verb, modelled on [`SessionClearParams`].
///
/// No ACP equivalent. The daemon validates `cwd` exactly as it validates
/// `session/create`'s `cwd` (BR-6): absolute, exists, is a directory; a refusal
/// names the path and the reason, and the root is unchanged. On success the
/// conversation is **cleared** (every carried block's provenance identity is
/// relative to the root it was minted under) and reported in the existing
/// `context_cleared` shape, alongside a `session_root_changed` event.
///
/// **User-only, by construction**, for [`SessionClearParams`]'s reason: no
/// tool wraps it, so a model can never move its own jail. Available at every
/// permission level — it moves the jail, it does not mutate files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetCwdParams {
    /// The session whose root moves.
    pub session_id: SessionId,
    /// The new root. Absolute after client-side resolution (`~` expansion,
    /// relative-to-shell-cwd joining); the daemon validates, the client does
    /// not canonicalize.
    pub cwd: std::path::PathBuf,
    /// The **bare name** the user typed, when the argument was one (REQ-584 BR-8).
    ///
    /// `/cd teton-code` sends both: `cwd` is `<shell cwd>/teton-code`, the
    /// reading REQ-583 has always given, and this is the spelling to try
    /// against the known-project registry **if and only if** that path is not a
    /// directory. Absent for every path spelling (`~/x`, `./x`, `/abs`, and
    /// anything containing a separator), so REQ-583's behaviour is unchanged
    /// wherever it applied.
    ///
    /// It rides the params rather than being re-derived daemon-side because by
    /// the time `cwd` arrives the bare name is gone — `teton-code` and
    /// `./teton-code` resolve to the same absolute path, and only the client
    /// knows which was typed. Additive: a client that never sends it gets
    /// exactly REQ-583's `/cd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hint: Option<String>,
}

/// Result of [`SessionSetCwdParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetCwdResult {
    /// The root the daemon settled on, as it will render everywhere.
    pub root: SessionRoot,
    /// How many retained blocks the accompanying clear dropped; `0` when the
    /// session had nothing to say yet ([`SessionClearResult::blocks_dropped`]).
    pub blocks_dropped: u64,
}

impl RpcMethod for SessionSetCwdParams {
    const METHOD: &'static str = "session/set_cwd";
    type Result = SessionSetCwdResult;
}

// ---------------------------------------------------------------------------
// skills (REQ-585)
// ---------------------------------------------------------------------------

/// Which of REQ-585's two discovery roots a skill was found under (BR-1).
///
/// `user` is `~/.claude/{skills,commands}`; `project` is the same pair under
/// the session root. The distinction is not cosmetic: it decides the name
/// contest (BR-2 — a project skill shadows a user skill of the same name), it
/// is half of the permission key (`skill:<source>:<name>`, ADR-6), and it is
/// why a `/cd` drops the project grants and keeps the user ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Found under the user's `~/.claude`.
    User,
    /// Found under the session root's `.claude`.
    Project,
}

/// Every name the built-in command table claims, so a skill can never take one.
///
/// **Why this lives above both crates.** BR-2 says a reserved name always wins,
/// and the table that defines "reserved" is `teton`'s `COMMANDS` — which
/// `tetond` cannot read, because the daemon does not depend on the CLI. So the
/// client enforced it and the daemon did not, and the daemon's name resolver —
/// then one function, since split into
/// `SkillRegistry::{dispatchable_by_user, invocable_by_model}` (REQ-587 ADR-12)
/// — happily answered for a skill named `cost`. That is invisible while the only
/// client is `teton`, and it is a hole the moment a second one exists: a
/// `session/prompt { skill: { name: "cost" } }` from a client carrying no table
/// runs a repo-supplied `.claude/skills/cost/SKILL.md`, and the spec's own
/// Assumptions say project skills may be authored by someone other than the
/// user. ADR-1's rule is that every rule with teeth lives in the daemon; this
/// one had none there (REQ-585 verify).
///
/// **It is a list here and a derivation there.** `teton::slash::table_claim`
/// still derives the same set from `COMMANDS` — rows, aliases, the first word
/// of every multi-word row, and `teton` — and a test asserts the two agree in
/// both directions, so adding a row without adding it here fails in the crate
/// that owns the row. A hand-written list nothing checks is LESSON-546's shape;
/// a hand-written list a derivation is checked against is a wire contract.
pub const RESERVED_SKILL_NAMES: &[&str] = &[
    "boundary",
    "cd",
    "clear",
    "cost",
    "doctor",
    "effort",
    "exit",
    "help",
    "model",
    "permissions",
    "policy",
    "projects",
    "provider",
    "quit",
    "teton",
    "verbose",
    "web",
];

/// True when the built-in command table claims `name`, so no skill may dispatch
/// under it (BR-2).
#[must_use]
pub fn is_reserved_skill_name(name: &str) -> bool {
    RESERVED_SKILL_NAMES.contains(&name)
}

/// Whether the **user** may reach a listed skill by typing `/name`, and why not
/// when they may not (REQ-587 BR-3).
///
/// Three states, because BR-3 has three and `Option<…>` has two: a caller must
/// be able to tell "another file owns this name" from "this file is the
/// model's, not yours".
///
/// Generic in the shadow payload because the two sides name the shadower
/// differently and legitimately so — the daemon carries a typed `ShadowedBy`,
/// the client a rendered sentence that can be more specific about a built-in it
/// alone has the table for. Only the **precedence** was ever the shared fact,
/// and that is what lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDispatch<S> {
    /// `/name` reaches this file.
    Allowed,
    /// Something else owns the spelling (BR-2). Listed, marked, never
    /// dispatched.
    Shadowed(S),
    /// `user-invocable: false`. Listed and marked, refused from `/name`, and
    /// still the model's — unless the row is model-invocable too, which is a
    /// named state ("invocable by nobody") and not a silent drop.
    ModelOnly,
}

/// [`UserDispatch`] for one row: **shadowing wins over model-only** (BUG-192).
///
/// **One home, because two crates enforced one rule.** This ordering existed
/// twice — `tetond`'s `Skill::user_dispatch` and `teton`'s `user_dispatch(&
/// SkillView)` — with both sides unit-tested and nothing cross-checking them,
/// so the precedence could drift on one side with every suite green
/// (LESSON-528's shape). No in-process bridge could close it: `Skill` is a
/// `tetond` type carrying a `PathBuf`, and a `tetond` dependency in `teton`
/// would invert the daemon/client boundary. Deleting the mirror was the way
/// out, and it works because both inputs are already wire facts on
/// [`SkillView`].
///
/// The order is the decision, not a detail. Only once nothing owns the spelling
/// is `user-invocable: false` the reason, so no surface can read "model-only"
/// off a row whose name resolves to a different file.
///
/// Callers compose their own preconditions **on top**, never folded in — the
/// client resolves its built-in table claim first and passes the result as
/// `shadowed`. That distinction is why the mirror grew a precondition on one
/// side in the first place, and keeping it outside is what lets one rule serve
/// both.
#[must_use]
pub fn user_dispatch<S>(shadowed: Option<S>, user_invocable: bool) -> UserDispatch<S> {
    match shadowed {
        Some(by) => UserDispatch::Shadowed(by),
        None if !user_invocable => UserDispatch::ModelOnly,
        None => UserDispatch::Allowed,
    }
}

/// The permission key a skill's dynamic context asks under: `skill:<source>:<name>`.
///
/// **One home, because two crates enforce one rule.** The daemon mints the key
/// and drops the project-scoped ones when the session root moves (ADR-6); the
/// client memoizes "allow for this session" answers under the *same* string and
/// has to forget them at the same moment. Those are the two halves of one
/// decision, and a decision with two stores needs one invalidation rule — so
/// the spelling and the predicate live here, above both, rather than being
/// written out twice and drifting.
#[must_use]
pub fn skill_permission_key(source: SkillSource, name: &str) -> String {
    format!("{}{name}", skill_permission_key_prefix(source))
}

/// The `skill:<source>:` prefix every key of one source shares.
#[must_use]
pub fn skill_permission_key_prefix(source: SkillSource) -> String {
    let word = match source {
        SkillSource::User => "user",
        SkillSource::Project => "project",
    };
    format!("skill:{word}:")
}

/// True when `key` is a **project** skill's dynamic-context key — the grants a
/// root move invalidates, on either side of the wire.
///
/// A user skill's file is the same file whatever the session root is, so its
/// grant survives; a project skill's name means a different file in a different
/// repo, which is the whole of ADR-6's argument.
#[must_use]
pub fn is_project_skill_key(key: &str) -> bool {
    key.starts_with(&skill_permission_key_prefix(SkillSource::Project))
}

/// The prefix every project-skill **acknowledgment** key starts with.
///
/// Deliberately not `skill:`. See [`project_skill_trust_key`].
pub const PROJECT_SKILL_TRUST_KEY_PREFIX: &str = "project_skill_trust:";

/// The permission key the project-skill acknowledgment is remembered under:
/// `project_skill_trust:<invoker>:<root>` (REQ-587 BR-4, architecture ADR-7,
/// REQ-591 D-7).
///
/// # The invoker is in the key, for `durable_row_for`'s reason (REQ-591 D-7)
///
/// It was `project_skill_trust:<root>` until D-7, and both doors minted it from
/// the same tree: the typed path from `probed.path`, the model's `skill` tool
/// from `ctx.repo_root()`, which `ToolContext::for_root` sets from that same
/// `probed`. One string, so a human answering "allow for this session" to a
/// `/deploy` **they typed** also settled the *model's* door in that tree for the
/// rest of the session, with no second prompt and nothing on any screen saying
/// so.
///
/// That widening is REQ-591's own: before it the typed path had no gate at all
/// and minted no grant, so there was no answer for the model's door to inherit.
///
/// D-2 already decided that the two doors are not the same question — a durable
/// row answers for the typed path and for nothing else. A **session** answer is
/// the same question asked at a shorter range, so it gets the same rule, applied
/// where [`crate::events::InvokedBy`] can be seen rather than at a call site
/// that might forget (LESSON-495: "make the key a function of the level … so
/// adding a level is a compile error rather than a silent grant"). Taking
/// `invoked_by` by value is that compile error.
///
/// The two invoker segments are fixed strings, so the families are disjoint
/// however a root is spelled: no `project_skill_trust:user:X` can equal a
/// `project_skill_trust:model:Y`, and within a door the key is injective in the
/// root exactly as it was before.
///
/// **Not** the level key. [`crate::events::InvokedBy`] scopes the *grant*; the
/// row a level's table decides this family by is `project_skill_trust`, spelled
/// once in the daemon and consulted through `Question::level_key`. They answer
/// different questions and neither is derived from the other.
///
/// **A different question, so a different key.** `skill:<source>:<name>` asks
/// "may these commands run?"; this one asks "may the model run *this
/// repository's* skills as instructions at all?", once per session per root and
/// before any expansion exists. LESSON-495's rule is that the key encodes the
/// question and that a remembered answer frees every later request whose key
/// matches — so folding the two into one string would let a `y` to one question
/// answer the other.
///
/// It is **not a skill key**, and that is load-bearing rather than cosmetic:
/// `authorize_skill` requires its key to be a skill key *and* to equal the key
/// `(source, name)` mints, and an acknowledgment satisfies neither. ADR-7 opens
/// a third gate door rather than widening two guards that are pinned in both
/// directions. The family half of each guard is an ordinary refusal rather than
/// a `debug_assert!`, so it is present in the shipped binary too (REQ-587
/// verify).
///
/// It is also absent from the permission **level table**, on purpose: an
/// unenumerated key falls to the level's default, which is exactly BR-4's
/// posture for this question — `guarded`/`edits` ask, `plan` denies, `full`
/// allows.
///
/// `root` is the session root's **home-relative display**
/// (`session_root::display_for`), never an absolute path: a client that does
/// not recognize the subject renders the request's key on its refusal line, and
/// `/Users/jane/dev/teton` on that line carries a username into a transcript
/// (REQ-585 BR-1's entity table).
///
/// The root is **not** truncated here. A key is matched, never read: two long
/// roots sharing a prefix must not collapse onto one key, or a grant for one
/// repository would answer for another — precisely the harm the per-root scope
/// exists to prevent. Bounding belongs to what is *rendered*, not to what is
/// *compared*.
///
/// # The display is lossy, and that is a known gap in this key
///
/// `display_for` ends in `Path::display`, which renders bytes that are not valid
/// UTF-8 as `U+FFFD`. Two roots differing only in such bytes therefore render
/// identically and mint **one** key here — the same collapse the paragraph above
/// refuses to introduce by truncation, arriving through the input instead.
///
/// The fix is to key on the raw `OsStr` bytes (or a hash of them) and keep the
/// display for the prompt, which is a change where the two are *minted* — the
/// caller that computes `display_for(ctx.repo_root(), …)` and passes it here —
/// not in this function, which never sees a path. Until that lands,
/// `PermissionGate::authorize_project_skill_trust` refuses a root whose display
/// carries `U+FFFD` rather than remembering an answer under an ambiguous name,
/// and `expires_on_session_root_change` bounds the exposure further: the key
/// does not outlive the root it was answered for.
#[must_use]
pub fn project_skill_trust_key(invoked_by: crate::events::InvokedBy, root: &str) -> String {
    format!(
        "{PROJECT_SKILL_TRUST_KEY_PREFIX}{door}:{root}",
        door = trust_door_segment(invoked_by)
    )
}

/// The key segment naming which door asked (REQ-591 D-7) — **the one spelling**,
/// read by [`project_skill_trust_key`] and by [`is_project_acknowledgment_key`].
///
/// An exhaustive `match`, so a third [`crate::events::InvokedBy`] is a compile
/// error here rather than a door whose grants quietly stop expiring on `/cd`.
/// [`TRUST_DOORS`] is the enumeration the predicate walks; keep the two together.
const fn trust_door_segment(invoked_by: crate::events::InvokedBy) -> &'static str {
    match invoked_by {
        crate::events::InvokedBy::User => "user",
        crate::events::InvokedBy::Model => "model",
    }
}

/// Every door a project-skill acknowledgment can be asked at (REQ-591 D-7).
///
/// Walked by [`is_project_acknowledgment_key`], which cannot `match` on a `&str`
/// back into the enum. `every_door_round_trips_through_the_acknowledgment_key`
/// is what keeps this list and [`trust_door_segment`] in step.
const TRUST_DOORS: [crate::events::InvokedBy; 2] = [
    crate::events::InvokedBy::User,
    crate::events::InvokedBy::Model,
];

/// True when `key` is a project-skill **acknowledgment** key (REQ-587 BR-4,
/// REQ-591 D-7).
///
/// Three things must hold, and each is a way the string could name no question:
/// the family prefix, a door segment [`trust_door_segment`] produces, and a
/// **non-empty** root after it. A bare prefix names no root, and a grant under
/// it would be an answer to nothing — the rule `is_skill_permission_key` already
/// applies to a bare `skill:project:`.
///
/// The door check is not decoration. Without it `project_skill_trust:model:`
/// reads as an acknowledgment key, clears
/// `PermissionGate::authorize_project_skill_trust`'s family guard, and reaches a
/// `debug_assert` — which in a release build is no guard at all, leaving an
/// answer remembered under a key naming no repository.
#[must_use]
pub fn is_project_acknowledgment_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix(PROJECT_SKILL_TRUST_KEY_PREFIX) else {
        return false;
    };
    let Some((door, root)) = rest.split_once(':') else {
        return false;
    };
    !root.is_empty() && TRUST_DOORS.iter().any(|&d| trust_door_segment(d) == door)
}

/// True when a session root move invalidates `key` — **the** invalidation rule,
/// spelled once above both crates (ASSUME-017).
///
/// Two families expire on `/cd`, and no others: a project skill's
/// dynamic-context grant ([`is_project_skill_key`]) and the project-skill
/// acknowledgment ([`is_project_acknowledgment_key`]). A user skill's grant
/// survives, because its file is the same file whatever the session root is.
///
/// **Why this is a function and not a `starts_with` at each call site.** The
/// daemon drops its grants when the root moves; the client drops its
/// `SessionGrants` memo of the same answers, and it consults that memo *before*
/// drawing any prompt. When the two disagree about which keys expire, the
/// client auto-answers the new root's question with the old root's answer and
/// no human is ever shown anything — ASSUME-017, reached in REQ-585 by writing
/// the rule out twice. A security decision with two stores needs one
/// invalidation rule, and the rule belongs above both.
#[must_use]
pub fn expires_on_session_root_change(key: &str) -> bool {
    is_project_skill_key(key) || is_project_acknowledgment_key(key)
}

/// `serde`'s `default` for a flag whose **absence means yes**.
fn absent_means_yes() -> bool {
    true
}

/// The `skip_serializing_if` companion to [`absent_means_yes`]: the key rides
/// only when the flag is `false`, so the ordinary row's bytes never change.
fn is_yes(flag: &bool) -> bool {
    *flag
}

/// One registered skill, as a client sees it (REQ-585 BR-3, ADR-1).
///
/// This is the whole of what the CLI holds: enough to classify a `/name` line
/// and to print a `/help` row, and nothing more. The body never crosses the
/// wire — the daemon expands it (ADR-3), so a client cannot compose a turn out
/// of file bytes it was handed.
///
/// `description` and `argument_hint` **are file bytes** and are treated as
/// such at both ends: the daemon bounds them with `session_root::bounded_field`
/// before they go on the wire, and the client defuses again at render through
/// `Surface::line`. Two layers, each where the frame is authored — ADR-009's
/// shape, and LESSON-517's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillView {
    /// The dispatchable spelling: the directory name (`skills/<name>/SKILL.md`)
    /// or the file stem (`commands/<name>.md`). A frontmatter `name` that
    /// differs creates no second spelling (BR-2).
    pub name: String,
    /// Which root it came from.
    pub source: SkillSource,
    /// The frontmatter `description`, bounded and one-line; absent when the
    /// file declares none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The frontmatter `argument-hint`, bounded and one-line; absent when the
    /// file declares none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// What owns this name instead, when something does — a built-in row, a
    /// project skill, or a `skills/` entry beating a `commands/` one (BR-2,
    /// ADR-6). `Some` means **listed but never dispatchable**: `/help` marks
    /// the row and `classify` must not return it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowed: Option<String>,
    /// Whether the model may invoke this skill through the `skill` tool
    /// (REQ-587 BR-3).
    ///
    /// `false` for a skill whose frontmatter says `disable-model-invocation:
    /// true` — absent from the roster, absent from the listing, and a model
    /// call naming it refused before the expander is asked.
    ///
    /// **Absent means `false`**, which is both the compat reading and the safe
    /// one. A daemon predating REQ-587 has no `skill` tool at all, so nothing
    /// it lists is model-invocable, and a client defaulting this to `true`
    /// would print REQ-587's marks for a capability that build does not have.
    /// It is equally the value BR-3 gives a *malformed* flag — hidden from the
    /// model, invocable by the user — so a typo in a repository's frontmatter
    /// can never widen what the model may run.
    ///
    /// Both flags `false` is a real state and a named diagnostic rather than a
    /// silent drop: a skill invocable by nobody.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub model_invocable: bool,
    /// Whether the user may dispatch this skill by typing `/name` (REQ-587
    /// BR-3).
    ///
    /// `false` for a skill whose frontmatter says `user-invocable: false` —
    /// model-only, which `/help` **marks** rather than hides (REQ-585 BR-3
    /// holds: `/help` never shows a *dispatchable* entry the table does not
    /// resolve, and a model-only entry is not dispatchable by the user), and
    /// which `classify` refuses with a hint naming the flag.
    ///
    /// **Absent means `true`** — the opposite default to
    /// [`Self::model_invocable`] and for the same two reasons: it is what a
    /// daemon predating REQ-587 meant by listing a skill at all, and it is
    /// BR-3's safe value. The wire therefore carries this key only for the
    /// unusual skill, and every ordinary row is byte-identical to the bytes
    /// REQ-585 wrote.
    #[serde(default = "absent_means_yes", skip_serializing_if = "is_yes")]
    pub user_invocable: bool,
}

/// One entry discovery found and did not register, with why (REQ-585 BR-1).
///
/// Named, never silent: BR-1's rule is that an unreadable, malformed,
/// mis-named or oversized file is *counted and named*, because a skill that
/// vanishes without a diagnostic is the LESSON-481 shape — a feature the user
/// cannot see is one the suite cannot see either. A missing directory is the
/// normal case and produces no entry, and a directory with no `SKILL.md` is
/// not a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSkipped {
    /// The name this entry **would have dispatched under**, empty when it names
    /// no skill at all (a root-level refusal or truncation, or an entry whose
    /// spelling was never a candidate).
    ///
    /// Named by the daemon rather than re-derived from [`Self::path`] by every
    /// client that needs it. BR-2's rule — a `skills/` entry is named by its
    /// directory, a `commands/` entry by its file stem — belongs to the side
    /// that owns discovery; a client re-deriving it from a display path is a
    /// second home for that rule in a crate that cannot see the four roots, and
    /// two spellings of one decision are identical only until one of them is
    /// edited (LESSON-528).
    ///
    /// **Untrusted, and bounded as such**: unlike [`SkillView::name`] — which is
    /// dispatchable and therefore matched `^[a-z0-9][a-z0-9_-]{0,63}$` before it
    /// was registered — this is whatever the filesystem spelled, *including* the
    /// invalid spellings that are why the entry was skipped. The daemon
    /// neutralizes and bounds it exactly as it does the description.
    ///
    /// Additive (REQ-585 ADR-2): a result from a daemon predating the field
    /// carries no key and reads empty, and an entry that names nothing emits no
    /// key rather than `""`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The file, **relative** and bounded, exactly as [`SkillView`]'s
    /// description is — from the session root for a `project` entry, from the
    /// home folder for a `user` one, the rule
    /// [`crate::events::SkillInvoked::path_display`] states.
    ///
    /// Not an absolute path: BR-1's entity table says a skill path is never
    /// shown as one, because `/Users/jane/.claude/skills/broken/SKILL.md`
    /// carries a username into a transcript and
    /// `/tmp/ci-4f2a/repo/.claude/skills/broken/SKILL.md` carries the working
    /// tree's location (BUG-187), and AC-6 puts these entries on a
    /// user-visible surface.
    pub path: String,
    /// Why it was skipped, in the daemon's own words — `unreadable (permission
    /// denied)`, `over 64 KiB (67,184 B)`, `not UTF-8`, `malformed
    /// frontmatter`, `invalid name`, `symlink not followed`, `shadowed by
    /// <what>` (ADR-4).
    pub reason: String,
}

/// List the skills this session would dispatch (REQ-585 BR-3, ADR-1/ADR-2).
///
/// **This method is the version handshake.** A client calls it after
/// `session/create` and again after every `session_root_changed`; a daemon
/// that answers [`crate::jsonrpc::error_code::METHOD_NOT_FOUND`] yields an
/// **empty** snapshot rather than an error, which makes `classify` incapable
/// of returning a skill and leaves a new CLI against an old daemon behaving
/// byte-for-byte as it does today. The capability is proven by a successful
/// call, never asserted from a version number — which is why
/// [`crate::PROTOCOL_VERSION`] does not move for any of REQ-585's additions.
///
/// It carries a `session_id` because half the answer is derived from the
/// session root (BR-1's two project globs), and the root moves under `/cd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsListParams {
    /// The session whose registry to report.
    pub session_id: SessionId,
}

/// Result of [`SkillsListParams`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsListResult {
    /// Every registered skill, including the shadowed ones — `/help` lists
    /// them and `classify` refuses them, and both need to see the same rows
    /// (BR-3: a skill cannot be dispatchable without appearing in `/help`).
    ///
    /// Ordered by the daemon, by name: APFS lists in hash order and ext4 does
    /// not, so an order the client re-derived would be a platform-flaky
    /// `/help` (LESSON-540).
    #[serde(default)]
    pub skills: Vec<SkillView>,
    /// Everything found and not registered. Rides the same result as `skills`
    /// so `/help`'s diagnostic line and BR-10's unknown-command hint read one
    /// list rather than two that can disagree.
    #[serde(default)]
    pub skipped: Vec<SkillSkipped>,
}

/// Ask the daemon for this machine's known projects (REQ-584 BR-9).
///
/// **The CLI never reads the registry file** (the REQ's Permissions table): the
/// daemon owns it, and `/projects` is a request rather than a read. That is
/// also what makes the scan happen in the one place BR-3 bounds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectsListParams {
    /// Optional filter, matched exactly as the `projects` tool matches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Whether the caller will accept BR-3's dev-folder scan when the registry
    /// cannot answer (REQ-584 BR-10).
    ///
    /// `/projects` sets it (the user asked); the **launch notice** does not,
    /// because BR-10 says the notice must not trigger a scan and BR-3 says a
    /// scan happens only when something asks for projects. A launch that walked
    /// eleven directories to decorate a warning would be the opposite of what
    /// REQ-583 set out to fix.
    ///
    /// Defaults to **true** so the ordinary request keeps its behaviour and an
    /// older client's params mean what they meant.
    #[serde(default = "crate::methods::default_true")]
    pub allow_scan: bool,
}

/// Serde default for [`ProjectsListParams::allow_scan`].
#[must_use]
pub fn default_true() -> bool {
    true
}

/// Result of [`ProjectsListParams`] — the rendered locator answer.
///
/// **Rendered text, not rows.** BR-9 requires one renderer for the tool's
/// output and the CLI's, and shipping rows here would invite the client to
/// build a second one. The CLI may style what it is given; it does not restate
/// it (REQ-582's rule).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectsListResult {
    /// The locator answer, already composed.
    pub rendered: String,
}

impl RpcMethod for ProjectsListParams {
    const METHOD: &'static str = "projects/list";
    type Result = ProjectsListResult;
}

impl RpcMethod for SkillsListParams {
    const METHOD: &'static str = "skills/list";
    type Result = SkillsListResult;
}

/// Ask which of a session's skills will not fit on the route it is on
/// (REQ-589 BR-13, ADR-11).
///
/// `skills/list`'s sibling, and the split is the question rather than the data:
/// that one reports the registry, this one reports a **measurement** of the
/// registry against the session's stamped route budget. They are two methods
/// because a `/help` listing must not pay for a measurement, and because a
/// daemon may have the first and not the second — the capability is proven by a
/// successful call here exactly as it is there, so neither
/// [`crate::PROTOCOL_VERSION`] nor [`crate::PROTOCOL_VERSION_MIN`] moves for
/// this addition and a client whose daemon answers
/// [`crate::jsonrpc::error_code::METHOD_NOT_FOUND`] reports a pending
/// capability rather than an error.
///
/// It carries a `session_id` for `skills/list`'s reason — half the answer comes
/// from the session root, which moves under `/cd` — and the route half comes
/// from the same session's stamped budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsPreflightParams {
    /// The session whose registry and stamped route to report on.
    pub session_id: SessionId,
    /// Whether the asking surface is in `/verbose` (REQ-589 AC-19).
    ///
    /// The count of skills that will not fit is reported either way; this is
    /// what adds the route's budget and bound beside it. It rides in the
    /// **params** rather than being applied client-side because the side that
    /// holds the budget is the side that words it — a client formatting the
    /// pair itself would be a second spelling of a figure the daemon already
    /// composes (LESSON-456).
    ///
    /// `#[serde(default)]` so a caller that omits it means "not verbose",
    /// which is what every pre-REQ-589 surface meant.
    #[serde(default)]
    pub verbose: bool,
}

/// Result of [`SkillsPreflightParams`] — the pre-flight answer, already
/// composed.
///
/// **Rendered text, not rows**, on [`ProjectsListResult`]'s precedent and for
/// its reason, with one addition specific to this REQ: every figure in the
/// report comes out of the daemon's one skill-budget composer, measured against
/// the budget the router stamped. Shipping rows would invite a client to build
/// a second sentence from them, and a surface naming a budget the turn was not
/// running under is precisely the defect REQ-586's verify pass found. The CLI
/// may style what it is given; it does not restate it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsPreflightResult {
    /// The report, one fact per line.
    pub rendered: String,
}

impl RpcMethod for SkillsPreflightParams {
    const METHOD: &'static str = "skills/preflight";
    type Result = SkillsPreflightResult;
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

/// A user-typed skill invocation, carried as a name and the rest of the line
/// (REQ-585 BR-4, architecture ADR-3).
///
/// **The invocation crosses the wire as a name, never as an expansion.** The
/// client never composes the body, which keeps the untrusted file bytes on the
/// side of the seam that sanitizes them (LESSON-517), and it never puts the
/// typed `/name …` line in `prompt` either — so a daemon that dropped this
/// field yields a visible empty turn, not a leaked command line reaching a
/// model.
///
/// Deliberately **not** a [`PromptBlock`] variant: that enum is
/// `#[serde(tag = "type")]`, where an unknown tag is a deserialization failure
/// rather than a degrade — and an invocation is not prompt content in the
/// first place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvocation {
    /// The skill's dispatchable name, as [`SkillView::name`] spells it.
    pub name: String,
    /// The rest of the typed line, **verbatim** — interior whitespace
    /// preserved, quotes uninterpreted, the line's edges trimmed as the
    /// classifier trims today.
    ///
    /// This is the one place the session does not use REQ-582 ADR-2's
    /// tokenization (BR-4), so it must never be re-joined from a token list
    /// anywhere on the path: `/alpha teton  code "repo"` reaches `$ARGUMENTS`
    /// with both interior spaces and both quotes intact, and a re-join would
    /// silently normalize them.
    pub raw_arguments: String,
}

/// Submit a prompt turn to a session. ACP equivalent: `session/prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTurnParams {
    /// Target session.
    pub session_id: SessionId,
    /// The prompt, as an ordered list of content blocks.
    pub prompt: Vec<PromptBlock>,
    /// A skill invocation to expand into this turn instead of `prompt`
    /// (REQ-585 BR-4, ADR-3).
    ///
    /// Additive, and absent on every turn a pre-REQ-585 client sends. Exactly
    /// one of `prompt`/`skill` is populated: the daemon refuses
    /// [`crate::jsonrpc::error_code::INVALID_PARAMS`] when **both** are, a
    /// combination that was never valid so nothing is narrowed. A request with
    /// *neither* is still accepted — `flatten_prompt(&[])` is `""` and such a
    /// turn runs today, so rejecting it would narrow an existing method for
    /// third-party clients while [`crate::PROTOCOL_VERSION`] is asserted
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillInvocation>,
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
    // The one method in this file that is a turn — the client's markdown fence
    // is dropped on its response and on no other (REQ-592 BR-8 / ADR-3).
    const ENDS_TURN: bool = true;
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
        ///
        /// **Single-choice, and it stays that way.** REQ-589's over-budget
        /// offer is two independent answers — send this turn's expansion, and
        /// write the going-forward fix — and its ADR-1 expresses all four
        /// combinations as four named ids on this field rather than widening
        /// this enum for one caller (see
        /// [`crate::events::OPTION_ID_OVER_BUDGET_PROCEED_ONCE`] and its three
        /// siblings). That is ASSUME-B's promise, and
        /// `permission_outcome_did_not_widen_for_the_over_budget_offer` is what
        /// keeps it: a second field here would be a wire change every client
        /// has to be taught, to carry a fact a string already carries.
        option_id: String,
    },
    /// The user dismissed the prompt without choosing.
    Cancelled,
    /// The client refused the request **without asking anyone** (REQ-585
    /// BR-11, architecture ADR-7/ADR-8).
    ///
    /// **Why this is not [`PermissionOutcome::Cancelled`].** `Cancelled`
    /// already means *the user dismissed the prompt* — it is what EOF on a
    /// pipe returns — so folding these into it would say a human declined when
    /// no human was ever reachable. AC-9 requires the not-run placeholders to
    /// say *no human could be asked*, which the daemon cannot know from a
    /// dismissal, and BR-11's whole point is that the refusal happens
    /// **before** `prompter.ask` reads a line: a refusal computed after that
    /// call has already eaten the user's next prompt line and turned a pasted
    /// `y` into consent (LESSON-537).
    ///
    /// Additive on a tagged enum, so it travels in one direction only and is
    /// only ever sent to a daemon that answered `skills/list` (ADR-2's
    /// handshake). A pre-REQ-585 daemon has no `refused` arm and would refuse
    /// the params outright — which is why the handshake, not serde tolerance,
    /// is what gates it.
    Refused {
        /// Which of BR-11's two closed doors this was.
        reason: RefusalReason,
    },
}

/// Why a client refused a permission request without asking (REQ-585 BR-11).
///
/// Closed on purpose: it is read by the daemon, which composes AC-9's
/// placeholder sentence from it, and a reason it cannot render is a refusal it
/// cannot explain. A future client inventing a third door fails the params
/// rather than having its answer silently rendered as one of these two.
///
/// **What "fails the params" actually costs** (BUG-186). The whole
/// `permission/respond` fails to deserialize, so the daemon answers
/// `INVALID_PARAMS` and the waiter is neither resolved nor withdrawn: the
/// prompt stays open and `rx.await` keeps waiting, with no timeout of its own.
///
/// That is the intended outcome, not an oversight, and it is the *same* rule
/// `handle_permission_respond` documents for a refusal it rejects: an answer
/// the daemon cannot act on must not consume the question. Withdrawing the
/// waiter here would be strictly worse in two ways. The parse is what failed,
/// so the `request_id` is not reliably in hand — there is no dependable
/// identity to withdraw. And if it were, a malformed message would become a
/// way to cancel any session's standing prompt, which is the denial of service
/// dressed as a safety check that the refusal path exists to prevent.
///
/// So the turn parks exactly as long as it would have if the client had simply
/// not answered yet, which is the ordinary waiting state. The client holds the
/// remedy: it gets a typed error and can re-send a well-formed answer against
/// the still-standing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// There is no terminal to ask at — piped stdin, at a level that would ask
    /// (BR-11). The commands are not run and the next stdin line stays the
    /// next prompt line.
    NoTerminal,
    /// The request carried a subject this client does not recognize, so it
    /// refused rather than falling through to `prompter.ask` (ADR-7's
    /// fail-closed rule; see [`crate::events::PermissionSubject::Unrecognized`]).
    UnrecognizedSubject,
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
    /// The provider's declared context window in tokens —
    /// `capabilities.max_context` (REQ-586 BR-3).
    ///
    /// On a snapshot, the daemon **always populates** this field: `Some(0)`
    /// means "unknown / unset — the budget is defaulted", which `/doctor` and
    /// `/provider list` state rather than hide. `None` means the snapshot came
    /// from a daemon that predates the field — the `RouteDecided::effort` rule,
    /// so `Option` is for **wire additivity only** and moves neither
    /// [`crate::PROTOCOL_VERSION`] nor [`crate::PROTOCOL_VERSION_MIN`].
    ///
    /// On a `RegisterProvider` update, `Some(v)` writes the window and `None`
    /// preserves whatever is stored (an older client's re-registration cannot
    /// zero a declared window — architecture ADR-7, field-wise merge).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_context: Option<u32>,
    /// A user ceiling on the context budget, in tokens, below the window —
    /// `capabilities.context_budget_cap` (REQ-586 BR-5). `Some(0)` is "no cap".
    /// Same additivity and merge rule as [`Self::max_context`]; a cap above the
    /// window is inert, not invalid (ADR-7).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context_budget_cap: Option<u32>,
    /// The pair turns to this provider actually run under, present **only**
    /// when the derivation had to **raise** it off this provider's own
    /// declaration (REQ-586 TASK-194 2b) — a snapshot field the daemon owns,
    /// and one a client never sends.
    ///
    /// The one fact `/doctor`'s advisory cannot compute: whether the floor bit
    /// depends on the generation reservation and the two budget ratios, which
    /// live in the daemon's derivation and have exactly one home there
    /// (LESSON-456). So the daemon answers it and the client renders the
    /// answer, the way the `window:` column renders [`Self::max_context`].
    ///
    /// `None` on a `RegisterProvider` update (there is nothing to declare here
    /// — the daemon ignores whatever a client puts in it), `None` on a
    /// snapshot from a daemon predating the field, and `None` on a provider
    /// whose budget was not floored. All three render nothing, which is why one
    /// spelling covers them.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub floored_budget: Option<FlooredBudget>,
}

/// The budget a provider whose declaration fell **below the floor** actually
/// runs under (REQ-586 TASK-194 2b).
///
/// The floor is the smallest budget that can still hold the harness's own
/// system prompt; a window or a `context_budget_cap` deriving under it is
/// raised to it, so the turn gets *more* than the declaration asked for. That
/// is a deliberate degradation with a documented cost — a budget that cannot
/// hold the system prompt would fail every turn instead — and this is what
/// carries it to a surface.
///
/// Carried as a pair rather than as a boolean because the advisory that renders
/// it has to say *what the turn gets instead* — "2,048 words / 16 KB" — and
/// those two numbers are the daemon's derivation to state, not the client's to
/// compute. Only one currency may have been raised, so this is the derived pair
/// rather than the floor constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlooredBudget {
    /// The word budget in force.
    pub budget_tokens: u64,
    /// The byte budget in force.
    pub budget_bytes: u64,
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
    /// The one sentence a registration that records a **big context window**
    /// earns (REQ-586 TASK-194, OQ-6 as amended): what one call to this
    /// provider may now carry, what one prompt may spend at worst, and the key
    /// that would bound it.
    ///
    /// Composed by the daemon, not by the client, because every figure in it
    /// comes from `harness::budget::derive` — the same derivation the router
    /// runs, and one no thin client may repeat (BR-8, AC-12). `/provider
    /// setup`'s preview carries the identical sentence in its own warning list;
    /// this field is how `teton provider add --max-context` gets it, so the two
    /// surfaces cannot drift into two wordings of one fact.
    ///
    /// `None` for every update that records no window above the threshold, and
    /// from a daemon that predates the field — both render nothing, which is
    /// exactly today's output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub budget_notice: Option<String>,
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
    /// Of [`Self::total_calls`], how many were connection tests (REQ-581 BR-5).
    ///
    /// A **subset** of the total, never a separate tally added to it: a probe is
    /// an ordinary model call, sent down the same path and priced from the same
    /// table, and the ledger counts it as one. What this field buys is the
    /// sentence `teton cost` can then print — "1 probe" — so a user reading a
    /// call they never asked a question for does not read it as a turn.
    ///
    /// `#[serde(default)]`, like [`Self::unpriced_models`]: a daemon built
    /// before REQ-581 sends no such key, and `0` is the honest reading of that
    /// silence rather than a guess — a daemon with no `provider/test` recorded
    /// no probes because it could not make one.
    #[serde(default)]
    pub probe_calls: u64,
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
    /// Whether this backend needs a key at all — what a client **may** default
    /// its key question to, stated by the daemon rather than re-derived from
    /// whether [`Self::auth_template`] happens to be present.
    ///
    /// A default a client is free not to take: today's CLI asks the key question
    /// unconditionally, with a yes default of its own, and uses this field only
    /// to say `(no key)` or `(needs a key)` beside the suggestion (REQ-572
    /// parity). The field is the daemon's statement of fact about the backend;
    /// what a client does with it at a prompt is the client's.
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

// ---------------------------------------------------------------------------
// guided provider setup (REQ-579)
// ---------------------------------------------------------------------------
//
// The second instance of the shape above, and deliberately a *copy* of it
// rather than a generalisation (REQ-579 ADR-1). Three stateless endpoints —
// plan, preview, commit — with no flow state on this wire, for the reason the
// web trio has none: the client collects answers locally and the daemon stays
// the sole authority on validation, on what the preview says, and on the write.
//
// A dedicated trio rather than riding `config/set`, because registering a
// provider and routing a tier to it is **one** durable write or it is a window
// in which the provider exists unrouted (BR-3, ADR-1) — and because a preview
// has to return the exact bytes a digest was taken over, which a general
// mutation RPC has nowhere to put.
//
// Like the web trio these are **client** RPCs and never harness tools, which is
// BR-12's structural half: tool dispatch and the client socket are distinct
// channels, so a model emitting a tool call named `provider/setup_commit`
// reaches nothing at all.
//
// Types only — the catalog, the candidate `Config`, the validator, the
// comment-preserving write and the routing re-derivation land with the daemon
// (TASK-153/154), and the walkthrough with the CLI (TASK-155).

/// One vendor recipe, served to a client as **data** (REQ-579 BR-4).
///
/// 1:1 with the daemon's `provider_recipes::ProviderRecipe` — same field names,
/// same types, same optionality — because the entry a client renders has to be
/// the entry the model would have named (ADR-4). The mapping is asserted total
/// on the daemon side, where both types are in scope; this crate depends on no
/// other teton crate (see the manifest test in [`crate`]), so what is pinned
/// here is the field set itself.
///
/// No `Default`, for [`WebBackendSuggestion`]'s reason: a recipe built out of
/// nothing is one a client could render as if a daemon had shipped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRecipeEntry {
    /// The id to offer as the default answer — the `<id>` of
    /// `teton provider add <id>`, and the name the routing step then binds a
    /// tier to. A suggestion, not a reservation: ids are the user's namespace.
    pub id_suggestion: String,
    /// The vendor's display name, spelled the way the vendor spells it.
    pub label: String,
    /// The same vendor, spelled the way the **bundled guide's** recipe line
    /// spells it — `Moonshot/Kimi` where [`Self::label`] is `Moonshot (Kimi)`.
    ///
    /// Carried onto the wire because the lenient vendor resolver matches
    /// against it (ADR-2): the model teaches a user the guide's spelling, so
    /// `/provider setup Moonshot/Kimi` has to land on this entry without the
    /// client keeping a second spelling table of its own (BR-4).
    pub guide_spelling: String,
    /// Which adapter the vendor speaks, and therefore which questions the flow
    /// asks — `anthropic` composes its own address and skips the endpoint
    /// prompt (ADR-7).
    pub kind: ProviderKind,
    /// The **absolute request URL** to offer, character for character, with no
    /// path joined on — not a base URL, which is the BUG-170 registration that
    /// 404s on first use.
    ///
    /// `Option` because [`ProviderKind`] is wider than this catalog and admits
    /// a kind carrying its own address; absent from the wire when there is
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// A model the vendor serves today, offered as the default answer to the
    /// model question (BR-6) and labeled as an example — never as a
    /// recommendation and never as "the current best".
    pub example_model: String,
    /// One bounded clause for a fact the recipe alone does not say, or `None`
    /// when it says everything. Absent from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The context window, in tokens, of [`Self::example_model`] — what
    /// `/provider setup` records as `capabilities.max_context` so the budget
    /// follows the route from the first turn (REQ-586 BR-3). Never `0` in the
    /// shipped catalog (the daemon's contract test pins that); `0` is what an
    /// entry from a daemon predating the field reads as, which is the
    /// "unknown" spelling the config already uses — so the field is not
    /// optional on the wire, and a client never has to tell absent from unset.
    #[serde(default)]
    pub max_context: u32,
}

/// A provider the config already holds, for the flow to show before it offers
/// to replace one (REQ-579 BR-14).
///
/// Three non-secret fields and nothing else: no `auth_ref`, no endpoint. The
/// question this answers is "does the id you just typed already exist, and as
/// what" — the BUG-155 silent replace-or-insert is what naming it prevents —
/// and neither a reference nor a URL is needed to answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingProvider {
    /// The configured id.
    pub id: ProviderId,
    /// Which adapter it speaks.
    pub kind: ProviderKind,
    /// The model it is pinned to, or `None` for a record that has none.
    ///
    /// `Option` because the config's own field is one: a provider missing its
    /// model is **incomplete, not invalid** — `Config::validate` is structural
    /// and lets it load, a separate pass marks it unusable, and the point of
    /// use refuses it (conventions.md; REQ-557 ADR-E, LESSON-506). A required
    /// `String` here would make this call unable to describe the very record a
    /// user is most likely to be fixing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What one routable tier is bound to today, for the routing question to show
/// (REQ-579 BR-7).
///
/// Distinct from [`TierBinding`] by exactly its `Option`: this is an *answer*
/// about the current world, in which "nothing is bound to `scan`" is a real and
/// common state, where a binding is a *request* and always names a provider.
/// Folding them into one type would make the unbound tier and the bound-to-
/// nothing binding the same value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierSummary {
    /// The tier.
    pub tier: Tier,
    /// What it currently routes to, or `None` when nothing is bound. Absent
    /// from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<ProviderId>,
    /// The row's configured fallback, when it carries one (REQ-579 OQ-2).
    ///
    /// Reported even though this flow asks no fallback question, because the
    /// flow **rewrites whole rows**: a `[[tiers]]` row this walkthrough re-binds
    /// keeps the fallback the user configured elsewhere (`teton policy
    /// set-tier --fallback`), and a routing question asked against a summary
    /// that omitted it would describe a row the file does not have.
    ///
    /// `#[serde(default)]` for [`provider_id`](Self::provider_id)'s reason and
    /// one more: a client built after this field existed still has to read a
    /// daemon built before it, and "the daemon did not say" and "no fallback"
    /// are the same actionable fact for a surface that only renders it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_id: Option<ProviderId>,
}

/// One tier→provider binding the candidate would write (REQ-579 BR-7).
///
/// Carries its own `provider_id` rather than being implied by the candidate's
/// id: the commit writes `[policy.tiers.<tier>]` rows, and a row that named its
/// provider only by position in a list is the by-index edit LESSON-522 exists
/// about. It is also what lets the commit result report what actually landed
/// rather than what was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBinding {
    /// The tier being routed.
    pub tier: Tier,
    /// The provider it routes to — normally the candidate's own id, and
    /// spelled out anyway so the row is readable on its own.
    pub provider_id: ProviderId,
}

/// Ask what registering a provider would involve (REQ-579 BR-3, BR-4).
///
/// Read-only, and the flow's first step: the vendor recipes this build ships,
/// the providers already configured, and what each routable tier points at
/// today. A client can render all of it as instructions and stop there, which is
/// BR-11's degradation path — the walkthrough is an enhancement over this
/// answer, never the only way to reach it.
///
/// It carries a `session_id` for [`WebSetupPlanParams`]'s reason: the gate that
/// admits it is session attachment, so the call has to name the session it
/// claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupPlanParams {
    /// The session asking.
    pub session_id: SessionId,
}

/// Result of [`ProviderSetupPlanParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupPlanResult {
    /// The vendor recipes this build ships, in the order a client should show
    /// them, from the same typed source the model's guide is gated against
    /// (BR-4, ADR-4).
    ///
    /// Required, unlike [`WebSetupPlanResult::suggestion_catalog`]: the catalog
    /// and this method ship together, so there is no daemon that can answer
    /// `provider/setup_plan` and have no catalog. A `#[serde(default)]` here
    /// would let a malformed answer read as "this build knows no vendors",
    /// which is a sentence no daemon has any way to mean.
    pub catalog: Vec<ProviderRecipeEntry>,
    /// The providers already configured, so the flow can say "that id already
    /// exists" before the user types a key for it (BR-14).
    ///
    /// Empty is the fresh-install truth this REQ exists for, and the field
    /// defaults to it rather than being an `Option` — "no providers" and "the
    /// daemon did not say" are the same actionable fact here, unlike a missing
    /// catalog.
    #[serde(default)]
    pub existing: Vec<ExistingProvider>,
    /// What each routable tier points at today — the current state the routing
    /// question is asked against (BR-7).
    #[serde(default)]
    pub tiers: Vec<TierSummary>,
}

impl RpcMethod for ProviderSetupPlanParams {
    const METHOD: &'static str = "provider/setup_plan";
    type Result = ProviderSetupPlanResult;
}

/// A candidate provider registration, as the client collected it (REQ-579
/// BR-3).
///
/// Sent to both [`ProviderSetupPreviewParams`] and
/// [`ProviderSetupCommitParams`], so the commit re-derives from the same
/// answers rather than trusting a blob the preview handed back (BR-9,
/// LESSON-501).
///
/// No `Default`, deliberately and specifically: an empty candidate would be
/// constructible without a [`key_ref`](Self::key_ref), and a missing credential
/// reference must be a compile-visible omission at every call site rather than
/// an empty string that reaches the validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupCandidate {
    /// The id to register under — the user's namespace, and the keychain
    /// account name (ADR-5).
    pub id: ProviderId,
    /// Which adapter the provider speaks.
    pub kind: ProviderKind,
    /// The **absolute request URL**, already composed from whatever the user
    /// typed by `teton_core::compose_endpoint` and already echoed back to them
    /// (BR-5, ADR-8). `None` for a kind that carries its own address, which is
    /// the `anthropic` case ADR-7 skips the prompt for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The model to pin. Required, never inferred from the id (BR-6, REQ-557
    /// BR-1) — a `String` rather than an `Option` because a candidate without
    /// one is refused before the key is ever asked for, so there is no legal
    /// value of this field to be absent.
    pub model: String,
    /// A **keychain reference, never a key value** — e.g.
    /// `keychain://teton/kimi` (BR-2, ADR-5).
    ///
    /// The secret's whole lifecycle stays in the client process: it is read
    /// echo-off, written to the OS keychain, and what crosses this socket is
    /// the name of the row. The daemon refuses any candidate whose `key_ref`
    /// does not parse as a reference — the same structural rule
    /// `Config::validate` already applies to `auth_ref` — so the rule is
    /// enforced rather than requested.
    ///
    /// Required, and required for the reason the type carries no `Default`: a
    /// provider registered with no way to authenticate is a row that fails on
    /// first use, and the omission should be visible where it is made.
    pub key_ref: String,
    /// The tiers to route to this provider, zero or more (BR-7).
    ///
    /// Empty is legal and is a stated outcome, not a degenerate one: a user may
    /// decline every binding and end with a registered-but-unrouted provider,
    /// which the flow then says plainly. A list rather than an `Option` so
    /// "route nothing" has one spelling.
    #[serde(default)]
    pub bindings: Vec<TierBinding>,
    /// The context window to record as `capabilities.max_context`, in tokens —
    /// the recipe's default, carried silently by the setup UI (REQ-586 BR-3,
    /// architecture ADR-9). `None` leaves the window unknown, which is the
    /// honest outcome for a candidate built from no recipe, and what a client
    /// predating the field sends; absent from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u32>,
}

/// Show exactly what registering the candidate would write, without writing it
/// (REQ-579 BR-9).
///
/// The daemon builds the candidate `Config`, runs the **same** `Config::validate`
/// startup runs, and renders the delta through the same comment-preserving
/// writer the commit uses — so the preview is the bytes, not a description of
/// them. Nothing is written and nothing is remembered: a preview the user
/// abandons leaves the daemon exactly as it found it, and no key has been stored
/// yet either (BR-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupPreviewParams {
    /// The session previewing.
    pub session_id: SessionId,
    /// The candidate to render.
    pub candidate: ProviderSetupCandidate,
}

/// Result of [`ProviderSetupPreviewParams`] — what the commit would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupPreviewResult {
    /// The `[[providers]]` table and any `[policy.tiers.<tier>]` rows,
    /// serialized exactly as they would be written (BR-9). The user confirms
    /// these bytes, and the commit re-derives them from the same candidate — so
    /// what was agreed to is what lands.
    pub toml: String,
    /// The host the endpoint parsed to, from the **dial-time** parser rather
    /// than a second one (BR-5, LESSON-528/529): the string shown at the
    /// confirm step is the destination the request builder would actually
    /// reach, not a separately-parsed lookalike.
    ///
    /// Carries the host, **plus `:port` when the endpoint states one
    /// explicitly** — and never userinfo, path, or query. The exclusions are
    /// the reason the whole web event family excludes them: a pasted URL can
    /// carry a credential in its authority, and a surface that echoed it back
    /// would put one on screen. The port is on the other side of that line
    /// because it is destination and not secret — `evil.example:8443` rendered
    /// as `evil.example` names a different socket in the familiar socket's
    /// words. A scheme-default port is not "explicit" to the parser that dials,
    /// so `https://x.example/` and `https://x.example:443/` render alike.
    pub dial_host: String,
    /// Non-fatal notes about the candidate — replacing an existing provider, a
    /// model the price table does not know, a cleartext endpoint.
    ///
    /// Warnings, never errors: a candidate the validator **refuses** is not a
    /// preview with a note attached, it is a
    /// [`PROVIDER_SETUP_INVALID`](crate::jsonrpc::error_code::PROVIDER_SETUP_INVALID)
    /// response carrying the validator's own sentence. Empty for a clean
    /// candidate, and a list rather than an `Option` so "nothing to say" has one
    /// spelling.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// A digest of the **whole document** this preview's candidate would write
    /// — what the client hands back as
    /// [`ProviderSetupCommitParams::expect_digest`] so the commit can refuse to
    /// write bytes the user never saw (BR-9).
    ///
    /// It covers the whole config rather than the rendered delta, for
    /// [`WebSetupPreviewResult::digest`]'s reason: the whole config is what the
    /// commit writes, and every field the flow does not collect rides along from
    /// whatever the live config held when the candidate was built. Another
    /// session editing any of it moves the file underneath a preview the user is
    /// still reading.
    ///
    /// Opaque to the client: it round-trips it and never parses it.
    #[serde(default)]
    pub digest: String,
    /// The provider this candidate would replace, when its id is already taken
    /// (BR-14), and absent from the wire otherwise.
    ///
    /// Stated by the daemon rather than re-derived by the client from
    /// [`ProviderSetupPlanResult::existing`]: the plan's snapshot can be several
    /// answers old by the time the preview is built, and the surface that knows
    /// whether the write replaces something is the one that built the candidate
    /// config. A silent replace-or-insert is the BUG-155 class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<ExistingProvider>,
}

impl RpcMethod for ProviderSetupPreviewParams {
    const METHOD: &'static str = "provider/setup_preview";
    type Result = ProviderSetupPreviewResult;
}

/// Write the candidate provider and its bindings, and make routing live
/// (REQ-579 BR-10, BR-15).
///
/// The **single commit point**: before this call nothing durable exists, and
/// this call either writes the file and re-derives routing or changes nothing at
/// all. Provider row and tier bindings land in one write, which is the whole of
/// ADR-1 — two writes would leave a window in which the provider exists unrouted
/// and a crash leaves it so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupCommitParams {
    /// The session committing.
    pub session_id: SessionId,
    /// The candidate to write — the same one the preview rendered, re-derived
    /// here rather than accepted as rendered bytes (BR-9, LESSON-501).
    pub candidate: ProviderSetupCandidate,
    /// [`ProviderSetupPreviewResult::digest`], handed back — the daemon writes
    /// only if the candidate it rebuilds still digests to this (BR-9).
    ///
    /// **Not a substitute for re-deriving**, exactly as
    /// [`WebSetupCommitParams::expect_digest`] is not: the candidate is still
    /// rebuilt and put through the same validator, and this is a *guard on the
    /// outcome*, so the worst a forged digest buys is a write the candidate
    /// already earned.
    ///
    /// `None` means "do not check", which is what a caller with no preview to
    /// compare against sends. The check is opt-in for that reason, not because
    /// it is optional in the flow: `/provider setup` always sends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_digest: Option<String>,
}

/// Result of [`ProviderSetupCommitParams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupCommitResult {
    /// Whether the config changed.
    ///
    /// `false` is not a failure and not an error response: it is a commit whose
    /// candidate matched what was already configured, and the distinction keeps
    /// the client's confirmation honest — [`WebSetupCommitResult::applied`]'s
    /// reason. A failed commit is an error response, never `applied: false`.
    pub applied: bool,
    /// The id now registered. Read back rather than assumed, like
    /// [`SessionPermissionsResult::level`]: a client that rendered the id it
    /// *sent* could confirm a state the daemon does not hold.
    pub provider_id: ProviderId,
    /// The bindings now in force — what actually landed, not what was asked
    /// for, so the completion line ("`think` now routes to it") is read off the
    /// daemon's answer rather than off the request.
    ///
    /// Empty is a legitimate answer: the registered-but-unrouted outcome BR-7
    /// permits.
    #[serde(default)]
    pub bindings: Vec<TierBinding>,
    /// The host this registration will be dialed at, from the **dial-time**
    /// parser (BR-5, LESSON-529) — the same reading
    /// [`ProviderSetupPreviewResult::dial_host`] showed at the confirm step,
    /// carried through to the answer that says the write landed.
    ///
    /// Completion is otherwise silent about where the key will now be sent: a
    /// surface that printed "registered; `think` now routes to it" named the id
    /// and never the destination, so a user who confirmed one host and had
    /// another written could not tell from the confirmation. Read off the
    /// derivation, **never** echoed from
    /// [`ProviderSetupCandidate::endpoint`] — this string is host, plus `:port`
    /// when the endpoint states one explicitly, and never userinfo, path or
    /// query, all by construction; the endpoint is none of those things.
    ///
    /// `#[serde(default)]` so a client built after this field still parses an
    /// older daemon's answer; empty then means "this daemon did not say", which
    /// a renderer shows as nothing rather than as a host.
    #[serde(default)]
    pub dial_host: String,
}

impl RpcMethod for ProviderSetupCommitParams {
    const METHOD: &'static str = "provider/setup_commit";
    type Result = ProviderSetupCommitResult;
}

// ---------------------------------------------------------------------------
// provider connection test (REQ-581)
// ---------------------------------------------------------------------------

/// Test one registered provider by making the smallest **real** call it serves
/// (REQ-581 BR-1).
///
/// The real path, minimal: the same adapter, the same credential-bound
/// transport and the same egress choke point a turn takes, carrying one fixed
/// message with no tools, no conversation context, and `max_tokens` at the
/// floor. Never a `GET /v1/models` shortcut — that would prove an endpoint
/// reachable that a turn never POSTs to, which is the reachability question
/// nobody asked (architecture ADR-1).
///
/// It carries a `session_id` for [`ProviderSetupPlanParams`]'s reason: the gate
/// that admits it is session attachment, so the call has to name the session it
/// claims. Here that gate is also the *spending* boundary — a connection that
/// may not drive this session, or a `teton provider test` the model spawned
/// through a tool, cannot make the user's provider bill them (architecture
/// ADR-5) — and it is what gives the resulting ledger row a session to belong
/// to (BR-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestParams {
    /// The session asking.
    pub session_id: SessionId,
    /// The provider to test, by its configured id.
    ///
    /// A `kind = "local"` provider is **refused rather than tested** (BR-8):
    /// there is no host to dial, and the answer to "does it work" there is the
    /// local tier's own state, which the refusal carries.
    pub provider_id: ProviderId,
}

/// A provider's routing health, in the words the router holds it in
/// (REQ-544 M-5).
///
/// The wire spelling of the daemon's own `ProviderHealth`, declared here rather
/// than imported for the reason every other shared word is: this crate depends
/// on no other teton crate (`the_protocol_crate_depends_on_no_other_teton_crate`
/// in [`crate`]), so the two are kept in step by their spellings and by the
/// daemon's own mapping, not by a dependency edge.
///
/// It rides here because a connection test **moves** health (BR-4) and a report
/// that said what came back without saying what the next turn will therefore do
/// would leave the user to guess the interesting half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealth {
    /// Up and reliable — eligible for the full loop.
    Healthy,
    /// Up but with weak tool-calling — used with a reduced profile, still the
    /// primary choice.
    Degraded,
    /// Down, erroring or timing out — routing falls back past it until its
    /// half-open cooldown expires.
    Unavailable,
}

/// What one connection test found — the daemon's classification, **typed**
/// (REQ-581 BR-3).
///
/// A client branches on the variant and renders the `reason` verbatim; it never
/// re-reads a sentence to work out what happened (LESSON-456). The variants are
/// drawn from the status the vendor answered with and the transport's failure
/// class and from nothing else — architecture ADR-2 is the whole mapping table,
/// and it draws the same lines the retry/fallback classifier already draws,
/// named here for a person.
///
/// Every `reason` is **the daemon's own sentence**, composed from facts the
/// product owns: the status, the dial host, the model the config declares, and
/// the credential *reference* (`keychain://teton/kimi`). Never a response body,
/// never a header, and never the credential value (ADR-3) — a vendor's error
/// body can echo the request back, and a test that pasted one into the
/// transcript would put a third party's prose, and possibly the user's own
/// bytes, where neither belongs. A user who wants the vendor's exact words has
/// the status to look them up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProviderTestOutcome {
    /// The call completed: the credential was accepted, the model answered, and
    /// what came back is what a turn would have got.
    Reached {
        /// Wall time from the request leaving to the stream ending. The whole
        /// round trip, not a header timing — it is what the user waited.
        latency_ms: u64,
        /// Prompt tokens the provider billed the probe for.
        input_tokens: u64,
        /// Completion tokens the provider billed the probe for.
        output_tokens: u64,
        /// What this call cost, in integer micro-USD, or `None` when the price
        /// table knows no entry for the model.
        ///
        /// Priced from the daemon's own table — the same one the ledger row is
        /// priced from — applied to the usage the *adapter* reported. The
        /// **ledger row is the record of spend**; this is the report's reading
        /// of the same call, and the two token readings behind them are taken
        /// independently (the adapter's completion here, the cost meter's byte
        /// scan there). They are pinned equal for the OpenAI SSE shape by a
        /// daemon-side test and are not guaranteed identical in general, so a
        /// consumer that needs the recorded figure asks the ledger for it.
        ///
        /// `None` is "unpriced", never `0`: a cost is recorded or it is not,
        /// and standing a zero in for "we have no price for this model" is
        /// displaying an estimate as an actual (REQ-544 BR-2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usd_micros: Option<i64>,
    },
    /// The vendor answered and would not serve the call — 401 and 403, and
    /// every other 4xx that is not one of the two below.
    ///
    /// The credential is the thing this outcome is usually about, so its
    /// `reason` names the *reference* the request authenticated with (AC-2).
    Refused {
        /// The HTTP status the vendor answered with.
        status: u16,
        /// The daemon's sentence for it (ADR-3).
        reason: String,
    },
    /// A 404: the endpoint exists — registration validated it and something
    /// answered — so the missing thing is the model the config declares.
    ///
    /// A 400 is deliberately **not** read this way and stays [`Self::Refused`]:
    /// guessing "that was about the model" from a bare 400 is exactly the
    /// re-reading of a vendor's prose this enum exists to avoid.
    UnknownModel {
        /// The HTTP status the vendor answered with (404).
        status: u16,
        /// The daemon's sentence for it, naming the model that was asked for.
        reason: String,
    },
    /// A 429: the vendor is up and holding the call off.
    RateLimited {
        /// How long the vendor asked the caller to wait, when it said so.
        ///
        /// Always `None` in v1, and the field is here anyway: the transport
        /// surfaces exactly one named header by design and this REQ does not
        /// grow that surface for a probe (architecture ADR-2, OQ-5 —
        /// **deferred, not dropped**). A client renders "try again shortly"
        /// when it is absent, and the day a second consumer earns the header
        /// this field carries it without a wire change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    /// A 5xx: the vendor answered and is failing. The configuration is not the
    /// suspect here, which is the whole reason it is a separate variant from
    /// [`Self::Refused`].
    ServerError {
        /// The HTTP status the vendor answered with.
        status: u16,
        /// The daemon's sentence for it.
        reason: String,
    },
    /// **Nothing answered** — DNS, TCP, TLS, a closed port, or bytes that could
    /// not be read as a response at all. The request may never have left; what
    /// is known is that no conversation happened.
    ///
    /// The two facts this variant used to carry in prose are their own variants
    /// now, because a client that had to read a sentence to tell them apart is
    /// the thing BR-3 exists to prevent (LESSON-456): something *did* answer, and
    /// not with a completion, is [`Self::NotACompletion`]; nothing answered
    /// *within the deadline* is [`Self::TimedOut`]. Each of the three sends the
    /// user somewhere different — check the address, check the path, check
    /// whether the vendor is up.
    Unreachable {
        /// The daemon's sentence, naming the host and the failure class.
        reason: String,
    },
    /// **Something answered, and it was not a completion.** The status was one a
    /// turn would have accepted (a 2xx or a 3xx) and what came back carried no
    /// text, no tool call and no tokens.
    ///
    /// A redirect that is deliberately not followed, an endpoint that does not
    /// stream, an endpoint that is not a chat-completions endpoint at all: a
    /// host is listening and the *path* is the suspect. Distinct from
    /// [`Self::Unreachable`] because the remedy is a different one: the address
    /// resolved and something is up, so there is nothing wrong with the address.
    ///
    /// It is emphatically not [`Self::Reached`]: a green answer for an endpoint
    /// no turn can use is this test's worst possible failure.
    NotACompletion {
        /// The daemon's sentence, naming the host that answered wrongly.
        reason: String,
    },
    /// **Nothing answered within the test's own deadline.** The connection may
    /// have been accepted; no completion arrived before the probe stopped
    /// waiting.
    ///
    /// Separate from [`Self::Unreachable`] because "did not answer *yet*" and
    /// "could not be reached" are different facts about a provider — the first
    /// is a host that is up and slow — and separate from a transport-level
    /// timeout, which is the transport's own verdict rather than this test's.
    TimedOut {
        /// The bound the test stopped at, in whole seconds — a *typed* figure
        /// rather than a number a client has to find in prose, since telling
        /// "slow" from "not answering" is the whole reason this variant exists.
        after_secs: u64,
        /// The daemon's sentence, naming the host that did not answer.
        reason: String,
    },
}

/// Result of [`ProviderTestParams`] — one real call, reported.
///
/// It names what was tested as well as what came back, because the answer to
/// "does my provider work" is only useful if the user can see it was asked of
/// the provider, model and host they meant (BR-2's preview, confirmed by the
/// report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestResult {
    /// The provider that was tested. Read back from the daemon rather than
    /// assumed by the client, like [`ProviderSetupCommitResult::provider_id`]:
    /// a report rendered from the id it *sent* could confirm a state the daemon
    /// does not hold.
    pub provider_id: ProviderId,
    /// The model the config pins that provider to — the one actually asked, so
    /// an `unknown_model` outcome names the string that needs fixing.
    pub model: String,
    /// The host the request was dialed at, from the **dial-time** parser
    /// (LESSON-529) — the destination the request builder actually reached, not
    /// a separately-parsed lookalike, and the same reading
    /// [`ProviderSetupPreviewResult::dial_host`] shows at a confirm step.
    ///
    /// Host, plus `:port` when the endpoint states one explicitly; never
    /// userinfo, path or query, all by construction. The endpoint itself does
    /// not travel here for [`crate::events::ProviderSetupCompleted`]'s reason:
    /// a pasted URL's authority can hide a credential.
    pub dial_host: String,
    /// What came back.
    pub outcome: ProviderTestOutcome,
    /// The health this test left the provider in — the same map the router
    /// reads at decision time (BR-4).
    ///
    /// A `reached` test clears a downgrade exactly as a served turn does, and a
    /// failure stamps the cooldown a failed turn would; this field is what lets
    /// the report say what the *next* turn will do rather than only what this
    /// call did.
    pub health_after: ProviderHealth,
}

impl RpcMethod for ProviderTestParams {
    const METHOD: &'static str = "provider/test";
    type Result = ProviderTestResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ChosenBand, GpuClass, TierBand};
    use crate::ParseCategoryError;
    use crate::GENERIC_SEARCH_AUTH_TEMPLATE;

    /// Reads `ENDS_TURN` the way [`crate::methods::RpcMethod`]'s one consumer
    /// does — through the trait, generically — rather than off a concrete type.
    /// Faithful to the call it stands in for, and it keeps the assertions below
    /// runtime ones with a sentence attached instead of const-folded literals.
    fn ends_turn<P: RpcMethod>() -> bool {
        P::ENDS_TURN
    }

    /// **`session/prompt` is the only turn (REQ-592 BR-8 / ADR-3).**
    ///
    /// The client drops its markdown fence on a turn's response and on no
    /// other. Through implementation that was decided by *where*
    /// `Connection::call` was called from, which made it some thirty separate
    /// judgements; the confirmation review found `/cost`, `/model` and
    /// `/config` each answering it wrongly and re-flowing a second client's
    /// streaming code block as prose. The answer moved here, beside the wire
    /// name, so there is one judgement and it is written down.
    ///
    /// The negative half is the substantive one: a defaulted `false` makes the
    /// positive trivially easy to get right and leaves a future turn-shaped RPC
    /// as the only thing that can go wrong.
    #[test]
    fn only_the_prompt_method_ends_a_turn() {
        assert!(
            ends_turn::<PromptTurnParams>(),
            "the one method that streams an assistant reply has to be the one \
             that ends a block, or a reply that finished inside an unterminated \
             fence renders every later reply of the session verbatim"
        );

        // The methods the confirmation review caught behind `call`: the slash
        // commands, the setup walkthroughs, and the status probes that run
        // beside a live stream.
        for (method, ends) in [
            (CostQueryParams::METHOD, ends_turn::<CostQueryParams>()),
            (ConfigGetParams::METHOD, ends_turn::<ConfigGetParams>()),
            (ConfigSetParams::METHOD, ends_turn::<ConfigSetParams>()),
            (ModelListParams::METHOD, ends_turn::<ModelListParams>()),
            (ModelSetParams::METHOD, ends_turn::<ModelSetParams>()),
            (ModelStatusParams::METHOD, ends_turn::<ModelStatusParams>()),
            (
                SessionCreateParams::METHOD,
                ends_turn::<SessionCreateParams>(),
            ),
            (SkillsListParams::METHOD, ends_turn::<SkillsListParams>()),
            (
                ProviderSetupCommitParams::METHOD,
                ends_turn::<ProviderSetupCommitParams>(),
            ),
            (
                WebSetupCommitParams::METHOD,
                ends_turn::<WebSetupCommitParams>(),
            ),
        ] {
            assert!(
                !ends,
                "`{method}` is not a turn, and a client that treated it as one \
                 would clear its markdown fence in the middle of somebody \
                 else's streaming code block (REQ-592 BR-6)"
            );
        }
    }

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
            root: None,
        });
        round_trip(&SessionCreateResult {
            session_id: SessionId::from("s1"),
            root: Some(SessionRoot {
                display: "~/Documents/GitHub/teton-code".to_owned(),
                kind: RootKind::Project,
                project_name: Some("teton-code".to_owned()),
                vcs_branch: Some("main".to_owned()),
            }),
        });
    }

    #[test]
    fn session_create_without_a_cwd_still_deserializes() {
        // Wire compatibility (BUG-147): an older client that sends no `cwd`
        // must still create a session — the field defaults to None.
        let params: SessionCreateParams = serde_json::from_str(r#"{"mode":"freeform"}"#).unwrap();
        assert_eq!(params.cwd, None);
    }

    /// REQ-583's additive `root` on the create result, both directions of the
    /// wire: an older daemon's answer without it still deserializes, an
    /// answer with it round-trips, and every [`RootKind`] spelling is the
    /// spec's snake_case one — `filesystem_root` in particular, which a
    /// derived camelCase or a hand-typed `FilesystemRoot` would both get wrong.
    #[test]
    fn session_create_result_without_root_still_deserializes() {
        let result: SessionCreateResult = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(result.session_id, SessionId::from("s1"));
        assert_eq!(result.root, None);

        // A `None` root emits no key at all, so an older client sees exactly
        // the shape it was written against.
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("root").is_none(), "{wire}");

        let with_root: SessionCreateResult = serde_json::from_str(
            r#"{"session_id":"s1","root":{"display":"/","kind":"filesystem_root"}}"#,
        )
        .unwrap();
        let root = with_root.root.clone().expect("root is present");
        assert_eq!(root.kind, RootKind::FilesystemRoot);
        assert_eq!(root.display, "/");
        assert_eq!(root.project_name, None);
        assert_eq!(root.vcs_branch, None);
        round_trip(&with_root);

        for (kind, spelling) in [
            (RootKind::Project, "project"),
            (RootKind::Home, "home"),
            (RootKind::FilesystemRoot, "filesystem_root"),
            (RootKind::Plain, "plain"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), spelling);
            let back: RootKind = serde_json::from_value(serde_json::json!(spelling)).unwrap();
            assert_eq!(back, kind);
        }
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

    /// REQ-583 ADR-4's one new method, pinned beside `session/clear` it is
    /// modelled on. The result carries the root both ways the two optionals
    /// can go — a project with a name and a branch, and a home root with
    /// neither — because a `skip_serializing_if` field that round-trips only
    /// when populated is the bug this test exists to catch.
    #[test]
    fn session_set_cwd_round_trips() {
        round_trip(&SessionSetCwdParams {
            session_id: SessionId::from("s1"),
            cwd: std::path::PathBuf::from("/Users/dev/repo"),
            name_hint: None,
        });
        round_trip(&SessionSetCwdResult {
            root: SessionRoot {
                display: "~/repo".to_owned(),
                kind: RootKind::Project,
                project_name: Some("repo".to_owned()),
                vcs_branch: Some("main".to_owned()),
            },
            blocks_dropped: 12,
        });
        let home = SessionSetCwdResult {
            root: SessionRoot {
                display: "~".to_owned(),
                kind: RootKind::Home,
                project_name: None,
                vcs_branch: None,
            },
            blocks_dropped: 0,
        };
        round_trip(&home);
        // The absent optionals emit no key: "no project name" is the absence
        // of the field, never a null a client must learn to read.
        let wire = serde_json::to_value(&home).unwrap();
        assert!(wire["root"].get("project_name").is_none(), "{wire}");
        assert!(wire["root"].get("vcs_branch").is_none(), "{wire}");
        assert_eq!(wire["root"]["kind"], "home");
        assert_eq!(wire["blocks_dropped"], 0);
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
            skill: None,
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
                    // REQ-586: what a daemon that has the field always sends —
                    // the declared window, and no cap.
                    max_context: Some(200_000),
                    context_budget_cap: None,
                    floored_budget: None,
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

    /// REQ-586's additive rule on the wire `ProviderConfig`, in the direction a
    /// **newer client** reads an **older daemon**: a provider record without
    /// `max_context`/`context_budget_cap` deserializes, and both read `None` —
    /// "the daemon predates the field", which is a different claim from
    /// `Some(0)`, "the daemon says the window is unknown". Keeping the two
    /// distinct is what lets `/doctor` say which one it is (BR-3).
    ///
    /// And the non-vacuity: a record that carries them round-trips with the
    /// values it carried, and one built with `None` emits no key rather than
    /// `null` — the same wire an older daemon writes.
    #[test]
    fn a_provider_record_without_the_window_fields_still_deserializes() {
        let pre_586: ProviderConfig = serde_json::from_str(
            r#"{"id":"kimi","kind":"openai-compatible",
                "endpoint":"https://api.moonshot.ai/v1/chat/completions",
                "model":"kimi-k3","auth_ref":"keychain://teton/kimi"}"#,
        )
        .unwrap();
        assert_eq!(pre_586.id, ProviderId::from("kimi"));
        assert_eq!(pre_586.max_context, None);
        assert_eq!(pre_586.context_budget_cap, None);
        let wire = serde_json::to_value(&pre_586).unwrap();
        assert!(wire.get("max_context").is_none(), "{wire}");
        assert!(wire.get("context_budget_cap").is_none(), "{wire}");

        // The daemon's "unknown" spelling is a present zero, not an absence.
        let unknown = ProviderConfig {
            max_context: Some(0),
            ..pre_586.clone()
        };
        round_trip(&unknown);
        let wire = serde_json::to_value(&unknown).unwrap();
        assert_eq!(wire["max_context"], 0, "{wire}");

        let declared = ProviderConfig {
            max_context: Some(131_072),
            context_budget_cap: Some(65_536),
            ..pre_586
        };
        round_trip(&declared);
        let wire = serde_json::to_value(&declared).unwrap();
        assert_eq!(wire["max_context"], 131_072, "{wire}");
        assert_eq!(wire["context_budget_cap"], 65_536, "{wire}");
    }

    /// The other direction of the same claim: a client that predates the
    /// window fields still reads a provider that carries them, and the
    /// snapshot around it.
    ///
    /// Serde ignores unknown fields by default and no type here opts out, but
    /// this posture is what keeps [`crate::PROTOCOL_VERSION`] still across the
    /// addition, so it is asserted rather than assumed — modelled by the
    /// pre-REQ-586 shape of the reader, exactly as
    /// `a_client_predating_redact_enabled_still_reads_a_snapshot_that_carries_it`
    /// does one REQ up.
    #[test]
    fn a_client_predating_the_window_fields_still_reads_a_provider_that_carries_them() {
        #[derive(Deserialize)]
        struct PreWindowProvider {
            id: ProviderId,
            kind: ProviderKind,
            model: Option<String>,
            auth_ref: Option<String>,
        }
        #[derive(Deserialize)]
        struct PreWindowSnapshot {
            providers: Vec<PreWindowProvider>,
            redact_enabled: bool,
        }

        let wire = serde_json::to_string(&ConfigSnapshot {
            providers: vec![ProviderConfig {
                id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
                model: Some("kimi-k3".to_owned()),
                auth_ref: Some("keychain://teton/kimi".to_owned()),
                max_context: Some(131_072),
                context_budget_cap: Some(65_536),
                floored_budget: None,
            }],
            redact_enabled: true,
            ..ConfigSnapshot::default()
        })
        .unwrap();
        assert!(
            wire.contains(r#""max_context":131072"#)
                && wire.contains(r#""context_budget_cap":65536"#),
            "the fixture must actually carry the new keys: {wire}"
        );

        let old: PreWindowSnapshot = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.providers.len(), 1, "the old reader still gets its rows");
        assert_eq!(old.providers[0].id, ProviderId::from("kimi"));
        assert_eq!(old.providers[0].kind, ProviderKind::OpenaiCompatible);
        assert_eq!(old.providers[0].model.as_deref(), Some("kimi-k3"));
        assert_eq!(
            old.providers[0].auth_ref.as_deref(),
            Some("keychain://teton/kimi")
        );
        assert!(old.redact_enabled);
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
                // REQ-586 BR-3/BR-5: a registration that declares both the
                // window and a cap below it — both fields set, so a round trip
                // that silently dropped either would fail rather than coincide
                // with the absent default.
                max_context: Some(128_000),
                context_budget_cap: Some(64_000),
                floored_budget: None,
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
        round_trip(&ConfigSetResult {
            applied: true,
            budget_notice: None,
        });
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
                probe_calls: 1,
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

    /// The same skew at REQ-581's field: a report from a daemon that predates
    /// `provider/test` carries no `probe_calls` key, and must read as **no
    /// probes** rather than fail the whole report.
    ///
    /// `0` is the honest reading of that silence — a daemon with no connection
    /// test recorded none — which is what makes the default safe here, unlike a
    /// field whose absence would have to be guessed at.
    #[test]
    fn a_cost_report_without_the_probe_count_still_deserializes() {
        let pre_581 = serde_json::json!({
            "total_usd_micros": 48_100,
            "total_calls": 3,
            "priced_calls": 2,
            "unpriced_calls": 1,
            "savings_usd_micros": 0,
            "baseline_usd_micros": 0,
            "baseline_model": "anthropic/claude-opus-4",
            "methodology": "Estimate, not a measurement.",
            "per_phase": [],
            "per_provider": [],
        });
        let decoded: CostReportView =
            serde_json::from_value(pre_581).expect("an older report still decodes");
        assert_eq!(decoded.probe_calls, 0);
        assert_eq!(decoded.total_calls, 3, "and the rest of it still arrives");

        // Non-vacuity: a report from *this* build carries its own count, so the
        // default above is reached by absence rather than by a field nothing
        // fills.
        let current = serde_json::to_value(CostReportView {
            total_calls: 3,
            probe_calls: 1,
            ..CostReportView::default()
        })
        .expect("serializes");
        assert_eq!(current["probe_calls"], 1, "{current}");
        let back: CostReportView = serde_json::from_value(current).expect("round-trips");
        assert_eq!(back.probe_calls, 1);
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
        // REQ-579's trio, pinned literally beside REQ-572's: these three strings
        // are the contract the daemon's dispatch match and the CLI's calls are
        // written against, and a rename that edited only one side would be a
        // `METHOD_NOT_FOUND` at the user's prompt.
        assert_eq!(ProviderSetupPlanParams::METHOD, "provider/setup_plan");
        assert_eq!(ProviderSetupPreviewParams::METHOD, "provider/setup_preview");
        assert_eq!(ProviderSetupCommitParams::METHOD, "provider/setup_commit");
        // REQ-581's one method, pinned for the same reason: the daemon's
        // dispatch match and both CLI call sites (`/provider test <id>` and
        // `teton provider test <id>`) are written against this literal.
        assert_eq!(ProviderTestParams::METHOD, "provider/test");
        // REQ-583's one method, pinned beside the verb it is modelled on: the
        // daemon's dispatch match and the CLI's `/cd` call are written against
        // this literal.
        assert_eq!(SessionClearParams::METHOD, "session/clear");
        assert_eq!(SessionSetCwdParams::METHOD, "session/set_cwd");
        // REQ-585's one method, and the literal matters more here than for the
        // rest: this string *is* the capability handshake (ADR-2). The daemon's
        // dispatch match and the CLI's post-`session/create` call are written
        // against it, and a rename that edited only one side would not be a
        // visible failure — it would be `METHOD_NOT_FOUND`, which the client
        // deliberately reads as "this daemon has no skills", so every `/name`
        // would quietly become `unknown command` on a daemon that has them.
        assert_eq!(SkillsListParams::METHOD, "skills/list");
        assert_eq!(
            request(Id::Number(2), ModelStatusParams::default()).method,
            "model/status"
        );
    }

    /// REQ-585's snapshot round-trips, including every state whose *absence* is
    /// a distinct fact: no description, no argument hint, nothing shadowing it,
    /// and an entirely empty registry.
    ///
    /// The empty result is the one worth naming. It is what a daemon with no
    /// `~/.claude` answers **and** what the client synthesizes from
    /// `METHOD_NOT_FOUND` on an old daemon (ADR-2), so it has to be a value the
    /// wire can carry rather than a shape that only exists in the client's
    /// head — and it must be reachable from `Default`, because that is how the
    /// old-daemon path constructs it.
    #[test]
    fn skills_list_round_trips_including_the_empty_registry() {
        round_trip(&SkillsListParams {
            session_id: SessionId::from("s1"),
        });
        round_trip(&SkillsListResult::default());
        assert!(SkillsListResult::default().skills.is_empty());
        assert!(SkillsListResult::default().skipped.is_empty());

        round_trip(&SkillsListResult {
            skills: vec![
                SkillView {
                    name: "alpha".to_owned(),
                    source: SkillSource::User,
                    description: Some("audit the repo".to_owned()),
                    argument_hint: Some("[path]".to_owned()),
                    shadowed: None,
                    model_invocable: true,
                    user_invocable: true,
                },
                // A project skill with nothing declared but its name — the
                // `.claude/commands/*.md` shape, which routinely has no
                // frontmatter at all.
                SkillView {
                    name: "gamma".to_owned(),
                    source: SkillSource::Project,
                    description: None,
                    argument_hint: None,
                    shadowed: None,
                    model_invocable: false,
                    user_invocable: true,
                },
                // Listed, never dispatchable (BR-2).
                SkillView {
                    name: "cost".to_owned(),
                    source: SkillSource::User,
                    description: None,
                    argument_hint: None,
                    shadowed: Some("a built-in command".to_owned()),
                    model_invocable: false,
                    user_invocable: true,
                },
            ],
            skipped: vec![SkillSkipped {
                name: "broken".to_owned(),
                path: "~/.claude/skills/broken/SKILL.md".to_owned(),
                reason: "malformed frontmatter".to_owned(),
            }],
        });

        // The two sources keep their spec spellings on the wire: the grant key
        // (`skill:<source>:<name>`) is built from them, so a rename here would
        // silently invalidate every remembered grant.
        assert_eq!(
            serde_json::to_string(&SkillSource::User).unwrap(),
            "\"user\""
        );
        assert_eq!(
            serde_json::to_string(&SkillSource::Project).unwrap(),
            "\"project\""
        );

        // An undeclared description emits **no key**, not `null` — a client
        // that renders `Some("null")` into a `/help` row would be printing the
        // absence rather than omitting it.
        let bare = serde_json::to_value(SkillView {
            name: "gamma".to_owned(),
            source: SkillSource::Project,
            description: None,
            argument_hint: None,
            shadowed: None,
            model_invocable: false,
            user_invocable: true,
        })
        .expect("serializes");
        assert!(bare.get("description").is_none(), "{bare}");
        assert!(bare.get("argument_hint").is_none(), "{bare}");
        assert!(bare.get("shadowed").is_none(), "{bare}");
        // REQ-587's two flags in their ordinary posture — not model-invocable,
        // invocable by the user — write **no keys at all**, so an ordinary
        // row's bytes are exactly the bytes REQ-585 wrote.
        assert!(bare.get("model_invocable").is_none(), "{bare}");
        assert!(bare.get("user_invocable").is_none(), "{bare}");
    }

    /// REQ-585 TASK-203: `SkillSkipped.name` is additive in both directions —
    /// the `route_decided` budget rule re-applied to the one field this task
    /// adds, rather than assumed inherited.
    ///
    /// The field exists so BR-2's naming rule ("a `skills/` entry is named by
    /// its directory, a `commands/` entry by its stem") has one home, on the
    /// side that owns discovery. That makes the skew question real: a client
    /// that already re-derives the name from the path will keep working against
    /// a daemon that has never sent one, and a daemon that sends one must not
    /// break a reader built before it.
    ///
    /// Four legs, as the rule asks: an absent key parses, an empty name emits
    /// **no** key rather than `""`, the new wire parses through a
    /// locally-declared pre-REQ struct, and the fixture that proves the third
    /// leg is checked for actually carrying the key (LESSON-502 — the vacuity
    /// is the failure mode).
    #[test]
    fn a_skipped_entrys_name_is_additive_in_both_directions() {
        // A result from a daemon predating the field: no `name` key at all.
        let older: SkillsListResult = serde_json::from_str(
            r#"{"skills":[],"skipped":[
                {"path":"~/.claude/skills/broken/SKILL.md","reason":"malformed frontmatter"}]}"#,
        )
        .expect("a result from a daemon predating the field must still parse");
        assert_eq!(older.skipped[0].name, "");
        assert_eq!(older.skipped[0].reason, "malformed frontmatter");

        // And an entry that names nothing — a root-level refusal — writes no
        // key, rather than an empty string a client would render as a name.
        let unnamed = serde_json::to_value(&older).unwrap();
        assert!(
            unnamed["skipped"][0].get("name").is_none(),
            "an entry that names no skill emits no key: {unnamed}"
        );

        // The other direction: a reader built before the field.
        #[derive(Deserialize)]
        struct PreNameSkillSkipped {
            path: String,
            reason: String,
        }
        #[derive(Deserialize)]
        struct PreNameSkillsListResult {
            skipped: Vec<PreNameSkillSkipped>,
        }
        let wire = serde_json::to_string(&SkillsListResult {
            skills: Vec::new(),
            skipped: vec![SkillSkipped {
                name: "broken".to_owned(),
                path: "~/.claude/skills/broken/SKILL.md".to_owned(),
                reason: "malformed frontmatter".to_owned(),
            }],
        })
        .unwrap();
        assert!(
            wire.contains(r#""name":"broken""#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreNameSkillsListResult =
            serde_json::from_str(&wire).expect("a client predating the field still reads the list");
        assert_eq!(old.skipped[0].path, "~/.claude/skills/broken/SKILL.md");
        assert_eq!(old.skipped[0].reason, "malformed frontmatter");

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "a named skipped entry is one more optional field, so the negotiated \
             version does not move"
        );
    }

    /// REQ-587 BR-3's two invocability flags, additive in both directions —
    /// the `SkillSkipped.name` rule re-applied rather than assumed inherited,
    /// with the **opposite defaults** asserted, because that is the half a
    /// copied test gets wrong.
    ///
    /// Four legs, as the rule asks: an absent key parses to the safe posture,
    /// an ordinary row emits **no** key rather than `false`/`true`, the new
    /// wire parses through a locally-declared pre-REQ-587 reader, and the
    /// fixture that proves the third leg is checked for actually carrying the
    /// keys (LESSON-502 — the vacuity is the failure mode).
    #[test]
    fn skill_view_invocability_flags_are_additive_in_both_directions() {
        // A row from a daemon predating the flags: neither key present. The two
        // defaults are not symmetric, and both are the safe reading — that
        // daemon has no `skill` tool, so nothing it lists is model-invocable,
        // and it listed the skill at all because the user may type it.
        let older: SkillView =
            serde_json::from_str(r#"{"name":"alpha","source":"user","description":"audit"}"#)
                .expect("a row from a daemon predating the flags must still parse");
        assert!(!older.model_invocable, "absent means not model-invocable");
        assert!(older.user_invocable, "absent means the user may type it");

        // And a row in that same posture writes neither key — not
        // `"model_invocable":false`, not `"user_invocable":true`. Downgrading
        // either `skip_serializing_if` to a bare `default` fails here.
        let wire = serde_json::to_value(&older).unwrap();
        assert!(wire.get("model_invocable").is_none(), "{wire}");
        assert!(wire.get("user_invocable").is_none(), "{wire}");

        // The other direction: a reader built before the flags.
        #[derive(Deserialize)]
        struct PreFlagsSkillView {
            name: String,
            source: SkillSource,
            #[serde(default)]
            shadowed: Option<String>,
        }
        let model_only = SkillView {
            name: "release".to_owned(),
            source: SkillSource::Project,
            description: Some("cut a release".to_owned()),
            argument_hint: None,
            shadowed: None,
            model_invocable: true,
            user_invocable: false,
        };
        round_trip(&model_only);
        let wire = serde_json::to_string(&model_only).unwrap();
        assert!(
            wire.contains(r#""model_invocable":true"#)
                && wire.contains(r#""user_invocable":false"#),
            "the fixture must actually carry the new keys: {wire}"
        );
        let old: PreFlagsSkillView =
            serde_json::from_str(&wire).expect("a client predating the flags still reads the row");
        assert_eq!(old.name, "release");
        assert_eq!(old.source, SkillSource::Project);
        // What that client makes of it, stated rather than left to inference:
        // an unmarked, dispatchable `/help` row. The flag has teeth in the
        // daemon (ADR-1), so the worst an old client does is offer a row whose
        // dispatch the daemon refuses — one refusal line, never a wrong
        // expansion.
        assert_eq!(old.shadowed, None);

        // BR-3's fourth state: both flags off — invocable by nobody, a named
        // diagnostic rather than a silent drop — survives the wire, and the
        // `false` half is the half that rides.
        let nobody = SkillView {
            name: "orphan".to_owned(),
            source: SkillSource::User,
            description: None,
            argument_hint: None,
            shadowed: None,
            model_invocable: false,
            user_invocable: false,
        };
        round_trip(&nobody);
        let wire = serde_json::to_value(&nobody).unwrap();
        assert!(wire.get("model_invocable").is_none(), "{wire}");
        assert_eq!(wire["user_invocable"], false, "{wire}");

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-587 adds optional fields, one subject variant and no method, so the \
             negotiated version does not move"
        );
    }

    /// **Every door round-trips, and that is what keeps the two lists in step**
    /// (REQ-591 D-7).
    ///
    /// `project_skill_trust_key` matches exhaustively on [`crate::events::InvokedBy`],
    /// so a third invoker is a compile error there. `is_project_acknowledgment_key`
    /// cannot match a `&str` back into the enum and walks `TRUST_DOORS` instead,
    /// which a third invoker would *not* break at compile time. This is the
    /// assertion that notices: a door missing from that list mints a key its own
    /// predicate rejects, and a rejected key is a grant that never expires on
    /// `/cd` (ASSUME-017's harm, arriving from the other side).
    #[test]
    fn every_door_round_trips_through_the_acknowledgment_key() {
        for door in TRUST_DOORS {
            let key = project_skill_trust_key(door, "~/dev/teton");
            assert!(
                is_project_acknowledgment_key(&key),
                "{door:?} mints `{key}`, which its own predicate does not recognize"
            );
            assert!(expires_on_session_root_change(&key), "{key}");
        }
        assert_eq!(
            TRUST_DOORS
                .iter()
                .map(|&door| project_skill_trust_key(door, "~/dev/teton"))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            TRUST_DOORS.len(),
            "two doors minted one key for one root, which is the widening D-7 removed"
        );
    }

    /// REQ-587 BR-4 / ADR-7: the acknowledgment key is its **own** family, it
    /// is nobody's skill key, and one predicate decides what a `/cd` expires.
    ///
    /// The three claims are here together because they are one claim about a
    /// string that two crates and two stores compare. ASSUME-017 is what
    /// happens when the rule is written out twice: the daemon expired its
    /// grants, the client's memo did not, and the client answered the new
    /// root's question with the old root's answer before a human saw anything.
    #[test]
    fn the_acknowledgment_key_is_its_own_family_and_expires_with_the_root() {
        let key = project_skill_trust_key(crate::events::InvokedBy::User, "~/dev/teton");
        assert_eq!(key, "project_skill_trust:user:~/dev/teton");
        // REQ-591 D-7: the other door is a different key for the same tree, so
        // a session answer at one cannot free the other.
        assert_eq!(
            project_skill_trust_key(crate::events::InvokedBy::Model, "~/dev/teton"),
            "project_skill_trust:model:~/dev/teton"
        );
        assert_ne!(
            key,
            project_skill_trust_key(crate::events::InvokedBy::Model, "~/dev/teton"),
            "one tree, two doors, two keys"
        );
        assert!(is_project_acknowledgment_key(&project_skill_trust_key(
            crate::events::InvokedBy::Model,
            "~/dev/teton"
        )));
        assert!(is_project_acknowledgment_key(&key));

        // Not a skill key, in either crate's spelling of that question. The
        // daemon's `is_skill_permission_key` matches on the `skill:<source>:`
        // prefixes, and `authorize_skill` `debug_assert!`s its key is one —
        // which is why ADR-7 opens a third door instead of widening the guard.
        assert!(!key.starts_with("skill:"), "{key}");
        assert!(!is_project_skill_key(&key), "{key}");
        assert!(
            !is_project_acknowledgment_key(&skill_permission_key(SkillSource::Project, "deploy")),
            "the two families do not overlap in the other direction either"
        );
        assert!(!is_project_acknowledgment_key(&skill_permission_key(
            SkillSource::User,
            "status"
        )));
        assert!(!is_project_acknowledgment_key("shell"));

        // A bare prefix names no root; a grant under it would be an answer to
        // no question.
        assert!(!is_project_acknowledgment_key(
            PROJECT_SKILL_TRUST_KEY_PREFIX
        ));
        // REQ-591 D-7: nor does a door segment with nothing after it, and nor
        // does a segment no door mints. Both would otherwise clear the gate's
        // family guard and land on a `debug_assert` — which a release build does
        // not have.
        for near_miss in [
            "project_skill_trust:user:",
            "project_skill_trust:model:",
            "project_skill_trust:~/dev/teton",
            "project_skill_trust:admin:~/dev/teton",
            "project_skill_trust:user~/dev/teton",
        ] {
            assert!(
                !is_project_acknowledgment_key(near_miss),
                "`{near_miss}` names no question this daemon can have asked"
            );
        }

        // Two roots, two keys — never one. The root is not truncated on its way
        // into a key, because a key is compared and not read: two long roots
        // sharing a prefix collapsing onto one string would let a grant for one
        // repository answer for another, which is the harm the per-root scope
        // exists to prevent.
        let long_a = format!("~/dev/{}/alpha", "nested/".repeat(40));
        let long_b = format!("~/dev/{}/beta", "nested/".repeat(40));
        assert_ne!(
            project_skill_trust_key(crate::events::InvokedBy::User, &long_a),
            project_skill_trust_key(crate::events::InvokedBy::User, &long_b)
        );
        assert!(
            project_skill_trust_key(crate::events::InvokedBy::User, &long_a).ends_with("/alpha")
        );

        // The root rides exactly as the caller spelled it, so the key the
        // answer is remembered under and the root the prompt showed name one
        // repository — and a home-relative display stays home-relative, since
        // an unrecognizing client renders this key on its refusal line.
        assert!(
            !project_skill_trust_key(crate::events::InvokedBy::User, "~/dev/teton")
                .contains("/Users/")
        );

        // One invalidation rule, both families, and only those two: a user
        // skill's file is the same file whatever the root is, so its grant
        // survives the move.
        assert!(expires_on_session_root_change(&key));
        assert!(expires_on_session_root_change(&skill_permission_key(
            SkillSource::Project,
            "deploy"
        )));
        assert!(!expires_on_session_root_change(&skill_permission_key(
            SkillSource::User,
            "status"
        )));
        assert!(!expires_on_session_root_change("shell"));
        assert!(!expires_on_session_root_change("web_search"));
    }

    /// REQ-585's additivity for `PromptTurnParams.skill`, both directions —
    /// the `route_decided` budget rule re-applied rather than assumed
    /// inherited.
    ///
    /// A turn from a client predating the field carries no key and reads
    /// `None`; a daemon that has never heard of the field still reads a turn
    /// that carries it. Serde ignores unknown fields by default and no type
    /// here opts out, but that posture is what keeps
    /// [`crate::PROTOCOL_VERSION`] still, so it is asserted rather than
    /// assumed.
    #[test]
    fn prompt_turn_skill_is_additive_in_both_directions() {
        // A pre-REQ-585 turn: no `skill` key — absent, not an error.
        let turn: PromptTurnParams =
            serde_json::from_str(r#"{"session_id":"s1","prompt":[{"type":"text","text":"hi"}]}"#)
                .expect("a turn from a client predating the field must still parse");
        assert_eq!(turn.skill, None);
        assert_eq!(turn.prompt.len(), 1);

        // And a turn that never populated it emits no key at all, rather than
        // `null` — the same wire an older client writes.
        let wire = serde_json::to_value(&turn).unwrap();
        assert!(wire.get("skill").is_none(), "{wire}");

        // The other direction: a reader built before the field.
        #[derive(Deserialize)]
        struct PreSkillPromptTurn {
            session_id: SessionId,
            prompt: Vec<PromptBlock>,
        }
        let wire = serde_json::to_string(&PromptTurnParams {
            session_id: SessionId::from("s1"),
            // ADR-3: the client sends an **empty** prompt, so a dropped
            // `skill` field yields a visible empty turn rather than a leaked
            // `/name …` command line reaching a model.
            prompt: vec![],
            skill: Some(SkillInvocation {
                name: "alpha".to_owned(),
                raw_arguments: "teton  code \"repo\"".to_owned(),
            }),
        })
        .unwrap();
        assert!(
            wire.contains(r#""skill":{"name":"alpha""#)
                && wire.contains(r#""raw_arguments":"teton  code \"repo\"""#),
            "the fixture must actually carry the new key: {wire}"
        );
        let old: PreSkillPromptTurn =
            serde_json::from_str(&wire).expect("a daemon predating the field still reads the turn");
        assert_eq!(old.session_id, SessionId::from("s1"));
        assert!(
            old.prompt.is_empty(),
            "the old reader still gets its fields"
        );

        // BR-4: the rest of the line rides **verbatim**. Two interior spaces
        // and both quotes survive the wire, because a re-join from a token
        // list is the one transformation this field must never suffer.
        let back: PromptTurnParams = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            back.skill.expect("the skill survives").raw_arguments,
            "teton  code \"repo\""
        );

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-585 adds only optional fields and one method, so the negotiated version \
             does not move — the capability is proven by a successful `skills/list`"
        );
    }

    /// `PermissionOutcome::Refused` is additive in the one direction it can be
    /// — and the direction it cannot be is asserted too, because that is the
    /// constraint the daemon's ordering rests on.
    ///
    /// This is a new **variant** on an internally tagged enum, not a new field,
    /// so the four-leg field rule does not transfer: there is no "absent key
    /// parses to the default" leg, and a reader that predates the variant
    /// cannot ignore an unknown tag the way it ignores an unknown key. What
    /// replaces it is the pin below — a pre-REQ-585 reader **fails**, which is
    /// exactly why ADR-2's handshake (not serde's tolerance) is what gates
    /// sending this: a client only ever sends `refused` to a daemon that
    /// answered `skills/list`.
    #[test]
    fn permission_outcome_refused_travels_only_to_a_daemon_that_knows_it() {
        for reason in [
            RefusalReason::NoTerminal,
            RefusalReason::UnrecognizedSubject,
        ] {
            round_trip(&PermissionRespondParams {
                request_id: RequestId::from("r1"),
                outcome: PermissionOutcome::Refused { reason },
            });
        }
        let wire = serde_json::to_value(PermissionOutcome::Refused {
            reason: RefusalReason::NoTerminal,
        })
        .unwrap();
        assert_eq!(wire["outcome"], "refused", "{wire}");
        assert_eq!(wire["reason"], "no_terminal", "{wire}");

        // The two outcomes that were already there keep their tags. `refused`
        // is emphatically **not** `cancelled`: that one means a human
        // dismissed the prompt (it is what EOF on a pipe returns), and AC-9
        // needs the daemon to tell "the user said no" from "no human could be
        // asked".
        assert_eq!(
            serde_json::to_value(PermissionOutcome::Cancelled).unwrap()["outcome"],
            "cancelled"
        );
        assert_eq!(
            serde_json::to_value(PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
            })
            .unwrap()["outcome"],
            "selected"
        );

        // The direction that does not work, pinned rather than assumed: a
        // daemon built before the variant refuses the params outright.
        #[derive(Deserialize)]
        #[serde(tag = "outcome", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum PreRefusalOutcome {
            Selected { option_id: String },
            Cancelled,
        }
        let refused = serde_json::to_string(&PermissionOutcome::Refused {
            reason: RefusalReason::NoTerminal,
        })
        .unwrap();
        assert!(
            serde_json::from_str::<PreRefusalOutcome>(&refused).is_err(),
            "an older daemon cannot read `refused`, which is why the handshake gates it: {refused}"
        );
        // And the old reader still reads what it always did, so nothing about
        // the existing two arms moved.
        let cancelled = serde_json::to_string(&PermissionOutcome::Cancelled).unwrap();
        assert!(serde_json::from_str::<PreRefusalOutcome>(&cancelled).is_ok());
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

    /// A sentinel candidate, so the provider-setup tests below differ only in
    /// the thing each is about.
    fn sentinel_candidate() -> ProviderSetupCandidate {
        ProviderSetupCandidate {
            id: ProviderId::from("kimi"),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.moonshot.ai/v1/chat/completions".to_owned()),
            model: "kimi-k2-turbo-preview".to_owned(),
            key_ref: "keychain://teton/kimi".to_owned(),
            bindings: vec![TierBinding {
                tier: Tier::Think,
                provider_id: ProviderId::from("kimi"),
            }],
            // REQ-586: the recipe's window, carried silently into the
            // candidate so the registration records one (BR-3).
            max_context: Some(131_072),
        }
    }

    /// REQ-579's three setup endpoints round-trip, including the answers whose
    /// *absence* is a distinct fact: no providers configured yet, no tier bound,
    /// no replacement, no bindings requested.
    ///
    /// Each is exercised at both ends of its own question — a fresh machine with
    /// nothing configured, and a full registration that replaces an existing
    /// provider — because the empty side is the state this REQ exists for and
    /// the one a default would quietly manufacture.
    #[test]
    fn the_provider_setup_methods_round_trip() {
        round_trip(&ProviderSetupPlanParams {
            session_id: SessionId::from("s1"),
        });

        // The fresh-install answer: recipes to offer, nothing registered, and
        // every routable tier unbound.
        let fresh = ProviderSetupPlanResult {
            catalog: vec![
                ProviderRecipeEntry {
                    id_suggestion: "sentinel".to_owned(),
                    label: "Sentinel (Vendor)".to_owned(),
                    guide_spelling: "Sentinel/Vendor".to_owned(),
                    kind: ProviderKind::OpenaiCompatible,
                    endpoint: Some("https://api.sentinel.example/v1/chat/completions".to_owned()),
                    example_model: "sentinel-1".to_owned(),
                    notes: Some("a sentinel note".to_owned()),
                    max_context: 131_072,
                },
                // The kind that carries its own address (ADR-7) and has nothing
                // to add: two of its keys are the ones that *aren't* there.
                ProviderRecipeEntry {
                    id_suggestion: "sentinel-native".to_owned(),
                    label: "Sentinel Native".to_owned(),
                    guide_spelling: "Sentinel Native".to_owned(),
                    kind: ProviderKind::Anthropic,
                    endpoint: None,
                    example_model: "sentinel-native-1".to_owned(),
                    notes: None,
                    max_context: 200_000,
                },
            ],
            existing: vec![],
            tiers: vec![
                TierSummary {
                    tier: Tier::Think,
                    provider_id: None,
                    fallback_id: None,
                },
                TierSummary {
                    tier: Tier::Build,
                    provider_id: None,
                    fallback_id: None,
                },
            ],
        };
        round_trip(&fresh);
        let wire = serde_json::to_value(&fresh).unwrap();
        assert_eq!(wire["catalog"][0]["kind"], "openai-compatible");
        assert_eq!(wire["catalog"][0]["guide_spelling"], "Sentinel/Vendor");
        assert_eq!(
            wire["catalog"][1]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            [
                "example_model",
                "guide_spelling",
                "id_suggestion",
                "kind",
                "label",
                "max_context"
            ],
            "a recipe with no endpoint and no notes drops both keys: {wire}"
        );
        // REQ-586: the window is never optional on a recipe — it is a fact
        // about the example model, carried as a number even when it is the
        // "unknown" zero, so a client does not have to tell absent from unset.
        assert_eq!(wire["catalog"][0]["max_context"], 131_072);
        assert_eq!(wire["catalog"][1]["max_context"], 200_000);
        assert!(
            wire["tiers"][0].get("provider_id").is_none(),
            "an unbound tier is an absent id, not an empty string: {wire}"
        );
        assert!(
            wire["tiers"][0].get("fallback_id").is_none(),
            "and a row with no fallback drops the key rather than sending null: {wire}"
        );
        assert_eq!(wire["existing"].as_array().unwrap().len(), 0);

        // The configured answer: something is registered, something is routed,
        // and the incomplete record (a provider with no model) is describable
        // rather than unrepresentable.
        let configured = ProviderSetupPlanResult {
            catalog: vec![],
            existing: vec![
                ExistingProvider {
                    id: ProviderId::from("kimi"),
                    kind: ProviderKind::OpenaiCompatible,
                    model: Some("kimi-k2-turbo-preview".to_owned()),
                },
                ExistingProvider {
                    id: ProviderId::from("half-done"),
                    kind: ProviderKind::OpenaiCompatible,
                    model: None,
                },
            ],
            tiers: vec![TierSummary {
                tier: Tier::Think,
                provider_id: Some(ProviderId::from("kimi")),
                fallback_id: Some(ProviderId::from("deepseek")),
            }],
        };
        round_trip(&configured);
        let wire = serde_json::to_value(&configured).unwrap();
        assert_eq!(wire["existing"][0]["model"], "kimi-k2-turbo-preview");
        assert!(
            wire["existing"][1].get("model").is_none(),
            "a provider that is incomplete, not invalid, says so by absence: {wire}"
        );
        assert_eq!(wire["tiers"][0]["provider_id"], "kimi");
        assert_eq!(
            wire["tiers"][0]["fallback_id"], "deepseek",
            "a configured fallback reaches the client, because this flow rewrites \
             whole rows and keeps it: {wire}"
        );

        // Preview and commit take the same candidate, deliberately — the commit
        // re-derives from the answers rather than from bytes the preview handed
        // back.
        round_trip(&ProviderSetupPreviewParams {
            session_id: SessionId::from("s1"),
            candidate: sentinel_candidate(),
        });
        // And the other end of the candidate: a native kind with no endpoint to
        // send, routed to nothing at all (BR-7's declined-every-binding case).
        round_trip(&ProviderSetupPreviewParams {
            session_id: SessionId::from("s1"),
            candidate: ProviderSetupCandidate {
                id: ProviderId::from("native"),
                kind: ProviderKind::Anthropic,
                endpoint: None,
                model: "claude-x".to_owned(),
                key_ref: "keychain://teton/native".to_owned(),
                bindings: vec![],
                // A candidate built from no recipe: the window stays unknown,
                // and the key stays off the wire (REQ-586).
                max_context: None,
            },
        });

        round_trip(&ProviderSetupPreviewResult {
            toml: "[[providers]]\nid = \"kimi\"\n".to_owned(),
            dial_host: "api.moonshot.ai".to_owned(),
            warnings: vec!["replaces existing provider `kimi`".to_owned()],
            digest: "a".repeat(64),
            replaces: Some(ExistingProvider {
                id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: Some("kimi-k2".to_owned()),
            }),
        });
        // A clean candidate replacing nothing: an empty warning list rather than
        // an absent one, and no `replaces` key at all.
        let clean = ProviderSetupPreviewResult {
            toml: "[[providers]]\nid = \"fresh\"\n".to_owned(),
            dial_host: "api.sentinel.example".to_owned(),
            warnings: vec![],
            digest: "b".repeat(64),
            replaces: None,
        };
        round_trip(&clean);
        let wire = serde_json::to_value(&clean).unwrap();
        assert!(
            wire.get("replaces").is_none(),
            "replacing nothing is an absent key, not an empty provider: {wire}"
        );

        round_trip(&ProviderSetupCommitParams {
            session_id: SessionId::from("s1"),
            candidate: sentinel_candidate(),
            expect_digest: Some("c".repeat(64)),
        });
        // A commit with no digest to check — legal, and what a caller with no
        // preview to compare against sends.
        round_trip(&ProviderSetupCommitParams {
            session_id: SessionId::from("s1"),
            candidate: sentinel_candidate(),
            expect_digest: None,
        });

        // Both answers: a commit that changed the config, and one whose
        // candidate matched what was already there. Neither is an error.
        let applied = ProviderSetupCommitResult {
            applied: true,
            provider_id: ProviderId::from("kimi"),
            bindings: vec![TierBinding {
                tier: Tier::Think,
                provider_id: ProviderId::from("kimi"),
            }],
            dial_host: "api.moonshot.ai".to_owned(),
        };
        round_trip(&applied);
        assert_eq!(
            serde_json::to_value(&applied).unwrap()["dial_host"],
            "api.moonshot.ai",
            "the answer names where the registration will be dialed, and names a \
             host — never the endpoint it was parsed out of"
        );
        let unrouted = ProviderSetupCommitResult {
            applied: false,
            provider_id: ProviderId::from("kimi"),
            bindings: vec![],
            dial_host: "api.moonshot.ai".to_owned(),
        };
        round_trip(&unrouted);
        assert_eq!(
            serde_json::to_value(&unrouted).unwrap()["bindings"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "registered-but-unrouted is an empty list, which is an answer"
        );

        // A daemon built before `dial_host` existed answers without the key, and
        // a client built after it still reads that answer (`#[serde(default)]`)
        // — the mixed-version skew ADR-007 endorses, at this field.
        let older: ProviderSetupCommitResult =
            serde_json::from_str(r#"{"applied":true,"provider_id":"kimi","bindings":[]}"#)
                .expect("an older daemon's commit answer still parses");
        assert!(
            older.dial_host.is_empty(),
            "and the absence reads as `this daemon did not say`, never as a host"
        );
    }

    /// AC-2's protocol half: [`ProviderRecipeEntry`] carries **exactly** the
    /// seven fields of the daemon's `provider_recipes::ProviderRecipe`.
    ///
    /// The 1:1 mapping itself is asserted on the daemon side, where both types
    /// are in scope; it cannot be checked here, because this crate depends on no
    /// other teton crate and must not start (see
    /// `the_protocol_crate_depends_on_no_other_teton_crate`). What is pinned
    /// here is the field set — spelled out, sorted, and with every optional
    /// field present — so a field added to the recipe without being added here
    /// fails on *this* side too, rather than only where the mapping is written.
    #[test]
    fn a_recipe_entry_carries_every_field_of_a_daemon_recipe() {
        let wire = serde_json::to_value(ProviderRecipeEntry {
            id_suggestion: "sentinel".to_owned(),
            label: "Sentinel (Vendor)".to_owned(),
            guide_spelling: "Sentinel/Vendor".to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.sentinel.example/v1/chat/completions".to_owned()),
            example_model: "sentinel-1".to_owned(),
            notes: Some("a sentinel note".to_owned()),
            max_context: 131_072,
        })
        .unwrap();
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "endpoint",
                "example_model",
                "guide_spelling",
                "id_suggestion",
                "kind",
                "label",
                "max_context",
                "notes"
            ],
            "{wire}"
        );
    }

    /// REQ-586's additivity on the two setup types: a recipe entry from a daemon
    /// predating the window reads as the "unknown" zero rather than failing,
    /// and a candidate from a client predating it reads as `None`; a reader
    /// built before either field still reads an entry and a candidate that
    /// carry them.
    #[test]
    fn recipe_entry_and_setup_candidate_window_fields_are_additive_in_both_directions() {
        // Older daemon → newer client: no key, the unknown spelling.
        let entry: ProviderRecipeEntry = serde_json::from_str(
            r#"{"id_suggestion":"kimi","label":"Moonshot (Kimi)",
                "guide_spelling":"Moonshot/Kimi","kind":"openai-compatible",
                "endpoint":"https://api.moonshot.ai/v1/chat/completions",
                "example_model":"kimi-k3"}"#,
        )
        .unwrap();
        assert_eq!(
            entry.max_context, 0,
            "absent reads as unknown, not as an error"
        );

        // Older client → newer daemon: no key, no window to record.
        let candidate: ProviderSetupCandidate = serde_json::from_str(
            r#"{"id":"kimi","kind":"openai-compatible",
                "endpoint":"https://api.moonshot.ai/v1/chat/completions",
                "model":"kimi-k3","key_ref":"keychain://teton/kimi"}"#,
        )
        .unwrap();
        assert_eq!(candidate.max_context, None);
        assert_eq!(candidate.bindings, vec![]);
        let wire = serde_json::to_value(&candidate).unwrap();
        assert!(
            wire.get("max_context").is_none(),
            "a candidate with no window emits no key, not null: {wire}"
        );

        // The other direction: readers built before the fields.
        #[derive(Deserialize)]
        struct PreWindowEntry {
            id_suggestion: String,
            example_model: String,
        }
        #[derive(Deserialize)]
        struct PreWindowCandidate {
            id: ProviderId,
            model: String,
            key_ref: String,
        }
        let wire = serde_json::to_string(&ProviderRecipeEntry {
            max_context: 131_072,
            ..entry
        })
        .unwrap();
        assert!(wire.contains(r#""max_context":131072"#), "{wire}");
        let old: PreWindowEntry = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.id_suggestion, "kimi");
        assert_eq!(old.example_model, "kimi-k3");

        let wire = serde_json::to_string(&sentinel_candidate()).unwrap();
        assert!(wire.contains(r#""max_context":131072"#), "{wire}");
        let old: PreWindowCandidate = serde_json::from_str(&wire).unwrap();
        assert_eq!(old.id, ProviderId::from("kimi"));
        assert_eq!(old.model, "kimi-k2-turbo-preview");
        assert_eq!(old.key_ref, "keychain://teton/kimi");
    }

    /// BR-2's wire half, re-applied at this second flow rather than assumed
    /// inherited from the first (LESSON-525): **no provider-setup type can carry
    /// the key**, in either direction. The secret's whole lifecycle stays in the
    /// client process, and what travels is a reference.
    ///
    /// Asserted on the serialized key sets rather than on a reading of the
    /// structs, so an `api_key` field added later turns this red instead of
    /// riding along with the reference that was supposed to replace it.
    #[test]
    fn no_provider_setup_payload_has_anywhere_to_put_the_key() {
        let planted = "sk-live-do-not-log-me";
        let preview = serde_json::to_string(&ProviderSetupPreviewParams {
            session_id: SessionId::from("s1"),
            candidate: sentinel_candidate(),
        })
        .unwrap();
        let commit = serde_json::to_string(&ProviderSetupCommitParams {
            session_id: SessionId::from("s1"),
            candidate: sentinel_candidate(),
            expect_digest: Some("d".repeat(64)),
        })
        .unwrap();

        for wire in [&preview, &commit] {
            assert!(!wire.contains(planted), "{wire}");
            // The only credential-shaped string is the reference.
            assert!(wire.contains("keychain://teton/kimi"), "{wire}");
        }

        // Every field name the candidate can carry, spelled out (sorted, which
        // is how `serde_json` hands back an object's keys): a key-carrying field
        // would have to be added to this list to be added to the type.
        let candidate_keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&commit)
            .unwrap()["candidate"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            candidate_keys,
            [
                "bindings",
                "endpoint",
                "id",
                "key_ref",
                "kind",
                "max_context",
                "model"
            ]
        );

        // And the params wrapping it carry only the session, the candidate, and
        // the digest guard.
        let commit_keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&commit)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(commit_keys, ["candidate", "expect_digest", "session_id"]);
    }

    /// The plan's two list fields default to empty, and its catalog does not.
    ///
    /// The asymmetry is the point (and the reason it is tested rather than
    /// merely documented): "no providers configured" and "no tiers bound" are
    /// facts a daemon can truthfully mean, so absence reads as empty; a plan
    /// answer with no `catalog` key is a *malformed* answer, because the catalog
    /// and this method ship together, and reading it as "this build knows no
    /// vendors" would send a user to a walkthrough with nothing to offer.
    #[test]
    fn a_plan_answer_may_omit_its_lists_but_never_its_catalog() {
        let bare = r#"{"catalog":[]}"#;
        let parsed: ProviderSetupPlanResult = serde_json::from_str(bare).unwrap();
        assert!(parsed.existing.is_empty());
        assert!(parsed.tiers.is_empty());

        assert!(
            serde_json::from_str::<ProviderSetupPlanResult>(r#"{"existing":[],"tiers":[]}"#)
                .is_err(),
            "a plan answer with no catalog is malformed, not an empty catalog"
        );
    }

    /// REQ-581 BR-3's wire half: **every** outcome the classifier can produce
    /// rides under its own snake_case tag and survives the round trip.
    ///
    /// The tags are spelled as literals rather than derived in the test, because
    /// they are the contract a client's `match` is written against: a variant
    /// renamed on one side only is a report that renders nothing, and the point
    /// of a typed outcome is that no client has to read a sentence to find out
    /// what happened (LESSON-456).
    #[test]
    fn every_provider_test_outcome_round_trips_under_its_wire_tag() {
        let cases: Vec<(ProviderTestOutcome, &str)> = vec![
            (
                ProviderTestOutcome::Reached {
                    latency_ms: 412,
                    input_tokens: 11,
                    output_tokens: 1,
                    usd_micros: Some(37),
                },
                "reached",
            ),
            (
                // The unpriced model: reached, billed by the vendor, and no
                // cost recorded because none is known.
                ProviderTestOutcome::Reached {
                    latency_ms: 412,
                    input_tokens: 11,
                    output_tokens: 1,
                    usd_micros: None,
                },
                "reached",
            ),
            (
                ProviderTestOutcome::Refused {
                    status: 401,
                    reason: "HTTP 401 from api.moonshot.ai — the vendor did not accept the \
                             credential at keychain://teton/kimi"
                        .to_owned(),
                },
                "refused",
            ),
            (
                ProviderTestOutcome::UnknownModel {
                    status: 404,
                    reason: "HTTP 404 from api.moonshot.ai — it does not know the model \
                             `kimi-k2-turbo-preview`"
                        .to_owned(),
                },
                "unknown_model",
            ),
            (
                ProviderTestOutcome::RateLimited {
                    retry_after_secs: None,
                },
                "rate_limited",
            ),
            (
                // Not what v1 sends (ADR-2 / OQ-5), and the shape is here so the
                // day it does is a value change and not a wire change.
                ProviderTestOutcome::RateLimited {
                    retry_after_secs: Some(7),
                },
                "rate_limited",
            ),
            (
                ProviderTestOutcome::ServerError {
                    status: 503,
                    reason: "HTTP 503 from api.moonshot.ai — the vendor answered and is failing"
                        .to_owned(),
                },
                "server_error",
            ),
            (
                ProviderTestOutcome::Unreachable {
                    reason: "could not reach api.moonshot.ai: connection error".to_owned(),
                },
                "unreachable",
            ),
            (
                // The three facts `unreachable` used to carry between them, now
                // one variant each: nothing answered, something answered wrongly,
                // and nothing answered in time.
                ProviderTestOutcome::NotACompletion {
                    reason: "api.moonshot.ai answered, but not with a completion (no tokens, no \
                             text)"
                        .to_owned(),
                },
                "not_a_completion",
            ),
            (
                ProviderTestOutcome::TimedOut {
                    after_secs: 30,
                    reason: "nothing came back from api.moonshot.ai before the test stopped \
                             waiting"
                        .to_owned(),
                },
                "timed_out",
            ),
        ];

        for (outcome, expected) in cases {
            round_trip(&outcome);
            let wire = serde_json::to_value(&outcome).unwrap();
            assert_eq!(wire["outcome"], expected, "{wire}");
        }

        // The two optionals are absent from the wire rather than `null`, and
        // both read back as `None` — "unpriced" and "the vendor said nothing
        // about when to retry", neither of which is a number.
        let unpriced = serde_json::to_value(ProviderTestOutcome::Reached {
            latency_ms: 1,
            input_tokens: 1,
            output_tokens: 1,
            usd_micros: None,
        })
        .unwrap();
        assert!(unpriced.get("usd_micros").is_none(), "{unpriced}");
        let limited = serde_json::to_value(ProviderTestOutcome::RateLimited {
            retry_after_secs: None,
        })
        .unwrap();
        assert_eq!(
            limited.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["outcome"],
            "a v1 rate-limit answer is the tag and nothing else: {limited}"
        );

        // The deadline the test stopped at rides as a **number** beside its tag.
        // A client renders "no answer within 30 s" from this field; finding the
        // figure inside `reason` would be the prose-reading BR-3 forbids.
        let timed_out = serde_json::to_value(ProviderTestOutcome::TimedOut {
            after_secs: 30,
            reason: "nothing came back before the test stopped waiting".to_owned(),
        })
        .unwrap();
        assert_eq!(timed_out["after_secs"], 30, "{timed_out}");
    }

    /// The `provider/test` call and its answer round-trip, and the answer names
    /// what was tested as well as what came back (BR-2/BR-4).
    #[test]
    fn the_provider_test_method_round_trips() {
        round_trip(&ProviderTestParams {
            session_id: SessionId::from("s1"),
            provider_id: ProviderId::from("kimi"),
        });

        let reached = ProviderTestResult {
            provider_id: ProviderId::from("kimi"),
            model: "kimi-k2-turbo-preview".to_owned(),
            dial_host: "api.moonshot.ai".to_owned(),
            outcome: ProviderTestOutcome::Reached {
                latency_ms: 412,
                input_tokens: 11,
                output_tokens: 1,
                usd_micros: Some(37),
            },
            health_after: ProviderHealth::Healthy,
        };
        round_trip(&reached);
        let wire = serde_json::to_value(&reached).unwrap();
        assert_eq!(wire["provider_id"], "kimi");
        assert_eq!(wire["model"], "kimi-k2-turbo-preview");
        assert_eq!(
            wire["dial_host"], "api.moonshot.ai",
            "the report names the destination that was dialed — a host, and only \
             a host: {wire}"
        );
        assert_eq!(wire["outcome"]["outcome"], "reached");
        assert_eq!(wire["outcome"]["latency_ms"], 412);
        assert_eq!(
            wire["health_after"], "healthy",
            "health is a value the client branches on, not a sentence: {wire}"
        );

        // A failing test still moves health, and the answer still says where it
        // landed — the half a report that only named the failure would omit.
        let unreachable = ProviderTestResult {
            provider_id: ProviderId::from("kimi"),
            model: "kimi-k2-turbo-preview".to_owned(),
            dial_host: "api.moonshot.ai".to_owned(),
            outcome: ProviderTestOutcome::Unreachable {
                reason: "could not reach api.moonshot.ai: timeout".to_owned(),
            },
            health_after: ProviderHealth::Unavailable,
        };
        round_trip(&unreachable);
        let wire = serde_json::to_value(&unreachable).unwrap();
        assert_eq!(wire["health_after"], "unavailable");
        assert_eq!(wire["outcome"]["outcome"], "unreachable");

        // The remaining health word, spelled on the wire so all three are
        // pinned against the daemon's own vocabulary.
        assert_eq!(
            serde_json::to_value(ProviderHealth::Degraded).unwrap(),
            "degraded"
        );

        // The request carries a session and a provider id and nothing else: no
        // model to override, no endpoint to redirect, no key. What is tested is
        // what the config says, which is the whole point of testing it.
        let asked = serde_json::to_value(ProviderTestParams {
            session_id: SessionId::from("s1"),
            provider_id: ProviderId::from("kimi"),
        })
        .unwrap();
        assert_eq!(
            asked.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["provider_id", "session_id"],
            "{asked}"
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

    /// REQ-589 ASSUME-B: the over-budget offer ships **without** widening
    /// [`PermissionOutcome`].
    ///
    /// Worth its own test rather than left to inspection, because the shape
    /// that tempts a widening is exactly the one BR-7 describes: two
    /// independent booleans, which a single-choice outcome cannot carry, and
    /// the obvious fix is a second field. ADR-1 chose four named option ids
    /// instead, so this pins the promise from the outside — the enum's tag set,
    /// and the fact that `selected` still carries one key and only one.
    ///
    /// The regression it catches is not cosmetic. A client that answers with an
    /// outcome shape the daemon predates gets `INVALID_PARAMS`, and
    /// [`RefusalReason`]'s own doc records what that costs: the params fail, the
    /// `request_id` is not reliably in hand, and the standing prompt is neither
    /// answered nor withdrawn. On this path that is an oversized turn a human
    /// approved and nothing sent.
    #[test]
    fn permission_outcome_did_not_widen_for_the_over_budget_offer() {
        use crate::events::{
            OPTION_ID_OVER_BUDGET_DECLINE, OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY,
            OPTION_ID_OVER_BUDGET_PROCEED_ONCE, OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
        };

        // All four answers ride the shipped `selected` shape, unchanged: one
        // tag, one key, and the id echoed back byte for byte.
        for id in [
            OPTION_ID_OVER_BUDGET_PROCEED_ONCE,
            OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY,
            OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
            OPTION_ID_OVER_BUDGET_DECLINE,
        ] {
            let outcome = PermissionOutcome::Selected {
                option_id: id.to_owned(),
            };
            round_trip(&PermissionRespondParams {
                request_id: RequestId::from("r1"),
                outcome: outcome.clone(),
            });

            let wire = serde_json::to_value(&outcome).unwrap();
            let mut keys: Vec<&str> = wire
                .as_object()
                .expect("an outcome is an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["option_id", "outcome"],
                "ASSUME-B: the over-budget answer is an id, not a second field: {wire}"
            );
            assert_eq!(wire["outcome"], "selected", "{wire}");
            assert_eq!(wire["option_id"], id, "{wire}");
        }

        // The tag set is exactly the three that shipped. A fourth arm — an
        // `OfferAnswer { proceed, apply_remedy }` by any name — reddens here.
        let tags: Vec<String> = [
            PermissionOutcome::Selected {
                option_id: OPTION_ID_OVER_BUDGET_DECLINE.to_owned(),
            },
            PermissionOutcome::Cancelled,
            PermissionOutcome::Refused {
                reason: RefusalReason::NoTerminal,
            },
        ]
        .iter()
        .map(|o| {
            serde_json::to_value(o).unwrap()["outcome"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
        assert_eq!(tags, ["selected", "cancelled", "refused"]);

        // The claim from the other side: a reader compiled against the shipped
        // three arms reads every over-budget answer. This is the leg that would
        // fail if the enum widened, because such a reader could not.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "outcome", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OutcomeAsShipped {
            Selected { option_id: String },
            Cancelled,
            Refused { reason: RefusalReason },
        }
        for id in [
            OPTION_ID_OVER_BUDGET_PROCEED_ONCE,
            OPTION_ID_OVER_BUDGET_PROCEED_AND_REMEDY,
            OPTION_ID_OVER_BUDGET_REMEDY_ONLY,
            OPTION_ID_OVER_BUDGET_DECLINE,
        ] {
            let wire = serde_json::to_string(&PermissionOutcome::Selected {
                option_id: id.to_owned(),
            })
            .unwrap();
            let back: OutcomeAsShipped = serde_json::from_str(&wire)
                .expect("a client predating REQ-589 reads every one of its answers");
            match back {
                OutcomeAsShipped::Selected { option_id } => assert_eq!(option_id, id),
                other => panic!("an over-budget answer must stay a `selected`: {other:?}"),
            }
        }

        assert_eq!(
            crate::PROTOCOL_VERSION,
            crate::ProtocolVersion(2),
            "REQ-589 adds one subject variant, four option ids and three events — all \
             additive on the daemon-to-client side — so the negotiated version does not move"
        );
    }
}
