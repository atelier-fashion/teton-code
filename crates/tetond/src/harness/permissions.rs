//! The tool permission model: per-tool policy, a client round-trip, and
//! session-scoped grants.
//!
//! Modeled on Claude Code's allowlist-plus-prompt (spec Permissions table). Each
//! tool has a policy:
//!
//! - **allow** — run without asking (read-only tools by default),
//! - **deny** — never run,
//! - **ask** — emit a [`permission_request`](teton_protocol::events::PermissionRequest)
//!   event and wait for a client to answer.
//!
//! The round-trip uses the daemon's real machinery: the request goes out over
//! TASK-004's [`EventBus`], and the client's reply arrives — in the server's
//! `permission/respond` handler — as a call to [`PendingPermissions::resolve`].
//! That call is the seam; this module owns everything up to it.
//!
//! Each waiter carries the [`SessionId`] whose tool call it blocks, and
//! [`PendingPermissions::owner_of`] hands that back. The server answers "may
//! this connection answer this prompt" with it (REQ-569 BR-9, ADR-F) — the
//! authorization lives there, not here; this module only makes the question
//! answerable.
//!
//! A `*_always` answer is remembered for the **session only** ([`PermissionGate`]
//! holds the grants), so the user is asked once per tool per session and never
//! persisted to disk.
//!
//! ## The subject is a *key*, not always a tool (REQ-563)
//!
//! Everything above says "tool" because for every built-in and every MCP tool
//! the subject of a permission question is the tool. Web lookup is the exception
//! the model was widened for: BR-3 grades the capability into three tiers and
//! requires each to be **separately consented**, so the subject is one of the
//! three keys in
//! [`WEB_PERMISSION_KEYS`](crate::harness::tools::web::WEB_PERMISSION_KEYS) —
//! `web_fetch_user_url`, `web_fetch_any_url`, `web_search` — and never the `web`
//! tool name. [`PermissionGate::decide`] is key-based throughout, which is what
//! makes that claim true rather than aspirational: the grant map, the policy
//! table and the prompt's `tool_name` all read the same string, so a grant can
//! never be wider than the question it answered.
//!
//! The one thing keyed on the *tier* rather than the key is the fifth prompt
//! option, `enable_permanent` — see [`options_for`].
//!
//! ## A second key family, and a prompt that is *addressed* (REQ-585)
//!
//! A skill's dynamic context — the `` !`command` `` slots in a `SKILL.md` body —
//! is the second subject that is not a tool. It asks under
//! `skill:<source>:<name>` ([`is_skill_permission_key`], ADR-6) and never under
//! `shell`, so that one "allow for this session" answered at a skill prompt
//! cannot free every later model-issued shell call, and an earlier allow-always
//! on `shell` cannot silently un-ask a skill's commands. [`authorize_skill`] is
//! its entry point, for the same reason [`PermissionGate::authorize_web`] is
//! web's: it carries what the generic door cannot.
//!
//! [`PermissionGate::authorize_skill`] differs from every other prompt this
//! module raises in one further way, and it is a security property rather than a
//! nicety: the request is **addressed to the connection that sent the
//! invocation**, and only that connection may answer it. Everything else here is
//! published on the bus, which reaches every connection attached to the session
//! — a supported topology (REQ-570) that would otherwise put a skill's consent
//! in front of a pre-REQ-585 client, which understands no
//! [`PermissionSubject`](teton_protocol::events::PermissionSubject), falls
//! through to its own `prompter.ask`, and on a pipe turns the user's next stdin
//! line into a `y` that authorizes shell commands. See
//! [`AddressedPermissionDelivery`] and [`PendingPermissions::resolve_from`].
//!
//! ## A third door, and a grant key that follows its arguments (REQ-587)
//!
//! [`PermissionGate::authorize_project_skill_trust`] asks a different question
//! from either of the two above: not "may these commands run?" but "may the
//! model run **this repository's** skills as instructions at all?" (BR-4). It is
//! a third entry point rather than a widened [`PermissionGate::authorize_skill`]
//! because that function's two `debug_assert!`s require its key to be a skill
//! key *and* to equal the key `(source, name)` mints, and an acknowledgment key
//! — [`project_skill_trust_key`], deliberately not `skill:` — is neither.
//! Widening them would loosen a guard that is pinned in both directions
//! (architecture ADR-7).
//!
//! The other half of REQ-587 lands *inside* the skill door: when any command in
//! a body interpolates `$ARGUMENTS`/`$N`, the grant is remembered under a key
//! carrying a digest of the **substituted** command set ([`skill_grant_key`],
//! BR-5/OQ-9). One rule for both callers — a user-typed `/name` and a
//! model-issued `skill` call of the same skill with different arguments do not
//! share an answer — and it is why `authorize_skill`'s second assertion and that
//! minting function are one decision rather than two.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use teton_core::config::WebTier;
use teton_protocol::events::{
    Event, InvokedBy, PermissionOption, PermissionOptionKind, PermissionRequest, PermissionSubject,
    ProjectSkillTrustEntry, WebConsentDecided, WebConsentScope, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{
    expires_on_session_root_change, is_project_acknowledgment_key, project_skill_trust_key,
    PermissionOutcome, RefusalReason,
};
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::{RequestId, SessionId};

use crate::broadcast::EventBus;
use crate::egress::to_protocol_web_tier;
use crate::grants::ConnectionId;
use crate::harness::tools::web::{permission_key_for, tier_name, WEB_PERMISSION_KEYS};
use crate::harness::tools::{DOCS_TOOL_NAME, SKILL_TOOL_NAME};
use crate::skills::{permission_key_for as skill_permission_key_for, SkillSource};

/// Policy for a single tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Run without prompting.
    Allow,
    /// Prompt the client and wait for an answer.
    Ask,
    /// Never run.
    Deny,
}

/// The resolved decision for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// The call may proceed.
    Allowed,
    /// The call is cancelled; the model is told and must not retry it.
    Denied,
}

/// The resolved answer for one skill consent — its dynamic context (REQ-585
/// BR-6) or the project-skill acknowledgment (REQ-587 BR-4) — and **why**, which
/// a [`PermissionDecision`] cannot carry.
///
/// A skill's not-run placeholder names its reason to the user and to the model
/// (`[dynamic context not run: `<cmd>` — <reason>]`), and the four ways to not
/// run are four different sentences that must not be collapsed into one. AC-9
/// is explicit that a piped session's placeholders say *no human could be
/// asked* rather than *declined*: a `Denied` that meant both would force the
/// caller to re-derive the difference from state the gate had in hand and threw
/// away, which is the re-derivation-at-a-distance shape LESSON-501 names.
///
/// This is a **separate type** rather than two more [`PermissionDecision`]
/// variants on purpose. Every tool call in the daemon matches that enum
/// exhaustively, and a decision a tool path cannot act on is not a decision it
/// should be made to handle; the extra facts exist for exactly one caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillConsent {
    /// Every command of the invocation may run.
    Allowed,
    /// The **level** settled it and nobody was asked (`plan`). The sentence
    /// comes from [`PermissionGate::denial_note`], so the daemon's and the
    /// client's account of one refusal cannot drift (LESSON-456).
    DeniedByLevel,
    /// A **human** decided against it — rejected once, rejected for the
    /// session, or the prompt was dismissed.
    Declined,
    /// The client refused **without asking anyone** (BR-11): no terminal to ask
    /// at, or a subject this client does not recognize. Nobody declined
    /// anything, and the placeholder must not say they did.
    Refused(RefusalReason),
    /// The question could never be put to the connection that asked it — no
    /// addressed-delivery route was wired, the connection would not take the
    /// frame, or it went away before answering. Fail-closed, and distinct from
    /// [`Self::Declined`] for the same reason [`Self::Refused`] is.
    Unanswerable,
}

impl SkillConsent {
    /// Whether the commands may run. The one question every caller asks; the
    /// variants exist for the sentence the *other* answer earns.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// A remembered, session-scoped answer for a tool.
///
/// Public because a caller that can reach a decision **without** prompting still
/// has to honour one that was already made: REQ-563's cache hit performs no
/// egress and therefore asks nothing, but a user who answered "reject for this
/// session" has refused the *capability*, not the packet, and serving them a
/// cached page would be the one path around their own answer. See
/// [`PermissionGate::remembered`], which is a read — it never prompts, never
/// records, and never turns an absent answer into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberedGrant {
    /// Always allow for the rest of the session.
    AllowAlways,
    /// Always reject for the rest of the session.
    RejectAlways,
}

/// The per-tool policy table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionConfig {
    default: PermissionPolicy,
    per_tool: HashMap<String, PermissionPolicy>,
}

impl PermissionConfig {
    /// A config with the given default and no per-tool overrides.
    #[must_use]
    pub fn with_default(default: PermissionPolicy) -> Self {
        Self {
            default,
            per_tool: HashMap::new(),
        }
    }

    /// Sensible coding defaults: read-only tools auto-allow, mutating tools ask.
    ///
    /// Since REQ-560 this is the [`PermissionLevel::Guarded`] table, and it is
    /// spelled as a delegation rather than as a second copy of the same rows:
    /// two constructors producing "the same" table is exactly the drift BR-15
    /// exists to prevent, and the only way to be sure they agree is for there to
    /// be one of them ([`table_for`]).
    #[must_use]
    pub fn coding_defaults() -> Self {
        table_for(PermissionLevel::Guarded)
    }

    /// A config that allows every **local, jailed** tool — the offline demo
    /// path, where the operator has pre-approved that tool set.
    ///
    /// The three web keys are excluded and stay [`PermissionPolicy::Ask`]
    /// (REQ-563 verify): the sentence above is the justification for this
    /// constructor, and a web lookup is neither local nor jailed. Pre-approving
    /// them here would have made "allow every tool" quietly mean "and talk to
    /// the internet without asking" on the one path that exists precisely
    /// because nothing should leave the machine.
    ///
    /// A machine that genuinely wants unprompted web lookups says so in config,
    /// with `[web] permission_allow`, which the daemon maps onto these same three
    /// keys — one member, one key.
    ///
    /// Since REQ-560 this is the [`PermissionLevel::Full`] table, delegating for
    /// the same reason [`Self::coding_defaults`] does. Note what that makes true
    /// of the level: `full` stops asking about `shell`, and still asks about the
    /// web — because "allow every tool" must not quietly come to mean "and talk
    /// to the internet without asking".
    #[must_use]
    pub fn permissive() -> Self {
        table_for(PermissionLevel::Full)
    }

    /// Map `[web] permission_allow` onto the web consent keys (REQ-563 BR-3/BR-4).
    ///
    /// The **one** place a config value becomes a web policy row, so the "did
    /// enable-permanent actually change anything" question has one answer.
    ///
    /// ## One member, one key — never a fan-out
    ///
    /// Each listed tier sets **its own** key and no other, through the single
    /// [`permission_key_for`](super::tools::web::permission_key_for) mapping. A
    /// tier not listed is left exactly as it was, which for the three web keys
    /// means `ask`.
    ///
    /// This used to be a two-valued `[web] permission` that fanned onto all three
    /// keys at once, and the fan-out was the bug: one `enable_permanent` answered
    /// at a `web_fetch_user_url` prompt permanently stopped asking about
    /// `web_fetch_any_url` and `web_search` too. BR-3 requires the three tiers to
    /// be separately consented precisely because they are different capabilities
    /// — a URL the user pasted is not a URL the model composed — and a durable
    /// answer that crosses them is the breadth violation BR-3 names, with a
    /// config file behind it.
    ///
    /// It never *widens* the ceiling — `[web] tier` is checked before any prompt
    /// exists to answer.
    ///
    /// ## It relaxes an `ask`; it never overrules a `deny` (REQ-560 ADR-C)
    ///
    /// A listed tier is upgraded **only** when its key currently sits at
    /// [`PermissionPolicy::Ask`]. A standing consent is an answer to a question,
    /// and a level that has already refused to *ask* the question has not left
    /// one for config to answer.
    ///
    /// Without this narrowing, a machine carrying `[web] permission_allow` would
    /// punch a hole straight through [`PermissionLevel::Plan`] — the one level
    /// whose entire promise is that nothing changes and nothing leaves — and it
    /// would do so from a config file, silently, on a session the user had just
    /// asked to be read-only. Every pre-REQ-560 case is unaffected, because
    /// every level except `plan` leaves the web keys at `ask`.
    pub fn apply_web_permission(&mut self, allow: &[WebTier]) {
        for tier in allow {
            // `Off` has no key (config validation refuses it as a member, and
            // `permission_key_for` answers `None`), so an unmappable member
            // silently changes nothing rather than borrowing a neighbour's key.
            if let Some(key) = permission_key_for(*tier) {
                if self.policy_for(key) == PermissionPolicy::Ask {
                    self.set(key, PermissionPolicy::Allow);
                }
            }
        }
    }

    /// Override the policy for a tool.
    pub fn set(&mut self, tool: impl Into<String>, policy: PermissionPolicy) {
        self.per_tool.insert(tool.into(), policy);
    }

    /// The policy that applies to `tool`.
    #[must_use]
    pub fn policy_for(&self, tool: &str) -> PermissionPolicy {
        self.per_tool.get(tool).copied().unwrap_or(self.default)
    }
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self::coding_defaults()
    }
}

/// The tools that read and change nothing — the **only** set any level
/// enumerates by name (REQ-560 ADR-A).
///
/// Safe to enumerate because it is first-party and closed. Its complement — the
/// set of tools that might change something — is open (every MCP server adds to
/// it), and is never enumerated anywhere.
///
/// [`DOCS_TOOL_NAME`] belongs here for a stronger reason than the other three:
/// they read the user's files, and this one reads nothing at all. Its bodies are
/// `include_str!`d into the binary and served from process memory — no path, no
/// transport, no user data (REQ-577 BR-6) — so there is no question a prompt
/// could usefully ask about a call. Leaving it off the list is what the
/// TASK-147 live A/B caught: at `guarded` it prompted (`? permission requested:
/// teton_docs`), and at `plan` it would have been **denied**, which is the
/// daemon refusing to read its own documentation on the level a user picks
/// precisely because they want reading and nothing else. The requirement's
/// Permissions row says "without a permission prompt"; this line is what makes
/// that true.
///
/// `skill` joins for the same reason with one addition of its own (REQ-587
/// BR-11). It reads no path and no network — the registry holds every body from
/// discovery, so a call opens no file — and the constraint BR-11 states is that
/// **no level ever raises an "allow `skill`?" prompt**: a knowledge tool that
/// asks at `guarded` or is denied at `plan` is indistinguishable from not
/// shipping it, which is the `teton_docs` lesson one REQ later (LESSON-524).
/// What a model invocation *can* raise is finer than the tool's name and is
/// asked under its own key — the project-skill acknowledgment
/// ([`PermissionGate::authorize_project_skill_trust`], BR-4) and the skill's
/// dynamic context ([`PermissionGate::authorize_skill`], BR-5) — so the tool's
/// own row being `allow` withholds nothing.
///
/// Spelled as the constant the registry registers under
/// ([`SKILL_TOOL_NAME`], REQ-587 TASK-216), never as a literal: this row and
/// the tool's name are two halves of one fact, and a literal here would let the
/// tool be renamed into a row that no longer matches it — at which point the
/// level table's `default` takes over and `plan` denies the tool outright,
/// silently, which is exactly the `teton_docs` failure LESSON-524 records.
/// `the_permission_row_and_the_registrys_name_are_one_value` pins it.
const READ_ONLY_TOOLS: &[&str] = &["read", "glob", "grep", DOCS_TOOL_NAME, SKILL_TOOL_NAME];

/// Expand a [`PermissionLevel`] into the policy table the gate enforces.
///
/// **This is the classifier** (REQ-560 BR-1, BR-15). One function, one
/// exhaustive match, and the only place in the daemon where a level becomes
/// policy. `coding_defaults()` and `permissive()` delegate here rather than
/// holding their own rows, so there is no second table left to drift from.
///
/// ## How a level classifies a tool it has never heard of (REQ-560 OQ-2)
///
/// By its `default` policy, and never by name. MCP tool names are
/// server-supplied and untrusted (ADR-003, ADR-009's residual), so a level that
/// enumerated mutating tools could not cover them and would be wrong the moment
/// a user registered a server. It does not have to: every name a level does not
/// mention falls to `default`, and `default` **is** the level's answer to
/// "something I do not recognise". So an MCP tool asks at `guarded` and `edits`,
/// **denies at `plan`**, and allows at `full`.
///
/// That inverts the risk in the direction it should be inverted. Adding a tool
/// to the tree without touching this function gets the conservative treatment at
/// every level, rather than being silently unclassified.
///
/// **What that costs a first-party read-only tool, stated accurately.** This
/// comment used to end "a new *read-only* first-party tool that nobody adds to
/// [`READ_ONLY_TOOLS`] merely asks — a degradation, not a hole", and REQ-577's
/// own live run falsified it. `teton_docs` was exactly that tool, and the
/// consequence was not "merely asks": at `guarded` it interrupted the turn with
/// a prompt for a read of bytes compiled into the binary, and at `plan` — the
/// level a user picks *because* they want reading and nothing else — the
/// default is `Deny`, so the daemon refused to read its own documentation
/// outright. The omission is silent in CI, too, because exposure tests assert
/// the tool is in the list and being *callable* is a different claim. So: the
/// fallback is safe in the direction that matters (nothing is silently
/// permitted), and it is not free — an unclassified read-only tool is denied at
/// `plan`, which for a knowledge tool is indistinguishable from not shipping it.
///
/// ## `full` is an allow-all table, not a skipped gate (REQ-560 BR-4)
///
/// Every level, including `full`, produces a table that
/// [`PermissionGate::decide`] evaluates. There is no `if level == Full { skip }`
/// anywhere, because a gate skipped when a flag is set is a guard whose
/// condition names something unrelated to what it guards — it becomes a silent
/// no-op the moment anything else moves that condition (LESSON-443).
#[must_use]
pub fn table_for(level: PermissionLevel) -> PermissionConfig {
    match level {
        // Byte-equal to the pre-REQ-560 `coding_defaults()`, including its
        // redundant explicit `edit`/`shell` rows: BR-1 asks for byte-equality,
        // not equivalence, and the rows also state the posture at the two tools
        // users ask about rather than leaving it implied by the default.
        PermissionLevel::Guarded => {
            let mut cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
            allow_read_only(&mut cfg);
            cfg.set("edit", PermissionPolicy::Ask);
            cfg.set("shell", PermissionPolicy::Ask);
            cfg
        }
        // The one row that separates this from `guarded`. `shell` stays asking,
        // which is the whole request this level exists to answer: "stop asking
        // me about every edit, but keep asking before you run a shell command".
        PermissionLevel::Edits => {
            let mut cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
            allow_read_only(&mut cfg);
            cfg.set("edit", PermissionPolicy::Allow);
            cfg.set("shell", PermissionPolicy::Ask);
            cfg
        }
        // Deny-by-default with a read-only allowlist. `edit` and `shell` are
        // *not* listed: they fall to the default, which is the point — listing
        // them would suggest the denial comes from naming them, when it comes
        // from not being on the read-only list. Web keys fall to the default
        // too, so `plan` performs no egress.
        PermissionLevel::Plan => {
            let mut cfg = PermissionConfig::with_default(PermissionPolicy::Deny);
            allow_read_only(&mut cfg);
            cfg
        }
        // Byte-equal to the pre-REQ-560 `permissive()`. The three web keys stay
        // asking: `full` is about tools this machine runs, and a web lookup is
        // neither local nor jailed.
        PermissionLevel::Full => {
            let mut cfg = PermissionConfig::with_default(PermissionPolicy::Allow);
            for key in WEB_PERMISSION_KEYS {
                cfg.set(key, PermissionPolicy::Ask);
            }
            cfg
        }
    }
}

/// Set every read-only tool to `allow` — the one enumeration any level performs.
fn allow_read_only(cfg: &mut PermissionConfig) {
    for tool in READ_ONLY_TOOLS {
        cfg.set(*tool, PermissionPolicy::Allow);
    }
}

/// One in-flight prompt: who is waiting, and **whose session** raised it.
///
/// The owner is stored beside the sender rather than derived later because
/// nothing else in the daemon can answer the question. The map is daemon-wide
/// and a `RequestId` is an opaque `perm-N`, so once a waiter is registered the
/// only record of which session's tool call it belongs to is this field — which
/// is what [`PendingPermissions::owner_of`] reads, and what lets
/// `permission/respond` require attachment to *that* session (REQ-569 BR-9,
/// ADR-F).
struct Waiter {
    /// The session whose tool call is blocked on this answer.
    owner: SessionId,
    /// The **one** connection this prompt was addressed to, when it was
    /// addressed to one (REQ-585 ADR-7).
    ///
    /// `None` is the shape every pre-REQ-585 prompt has and keeps: published on
    /// the bus, and answerable by any connection the server's
    /// [`PendingPermissions::owner_of`] check admits. `Some` is strictly
    /// narrower — the request never went on the bus, and no other connection may
    /// answer it, which is what [`PendingPermissions::resolve_from`] enforces.
    ///
    /// It is recorded here, beside the owner, for the reason the owner is: once
    /// the waiter is registered this map is the only record of who the question
    /// was put to, and a fact re-derived later is a fact derived where the
    /// knowledge no longer exists (LESSON-501).
    addressee: Option<ConnectionId>,
    /// Resolved by [`PendingPermissions::resolve`]; dropping it denies.
    tx: oneshot::Sender<PermissionOutcome>,
}

/// The registry of in-flight permission prompts, keyed by request id.
///
/// The harness registers a waiter here and awaits it; a client's
/// `permission/respond` calls [`Self::resolve`]. Kept separate from
/// [`PermissionGate`] because it is daemon-wide (one client reply must find the
/// waiter regardless of which session raised it), whereas grants are
/// per-session — and because being daemon-wide is exactly why each waiter has to
/// carry its owning [`SessionId`] ([`Waiter`]).
#[derive(Default)]
pub struct PendingPermissions {
    waiters: Mutex<HashMap<RequestId, Waiter>>,
    // The request-id counter lives HERE — daemon-wide — not on `PermissionGate`,
    // so the id namespace matches the resolution namespace this map defines
    // (BUG-161). A per-session counter minted `perm-0`, `perm-1`, … in every
    // session, and this map is shared across all sessions' gates, so two
    // sessions collided on `perm-0` and one's `register` overwrote the other's
    // waiter — resolving one session's prompt then answered the other's tool
    // call. A single monotonic counter makes every id unique by construction.
    counter: AtomicU64,
}

impl PendingPermissions {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint the next request id. Daemon-wide and monotonic, so no two prompts —
    /// in the same session or across sessions — ever share an id (BUG-161).
    fn next_request_id(&self) -> RequestId {
        RequestId::from(format!(
            "perm-{}",
            self.counter.fetch_add(1, Ordering::SeqCst)
        ))
    }

    /// Register a waiter for `owner`'s prompt and return the receiver the caller
    /// awaits.
    ///
    /// `owner` is the session whose tool call is blocked. It is taken as a
    /// parameter rather than read off anything global because
    /// [`PermissionGate`] is the only thing that knows it, and it is the fact
    /// `permission/respond` later authorizes against ([`Self::owner_of`]).
    ///
    /// Refuses to overwrite an existing waiter rather than replacing it. With
    /// [`next_request_id`](Self::next_request_id) minting unique ids this is
    /// unreachable, so a collision here means a per-scope counter has crept back
    /// (the BUG-161 shape) — we keep the first waiter and let this caller's
    /// receiver resolve to the safe default (`Denied`, via the dropped sender),
    /// never silently stealing another prompt's answer. Note what that now also
    /// protects: the *owner* recorded for a live request id can never be
    /// rewritten by a later registration, so the authorization subject of a
    /// pending prompt is fixed the moment it is raised.
    ///
    /// `addressee` is `Some` only for a request that is routed to one
    /// connection rather than published (REQ-585 ADR-7); see [`Waiter`].
    fn register(
        &self,
        id: RequestId,
        owner: SessionId,
        addressee: Option<ConnectionId>,
    ) -> oneshot::Receiver<PermissionOutcome> {
        let (tx, rx) = oneshot::channel();
        let mut waiters = self
            .waiters
            .lock()
            .expect("pending permissions mutex poisoned");
        match waiters.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(Waiter {
                    owner,
                    addressee,
                    tx,
                });
            }
            Entry::Occupied(existing) => {
                // request_id is `perm-N`, never content — safe to log (conventions).
                eprintln!(
                    "tetond: permission request_id collision on {:?} — refusing to overwrite the waiting prompt (BUG-161 tripwire)",
                    existing.key()
                );
                // `tx` drops here → `rx` yields `RecvError` → the caller's
                // `authorize` takes its `Denied` arm.
            }
        }
        rx
    }

    /// The session whose tool call is waiting on `id`, or `None` if no prompt by
    /// that id is outstanding (never raised, already answered, or expired with
    /// its turn).
    ///
    /// The authorization question `permission/respond` asks (REQ-569 BR-9,
    /// ADR-F): *whose* prompt is this, so the daemon can require that the
    /// answering connection is attached to that session. A read only — it never
    /// consumes the waiter, because a caller that is about to be **refused**
    /// must leave the prompt standing for whoever may rightfully answer it.
    #[must_use]
    pub fn owner_of(&self, id: &RequestId) -> Option<SessionId> {
        self.waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .get(id)
            .map(|waiter| waiter.owner.clone())
    }

    /// Deliver a client's answer to the waiting harness. Returns `true` if a
    /// waiter was present. This is the entry point the server's
    /// `permission/respond` handler calls — *after* it has checked
    /// [`Self::owner_of`], because this call consumes the waiter.
    ///
    /// **An addressed prompt is never resolved through here** (REQ-585 ADR-7).
    /// This entry point names no answering connection, so it cannot establish
    /// that the answer came from the connection the question was put to — and
    /// for a skill's dynamic context that is the whole guard, not a refinement.
    /// Such a waiter is left standing (as [`Self::owner_of`]'s refusal path
    /// leaves it standing, so whoever may rightfully answer still can) and this
    /// answers `false`. Callers that know their connection use
    /// [`Self::resolve_from`].
    pub fn resolve(&self, id: &RequestId, outcome: PermissionOutcome) -> bool {
        self.deliver(id, outcome, None)
    }

    /// Deliver `answering`'s answer, honouring an addressed prompt's addressee
    /// (REQ-585 ADR-7).
    ///
    /// For an ordinary broadcast prompt this is [`Self::resolve`] with the
    /// answering connection recorded but unused — the delivery policy for those
    /// is attachment, and the server checks it against [`Self::owner_of`]. For
    /// an **addressed** prompt it is the enforcement point: an answer from any
    /// connection other than the addressee is refused, the waiter is left
    /// standing for the connection that was actually asked, and this answers
    /// `false`.
    ///
    /// Refusing rather than ignoring matters in one specific direction. Two
    /// clients attached to one session is a consented topology (REQ-570), so
    /// the second client is not an attacker — it is an older build that saw a
    /// request it could not understand. Leaving its answer inert is what keeps
    /// its `prompter.ask` from having authorized a shell command; and leaving
    /// the prompt standing is what keeps the real client's answer arriving
    /// afterwards from finding nothing to answer.
    pub fn resolve_from(
        &self,
        id: &RequestId,
        outcome: PermissionOutcome,
        answering: ConnectionId,
    ) -> bool {
        self.deliver(id, outcome, Some(answering))
    }

    /// The body of both entry points. `answering` is `None` when the caller
    /// cannot name a connection at all, which an addressed waiter treats
    /// exactly as it treats the wrong one.
    fn deliver(
        &self,
        id: &RequestId,
        outcome: PermissionOutcome,
        answering: Option<ConnectionId>,
    ) -> bool {
        let waiter = {
            let mut waiters = self
                .waiters
                .lock()
                .expect("pending permissions mutex poisoned");
            // Checked and removed under one lock: an entitled answer and an
            // unentitled one racing on the same id must not both find a waiter.
            let entitled = match waiters.get(id) {
                None => return false,
                // Unaddressed: the pre-REQ-585 delivery policy, unchanged.
                Some(waiter) => match waiter.addressee {
                    None => true,
                    Some(addressee) => answering == Some(addressee),
                },
            };
            if !entitled {
                return false;
            }
            waiters.remove(id)
        };
        match waiter {
            Some(waiter) => waiter.tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Forget a waiter without answering it.
    ///
    /// For the one path that registers a waiter and then discovers there is
    /// nobody to ask: an addressed request whose connection would not take the
    /// frame. Registering first is what keeps an answer that arrives before the
    /// publish from finding no waiter, so the failure has to be undone here
    /// rather than avoided by publishing first.
    fn withdraw(&self, id: &RequestId) {
        self.waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .remove(id);
    }

    /// Number of prompts currently awaiting an answer.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .len()
    }
}

/// Where an `enable_permanent` answer is written down (REQ-563 BR-4).
///
/// A seam rather than a `DaemonRuntime` handle because this module is the
/// permission *model* and owns no configuration: it knows that one answer is
/// durable and four are not, and nothing about where durable lives. The daemon
/// implements it over its atomic config write; a test implements it over a cell
/// and can assert what the gate asked for without a file.
///
/// The `Result` is load-bearing, not decoration. "Permanent" is a claim made to
/// a user, and a persistence that did not happen must not be reported as one —
/// the gate downgrades the recorded scope to
/// [`WebConsentScope::Session`] when this answers `Err`, which is the honest
/// description of what the answer then bought.
pub trait WebTierPersistence: Send + Sync {
    /// Persist `tier` as the configured web ceiling.
    ///
    /// # Errors
    /// A human-readable sentence naming what stopped the write.
    fn persist_web_tier(&self, tier: WebTier) -> Result<(), String>;
}

/// Where an **addressed** permission request is delivered (REQ-585 ADR-7).
///
/// A seam rather than a connection registry handle for the reason
/// [`WebTierPersistence`] is one: this module is the permission *model*, and it
/// knows that one request goes to exactly one connection and nothing about how
/// a connection is reached. The daemon implements it over the outbound frame
/// channels it already routes REQ-569's consent prompts and BUG-177's lifecycle
/// replay through; a test implements it over a channel and can assert who was
/// asked without a socket.
///
/// **Why a seam at all, rather than the [`EventBus`] this module already
/// holds.** The bus is a fan-out: everything published on it reaches every
/// connection attached to the session, and any of them may answer. That is
/// correct for a tool call and wrong for a skill's dynamic context, because a
/// pre-REQ-585 client attached to the same session — a supported topology
/// (REQ-570) — would receive a request carrying a
/// [`PermissionSubject`](teton_protocol::events::PermissionSubject) it has
/// never heard of, fall through to its own `prompter.ask`, and on a pipe read
/// the user's next stdin line as the answer. So the request must not be
/// published at all, and a gate with no route to address it asks **nobody**
/// (`SkillConsent::Unanswerable`) rather than falling back to the bus.
pub trait AddressedPermissionDelivery: Send + Sync {
    /// Put `request` in front of `connection` and no one else.
    ///
    /// Answers whether the frame was accepted. `false` — no such live
    /// connection, or an outbound channel that would not take it — is a prompt
    /// nobody will ever see, and the gate turns it into a refusal rather than
    /// waiting for an answer that cannot come.
    fn deliver(
        &self,
        connection: ConnectionId,
        session_id: &SessionId,
        request: PermissionRequest,
    ) -> bool;
}

/// The connection a request is addressed to, and what it is about.
///
/// The two travel together because neither is meaningful alone: a subject with
/// no addressee would be broadcast (the hole ADR-7 closes), and an addressee
/// with no subject would be a request the addressed client cannot recognize
/// without parsing the permission key, which BR-11 forbids.
struct Addressed {
    connection: ConnectionId,
    subject: PermissionSubject,
}

/// How one decision was settled — the decision itself, plus *who* settled it.
///
/// [`PermissionGate::authorize`] narrows this to a [`PermissionDecision`],
/// because a tool call can act on nothing more. [`PermissionGate::authorize_skill`]
/// does not: the sentence a not-run placeholder carries is exactly this
/// distinction (REQ-585 AC-9), and it is known here and nowhere later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// The level's table answered; nobody was asked.
    ByLevel(PermissionDecision),
    /// A remembered session grant answered — a human, earlier.
    ByGrant(PermissionDecision),
    /// A human answered this prompt.
    ByHuman(PermissionDecision),
    /// The client refused without asking anyone (BR-11).
    Refused(RefusalReason),
    /// Nobody could be asked: no route to the addressee, or it went away.
    Unanswerable,
}

impl Settled {
    /// What the caller may do, with the provenance dropped.
    const fn decision(self) -> PermissionDecision {
        match self {
            Self::ByLevel(decision) | Self::ByGrant(decision) | Self::ByHuman(decision) => decision,
            // Every non-answer denies. This is the safe default the whole
            // module is built on, stated once.
            Self::Refused(_) | Self::Unanswerable => PermissionDecision::Denied,
        }
    }
}

/// Whether the level table's `allow` row settles a question (REQ-587 BR-4).
///
/// Every decision in the daemon but one is [`Self::Settles`]: an `allow` row is
/// configuration saying nobody need be asked, and that is the whole of `full`.
///
/// The exception is narrow enough to state in a sentence. A project skill that
/// **shadows** a user skill is the one case a `full` session can be surprised
/// by — the model asks for `validate` meaning the file the user installed and
/// gets a body the repository substituted — so BR-4 acknowledges that swap once
/// per session per root even in the unattended posture.
///
/// ## Why this is not the second path around the gate
///
/// REQ-560 BR-1 forbids a decision that skips [`PermissionGate::decide`]'s
/// table; this is not one. There is still exactly one enforcement path, and the
/// override is **allow-only and ask-more**: `deny` still denies (so `plan` is
/// untouched), a remembered grant still answers (so "once per session" still
/// means once), and the only thing that changes is that an `allow` row stops
/// being the end of the conversation. A knob that could turn a `deny` into an
/// `allow` would be the hole; a knob whose whole range is "ask anyway" cannot
/// widen anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelAllow {
    /// The level's `allow` settles it, and nobody is asked.
    Settles,
    /// The level's `allow` does not settle it; the grant map is consulted and,
    /// failing that, the question is put to the addressee.
    DoesNotSettle,
}

/// The most model-invocable project skills the acknowledgment prompt names
/// before collapsing the tail into a count (REQ-587 BR-4).
///
/// Twenty, because an unbounded prompt is LESSON-517's shape: a repository with
/// two hundred skills would put two hundred file-supplied names in front of a
/// user who is being asked one question about the set. The tail rides as
/// [`PermissionSubject::ProjectSkillTrust`]'s `more` count — "and 5 more" and
/// "and some more" are different facts, and the user is being asked to trust the
/// whole set.
const MAX_LISTED_PROJECT_SKILLS: usize = 20;

/// Truncate the acknowledgment's skill list to [`MAX_LISTED_PROJECT_SKILLS`],
/// answering the listed entries and how many were left out.
///
/// A `u32` because the wire field is one; a repository with more than four
/// billion skills has a different problem, and saturating is the only honest
/// answer that cannot panic.
fn bound_listed_skills(skills: &[ProjectSkillTrustEntry]) -> (Vec<ProjectSkillTrustEntry>, u32) {
    let listed = skills
        .iter()
        .take(MAX_LISTED_PROJECT_SKILLS)
        .cloned()
        .collect();
    let more =
        u32::try_from(skills.len().saturating_sub(MAX_LISTED_PROJECT_SKILLS)).unwrap_or(u32::MAX);
    (listed, more)
}

/// Whether a skill body's commands interpolate the invocation's arguments
/// (REQ-587 BR-5, OQ-9).
///
/// An enum rather than a `bool` because it is read at a call site that already
/// carries a source, a name and a command list, and `true` there would say
/// nothing about which of those facts it is about.
///
/// The fact itself belongs to the **expander**, which is the only thing that
/// sees the body before substitution: after `$ARGUMENTS`/`$N` are replaced the
/// substituted command carries no trace of having interpolated. That is why this
/// rides in from the caller rather than being derived here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentInterpolation {
    /// No command in the body mentions `$ARGUMENTS` or `$N`, so the commands are
    /// a property of the file alone and the grant keys per skill — REQ-585
    /// BR-6's behaviour, unchanged.
    None,
    /// At least one command does, so the commands are a property of the file
    /// **and** the arguments, and the grant must key on both.
    Substituted,
}

/// The key a skill's dynamic-context grant is remembered under — **the** minter,
/// for both callers and both spellings (REQ-587 BR-5, OQ-9).
///
/// - [`ArgumentInterpolation::None`] → `skill:<source>:<name>`, exactly REQ-585
///   BR-6's key. One answer covers the session, which is right when the commands
///   cannot change.
/// - [`ArgumentInterpolation::Substituted`] → that key plus `#<digest>`, where
///   the digest is taken over the **substituted** command set in document order.
///
/// ## Why the digest exists
///
/// "Allow for this session" under a skill's key answers later invocations of
/// that skill — that is what a session grant means (LESSON-495). When a command
/// interpolates the arguments, a later caller chooses part of what the
/// remembered grant runs, and a **model** is one of the callers as of this REQ.
/// One rule for both: a user-typed `/deploy staging` and a model-issued
/// `deploy prod` do not share an answer. REQ-585's Assumption that
/// per-command-string remembering "would be new machinery and is not needed" is
/// half-kept — it is new machinery, and this is the REQ that needed it.
///
/// ## Why the shape still reads as a skill key
///
/// The digest is appended *after* the name, so the string still starts with
/// `skill:<source>:` and still has a non-empty tail. That is load-bearing three
/// times over: [`is_skill_permission_key`] still admits it, so
/// [`PermissionGate::authorize_skill`]'s first guard is unchanged;
/// [`teton_protocol::methods::is_project_skill_key`] still matches, so a
/// digest-keyed **project** grant still dies at `/cd`; and `#` cannot occur in a
/// registered skill name (`^[a-z0-9][a-z0-9_-]{0,63}$`), so the two spellings
/// cannot collide.
///
/// ## Why SHA-256 and not a `Hash` impl
///
/// The bytes being digested are model-influenced: the arguments are the model's,
/// so the substituted commands are partly the model's. A 64-bit hash with a
/// known key is not collision-resistant, and a found collision here would be one
/// command set answered by another command set's grant — the exact harm the
/// digest exists to prevent. The commands are length-prefixed rather than
/// joined by a separator because a command may contain any byte a shell accepts,
/// newlines included: `["ab", "c"]` and `["a", "bc"]` must not digest alike.
#[must_use]
pub fn skill_grant_key(
    source: SkillSource,
    skill: &str,
    commands: &[String],
    interpolation: ArgumentInterpolation,
) -> String {
    let base = skill_permission_key_for(source, skill);
    match interpolation {
        ArgumentInterpolation::None => base,
        ArgumentInterpolation::Substituted => {
            let mut buf = String::new();
            for command in commands {
                buf.push_str(&command.len().to_string());
                buf.push(':');
                buf.push_str(command);
            }
            format!(
                "{base}{SKILL_GRANT_DIGEST_SEPARATOR}{}",
                teton_inference::sha256_hex(buf.as_bytes())
            )
        }
    }
}

/// What separates a skill key from the digest of its substituted commands.
///
/// Outside the registered-name alphabet on purpose, so no skill can be named
/// such that its plain key collides with another skill's digest key.
const SKILL_GRANT_DIGEST_SEPARATOR: char = '#';

/// Whether `key` is a grant key this `(source, skill, commands)` could have
/// minted — either spelling, and nothing else.
///
/// This and [`skill_grant_key`] are one decision. The assertion in
/// [`PermissionGate::authorize_skill`] used to be a `debug_assert_eq!` against
/// the single key `(source, skill)` mints; BR-5 gave the same triple a second
/// legal spelling, and an assertion that did not move with the minter would fire
/// on every debug build the first time a body interpolated its arguments. Both
/// spellings are still exact — this is not "any skill key".
fn is_grant_key_for(key: &str, source: SkillSource, skill: &str, commands: &[String]) -> bool {
    [
        ArgumentInterpolation::None,
        ArgumentInterpolation::Substituted,
    ]
    .into_iter()
    .any(|interpolation| key == skill_grant_key(source, skill, commands, interpolation))
}

/// Where a gate's policy table comes from (REQ-560).
///
/// Two sources, **one enforcement path**: whichever this is, the table it yields
/// is evaluated by the same [`PermissionGate::decide`]. BR-1 forbids a second
/// path around the gate; a second way to *obtain a table* is not that.
#[derive(Debug, Clone)]
enum PolicySource {
    /// A named level, expanded through [`table_for`], with the tiers
    /// `[web] permission_allow` listed folded on top. What the daemon uses.
    Level {
        level: PermissionLevel,
        web_allow: Vec<WebTier>,
    },
    /// An exact table, used as given.
    ///
    /// For fixtures that need a policy no level expresses — "every tool asks",
    /// or one tool denied and the rest allowed. Keeping this available means a
    /// test can still say precisely what it means instead of reaching for the
    /// nearest level and inheriting rows it did not ask for.
    Fixed(PermissionConfig),
}

impl PolicySource {
    /// The table this source yields right now.
    fn table(&self) -> PermissionConfig {
        match self {
            Self::Level { level, web_allow } => {
                let mut cfg = table_for(*level);
                // REQ-563 BR-4: one listed tier, one key — and since REQ-560
                // this can only relax an `ask`, so it cannot lift `plan`'s deny.
                cfg.apply_web_permission(web_allow);
                cfg
            }
            Self::Fixed(cfg) => cfg.clone(),
        }
    }

    /// The level this source names, if it names one.
    const fn level(&self) -> Option<PermissionLevel> {
        match self {
            Self::Level { level, .. } => Some(*level),
            Self::Fixed(_) => None,
        }
    }
}

/// The session-scoped permission authority.
///
/// Publishes prompts to the event bus, awaits answers via [`PendingPermissions`],
/// and remembers `*_always` answers for the life of the session.
pub struct PermissionGate {
    session_id: SessionId,
    /// Where this session's policy table comes from (REQ-560).
    ///
    /// Behind a `Mutex` because a level is **session-scoped and mutable**:
    /// `/permissions <level>` writes here and nowhere else, which is what makes
    /// BR-6 true by construction — there is no path from this field to disk, so
    /// every new session starts from config again.
    ///
    /// Read once at the top of [`Self::decide`] and never again for that
    /// decision, which is what makes BR-7 structural rather than guarded.
    policy: Mutex<PolicySource>,
    grants: Mutex<HashMap<String, RememberedGrant>>,
    events: Arc<EventBus>,
    pending: Arc<PendingPermissions>,
    /// Where `enable_permanent` writes, when anything offers it (REQ-563 BR-4).
    ///
    /// `None` on a gate nobody wired one into: the option is still offered and
    /// still allows, and the decision is recorded at the scope it actually
    /// achieved. An unwired sink is a gate that cannot promise permanence, not a
    /// gate that lies about it.
    web_persistence: Option<Arc<dyn WebTierPersistence>>,
    /// Where an addressed request is routed (REQ-585 ADR-7).
    ///
    /// `None` on a gate nobody wired one into — which, unlike
    /// [`Self::web_persistence`], is not a degraded prompt but **no prompt**:
    /// there is no honest fallback, because the fallback would be the bus, and
    /// the bus is what ADR-7 exists to keep a skill consent off. Such a gate
    /// answers [`SkillConsent::Unanswerable`] and asks nobody.
    addressed: Option<Arc<dyn AddressedPermissionDelivery>>,
}

impl PermissionGate {
    /// A gate for `session_id` pinned to an exact `config`, publishing to
    /// `events` and awaiting answers on `pending`.
    ///
    /// The gate has **no level**: [`Self::level`] answers `None` and the status
    /// row has nothing to show. For a session the user can steer, use
    /// [`Self::with_level`], which is what the daemon does.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        config: PermissionConfig,
        events: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
    ) -> Self {
        Self::from_source(session_id, PolicySource::Fixed(config), events, pending)
    }

    /// A gate for `session_id` at a named `level`, with the tiers
    /// `[web] permission_allow` lists folded onto every table it produces
    /// (REQ-560).
    ///
    /// The daemon's constructor. The level is what `/permissions` reads and
    /// writes, and what the entry frame's status row renders.
    #[must_use]
    pub fn with_level(
        session_id: SessionId,
        level: PermissionLevel,
        web_allow: Vec<WebTier>,
        events: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
    ) -> Self {
        Self::from_source(
            session_id,
            PolicySource::Level { level, web_allow },
            events,
            pending,
        )
    }

    fn from_source(
        session_id: SessionId,
        policy: PolicySource,
        events: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
    ) -> Self {
        Self {
            session_id,
            policy: Mutex::new(policy),
            grants: Mutex::new(HashMap::new()),
            events,
            pending,
            web_persistence: None,
            addressed: None,
        }
    }

    /// The session this gate answers for.
    ///
    /// Exposed for a holder that must publish *about* the same session it asks
    /// about — the `skill` tool's BR-9 echo (REQ-587 TASK-217), which rides the
    /// gate rather than a second pair of fields precisely so the record and the
    /// consent cannot come to disagree about which session they belong to.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The bus this gate publishes its own prompts on, for the same holder and
    /// the same reason as [`Self::session_id`].
    #[must_use]
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// This session's permission level, or `None` on a gate pinned to an exact
    /// table.
    #[must_use]
    pub fn level(&self) -> Option<PermissionLevel> {
        self.policy
            .lock()
            .expect("permission policy mutex poisoned")
            .level()
    }

    /// Set this session's level, answering whether it changed (REQ-560 BR-6).
    ///
    /// Session-scoped and **writes nothing**: there is deliberately no path from
    /// here to the config file. A `full` that survived a restart would remove a
    /// guardrail invisibly, in a session the user does not remember configuring.
    ///
    /// The `changed` answer exists for the same reason
    /// [`WebTaintOverride::lift`](crate::runtime::WebTaintOverride)'s does — so a
    /// confirmation stays honest and re-setting the level a session already holds
    /// is not announced as a change.
    ///
    /// Setting a level on a gate that was pinned to an exact table converts it to
    /// a leveled gate. The user named a posture; the posture governs from here.
    pub fn set_level(&self, level: PermissionLevel) -> bool {
        let mut policy = self
            .policy
            .lock()
            .expect("permission policy mutex poisoned");
        if policy.level() == Some(level) {
            return false;
        }
        let web_allow = match &*policy {
            PolicySource::Level { web_allow, .. } => web_allow.clone(),
            PolicySource::Fixed(_) => Vec::new(),
        };
        *policy = PolicySource::Level { level, web_allow };
        true
    }

    /// The table in force for the **next** decision.
    fn effective_table(&self) -> PermissionConfig {
        self.policy
            .lock()
            .expect("permission policy mutex poisoned")
            .table()
    }

    /// The sentence a call refused **by the level** returns, or `None` when the
    /// level would not refuse it (REQ-560 BR-15).
    ///
    /// `Some` means nobody was asked: the level settled the call. That is a
    /// different fact from a user declining a prompt, and the two must not share
    /// a sentence — a model told the wrong reason proposes the wrong remedy, and
    /// a user told "you declined this" about something they were never asked is
    /// being told something false.
    ///
    /// Derived from the same [`PermissionLevel`] the client renders, through the
    /// same [`PermissionLevel::denial_sentence`], so the daemon's and the
    /// client's account of one denial cannot drift (LESSON-456).
    #[must_use]
    pub fn denial_note(&self, tool: &str) -> Option<String> {
        let policy = self
            .policy
            .lock()
            .expect("permission policy mutex poisoned");
        let level = policy.level()?;
        (policy.table().policy_for(tool) == PermissionPolicy::Deny)
            .then(|| level.denial_sentence(tool))
    }

    /// Wire the sink `enable_permanent` writes through (REQ-563 BR-4).
    #[must_use]
    pub fn with_web_persistence(mut self, sink: Arc<dyn WebTierPersistence>) -> Self {
        self.web_persistence = Some(sink);
        self
    }

    /// Wire the route an addressed request is delivered on (REQ-585 ADR-7).
    ///
    /// Without it [`Self::authorize_skill`] asks nobody — see
    /// [`AddressedPermissionDelivery`] for why that is the only safe absence.
    #[must_use]
    pub fn with_addressed_delivery(mut self, route: Arc<dyn AddressedPermissionDelivery>) -> Self {
        self.addressed = Some(route);
        self
    }

    /// Decide whether `tool_name` may run, prompting the client if the policy is
    /// `ask` and no session grant already answers.
    ///
    /// A cancelled prompt, a `reject_*`, or a dropped client (channel closed) all
    /// resolve to [`PermissionDecision::Denied`] — the safe default.
    ///
    /// Web lookups do not come through here: they carry a tier, and
    /// [`Self::authorize_web`] is the entry point that takes one. Skills do not
    /// either: they carry a subject and an addressee, and
    /// [`Self::authorize_skill`] is the entry point that takes those.
    ///
    /// ## What the guard below asserts, and what it deliberately does not
    ///
    /// Each specialized entry point exists because it carries something this
    /// one cannot, and each guards the misroute **at the door that would drop
    /// it**. The web guard lives here, because a web key arriving here is a
    /// consent event with a missing tier; the skill guard lives in
    /// [`Self::authorize_skill`], because what goes wrong there is a skill
    /// consent asked under somebody else's key, and *that* is a fact only the
    /// skill door has the name and source to check.
    ///
    /// So this assertion stays narrow on purpose: it fires for a web key and
    /// **does not** fire for a skill key, nor — since REQ-587 — for a
    /// project-skill acknowledgment key. A guard whose precondition is untested
    /// is a guard whose claim is untested (LESSON-504), so every direction is
    /// asserted — see
    /// [`the_generic_door_refuses_a_web_key_and_admits_a_skill_key`](tests::the_generic_door_refuses_a_web_key_and_admits_a_skill_key),
    /// [`the_generic_door_admits_a_skill_key`](tests::the_generic_door_admits_a_skill_key)
    /// and
    /// [`the_generic_door_admits_a_project_acknowledgment_key`](tests::the_generic_door_admits_a_project_acknowledgment_key).
    /// Widening it to reject `skill:` or `project_skill_trust:` keys would turn
    /// a read of a remembered grant, or any future generic caller holding a key
    /// string, into a panic for no gain: an addressed request cannot be raised
    /// from here in the first place, because this path has no addressee to raise
    /// it to. REQ-587 adding a third key family is exactly the pressure to
    /// widen it, and exactly why it is not widened — the guard each door needs
    /// is the one that door can check, and this door can check none of them.
    pub async fn authorize(
        &self,
        tool_name: &str,
        description: Option<String>,
    ) -> PermissionDecision {
        // A web key arriving without its tier would be offered the permanent
        // option and then be unable to say which tier it made permanent. The
        // types cannot forbid it (the key is a `&str`), so it is asserted where
        // it would happen rather than left to be found in a consent event with a
        // missing tier.
        debug_assert!(
            !is_web_permission_key(tool_name),
            "`{tool_name}` is a web tier key and must be authorized through \
             `authorize_web`, which carries the tier the decision is about"
        );
        self.decide(tool_name, description, None).await
    }

    /// Decide whether one skill invocation's dynamic context may run — **one
    /// question, every command** (REQ-585 BR-6, ADR-6, ADR-7).
    ///
    /// ## One prompt per invocation, never one per command
    ///
    /// `commands` is the whole invocation, in document order, already
    /// substituted (BR-4 puts substitution before execution precisely so the
    /// consent shows what will run). A prompt per command is REQ-560 BR-2's
    /// named anti-pattern, and it is why the commands ride
    /// [`PermissionSubject::SkillDynamicContext`] as a list rather than a
    /// description string: `Surface::line` destroys newlines, so a one-line
    /// description could not list three commands verbatim (ADR-7).
    ///
    /// ## `key` is the skill's own, and never `shell`
    ///
    /// A remembered answer is attached to its key, not to the question that
    /// produced it, and every later request whose key matches inherits it
    /// (LESSON-495). Under `shell` this would run both ways: one "allow for this
    /// session" answered at a skill prompt would free every later model-issued
    /// shell call, and an earlier allow-always on `shell` would silently un-ask
    /// a skill's commands. The key also carries the **source**, because after a
    /// `/cd` the bare name would denote a different file — see
    /// [`Self::drop_project_skill_grants`] for the other half of that.
    ///
    /// `key` is taken as a parameter and *checked* against `(source, skill)`
    /// rather than derived here, so the caller's key and the gate's key are
    /// provably the same string rather than two spellings that happen to agree.
    ///
    /// ## …and since REQ-587 it may carry a digest of the substituted commands
    ///
    /// "Allow for this session" under a skill's key answers later invocations of
    /// that skill — that is what a session grant means, and it is sound while
    /// the commands do not depend on the arguments. When a command interpolates
    /// `$ARGUMENTS`/`$N` it is not sound: a caller could change what the
    /// remembered grant runs. BR-5's rule, one rule for **both** callers, is
    /// that such a grant is remembered under a key carrying a digest of the
    /// **substituted** command set — [`skill_grant_key`] is the one function
    /// that mints either spelling.
    ///
    /// So the second assertion below checks `key` against *what that minter
    /// could have produced for this `(source, skill, commands)`* rather than
    /// against one string. That is the lockstep the digest forced: a minter and
    /// an assertion that disagreed by one spelling would fire on every debug
    /// build, and an assertion loosened to "any skill key" would stop catching
    /// the misroute it exists for. Both spellings still pin the source, the name
    /// **and** — in the digest case — this exact command set; a `shell` key,
    /// another skill's key, and a digest over a different command set all still
    /// fire.
    ///
    /// ## `invoked_by` is the caller's, and it is a parameter for that reason
    ///
    /// BR-5 requires the consent to say **who asked**: "you asked for `deploy`"
    /// and "the model decided to run `deploy`" carry the same command list and
    /// are different questions. The gate cannot derive it — both callers reach
    /// this one door — so it is passed, and the wrong default would be the
    /// silent one (ADR-8).
    ///
    /// ## The prompt is addressed to `addressee`, and only it may answer
    ///
    /// Not a refinement — the guard. See [`AddressedPermissionDelivery`].
    pub async fn authorize_skill(
        &self,
        key: &str,
        skill: &str,
        source: SkillSource,
        commands: Vec<String>,
        invoked_by: InvokedBy,
        addressee: ConnectionId,
    ) -> SkillConsent {
        // The misroute this door drops, guarded at this door: a skill consent
        // asked under a key that is not the skill's own — `shell` above all —
        // is a grant remembered against the wrong question, and nothing
        // downstream can tell that from a legitimate one.
        //
        // An **acknowledgment** key fails this too, and that is the point:
        // `project_skill_trust:<root>` is deliberately not a `skill:` key, so
        // BR-4's question cannot be smuggled through this door (ADR-7).
        debug_assert!(
            is_skill_permission_key(key),
            "`{key}` is not a skill consent key; a skill's dynamic context must \
             ask under `skill:<source>:<name>` and never under a tool's name, \
             and never under the project-skill acknowledgment's key"
        );
        debug_assert!(
            is_grant_key_for(key, source, skill, &commands),
            "the key a skill's consent is remembered under must be one this \
             skill's own name, source and substituted commands mint: expected \
             `{}` or `{}`, got `{key}`",
            skill_grant_key(source, skill, &commands, ArgumentInterpolation::None),
            skill_grant_key(source, skill, &commands, ArgumentInterpolation::Substituted),
        );

        let addressed = Addressed {
            connection: addressee,
            subject: PermissionSubject::SkillDynamicContext {
                skill: skill.to_owned(),
                source,
                commands,
                invoked_by,
            },
        };

        // No `description`: the subject already carries the skill, its source
        // and every command, and a sentence restating them would be a second
        // spelling of one fact for the two to drift apart at (LESSON-456).
        self.settle_skill_consent(key, addressed, LevelAllow::Settles)
            .await
    }

    /// Decide whether the model may run **this repository's** skills as
    /// instructions at all — the project-skill acknowledgment (REQ-587 BR-4,
    /// architecture ADR-7).
    ///
    /// ## Why this is a third door and not a widened [`Self::authorize_skill`]
    ///
    /// That function asserts its key is a skill key *and* that it is one the
    /// skill's own name, source and commands mint. An acknowledgment key is
    /// neither, because it is a different question: not "may these commands
    /// run?" but "may repository text reach the model labelled *instructions*
    /// with no human typing its name?". LESSON-495's rule is that the key
    /// encodes the question and that a remembered answer frees every later
    /// request whose key matches — so one key for both would let a `y` to one
    /// answer the other. Widening two assertions pinned in both directions to
    /// admit a third key family is the change ADR-7 declined; a third door costs
    /// one function and keeps [`Self::authorize`]'s narrow web guard untouched.
    ///
    /// Nothing here grants an **effect**. `shell`, `edit` and each skill's
    /// dynamic-context key gate effects exactly as they did.
    ///
    /// ## The level table needs no row, and gets none
    ///
    /// `project_skill_trust:<root>` is unenumerated, so it falls to the level's
    /// default — which *is* BR-4's posture: `guarded`/`edits` ask, `plan`
    /// denies, `full` allows. A skill-name row is never added, for the reason
    /// REQ-560 ADR-A refuses to enumerate the open set at all.
    ///
    /// ## …except the one case `full` can be surprised by
    ///
    /// `shadows_user_skill` says this invocation is of a project skill that
    /// takes its name from a user skill the user installed. The model asks for
    /// `validate` meaning the file in `~/.claude` and gets a body the repository
    /// substituted, and BR-4 asks about that swap **even at `full`** — once per
    /// session per root, like every other answer under this key. That is spelled
    /// as [`LevelAllow::DoesNotSettle`]: an `allow` row stops settling the
    /// question, a remembered grant still answers it, and a `deny` row still
    /// denies. The override only ever asks *more*, so `plan` is unaffected —
    /// which is what keeps this a narrowing of one enforcement path rather than
    /// the second path around the gate REQ-560 BR-1 forbids.
    ///
    /// ## The listed skills are bounded here
    ///
    /// The caller hands the project's whole model-invocable set; this door
    /// truncates to [`MAX_LISTED_PROJECT_SKILLS`] and reports the tail as a
    /// count. Bounding at the door that mints the subject is what makes "at most
    /// twenty names, then `+N more`" true of every prompt rather than of every
    /// caller that remembered — an unbounded prompt is LESSON-517's shape.
    pub async fn authorize_project_skill_trust(
        &self,
        key: &str,
        root: &str,
        skills: &[ProjectSkillTrustEntry],
        shadows_user_skill: bool,
        addressee: ConnectionId,
    ) -> SkillConsent {
        // The misroute this door drops. A skill's own key here would remember
        // "the model may run this repository's skills" under the question "may
        // `/deploy`'s commands run", and nothing downstream could tell the two
        // apart — the mirror image of the guard one function up.
        debug_assert!(
            is_project_acknowledgment_key(key),
            "`{key}` is not a project-skill acknowledgment key; BR-4's question \
             asks under `project_skill_trust:<root>` and never under a skill's \
             own key or a tool's name"
        );
        // The display and the key are two renderings of one root, and this door
        // is handed both rather than deriving one from the other — it cannot.
        // `root` is `display_for`'s spelling, which is lossy; `key` is minted
        // from `key_form_for`'s, which is injective. That gap is the whole
        // point: a root whose bytes are not valid UTF-8 has a display that names
        // it ambiguously and a key that does not. The two agree exactly when the
        // display is faithful, so that is what is checked here, and the pairing
        // itself is guaranteed where both are minted — `harness/tools/skill.rs`,
        // the one place either is computed (ASSUME-017).
        debug_assert!(
            root.contains(char::REPLACEMENT_CHARACTER)
                || root.contains('%')
                || key == project_skill_trust_key(root),
            "the key the acknowledgment is remembered under must be the key this \
             root mints, or the user answers about one repository and the grant \
             is kept for another"
        );

        let (listed, more) = bound_listed_skills(skills);
        let addressed = Addressed {
            connection: addressee,
            subject: PermissionSubject::ProjectSkillTrust {
                root: root.to_owned(),
                skills: listed,
                more,
            },
        };

        let level_allow = if shadows_user_skill {
            LevelAllow::DoesNotSettle
        } else {
            LevelAllow::Settles
        };
        // No `description`, for [`Self::authorize_skill`]'s reason: the subject
        // carries the root and the named set, and a sentence restating them
        // would be a second spelling of one fact (LESSON-456).
        self.settle_skill_consent(key, addressed, level_allow).await
    }

    /// The shared tail of both skill doors: settle an addressed request and
    /// narrow the provenance to the five answers a skill caller can act on.
    ///
    /// One body, so the two doors cannot come to disagree about which settlement
    /// is a decline and which is a refusal — the distinction REQ-585 AC-9 exists
    /// for, and the one a second copy would drift on first.
    async fn settle_skill_consent(
        &self,
        key: &str,
        addressed: Addressed,
        level_allow: LevelAllow,
    ) -> SkillConsent {
        match self
            .settle(key, None, None, Some(addressed), level_allow)
            .await
        {
            Settled::ByLevel(PermissionDecision::Allowed)
            | Settled::ByGrant(PermissionDecision::Allowed)
            | Settled::ByHuman(PermissionDecision::Allowed) => SkillConsent::Allowed,
            Settled::ByLevel(PermissionDecision::Denied) => SkillConsent::DeniedByLevel,
            Settled::ByGrant(PermissionDecision::Denied)
            | Settled::ByHuman(PermissionDecision::Denied) => SkillConsent::Declined,
            Settled::Refused(reason) => SkillConsent::Refused(reason),
            Settled::Unanswerable => SkillConsent::Unanswerable,
        }
    }

    /// Forget every remembered grant a session root move invalidates (REQ-585
    /// ADR-6, REQ-587 ASSUME-017), answering how many were dropped.
    ///
    /// Called on `/cd`. The grant map is state carried past the thing that gave
    /// it meaning, and carried state sheds its invariants silently (LESSON-501):
    /// `skill:project:deploy` named one repo's file when the user consented to
    /// it and names another repo's file the instant the session root moves. The
    /// source in the key narrows the collision to project-vs-project; dropping
    /// these closes it.
    ///
    /// **Two families since REQ-587**, and the reason they are swept together is
    /// the reason the predicate is a function: a project skill's
    /// dynamic-context grant *and* the project-skill acknowledgment
    /// ([`project_skill_trust_key`]) both mean "this root", and an
    /// acknowledgment that outlived the root would let the model run a *second*
    /// repository's skills as instructions on an answer the user gave about a
    /// first. [`expires_on_session_root_change`] is the one invalidation rule,
    /// spelled above both crates because the client's `SessionGrants` memoizes
    /// the same keys and consults its copy *before* drawing any prompt — two
    /// stores that disagreed about which keys expire would auto-answer the new
    /// root's question with the old root's answer, and no human would be shown
    /// anything (ASSUME-017). The daemon-side drop and the client-side drop are
    /// the same moment.
    ///
    /// The name is REQ-585's and is now narrower than what the sweep does; the
    /// call sites (`DaemonRuntime::drop_project_skill_grants`) belong to
    /// TASK-217's file, so renaming it is left to the task that already edits
    /// them.
    ///
    /// **Every** grant, not just the allowing ones. A `reject_always` recorded
    /// against one repo's `deploy` is an answer about that file too, and the
    /// worst it costs to drop is one question the user is asked again — which is
    /// the direction to be wrong in.
    ///
    /// User grants are deliberately kept: `~/.claude` does not move when the
    /// session root does, so `skill:user:status` names the same file it named
    /// when it was answered.
    pub fn drop_project_skill_grants(&self) -> usize {
        let mut grants = self
            .grants
            .lock()
            .expect("permission grants mutex poisoned");
        let before = grants.len();
        grants.retain(|key, _| !expires_on_session_root_change(key));
        before - grants.len()
    }

    /// Decide whether a web lookup at `tier` may run (REQ-563 BR-3/BR-4).
    ///
    /// Two things separate this from [`Self::authorize`], and both come from the
    /// tier being in hand:
    ///
    /// - the prompt offers a fifth option, `enable_permanent`, which writes the
    ///   tier to config (BR-4 makes that the only consent path that writes
    ///   anything);
    /// - whatever the user answers is published as a
    ///   [`WebConsentDecided`] event, at the scope the answer actually achieved.
    ///
    /// `permission_key` stays separate from `tier` because they are different
    /// questions asked of different things. The key names the **subject a
    /// session grant is remembered under** — one per tier above `off`
    /// (`web_fetch_user_url`, `web_fetch_any_url`, `web_search`); the tier names
    /// **what an `enable_permanent` answer writes to config**. They are in step
    /// today ([`permission_key_for`](super::tools::web::permission_key_for) is
    /// the one mapping), and the parameters stay separate anyway: the tier here
    /// is the *lookup's*, already checked against the configured ceiling, so a
    /// prompt can never offer to persist a tier this lookup was not entitled to.
    pub async fn authorize_web(
        &self,
        permission_key: &str,
        description: Option<String>,
        tier: WebTier,
    ) -> PermissionDecision {
        self.decide(permission_key, description, Some(tier)).await
    }

    /// The shared body of both entry points; `web` carries the tier when this is
    /// a web-lookup decision and is `None` for every other tool.
    ///
    /// ## The level is read once, here, and never again (REQ-560 BR-7)
    ///
    /// The table is snapshotted at the top and everything below decides against
    /// that snapshot. Nothing after the `await` re-reads the level, and nothing
    /// on the level-change path touches [`PendingPermissions`] — so a
    /// `/permissions` arriving while this call is parked on its prompt cannot
    /// resolve it in either direction. The user's own answer decides the call
    /// they were asked about; the *next* call decides at the new level. That is
    /// BR-7, and it holds because of the shape of this function rather than
    /// because of a check someone remembered to add.
    async fn decide(
        &self,
        tool_name: &str,
        description: Option<String>,
        web: Option<WebTier>,
    ) -> PermissionDecision {
        self.settle(tool_name, description, web, None, LevelAllow::Settles)
            .await
            .decision()
    }

    /// [`Self::decide`] with the provenance of the answer kept (REQ-585 AC-9)
    /// and, when `addressed` is `Some`, the request routed to one connection
    /// instead of published (ADR-7).
    ///
    /// Everything about the ordering below is [`Self::decide`]'s and unchanged:
    /// the level is read once at the top, grants are consulted after it, and
    /// nothing re-reads the level across the await.
    ///
    /// `level_allow` is [`LevelAllow::Settles`] for every caller but BR-4's
    /// shadowing case — see that type for why an override that can only ever ask
    /// *more* is a narrowing of this one path rather than a second one around it.
    async fn settle(
        &self,
        tool_name: &str,
        description: Option<String>,
        web: Option<WebTier>,
        addressed: Option<Addressed>,
        level_allow: LevelAllow,
    ) -> Settled {
        // ## Level before grants (REQ-560 BR-5)
        //
        // A grant is an answer to a question the level decides whether to ask,
        // so a grant can never outrank the level that would not have asked. This
        // ordering is the inverse of the pre-REQ-560 one and it is the whole of
        // BR-5: switching to `plan` denies a tool the user allow-always'd
        // earlier in the session, and switching back restores it — the grant was
        // never discarded, it simply stopped being consulted while the level had
        // no question to answer.
        //
        // The consequence in the other direction is deliberate: an `allow` from
        // the level supersedes a `reject_always` grant. `/permissions full` is a
        // typed act by the same user, later in time than the grant, and a level
        // change that silently did not do what it said is the guard-that-quietly-
        // stops-guarding shape BR-4 exists to forbid. The web keys are unaffected
        // — they sit at `ask` at every level, so REQ-563's capability refusals
        // still reach the grant below.
        //
        // Nothing is published for a policy answer: `allow` and `deny` rows are
        // configuration, and no one decided anything just now.
        match self.effective_table().policy_for(tool_name) {
            // The one caller that does not take this arm is BR-4's shadowing
            // acknowledgment, which falls through to the grant and then to the
            // prompt. `deny` below is *not* overridable, so the override can
            // only ever ask more.
            PermissionPolicy::Allow if level_allow == LevelAllow::Settles => {
                return Settled::ByLevel(PermissionDecision::Allowed)
            }
            PermissionPolicy::Deny => return Settled::ByLevel(PermissionDecision::Denied),
            PermissionPolicy::Allow | PermissionPolicy::Ask => {}
        }

        // A remembered session grant answers the question the level just asked
        // (asked once).
        //
        // No consent event here, deliberately: the decision this replays was
        // published when it was *made*, and re-announcing it per lookup would
        // turn one decision into a stream of them.
        if let Some(grant) = self.session_grant(tool_name) {
            return match grant {
                RememberedGrant::AllowAlways => Settled::ByGrant(PermissionDecision::Allowed),
                RememberedGrant::RejectAlways => Settled::ByGrant(PermissionDecision::Denied),
            };
        }

        // An addressed request has exactly one recipient, and a gate with no
        // route to it has nowhere to ask. Checked **before** a waiter is
        // registered, so the fail-closed path leaves no entry behind — and
        // never by falling back to the bus, which is the one thing addressing
        // exists to prevent (ADR-7).
        let route = match &addressed {
            Some(_) => match &self.addressed {
                Some(route) => Some(Arc::clone(route)),
                None => return Settled::Unanswerable,
            },
            None => None,
        };

        // Register the waiter, deliver the prompt, then await — no lock is held
        // across the await.
        let request_id = self.pending.next_request_id();
        // The owning session travels with the waiter, so the answer that comes
        // back can be authorized against it (REQ-569 BR-9): this gate is the
        // only place that knows whose tool call is about to block. So does the
        // addressee, for the same reason and with a stricter consequence — an
        // answer from anyone else is refused (REQ-585 ADR-7).
        let addressee = addressed.as_ref().map(|a| a.connection);
        let rx = self
            .pending
            .register(request_id.clone(), self.session_id.clone(), addressee);

        let request = PermissionRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.to_owned(),
            description,
            // Present only for a request a client must be able to recognize
            // **without parsing the key** (REQ-585 BR-11). Every request raised
            // for a tool call is a tool call, and has no subject.
            subject: addressed.map(|a| a.subject),
            options: options_for(web),
        };

        match route {
            // Addressed: routed to one connection, never published. A frame the
            // connection will not take is a prompt nobody will ever see, so the
            // waiter is withdrawn rather than left parked on an answer that
            // cannot come.
            Some(route) => {
                if !route.deliver(
                    addressee.expect("a route implies an addressee"),
                    &self.session_id,
                    request,
                ) {
                    self.pending.withdraw(&request_id);
                    return Settled::Unanswerable;
                }
            }
            None => self.events.publish(
                Some(self.session_id.clone()),
                Event::PermissionRequest(request),
            ),
        }

        match rx.await {
            Ok(outcome) => self.interpret(tool_name, outcome, web),
            // Client disconnected before answering: deny (never run unapproved).
            // Not a consent decision — nobody decided it — so nothing is
            // published; a `web_consent_decided { granted: false }` here would
            // record a refusal the user never gave. For the same reason it is
            // not a *decline* either: the caller is told nobody could be asked
            // (REQ-585 AC-9), not that someone said no.
            Err(_) => Settled::Unanswerable,
        }
    }

    /// Interpret a client's chosen option, recording any `*_always` grant and —
    /// for a web decision — publishing it at the scope it achieved.
    fn interpret(
        &self,
        tool_name: &str,
        outcome: PermissionOutcome,
        web: Option<WebTier>,
    ) -> Settled {
        // A client that refused fail-closed did not make a consent decision —
        // nobody decided it. Deny, remember nothing, and publish nothing, for
        // the same reason the disconnect arm above publishes nothing: a
        // `web_consent_decided { granted: false }` here would record a refusal
        // the user never gave.
        //
        // The `reason` rides out to the caller in [`Settled::Refused`], so a
        // skill's not-run placeholder can say *no human could be asked* rather
        // than *declined* (REQ-585 AC-9). A tool call cannot use it and
        // [`Settled::decision`] drops it there — dropped where it is useless,
        // never before the one caller that needs it.
        if let PermissionOutcome::Refused { reason } = outcome {
            return Settled::Refused(reason);
        }

        let (decision, scope) = match outcome {
            PermissionOutcome::Selected { option_id } => match option_id.as_str() {
                OPTION_ALLOW_ONCE => (PermissionDecision::Allowed, WebConsentScope::Once),
                OPTION_ALLOW_ALWAYS => {
                    self.remember(tool_name, RememberedGrant::AllowAlways);
                    (PermissionDecision::Allowed, WebConsentScope::Session)
                }
                // Only reachable when the option was offered, which is only when
                // a tier is in hand: an id that was not on the prompt is not an
                // answer to it, and falls through to the deny arm below.
                OPTION_ID_ENABLE_PERMANENT if web.is_some() => {
                    // The session grant is recorded whether or not the write
                    // lands: the user said yes to this session either way, and
                    // making the answer contingent on a filesystem would re-ask
                    // a question already answered.
                    self.remember(tool_name, RememberedGrant::AllowAlways);
                    let scope = self.persist_web_tier(web.unwrap_or(WebTier::Off));
                    (PermissionDecision::Allowed, scope)
                }
                OPTION_REJECT_ALWAYS => {
                    self.remember(tool_name, RememberedGrant::RejectAlways);
                    (PermissionDecision::Denied, WebConsentScope::Session)
                }
                // reject_once and any unknown id: deny this once.
                _ => (PermissionDecision::Denied, WebConsentScope::Once),
            },
            PermissionOutcome::Cancelled => (PermissionDecision::Denied, WebConsentScope::Once),
            // Handled above, ahead of the web publish.
            PermissionOutcome::Refused { .. } => {
                (PermissionDecision::Denied, WebConsentScope::Once)
            }
        };

        if let Some(tier) = web {
            self.events.publish(
                Some(self.session_id.clone()),
                Event::WebConsentDecided(WebConsentDecided {
                    scope,
                    tier: to_protocol_web_tier(tier),
                    granted: decision == PermissionDecision::Allowed,
                }),
            );
        }
        Settled::ByHuman(decision)
    }

    /// Write `tier` through the persistence seam, answering with the scope the
    /// attempt actually achieved.
    ///
    /// A failure is reported to the operator's stderr and downgraded to
    /// [`WebConsentScope::Session`] rather than turned into a denial: the user
    /// said yes, and the only thing that did not happen is the part that would
    /// have outlived the session. The line names the reason so "why am I being
    /// asked again next time" has an answer on this machine.
    ///
    /// **Deliberately NOT a BR-10(b) commitment (REQ-576 ADR-3).** This durable
    /// write reaches the same `config.toml` the daemon-wide commitments do, so it
    /// meets REQ-575 BR-5's classification trigger — and it is **accepted, not
    /// gated**. It is raise-only within an already-configured `[web]` table (it
    /// appends the answered tier and raises the ceiling — see
    /// [`crate::runtime::DaemonRuntime::persist_web_tier`], the seam it writes
    /// through) and cannot author an endpoint, a credential, or a new capability —
    /// the powers `config/set`/`web/setup_commit` gate.
    ///
    /// **The residual, stated honestly.** The tier raise *is* a bounded capability
    /// increase, so the "a same-UID process can edit `config.toml` anyway"
    /// argument — the very one REQ-575/REQ-576 reject for `config/set` — is what is
    /// being leaned on here. The distinction that justifies the asymmetry: this
    /// path can only *raise a tier within web the user already configured*, up to
    /// the ceiling they set and past `Config::validate`, reached only when the user
    /// themselves answers `enable_permanent` on a genuine web-consent prompt in
    /// their own session (a `permission/respond` answer, not a daemon-wide
    /// commitment method) — never author a *new* endpoint/boundary the way
    /// `config/set` can. Gating it would put a Touch ID prompt on that ordinary
    /// "yes, permanently" answer (a REQ-570 AC-8 regression) for marginal value.
    /// Whether a consent answer should be further bounded in *which* tier it may
    /// persist is left as a follow-up (REQ-576 OQ). The happy path is exercised by
    /// `enable_permanent_writes_a_ceiling_the_next_daemon_start_honours`
    /// (web_consent_matrix.rs) — which shows it persists with no presence step, but
    /// is not a fail-closed regression pin (see that test's own note).
    fn persist_web_tier(&self, tier: WebTier) -> WebConsentScope {
        let Some(sink) = &self.web_persistence else {
            eprintln!(
                "teton: web lookup was enabled for this session only — this daemon has no \
                 configured place to record the choice permanently."
            );
            return WebConsentScope::Session;
        };
        match sink.persist_web_tier(tier) {
            Ok(()) => WebConsentScope::Persistent,
            Err(err) => {
                eprintln!(
                    "teton: web lookup was enabled for this session only — the choice could \
                     not be made permanent ({err})."
                );
                WebConsentScope::Session
            }
        }
    }

    /// The session answer already recorded for `tool_name`, if any — a **read**,
    /// with no prompt, no policy consultation and no side effect.
    ///
    /// For the one caller that reaches a decision without going through
    /// [`Self::decide`]: REQ-563's cache hit, which performs no egress and so
    /// asks no consent, but must still refuse when the user has said "reject for
    /// this session". It deliberately does *not* fold in
    /// [`PermissionConfig::policy_for`] — a `deny` policy row is a different
    /// fact from a user's answer, and a caller that wants the full decision has
    /// [`Self::authorize_web`] for it.
    #[must_use]
    pub fn remembered(&self, tool_name: &str) -> Option<RememberedGrant> {
        self.session_grant(tool_name)
    }

    fn session_grant(&self, tool_name: &str) -> Option<RememberedGrant> {
        self.grants
            .lock()
            .expect("permission grants mutex poisoned")
            .get(tool_name)
            .copied()
    }

    fn remember(&self, tool_name: &str, grant: RememberedGrant) {
        self.grants
            .lock()
            .expect("permission grants mutex poisoned")
            .insert(tool_name.to_owned(), grant);
    }
}

const OPTION_ALLOW_ONCE: &str = "allow_once";
const OPTION_ALLOW_ALWAYS: &str = "allow_always";
const OPTION_REJECT_ONCE: &str = "reject_once";
const OPTION_REJECT_ALWAYS: &str = "reject_always";

/// Whether `tool_name` is one of the web tiers' consent keys (REQ-563 BR-3).
///
/// **Three** keys and not one: a grant is remembered under exactly the string it
/// was asked about, so `web` as a single key would have made one "allow for this
/// session" on a page fetch silently grant every search — and `web_fetch` as a
/// shared fetch key would have made one answer about a URL the *user pasted*
/// grant every URL the *model composes*, which is the mixed-authorship case BR-3
/// names first.
#[must_use]
pub fn is_web_permission_key(tool_name: &str) -> bool {
    WEB_PERMISSION_KEYS.contains(&tool_name)
}

/// Whether `key` is a skill's dynamic-context consent key (REQ-585 BR-6,
/// ADR-6).
///
/// **One key per skill *and per source*, and never `shell`** — the same
/// argument [`is_web_permission_key`] makes for having three keys instead of
/// one, applied to a family that is open rather than closed. A remembered answer
/// is not attached to the question that produced it; it is attached to its key,
/// and every later request whose key matches inherits that answer whether or not
/// a human would call it the same question (LESSON-495). So:
///
/// - not `shell`, in **both** directions — an earlier allow-always on `shell`
///   would silently un-ask a skill's commands, and one "allow for this session"
///   answered at a skill prompt would free every later model-issued shell call;
/// - not one key for all skills — "may `/status` run its commands?" and "may
///   `/deploy` run its commands?" are different sentences with different
///   commands under them;
/// - and the **source** is in the key, because `skill:analyze` names one repo's
///   file before a `/cd` and another repo's file after it. That narrows the
///   collision to project-vs-project, which
///   [`PermissionGate::drop_project_skill_grants`] then closes.
///
/// The prefixes are taken from the one function that mints these keys
/// ([`crate::skills::permission_key_for`]) rather than spelled again here, so
/// the recognizer and the minter cannot drift into disagreeing about what a
/// skill key looks like — which, on a predicate that gates
/// [`PermissionGate::authorize_skill`]'s guard, would be a misrouted consent
/// nothing downstream could detect.
#[must_use]
pub fn is_skill_permission_key(key: &str) -> bool {
    // Both sources, listed: `SkillSource` is a closed two-variant enum, and a
    // third would have to be added here to be recognized — a visible edit
    // rather than a key family that silently stops matching.
    [SkillSource::User, SkillSource::Project]
        .into_iter()
        .any(|source| {
            key.strip_prefix(skill_key_prefix(source).as_str())
                // A bare `skill:user:` names no skill; a grant under it would be
                // an answer to no question.
                .is_some_and(|name| !name.is_empty())
        })
}

/// The `skill:<source>:` prefix every key from `source` starts with, minted by
/// the one mapping that mints the keys themselves.
fn skill_key_prefix(source: SkillSource) -> String {
    skill_permission_key_for(source, "")
}

/// The options offered on a prompt: the four standard ones, plus the persistent
/// enable when `web` names the tier a decision could be written down at.
///
/// Keyed on the tier rather than on the tool name, so the option and the value
/// it would persist come from **one** source: an `enable_permanent` that could
/// be offered without a tier in hand is an option the daemon could not honour.
/// That is also why the fifth option is web-only rather than universal — there
/// is no `[shell] tier` to write, and an "always" that quietly edited config
/// would be a much larger promise than the one the prompt makes.
///
/// A skill's dynamic context gets the standard four for exactly that reason
/// (REQ-585): there is no `[skills] tier` either, and "never ask about
/// `/deploy` on this machine again" is a durable grant over file-supplied shell
/// commands in a file the daemon re-reads every session — a promise a consent
/// prompt has no business making. The absence is asserted rather than assumed,
/// because it is the kind of option that gets added for symmetry.
fn options_for(web: Option<WebTier>) -> Vec<PermissionOption> {
    let mut options = vec![
        PermissionOption {
            option_id: OPTION_ALLOW_ONCE.to_owned(),
            label: "Allow once".to_owned(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: OPTION_ALLOW_ALWAYS.to_owned(),
            label: "Allow for this session".to_owned(),
            kind: PermissionOptionKind::AllowAlways,
        },
    ];
    if let Some(tier) = web {
        // Named in the label, because BR-4 wants consent concrete: "enable
        // permanently" alone does not say what is being enabled, and the two
        // fetch tiers are a distinction a user has to be able to see.
        //
        // The label names the key that is actually written. It used to promise
        // `[web] tier = "…"`, which is the raise-only ceiling — and the ceiling
        // is checked *before* any prompt exists, so the tier write was a no-op in
        // every case a user could reach this option. The durable effect is the
        // consent list, and the label says so, including the "+=" that makes the
        // per-tier append visible: this answer adds one tier, and leaves the
        // other two asking.
        let name = tier_name(tier);
        options.push(PermissionOption {
            option_id: OPTION_ID_ENABLE_PERMANENT.to_owned(),
            label: format!(
                "Enable permanently (writes `[web] permission_allow += \"{name}\"` — stop asking \
                 about {name} lookups on this machine)"
            ),
            kind: PermissionOptionKind::AllowAlways,
        });
    }
    options.push(PermissionOption {
        option_id: OPTION_REJECT_ONCE.to_owned(),
        label: "Reject once".to_owned(),
        kind: PermissionOptionKind::RejectOnce,
    });
    options.push(PermissionOption {
        option_id: OPTION_REJECT_ALWAYS.to_owned(),
        label: "Reject for this session".to_owned(),
        kind: PermissionOptionKind::RejectAlways,
    });
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::grants::GrantRegistry;
    use crate::harness::tools::web::{
        permission_key_for, PERMISSION_KEY_FETCH_ANY_URL, PERMISSION_KEY_FETCH_USER_URL,
        PERMISSION_KEY_SEARCH,
    };

    // ---- REQ-560: the level → table classifier -------------------------------

    /// The expected table for every level, **spelled out here rather than
    /// derived from the code under test** (REQ-560 AC-1).
    ///
    /// This is the point of the test. `coding_defaults()` and `permissive()` now
    /// delegate to [`table_for`], so asserting `table_for(Guarded) ==
    /// coding_defaults()` would be a tautology that catches nothing. Writing the
    /// rows out by hand is what makes a change to the one table fail here — the
    /// rows are the specification, and the code has to keep agreeing with them.
    ///
    /// `None` in the second slot means "this tool is not listed and must resolve
    /// to the level's default".
    fn expected_rows(
        level: PermissionLevel,
    ) -> (PermissionPolicy, Vec<(&'static str, PermissionPolicy)>) {
        use PermissionPolicy::{Allow, Ask, Deny};
        match level {
            PermissionLevel::Guarded => (
                Ask,
                vec![
                    ("read", Allow),
                    ("glob", Allow),
                    ("grep", Allow),
                    (DOCS_TOOL_NAME, Allow),
                    // REQ-587 BR-11: the tool's own posture is read-only at
                    // every level, so no level raises an "allow `skill`?"
                    // prompt. The finer questions have their own keys.
                    ("skill", Allow),
                    ("edit", Ask),
                    ("shell", Ask),
                ],
            ),
            PermissionLevel::Edits => (
                Ask,
                vec![
                    ("read", Allow),
                    ("glob", Allow),
                    ("grep", Allow),
                    (DOCS_TOOL_NAME, Allow),
                    ("skill", Allow),
                    ("edit", Allow),
                    ("shell", Ask),
                ],
            ),
            PermissionLevel::Plan => (
                Deny,
                vec![
                    ("read", Allow),
                    ("glob", Allow),
                    ("grep", Allow),
                    // The level whose whole promise is "reading only" must not
                    // be the level that refuses the daemon's own documentation
                    // (REQ-577 BR-6; TASK-147 F-2 found it denied here).
                    (DOCS_TOOL_NAME, Allow),
                    // Same argument, one REQ later: `plan` must not be the
                    // level that refuses to read a skill the user installed.
                    // BR-4 still denies the *project* acknowledgment here —
                    // that is a different key, and it falls to this level's
                    // `Deny` default without a row.
                    ("skill", Allow),
                    ("edit", Deny),
                    ("shell", Deny),
                ],
            ),
            PermissionLevel::Full => (
                Allow,
                vec![
                    ("read", Allow),
                    ("edit", Allow),
                    ("shell", Allow),
                    ("skill", Allow),
                    (PERMISSION_KEY_FETCH_USER_URL, Ask),
                    (PERMISSION_KEY_FETCH_ANY_URL, Ask),
                    (PERMISSION_KEY_SEARCH, Ask),
                ],
            ),
        }
    }

    #[test]
    fn each_level_expands_to_its_documented_table() {
        for level in PermissionLevel::ALL {
            let table = table_for(*level);
            let (default, rows) = expected_rows(*level);
            for (tool, want) in rows {
                assert_eq!(
                    table.policy_for(tool),
                    want,
                    "{level}: `{tool}` should be {want:?}"
                );
            }
            // An unlisted name resolves to the level's default — which is what
            // makes the default the level's classification of the open set.
            assert_eq!(
                table.policy_for("a-tool-no-level-mentions"),
                default,
                "{level}: an unlisted tool should fall to the default"
            );
        }
    }

    /// The delegation itself, so a future edit that reintroduces a second copy
    /// of these rows fails here rather than drifting quietly (REQ-560 BR-1).
    #[test]
    fn the_legacy_presets_are_exactly_the_guarded_and_full_levels() {
        assert_eq!(
            PermissionConfig::coding_defaults(),
            table_for(PermissionLevel::Guarded)
        );
        assert_eq!(
            PermissionConfig::permissive(),
            table_for(PermissionLevel::Full)
        );
    }

    /// REQ-560 AC-17, table half: driven off `ALL`, so a fifth level is covered
    /// the moment it joins the array.
    #[test]
    fn every_level_answers_for_every_surface() {
        for level in PermissionLevel::ALL {
            let table = table_for(*level);
            // Total: every tool name resolves to *some* policy, listed or not.
            for tool in ["read", "edit", "shell", "", "wildly::unknown"] {
                let _: PermissionPolicy = table.policy_for(tool);
            }
            assert!(!level.summary().is_empty());
            assert!(level.denial_sentence("edit").contains(level.name()));
        }
    }

    /// REQ-560 OQ-2: an MCP tool's name is server-supplied, so no level may
    /// enumerate it — and none has to. The name below appears nowhere in the
    /// daemon, which is exactly the point.
    /// **Extended by REQ-585 (ADR-6), not replaced.** A skill's consent key is
    /// the second name no level enumerates, and it must ride the same default
    /// — `guarded` ask, `edits` ask, `plan` deny, `full` allow — so that
    /// [`table_for`] and [`READ_ONLY_TOOLS`] need no skill row at all. Asserted
    /// here, beside the MCP case, because it is the *same* claim about the same
    /// mechanism: a level's `default` **is** its answer to a name it has never
    /// heard of, and a skill name is user-supplied for the same reason an MCP
    /// tool name is server-supplied. Giving skills their own row would be the
    /// beginning of the enumeration REQ-560 ADR-A refuses.
    #[test]
    fn an_unknown_server_supplied_tool_is_classified_by_the_levels_default() {
        let unknown = "mcp__some_server__some_tool_nobody_declared";
        let skill = skill_permission_key_for(SkillSource::User, "status");
        for name in [unknown, skill.as_str()] {
            assert_eq!(
                table_for(PermissionLevel::Guarded).policy_for(name),
                PermissionPolicy::Ask,
                "`{name}` at guarded"
            );
            assert_eq!(
                table_for(PermissionLevel::Edits).policy_for(name),
                PermissionPolicy::Ask,
                "`{name}` at edits"
            );
            // Fail-closed at the level whose promise is that nothing changes.
            assert_eq!(
                table_for(PermissionLevel::Plan).policy_for(name),
                PermissionPolicy::Deny,
                "`{name}` at plan"
            );
            assert_eq!(
                table_for(PermissionLevel::Full).policy_for(name),
                PermissionPolicy::Allow,
                "`{name}` at full"
            );
        }
    }

    /// **The key family recognizer, in both directions** (REQ-585 ADR-6).
    ///
    /// The negative half is the load-bearing one: `shell` is what the key must
    /// never be, and `web_*` is the neighbouring family whose own guard must not
    /// start catching this one.
    #[test]
    fn only_a_sourced_skill_key_reads_as_a_skill_key() {
        for source in [SkillSource::User, SkillSource::Project] {
            let key = skill_permission_key_for(source, "status");
            assert!(is_skill_permission_key(&key), "`{key}` is a skill key");
            assert!(
                !is_web_permission_key(&key),
                "`{key}` must not read as a web key"
            );
        }
        for other in [
            "shell",
            "edit",
            "read",
            // The source is not optional: a key that dropped it is not a skill
            // key, because it is not a key this daemon could have minted.
            "skill:status",
            // Nor is a prefix with no skill behind it.
            "skill:user:",
            "skill:",
            // Nor a name that merely starts the same way.
            "skillful:user:status",
            PERMISSION_KEY_SEARCH,
        ] {
            assert!(
                !is_skill_permission_key(other),
                "`{other}` must not read as a skill key"
            );
        }
    }

    /// **REQ-585 ADR-6 / LESSON-504: the generic door's guard, in both
    /// directions.**
    ///
    /// A guard whose precondition is untested is a guard whose claim is
    /// untested. Two claims, and they are separate: the web assertion still
    /// fires (so a web key cannot reach [`PermissionGate::decide`] without its
    /// tier), and it does **not** fire for a skill key (so the new family did
    /// not silently widen a predicate the web path depends on being exact
    /// about). The positive half lives in
    /// [`the_generic_door_admits_a_skill_key`] because a `should_panic` test
    /// cannot also assert what happens when nothing panics.
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "must be authorized through `authorize_web`")]
    async fn the_generic_door_refuses_a_web_key_and_admits_a_skill_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate.authorize(PERMISSION_KEY_SEARCH, None).await;
    }

    /// The other direction of
    /// [`the_generic_door_refuses_a_web_key_and_admits_a_skill_key`]: a skill
    /// key through [`PermissionGate::authorize`] does not trip the web guard.
    ///
    /// It is not how a skill's dynamic context is authorized — that is
    /// [`PermissionGate::authorize_skill`], which carries the addressee this
    /// door has no way to name — but reading a policy row or a remembered grant
    /// by key must not be a panic, and widening the web guard would make it one.
    #[tokio::test]
    async fn the_generic_door_admits_a_skill_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        assert_eq!(
            gate.authorize(&skill_permission_key_for(SkillSource::User, "status"), None)
                .await,
            PermissionDecision::Allowed
        );
    }

    /// **The skill door's own guard** (REQ-585 ADR-6): a skill consent asked
    /// under a key that is not the skill's own is the exact defect the key
    /// exists to prevent, and `shell` is the one it would be.
    ///
    /// Placed at that door rather than at [`PermissionGate::authorize`]'s,
    /// because only this door holds the name and source the key must agree with.
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "is not a skill consent key")]
    async fn the_skill_door_refuses_a_key_that_is_not_the_skills_own() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate
            .authorize_skill(
                "shell",
                "status",
                SkillSource::User,
                vec!["git status".to_owned()],
                InvokedBy::User,
                GrantRegistry::new().next_connection_id(),
            )
            .await;
    }

    /// A key of the right *shape* still has to be the key this skill's own name
    /// and source mint — otherwise one skill's answer is remembered against
    /// another's question (LESSON-495).
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "source and substituted commands mint")]
    async fn the_skill_door_refuses_another_skills_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate
            .authorize_skill(
                &skill_permission_key_for(SkillSource::Project, "canary"),
                "status",
                SkillSource::User,
                vec!["git status".to_owned()],
                InvokedBy::User,
                GrantRegistry::new().next_connection_id(),
            )
            .await;
    }

    // ---- REQ-587 BR-4 / ADR-7: the third door ------------------------------

    /// **ADR-7, the mutation the whole task is about: an acknowledgment cannot
    /// ride [`PermissionGate::authorize_skill`].**
    ///
    /// `project_skill_trust:<root>` is deliberately not a `skill:` key, so the
    /// skill door's *first* guard rejects it — which is the mechanical reason a
    /// third door exists rather than a widened one. An implementation that
    /// "simplified" BR-4 by reusing `authorize_skill` fails here on the first
    /// debug build.
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "is not a skill consent key")]
    async fn the_skill_door_refuses_the_project_acknowledgment_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate
            .authorize_skill(
                &project_skill_trust_key("~/dev/teton"),
                "status",
                SkillSource::User,
                vec!["git status".to_owned()],
                InvokedBy::Model,
                GrantRegistry::new().next_connection_id(),
            )
            .await;
    }

    /// The mirror image, and the half that keeps the third door from becoming a
    /// second way to ask the *first* question: a skill's own key here would
    /// remember "the model may run this repository's skills" under "may
    /// `/deploy`'s commands run".
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "is not a project-skill acknowledgment key")]
    async fn the_acknowledgment_door_refuses_a_skills_own_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate
            .authorize_project_skill_trust(
                &skill_permission_key_for(SkillSource::Project, "deploy"),
                "~/dev/teton",
                &[],
                false,
                GrantRegistry::new().next_connection_id(),
            )
            .await;
    }

    /// The acknowledgment's second guard, pinned in the same direction
    /// `authorize_skill`'s is: a key of the right *shape* still has to be the
    /// key **this root** mints, or the user answers about one repository and
    /// the grant is kept for another.
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "must be the key this root mints")]
    async fn the_acknowledgment_door_refuses_another_roots_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        let _ = gate
            .authorize_project_skill_trust(
                &project_skill_trust_key("~/dev/other"),
                "~/dev/teton",
                &[],
                false,
                GrantRegistry::new().next_connection_id(),
            )
            .await;
    }

    /// **The generic door admits an acknowledgment key too** — the other half of
    /// [`the_generic_door_refuses_a_web_key_and_admits_a_skill_key`], extended
    /// to the family REQ-587 adds.
    ///
    /// [`PermissionGate::authorize`]'s guard is narrow on purpose: it fires for
    /// a web key and for nothing else. Widening it to reject the two skill
    /// families would turn a read of a remembered grant — or any future generic
    /// caller holding a key string — into a panic for no gain, since neither
    /// addressed question can be raised from a door with no addressee.
    #[tokio::test]
    async fn the_generic_door_admits_a_project_acknowledgment_key() {
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Allow));
        assert_eq!(
            gate.authorize(&project_skill_trust_key("~/dev/teton"), None)
                .await,
            PermissionDecision::Allowed
        );
    }

    /// **BR-4: the acknowledgment's key is unenumerated, so it rides the
    /// level's default — and no skill-name row is ever added.**
    ///
    /// The same claim `an_unknown_server_supplied_tool_is_classified_by_the_levels_default`
    /// makes about an MCP tool and a skill key, extended to the third family.
    /// A row for this key would be the beginning of the enumeration REQ-560
    /// ADR-A refuses, and it would also be *wrong*: the default already is
    /// BR-4's posture, exactly.
    #[test]
    fn the_acknowledgment_key_is_unenumerated_and_rides_the_levels_default() {
        let key = project_skill_trust_key("~/dev/teton");
        for (level, want) in [
            (PermissionLevel::Guarded, PermissionPolicy::Ask),
            (PermissionLevel::Edits, PermissionPolicy::Ask),
            (PermissionLevel::Plan, PermissionPolicy::Deny),
            (PermissionLevel::Full, PermissionPolicy::Allow),
        ] {
            assert_eq!(
                table_for(level).policy_for(&key),
                want,
                "`{key}` at {level} must come from the level's default"
            );
        }
        // And the level table names no skill and no root, at any level: the
        // acknowledgment's key and every skill key resolve to the default, which
        // is the whole of ADR-A's "the only enumerated set is READ_ONLY_TOOLS".
        for level in PermissionLevel::ALL {
            let table = table_for(*level);
            let default = table.policy_for("a-name-no-level-mentions");
            for unlisted in [
                key.as_str(),
                "project_skill_trust:~/dev/other",
                "skill:project:validate",
                "skill:user:validate",
            ] {
                assert_eq!(
                    table.policy_for(unlisted),
                    default,
                    "{level}: `{unlisted}` must not have a row of its own"
                );
            }
        }
    }

    /// **BR-11: `skill` is read-only at every level, so no level asks about the
    /// tool's own name.**
    ///
    /// The unit half — `a_bundled_docs_read_is_allowed_at_every_level_and_asks_nothing`
    /// is the behavioural one for `teton_docs`, and this makes the same claim
    /// for the tool REQ-587 adds without paying for four gate round-trips.
    /// Dropping `skill` from [`READ_ONLY_TOOLS`] fails here at `plan` first,
    /// which is the failure that matters: a knowledge tool denied at `plan` is
    /// indistinguishable from not shipping it (LESSON-524).
    #[test]
    fn the_skill_tool_never_asks_and_is_never_denied_at_any_level() {
        for level in PermissionLevel::ALL {
            assert_eq!(
                table_for(*level).policy_for("skill"),
                PermissionPolicy::Allow,
                "{level}: the `skill` tool must not ask and must not be denied —                  BR-11's constraint is that no level ever raises an \"allow                  `skill`?\" prompt"
            );
        }
    }

    /// **The row and the registry's name are one value (REQ-587 TASK-216).**
    ///
    /// [`READ_ONLY_TOOLS`] used to spell `skill` as a bare literal, because the
    /// tool did not exist when the row was written. The two are halves of one
    /// fact, and a literal is how they drift: rename the tool and the row stops
    /// matching it, at which point the level table's `default` takes over and
    /// `plan` **denies** the tool outright — silently, since an exposure test
    /// asserts the tool is in the list and being *callable* is a different
    /// claim. That is the `teton_docs` failure REQ-577 found live (LESSON-524),
    /// and this is the assertion that would have caught it.
    #[tokio::test]
    async fn the_permission_row_and_the_registrys_name_are_one_value() {
        use crate::harness::tools::{SkillTool, Tool};

        assert!(
            READ_ONLY_TOOLS.contains(&SKILL_TOOL_NAME),
            "the `skill` tool is not in the read-only set: {READ_ONLY_TOOLS:?}"
        );
        // Non-vacuity, and the drift itself: the row must be the name the tool
        // *answers to*, not a second spelling that happens to match today.
        let (_bus, _pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
        let tool = SkillTool::new(
            Arc::new(crate::skills::SkillRegistry::default()),
            Arc::new(gate),
            None,
            tokio::runtime::Handle::current(),
            1_000,
        );
        assert_eq!(
            tool.name(),
            SKILL_TOOL_NAME,
            "the constant the permission row reads and the name the tool registers \
             under have diverged"
        );
        assert_eq!(
            table_for(PermissionLevel::Plan).policy_for(tool.name()),
            PermissionPolicy::Allow,
            "`plan` denies the name the model actually calls"
        );
    }

    /// **BR-5 / OQ-9: the grant key follows the substituted commands, and only
    /// when a command interpolated them.**
    ///
    /// Four claims in one place because they are one decision: the
    /// non-interpolating spelling is byte-identical to REQ-585's (so a skill
    /// whose commands cannot change still keys per skill), the interpolating
    /// spelling differs per command set, it still reads as a skill key, and a
    /// **project** digest key still expires on a root move.
    #[test]
    fn a_digest_keyed_grant_follows_its_substituted_commands() {
        let plain = skill_grant_key(
            SkillSource::User,
            "deploy",
            &["./deploy.sh staging".to_owned()],
            ArgumentInterpolation::None,
        );
        assert_eq!(
            plain,
            skill_permission_key_for(SkillSource::User, "deploy"),
            "with no interpolation the key is REQ-585's, unchanged — a skill              whose commands cannot change is answered once for the session"
        );
        // And it does not depend on the commands at all, which is the same
        // claim said the other way.
        assert_eq!(
            plain,
            skill_grant_key(
                SkillSource::User,
                "deploy",
                &["something else entirely".to_owned()],
                ArgumentInterpolation::None,
            )
        );

        let staging = skill_grant_key(
            SkillSource::User,
            "deploy",
            &["./deploy.sh staging".to_owned()],
            ArgumentInterpolation::Substituted,
        );
        let prod = skill_grant_key(
            SkillSource::User,
            "deploy",
            &["./deploy.sh prod".to_owned()],
            ArgumentInterpolation::Substituted,
        );
        assert_ne!(
            staging, prod,
            "a grant answered for `staging` must not answer for `prod`"
        );
        assert_ne!(
            staging, plain,
            "an interpolating body's grant is not the plain per-skill grant"
        );
        // Deterministic: the same command set mints the same key, or the grant
        // would never be found again and the user would be asked every time.
        assert_eq!(
            staging,
            skill_grant_key(
                SkillSource::User,
                "deploy",
                &["./deploy.sh staging".to_owned()],
                ArgumentInterpolation::Substituted,
            )
        );

        // The command set is a *sequence*, and the encoding is unambiguous: a
        // separator-joined digest would collide these two.
        assert_ne!(
            skill_grant_key(
                SkillSource::User,
                "deploy",
                &["ab".to_owned(), "c".to_owned()],
                ArgumentInterpolation::Substituted,
            ),
            skill_grant_key(
                SkillSource::User,
                "deploy",
                &["a".to_owned(), "bc".to_owned()],
                ArgumentInterpolation::Substituted,
            ),
        );

        // It is still a skill key, so `authorize_skill`'s first guard and the
        // level's default both still apply to it.
        assert!(is_skill_permission_key(&staging), "`{staging}`");
        assert!(!is_project_acknowledgment_key(&staging), "`{staging}`");

        // And a **project** digest key still dies at `/cd`, which is the half a
        // suffix appended in the wrong place would silently break.
        let project = skill_grant_key(
            SkillSource::Project,
            "deploy",
            &["./deploy.sh staging".to_owned()],
            ArgumentInterpolation::Substituted,
        );
        assert!(
            expires_on_session_root_change(&project),
            "`{project}` names a repository's file and must not outlive the root"
        );
        assert!(
            !expires_on_session_root_change(&staging),
            "`{staging}` names `~/.claude`, which does not move"
        );
    }

    /// **The lockstep, asserted as a lockstep** (ADR-7): the door accepts
    /// exactly the two spellings [`skill_grant_key`] can mint for a triple, and
    /// nothing else.
    ///
    /// Dropping the digest from the minter without moving the assertion — or
    /// moving the assertion without the minter — makes one of these legs fail
    /// rather than making every debug build panic somewhere unrelated.
    #[test]
    fn the_skill_doors_guard_accepts_exactly_what_the_minter_produces() {
        let commands = vec!["./deploy.sh prod".to_owned()];
        for interpolation in [
            ArgumentInterpolation::None,
            ArgumentInterpolation::Substituted,
        ] {
            let key = skill_grant_key(SkillSource::User, "deploy", &commands, interpolation);
            assert!(
                is_grant_key_for(&key, SkillSource::User, "deploy", &commands),
                "`{key}` came from the minter and must satisfy the guard"
            );
        }
        // Everything the guard must still reject.
        let mismatched = vec!["./deploy.sh staging".to_owned()];
        for wrong in [
            "shell".to_owned(),
            skill_permission_key_for(SkillSource::Project, "deploy"),
            skill_permission_key_for(SkillSource::User, "canary"),
            project_skill_trust_key("~/dev/teton"),
            // The digest over a *different* command set: same skill, same
            // source, different question.
            skill_grant_key(
                SkillSource::User,
                "deploy",
                &mismatched,
                ArgumentInterpolation::Substituted,
            ),
        ] {
            assert!(
                !is_grant_key_for(&wrong, SkillSource::User, "deploy", &commands),
                "`{wrong}` must not pass the skill door's guard"
            );
        }
    }

    /// **BR-4: the prompt lists at most twenty names, and the tail is a
    /// count.**
    ///
    /// Bounded at the door that mints the subject, so it is true of every prompt
    /// rather than of every caller that remembered (LESSON-517).
    #[test]
    fn the_acknowledgment_lists_at_most_twenty_skills_and_counts_the_rest() {
        let entry = |n: usize| ProjectSkillTrustEntry {
            name: format!("skill-{n}"),
            shadows_user_skill: n == 0,
        };

        let (listed, more) = bound_listed_skills(&[]);
        assert!(listed.is_empty());
        assert_eq!(more, 0);

        let three: Vec<_> = (0..3).map(entry).collect();
        let (listed, more) = bound_listed_skills(&three);
        assert_eq!(listed, three, "a short list is passed through unchanged");
        assert_eq!(more, 0, "`+0 more` is not a thing the prompt should say");
        assert!(
            listed[0].shadows_user_skill,
            "shadowing rides as a bool the client renders, not as pre-marked prose"
        );

        let exactly = (0..MAX_LISTED_PROJECT_SKILLS)
            .map(entry)
            .collect::<Vec<_>>();
        let (listed, more) = bound_listed_skills(&exactly);
        assert_eq!(listed.len(), MAX_LISTED_PROJECT_SKILLS);
        assert_eq!(more, 0, "the boundary is inclusive");

        let many = (0..MAX_LISTED_PROJECT_SKILLS + 5)
            .map(entry)
            .collect::<Vec<_>>();
        let (listed, more) = bound_listed_skills(&many);
        assert_eq!(listed.len(), MAX_LISTED_PROJECT_SKILLS);
        assert_eq!(more, 5, "the tail is a count, not a truncation flag");
        assert_eq!(
            listed.last().map(|e| e.name.as_str()),
            Some("skill-19"),
            "the head is kept in order"
        );
    }

    /// **REQ-585 ADR-6 / REQ-587 ASSUME-017: `/cd` drops every grant a root move
    /// invalidates, and keeps every other.**
    ///
    /// The unit half — that the sweep is [`expires_on_session_root_change`] and
    /// nothing else. `skill_consent_matrix.rs` asserts the consequence: that a
    /// dropped grant makes the next invocation ask again.
    ///
    /// Two families, swept together, because a `/cd` invalidates both for the
    /// same reason. A project skill's dynamic-context grant names a repository's
    /// file; the **acknowledgment** names the repository itself, and one that
    /// outlived the root would let the model run a second repository's skills as
    /// instructions on an answer the user gave about a first — the harm BR-4
    /// exists to prevent, reached by the door BR-4 opened. The predicate is the
    /// shared one so the client's `SessionGrants` cannot come to disagree about
    /// which keys expire (ASSUME-017); it is a function above both crates, and a
    /// `starts_with("skill:project:")` here is what this test fails on.
    #[test]
    fn dropping_project_skill_grants_keeps_every_other_remembered_answer() {
        let (_bus, _pending, gate) = gate(PermissionConfig::coding_defaults());
        let project = skill_permission_key_for(SkillSource::Project, "deploy");
        let user = skill_permission_key_for(SkillSource::User, "deploy");
        let acknowledgment = project_skill_trust_key("~/dev/teton");
        gate.remember(&project, RememberedGrant::AllowAlways);
        // A refusal is a grant too, and it is about the same moved file.
        gate.remember(
            &skill_permission_key_for(SkillSource::Project, "canary"),
            RememberedGrant::RejectAlways,
        );
        gate.remember(&user, RememberedGrant::AllowAlways);
        gate.remember(&acknowledgment, RememberedGrant::AllowAlways);
        gate.remember("shell", RememberedGrant::AllowAlways);

        assert_eq!(gate.drop_project_skill_grants(), 3);

        assert_eq!(gate.remembered(&project), None);
        assert_eq!(
            gate.remembered(&skill_permission_key_for(SkillSource::Project, "canary")),
            None,
            "a project reject_always is about the moved file too"
        );
        assert_eq!(
            gate.remembered(&acknowledgment),
            None,
            "the acknowledgment names the root itself, so it cannot survive the \
             root moving"
        );
        assert_eq!(
            gate.remembered(&user),
            Some(RememberedGrant::AllowAlways),
            "`~/.claude` does not move when the session root does"
        );
        assert_eq!(
            gate.remembered("shell"),
            Some(RememberedGrant::AllowAlways),
            "the sweep is a skill-key sweep, not a grant reset"
        );
        // Idempotent: a second `/cd` with nothing left to drop drops nothing.
        assert_eq!(gate.drop_project_skill_grants(), 0);
    }

    /// REQ-560 ADR-C: a standing config consent relaxes an `ask` and never
    /// overrules a `deny`, so `[web] permission_allow` cannot punch through
    /// `plan`.
    #[test]
    fn a_config_web_consent_relaxes_an_ask_but_never_a_deny() {
        // Today's behaviour, unchanged: at `full` the web keys ask, and a listed
        // tier upgrades its own key to allow.
        let mut full = table_for(PermissionLevel::Full);
        full.apply_web_permission(&[WebTier::FetchUserUrl]);
        assert_eq!(
            full.policy_for(PERMISSION_KEY_FETCH_USER_URL),
            PermissionPolicy::Allow
        );
        // One member, one key — the neighbours are untouched.
        assert_eq!(
            full.policy_for(PERMISSION_KEY_FETCH_ANY_URL),
            PermissionPolicy::Ask
        );

        // The new rule: `plan` denies web by default, and config cannot lift it.
        let mut plan = table_for(PermissionLevel::Plan);
        plan.apply_web_permission(&[WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search]);
        for key in WEB_PERMISSION_KEYS {
            assert_eq!(
                plan.policy_for(key),
                PermissionPolicy::Deny,
                "`{key}` must stay denied at plan even with a standing consent"
            );
        }
    }

    fn gate(config: PermissionConfig) -> (Arc<EventBus>, Arc<PendingPermissions>, PermissionGate) {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            config,
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        (bus, pending, gate)
    }

    // ---- REQ-560: a session at a level -------------------------------------

    /// A leveled gate — the daemon's shape — plus the bus and pending registry.
    fn leveled_gate(
        level: PermissionLevel,
    ) -> (Arc<EventBus>, Arc<PendingPermissions>, PermissionGate) {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let gate = PermissionGate::with_level(
            SessionId::from("s1"),
            level,
            Vec::new(),
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        (bus, pending, gate)
    }

    /// Answer the next prompt the bus carries with `option_id`, returning the
    /// request id that was answered.
    ///
    /// **Bounded**, and the bound is load-bearing rather than defensive. Every
    /// caller here is asserting that a prompt *happens*; the failure mode of the
    /// regressions they guard against — a level that decides a call without
    /// asking — is that no prompt is ever published, and an unbounded `recv`
    /// turns that into a hung test instead of a red one. A hang reads as
    /// infrastructure trouble and gets retried; a failure gets read. (Found by
    /// mutating `full` into a gate-skip, which is exactly the BR-4 shape AC-14
    /// checks for: the suite went red, but only after hanging.)
    async fn answer_next(
        sub: &mut crate::broadcast::Subscription,
        pending: &PendingPermissions,
        option_id: &str,
    ) -> RequestId {
        let env = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("a prompt must be published — none arrived within the timeout")
            .expect("a prompt was published");
        let rid = match env.event {
            Event::PermissionRequest(pr) => pr.request_id,
            other => panic!("expected permission_request, got {other:?}"),
        };
        assert!(pending.resolve(
            &rid,
            PermissionOutcome::Selected {
                option_id: option_id.to_owned()
            }
        ));
        rid
    }

    /// REQ-560 AC-2, the three legs at the gate: `guarded` asks about an edit,
    /// `edits` runs it and still asks about a shell, `plan` denies both.
    #[tokio::test]
    async fn each_level_decides_edit_and_shell_as_documented() {
        // guarded: the edit asks.
        let (bus, pending, gate) = leveled_gate(PermissionLevel::Guarded);
        let mut sub = bus.subscribe(16);
        let (decision, _rid) = tokio::join!(
            gate.authorize("edit", None),
            answer_next(&mut sub, &pending, "allow_once")
        );
        assert_eq!(decision, PermissionDecision::Allowed);

        // edits: the edit runs unprompted; the shell still asks.
        let (bus, pending, gate) = leveled_gate(PermissionLevel::Edits);
        assert_eq!(
            gate.authorize("edit", None).await,
            PermissionDecision::Allowed
        );
        assert_eq!(
            bus.subscriber_count(),
            0,
            "an allowed edit published a prompt"
        );
        let mut sub = bus.subscribe(16);
        let (decision, _rid) = tokio::join!(
            gate.authorize("shell", None),
            answer_next(&mut sub, &pending, "allow_once")
        );
        assert_eq!(decision, PermissionDecision::Allowed);

        // plan: both are denied, and nothing was ever asked.
        let (_bus, pending, gate) = leveled_gate(PermissionLevel::Plan);
        for tool in ["edit", "shell"] {
            assert_eq!(
                gate.authorize(tool, None).await,
                PermissionDecision::Denied,
                "`{tool}` should be denied at plan"
            );
        }
        assert_eq!(
            pending.pending_count(),
            0,
            "plan denies without asking — a prompt was registered"
        );
    }

    /// **REQ-577 BR-6: a bundled-docs read is never a question, at any level.**
    ///
    /// The test class TASK-147's live A/B showed was missing. Everything about
    /// `teton_docs` was pinned except whether it could actually be *called*:
    /// `tools/mod.rs` asserts it is exposed on every profile and survives the
    /// `max_tools` cap (BR-7), and those tests were green while a real session
    /// printed `? permission requested: teton_docs` and, on a denial, `[failed]`.
    /// Exposure is the tool being offered; this is the tool being usable, and
    /// they are different claims with different failure modes.
    ///
    /// Asserted at **every** level rather than at `guarded` alone, because the
    /// two failures are not the same shape: `guarded` and `edits` interrupted a
    /// turn with a question that has no useful answer, and `plan` — the level a
    /// user picks *because* they want reading and nothing else — refused
    /// outright. Driven off `ALL`, so a fifth level is covered the day it lands.
    ///
    /// A maintainer who reaches this assertion after moving `teton_docs` off
    /// [`READ_ONLY_TOOLS`]: the requirement's Permissions row is what this pins
    /// ("call `teton_docs` … without a permission prompt"), so change the row
    /// and this test together, or leave both alone. Deleting the assertion
    /// restores a defect that CI could not see for a whole REQ.
    ///
    /// **The timeout is load-bearing**, for the reason [`answer_next`]'s comment
    /// gives one frame up and this test found the hard way: under the very
    /// regression it guards, the policy becomes `ask`, `authorize` publishes a
    /// prompt and waits for an answer that no one in this test will ever give,
    /// and an un-bounded await turns the regression into a **hang** rather than
    /// a failure. A hang reads as infrastructure trouble and gets retried; a
    /// failure gets read. (Verified by mutation: with `teton_docs` removed from
    /// [`READ_ONLY_TOOLS`] the first draft of this test hung until it was
    /// killed, which is how this paragraph got written.)
    #[tokio::test]
    async fn a_bundled_docs_read_is_allowed_at_every_level_and_asks_nothing() {
        for level in PermissionLevel::ALL {
            let (bus, pending, gate) = leveled_gate(*level);
            let mut sub = bus.subscribe(16);

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                gate.authorize(DOCS_TOOL_NAME, Some("teton_docs providers".to_owned())),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{level}: `{DOCS_TOOL_NAME}` blocked waiting for an answer, so its \
                     policy is `ask` — it is no longer in READ_ONLY_TOOLS, and a docs \
                     read now stops every turn to ask a question with no useful answer"
                )
            });
            assert_eq!(
                decision,
                PermissionDecision::Allowed,
                "`{DOCS_TOOL_NAME}` must run at {level} — it reads no path, no \
                 network and no user data, so there is nothing to consent to"
            );
            assert_eq!(
                pending.pending_count(),
                0,
                "{level}: `{DOCS_TOOL_NAME}` registered a prompt — a docs read \
                 must not stop a turn to ask"
            );
            assert!(
                sub.try_recv().is_none(),
                "{level}: `{DOCS_TOOL_NAME}` published an event; a call that \
                 asks nothing must be silent on the bus"
            );
        }
    }

    /// REQ-560 BR-5 / AC-3: a grant is an answer to a question the level decides
    /// whether to ask, so a tightened level outranks it — and loosening back
    /// restores it, because the grant was never discarded.
    #[tokio::test]
    async fn a_tightened_level_outranks_a_session_grant_and_loosening_restores_it() {
        let (bus, pending, gate) = leveled_gate(PermissionLevel::Guarded);
        let mut sub = bus.subscribe(16);

        // Allow-always `shell` at guarded.
        let (decision, _rid) = tokio::join!(
            gate.authorize("shell", None),
            answer_next(&mut sub, &pending, "allow_always")
        );
        assert_eq!(decision, PermissionDecision::Allowed);
        // The grant answers the next call with no second prompt.
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Allowed
        );
        assert_eq!(pending.pending_count(), 0);

        // Tighten: the grant does not survive a level that would not have asked.
        assert!(gate.set_level(PermissionLevel::Plan));
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Denied,
            "an allow_always grant outranked a tightened level"
        );

        // Loosen back: the grant applies again, and still without re-prompting.
        assert!(gate.set_level(PermissionLevel::Guarded));
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Allowed
        );
        assert_eq!(
            pending.pending_count(),
            0,
            "the restored grant re-prompted instead of being remembered"
        );
    }

    /// REQ-560 BR-7 / AC-15: a level change never resolves a prompt already in
    /// flight, in **either** direction.
    ///
    /// Asserted against `PendingPermissions` state rather than by timing: the
    /// claim is that the waiter is still registered and still awaiting a human,
    /// which is a fact about the registry, not about how long we waited.
    #[tokio::test]
    async fn a_level_change_leaves_an_in_flight_prompt_pending() {
        for (arriving, label) in [
            (PermissionLevel::Full, "loosening"),
            (PermissionLevel::Plan, "tightening"),
        ] {
            let (bus, pending, gate) = leveled_gate(PermissionLevel::Guarded);
            let gate = Arc::new(gate);
            let mut sub = bus.subscribe(16);

            let asking = {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move { gate.authorize("shell", None).await })
            };

            // Wait for the prompt to actually be in flight.
            let env = sub.recv().await.expect("a prompt was published");
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            assert_eq!(pending.pending_count(), 1);

            // The level changes under the open prompt.
            assert!(gate.set_level(arriving));
            tokio::task::yield_now().await;

            // Still pending, still awaiting the user: the level answered nothing.
            assert_eq!(
                pending.pending_count(),
                1,
                "{label} to {arriving} resolved an in-flight prompt"
            );
            assert!(!asking.is_finished(), "{label} decided the parked call");

            // The user's own answer decides the call they were asked about.
            assert!(pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned()
                }
            ));
            assert_eq!(
                asking.await.expect("the authorize task joins"),
                PermissionDecision::Allowed,
                "{label}: the user's answer did not decide the call"
            );

            // And the *next* call evaluates at the new level.
            let next = gate.authorize("shell", None);
            match arriving {
                PermissionLevel::Full => {
                    assert_eq!(next.await, PermissionDecision::Allowed);
                }
                PermissionLevel::Plan => {
                    assert_eq!(next.await, PermissionDecision::Denied);
                }
                other => panic!("unexpected level in this table: {other}"),
            }
        }
    }

    /// REQ-563's capability refusals are untouched by REQ-560's ordering flip,
    /// because the web keys sit at `ask` at every level — so a `reject_always`
    /// on a web key still reaches the grant, even at `full`.
    #[tokio::test]
    async fn a_web_capability_refusal_still_holds_at_full() {
        let (bus, pending, gate) = leveled_gate(PermissionLevel::Full);
        let mut sub = bus.subscribe(16);

        let (decision, _rid) = tokio::join!(
            gate.authorize_web(PERMISSION_KEY_SEARCH, None, WebTier::Search),
            answer_next(&mut sub, &pending, "reject_always")
        );
        assert_eq!(decision, PermissionDecision::Denied);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_SEARCH, None, WebTier::Search)
                .await,
            PermissionDecision::Denied,
            "a refused web capability was reopened by the full level"
        );
    }

    /// REQ-560 BR-15: only a level denial gets the level's sentence, and it is
    /// the level's own `denial_sentence` rather than a second string.
    #[test]
    fn the_denial_note_names_the_level_only_when_the_level_refused() {
        let (_bus, _pending, planned) = leveled_gate(PermissionLevel::Plan);
        let note = planned.denial_note("edit").expect("plan refuses edit");
        assert_eq!(note, PermissionLevel::Plan.denial_sentence("edit"));
        // A tool the level allows was not refused by the level.
        assert_eq!(planned.denial_note("read"), None);

        // At a level that asks, a denial came from the user, not the level.
        let (_bus, _pending, guarded) = leveled_gate(PermissionLevel::Guarded);
        assert_eq!(guarded.denial_note("shell"), None);

        // A gate pinned to an exact table has no level to blame.
        let (_bus, _pending, fixed) =
            self::gate(PermissionConfig::with_default(PermissionPolicy::Deny));
        assert_eq!(fixed.denial_note("shell"), None);
    }

    /// REQ-560 BR-6: `set_level` is idempotent and reports honestly, so a
    /// confirmation cannot claim a change that did not happen.
    #[test]
    fn setting_the_level_a_session_already_holds_is_not_a_change() {
        let (_bus, _pending, gate) = leveled_gate(PermissionLevel::Guarded);
        assert_eq!(gate.level(), Some(PermissionLevel::Guarded));
        assert!(!gate.set_level(PermissionLevel::Guarded));
        assert!(gate.set_level(PermissionLevel::Edits));
        assert_eq!(gate.level(), Some(PermissionLevel::Edits));
    }

    #[tokio::test]
    async fn allow_policy_needs_no_prompt() {
        let (bus, _pending, gate) = gate(PermissionConfig::permissive());
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Allowed
        );
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn deny_policy_needs_no_prompt() {
        let mut cfg = PermissionConfig::with_default(PermissionPolicy::Deny);
        cfg.set("shell", PermissionPolicy::Deny);
        let (_bus, _pending, gate) = gate(cfg);
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Denied
        );
    }

    #[tokio::test]
    async fn ask_then_reject_always_denies_and_persists_for_the_session() {
        let mut cfg = PermissionConfig::permissive();
        cfg.set("shell", PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", Some("run tests".to_owned()));
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            assert!(pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: "reject_always".to_owned()
                }
            ));
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Denied);

        // Second call: the reject_always grant answers with no new prompt.
        assert_eq!(
            gate.authorize("shell", None).await,
            PermissionDecision::Denied
        );
        assert_eq!(pending.pending_count(), 0);
    }

    #[tokio::test]
    async fn concurrent_sessions_get_distinct_ids_and_resolve_independently() {
        // BUG-161 regression. Two sessions' gates share one `PendingPermissions`
        // (the production wiring: `session_gates` all hold the same
        // `Arc<PendingPermissions>`). A per-session counter minted `perm-0` in
        // both, so the second `register` overwrote the first's waiter and one
        // session's answer resolved the other's tool call. With a daemon-wide
        // counter the two ids differ and each answer routes to its own session.
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let mut cfg = PermissionConfig::permissive();
        cfg.set("shell", PermissionPolicy::Ask);
        let gate_a = PermissionGate::new(
            SessionId::from("s1"),
            cfg.clone(),
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        let gate_b = PermissionGate::new(
            SessionId::from("s2"),
            cfg,
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        let mut sub = bus.subscribe(16);

        // Both sessions prompt at once; A is answered allow, B is answered reject.
        let decide_a = gate_a.authorize("shell", Some("A".to_owned()));
        let decide_b = gate_b.authorize("shell", Some("B".to_owned()));
        let drive = async {
            // Collect the two prompts and the id each session was assigned.
            let mut a_rid: Option<RequestId> = None;
            let mut b_rid: Option<RequestId> = None;
            while a_rid.is_none() || b_rid.is_none() {
                let env = sub.recv().await.unwrap();
                let session = env.session_id.clone();
                if let Event::PermissionRequest(pr) = env.event {
                    if session == Some(SessionId::from("s1")) {
                        a_rid = Some(pr.request_id);
                    } else if session == Some(SessionId::from("s2")) {
                        b_rid = Some(pr.request_id);
                    } else {
                        panic!("unexpected session {session:?}");
                    }
                }
            }
            let a_rid = a_rid.unwrap();
            let b_rid = b_rid.unwrap();
            // The heart of the fix: the two sessions did NOT collide on one id.
            assert_ne!(
                a_rid, b_rid,
                "concurrent sessions must not share a request id"
            );
            // Each answer resolves exactly its own session's waiter.
            assert!(pending.resolve(
                &a_rid,
                PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned()
                }
            ));
            assert!(pending.resolve(
                &b_rid,
                PermissionOutcome::Selected {
                    option_id: "reject_once".to_owned()
                }
            ));
        };
        let (decision_a, decision_b, ()) = tokio::join!(decide_a, decide_b, drive);
        // A said allow, B said reject — no cross-answer.
        assert_eq!(decision_a, PermissionDecision::Allowed);
        assert_eq!(decision_b, PermissionDecision::Denied);
        assert_eq!(pending.pending_count(), 0);
    }

    /// REQ-569 BR-9: a live request id resolves to the session that raised it,
    /// an unknown one to nothing, and an answered one to nothing again.
    ///
    /// The three arms are one test because the gate above this reads all three:
    /// `Some(owner)` is what it authorizes against, and both `None`s are the
    /// "no waiter" path that must stay the unchanged, always-acknowledged
    /// idempotent reply. Driven through two gates sharing one registry — the
    /// production wiring — so what is asserted is that each id carries *its own*
    /// session rather than the last one to register.
    #[tokio::test]
    async fn owner_of_names_the_session_that_raised_the_prompt() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
        let gate_a = PermissionGate::new(
            SessionId::from("s1"),
            cfg.clone(),
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        let gate_b = PermissionGate::new(
            SessionId::from("s2"),
            cfg,
            Arc::clone(&bus),
            Arc::clone(&pending),
        );
        let mut sub = bus.subscribe(16);

        let decide_a = gate_a.authorize("shell", None);
        let decide_b = gate_b.authorize("edit", None);
        let drive = async {
            let mut a_rid: Option<RequestId> = None;
            let mut b_rid: Option<RequestId> = None;
            while a_rid.is_none() || b_rid.is_none() {
                let env = sub.recv().await.unwrap();
                let session = env.session_id.clone();
                if let Event::PermissionRequest(pr) = env.event {
                    if session == Some(SessionId::from("s1")) {
                        a_rid = Some(pr.request_id);
                    } else {
                        b_rid = Some(pr.request_id);
                    }
                }
            }
            let a_rid = a_rid.unwrap();
            let b_rid = b_rid.unwrap();

            // Each id names its own session — not the other's, and not the last
            // registration to touch the map.
            assert_eq!(pending.owner_of(&a_rid), Some(SessionId::from("s1")));
            assert_eq!(pending.owner_of(&b_rid), Some(SessionId::from("s2")));

            // An id nobody registered belongs to nobody. This is the arm the
            // server turns into "no waiter, acknowledge anyway", so it must be
            // an absence rather than a guess.
            assert_eq!(
                pending.owner_of(&RequestId::from("perm-never-minted")),
                None
            );

            // Reading the owner does not consume the waiter: a refused answer
            // must leave the prompt standing for whoever may rightfully answer.
            assert_eq!(pending.pending_count(), 2);
            assert_eq!(pending.owner_of(&a_rid), Some(SessionId::from("s1")));

            assert!(pending.resolve(
                &a_rid,
                PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned()
                }
            ));
            assert!(pending.resolve(
                &b_rid,
                PermissionOutcome::Selected {
                    option_id: "reject_once".to_owned()
                }
            ));

            // Answered is indistinguishable from never-asked, which is what
            // makes a duplicate `permission/respond` harmless rather than a
            // second authorization decision.
            assert_eq!(pending.owner_of(&a_rid), None);
            assert_eq!(pending.owner_of(&b_rid), None);
        };
        let (decision_a, decision_b, ()) = tokio::join!(decide_a, decide_b, drive);
        assert_eq!(decision_a, PermissionDecision::Allowed);
        assert_eq!(decision_b, PermissionDecision::Denied);
    }

    /// The BUG-161 tripwire, read for what REQ-569 now also rests on it: a
    /// colliding registration cannot rewrite the **owner** of a live request id.
    ///
    /// If it could, a second session could claim an answer already promised to
    /// the first — the gate above would then check attachment against the wrong
    /// session and let the wrong connection answer. The refuse-not-overwrite arm
    /// is what makes the authorization subject of a pending prompt immutable.
    #[test]
    fn a_colliding_registration_cannot_steal_the_owner_of_a_live_request() {
        let pending = PendingPermissions::new();
        let id = RequestId::from("perm-0");
        let mut first = pending.register(id.clone(), SessionId::from("s1"), None);
        let mut second = pending.register(id.clone(), SessionId::from("s2"), None);

        assert_eq!(
            pending.owner_of(&id),
            Some(SessionId::from("s1")),
            "the second registration must not repoint the id at its own session"
        );
        // The loser's sender was dropped by `register`, so its caller's
        // `authorize` takes the safe `Denied` arm rather than sharing the
        // winner's answer.
        assert!(
            second.try_recv().is_err(),
            "the colliding registration must not have been given a live channel"
        );

        assert!(pending.resolve(
            &id,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        ));
        assert_eq!(
            first.try_recv().ok(),
            Some(PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }),
            "the first waiter keeps its own prompt's answer"
        );
    }

    #[tokio::test]
    async fn ask_then_allow_always_allows_and_persists() {
        let cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("edit", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: "allow_always".to_owned(),
                },
            );
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Allowed);

        // Persisted: allowed again with no prompt.
        assert_eq!(
            gate.authorize("edit", None).await,
            PermissionDecision::Allowed
        );
    }

    #[tokio::test]
    async fn cancelled_prompt_denies() {
        let mut cfg = PermissionConfig::with_default(PermissionPolicy::Ask);
        cfg.set("shell", PermissionPolicy::Ask);
        let (bus, pending, gate) = gate(cfg);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                _ => unreachable!(),
            };
            pending.resolve(&rid, PermissionOutcome::Cancelled);
        };
        let (decision, ()) = tokio::join!(decide, drive);
        assert_eq!(decision, PermissionDecision::Denied);
    }

    // ------------------------------------------------------------------
    // REQ-563: the fifth option, and the consent event (BR-4, AC-2)
    // ------------------------------------------------------------------

    /// A [`WebTierPersistence`] that records what it was asked to write and can
    /// be told to fail, so both halves of the "permanent" claim are testable
    /// without a config file.
    #[derive(Default)]
    struct RecordingSink {
        written: Mutex<Vec<WebTier>>,
        fails: bool,
    }

    impl RecordingSink {
        fn failing() -> Self {
            Self {
                written: Mutex::new(Vec::new()),
                fails: true,
            }
        }

        fn written(&self) -> Vec<WebTier> {
            self.written.lock().expect("sink mutex").clone()
        }
    }

    impl WebTierPersistence for RecordingSink {
        fn persist_web_tier(&self, tier: WebTier) -> Result<(), String> {
            self.written.lock().expect("sink mutex").push(tier);
            if self.fails {
                return Err("the disk said no".to_owned());
            }
            Ok(())
        }
    }

    /// Drive one Ask prompt to `option_id`, returning the decision, the options
    /// the prompt carried, and every event the bus saw.
    async fn answer_web(
        gate: &PermissionGate,
        bus: &Arc<EventBus>,
        pending: &Arc<PendingPermissions>,
        key: &str,
        tier: WebTier,
        option_id: &str,
    ) -> (PermissionDecision, Vec<PermissionOption>, Vec<Event>) {
        let mut sub = bus.subscribe(16);
        let decide = gate.authorize_web(key, Some("fetch https://docs.rs".to_owned()), tier);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let (rid, options) = match env.event {
                Event::PermissionRequest(pr) => (pr.request_id, pr.options),
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: option_id.to_owned(),
                },
            );
            options
        };
        let (decision, options) = tokio::join!(decide, drive);
        let mut rest = Vec::new();
        while let Some(env) = sub.try_recv() {
            rest.push(env.event);
        }
        (decision, options, rest)
    }

    /// AC-2's option list, and its negative half: the persistent choice belongs
    /// to the web tiers alone. `shell` and `edit` keep exactly four, because a
    /// consent answer that quietly edited config would be a far larger promise
    /// than "allow for this session".
    #[tokio::test]
    async fn only_the_web_keys_are_offered_the_persistent_option() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));

        for key in WEB_PERMISSION_KEYS {
            let (_, options, _) = answer_web(
                &gate,
                &bus,
                &pending,
                key,
                WebTier::FetchAnyUrl,
                OPTION_REJECT_ONCE,
            )
            .await;
            let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    OPTION_ALLOW_ONCE,
                    OPTION_ALLOW_ALWAYS,
                    OPTION_ID_ENABLE_PERMANENT,
                    OPTION_REJECT_ONCE,
                    OPTION_REJECT_ALWAYS,
                ],
                "`{key}` must offer the five web options, in prompt order"
            );
            assert!(
                options
                    .iter()
                    .any(|o| o.option_id == OPTION_ID_ENABLE_PERMANENT
                        && o.label.contains("fetch_any_url")),
                "the label must name the tier it would write: {options:?}"
            );
        }

        // The negative half, driven through the same publisher so this is the
        // prompt a client actually receives rather than a helper's return value.
        for tool in ["shell", "edit", "read"] {
            let mut sub = bus.subscribe(16);
            let decide = gate.authorize(tool, None);
            let drive = async {
                let env = sub.recv().await.unwrap();
                let (rid, options) = match env.event {
                    Event::PermissionRequest(pr) => (pr.request_id, pr.options),
                    other => panic!("expected permission_request, got {other:?}"),
                };
                pending.resolve(&rid, PermissionOutcome::Cancelled);
                options
            };
            let (_, options) = tokio::join!(decide, drive);
            let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    OPTION_ALLOW_ONCE,
                    OPTION_ALLOW_ALWAYS,
                    OPTION_REJECT_ONCE,
                    OPTION_REJECT_ALWAYS,
                ],
                "`{tool}` is not a web tier and must keep exactly four options"
            );
        }
    }

    /// `enable_permanent` writes the tier it was offered for — the *lookup's*
    /// tier, taken from the argument rather than re-derived from the permission
    /// key, so a fetch of a URL the user pasted can never enable model-chosen
    /// ones (BR-3).
    #[tokio::test]
    async fn enable_permanent_writes_the_tier_the_prompt_named() {
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            let bus = Arc::new(EventBus::new());
            let pending = Arc::new(PendingPermissions::new());
            let sink = Arc::new(RecordingSink::default());
            let gate = PermissionGate::new(
                SessionId::from("s1"),
                PermissionConfig::with_default(PermissionPolicy::Ask),
                Arc::clone(&bus),
                Arc::clone(&pending),
            )
            .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);

            // The key is a function of the tier, through the one mapping —
            // never a hand-written `if`, which is what let the two fetch tiers
            // share a grant.
            let key = permission_key_for(tier).expect("every tier above off has a key");
            let (decision, _, events) =
                answer_web(&gate, &bus, &pending, key, tier, OPTION_ID_ENABLE_PERMANENT).await;

            assert_eq!(decision, PermissionDecision::Allowed);
            assert_eq!(sink.written(), vec![tier], "wrote the wrong tier");
            assert_eq!(
                events,
                vec![Event::WebConsentDecided(WebConsentDecided {
                    scope: WebConsentScope::Persistent,
                    tier: to_protocol_web_tier(tier),
                    granted: true,
                })],
                "a persisted grant is recorded at persistent scope"
            );

            // And it is a session grant thereafter: the next lookup at the same
            // key asks nobody and writes nothing more.
            assert_eq!(
                gate.authorize_web(key, None, tier).await,
                PermissionDecision::Allowed
            );
            assert_eq!(
                sink.written(),
                vec![tier],
                "the remembered grant must not re-write config on every lookup"
            );
        }
    }

    /// A write that does not land is reported as a **session** grant, not a
    /// persistent one. The user still said yes — so the lookup proceeds — but
    /// the event must not claim a durability that does not exist.
    #[tokio::test]
    async fn a_failed_write_is_recorded_as_a_session_grant_not_a_permanent_one() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let sink = Arc::new(RecordingSink::failing());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            PermissionConfig::with_default(PermissionPolicy::Ask),
            Arc::clone(&bus),
            Arc::clone(&pending),
        )
        .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);

        let (decision, _, events) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_SEARCH,
            WebTier::Search,
            OPTION_ID_ENABLE_PERMANENT,
        )
        .await;

        assert_eq!(
            decision,
            PermissionDecision::Allowed,
            "a filesystem failure must not overturn the user's answer"
        );
        assert_eq!(
            events,
            vec![Event::WebConsentDecided(WebConsentDecided {
                scope: WebConsentScope::Session,
                tier: to_protocol_web_tier(WebTier::Search),
                granted: true,
            })]
        );
    }

    /// A gate with no persistence seam still allows, and still tells the truth
    /// about the scope it achieved.
    #[tokio::test]
    async fn enable_permanent_without_a_sink_degrades_to_a_session_grant() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
        let (decision, _, events) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_FETCH_ANY_URL,
            WebTier::FetchAnyUrl,
            OPTION_ID_ENABLE_PERMANENT,
        )
        .await;
        assert_eq!(decision, PermissionDecision::Allowed);
        assert_eq!(
            events,
            vec![Event::WebConsentDecided(WebConsentDecided {
                scope: WebConsentScope::Session,
                tier: to_protocol_web_tier(WebTier::FetchAnyUrl),
                granted: true,
            })]
        );
    }

    /// Every web answer is recorded, refusals included, at the scope it refuses
    /// — "declined this once" and "declined for the session" decide whether the
    /// user is asked again.
    #[tokio::test]
    async fn every_web_answer_is_recorded_at_the_scope_it_holds() {
        let cases = [
            (OPTION_ALLOW_ONCE, WebConsentScope::Once, true),
            (OPTION_ALLOW_ALWAYS, WebConsentScope::Session, true),
            (OPTION_REJECT_ONCE, WebConsentScope::Once, false),
            (OPTION_REJECT_ALWAYS, WebConsentScope::Session, false),
        ];
        for (option_id, scope, granted) in cases {
            let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
            let (decision, _, events) = answer_web(
                &gate,
                &bus,
                &pending,
                PERMISSION_KEY_FETCH_USER_URL,
                WebTier::FetchUserUrl,
                option_id,
            )
            .await;
            assert_eq!(
                decision == PermissionDecision::Allowed,
                granted,
                "{option_id} decided the wrong way"
            );
            assert_eq!(
                events,
                vec![Event::WebConsentDecided(WebConsentDecided {
                    scope,
                    tier: to_protocol_web_tier(WebTier::FetchUserUrl),
                    granted,
                })],
                "{option_id} was recorded wrongly"
            );
        }
    }

    /// An option id that was never on the prompt is not an answer to it. In
    /// particular `enable_permanent` sent for a **non-web** tool denies rather
    /// than granting — the fail-closed default that keeps a client (or a
    /// replayed message) from reaching an option the daemon did not offer.
    #[tokio::test]
    async fn an_unoffered_option_id_denies() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let sink = Arc::new(RecordingSink::default());
        let gate = PermissionGate::new(
            SessionId::from("s1"),
            PermissionConfig::with_default(PermissionPolicy::Ask),
            Arc::clone(&bus),
            Arc::clone(&pending),
        )
        .with_web_persistence(Arc::clone(&sink) as Arc<dyn WebTierPersistence>);
        let mut sub = bus.subscribe(16);

        let decide = gate.authorize("shell", None);
        let drive = async {
            let env = sub.recv().await.unwrap();
            let rid = match env.event {
                Event::PermissionRequest(pr) => pr.request_id,
                other => panic!("expected permission_request, got {other:?}"),
            };
            pending.resolve(
                &rid,
                PermissionOutcome::Selected {
                    option_id: OPTION_ID_ENABLE_PERMANENT.to_owned(),
                },
            );
        };
        let (decision, ()) = tokio::join!(decide, drive);

        assert_eq!(decision, PermissionDecision::Denied);
        assert!(
            sink.written().is_empty(),
            "a non-web prompt must never reach the config writer"
        );
        assert!(
            sub.try_recv().is_none(),
            "a non-web decision publishes no web consent event"
        );
    }

    /// A remembered grant replays without re-announcing itself: one decision is
    /// one event, however many lookups it later answers.
    #[tokio::test]
    async fn a_replayed_session_grant_publishes_no_second_decision() {
        let (bus, pending, gate) = gate(PermissionConfig::with_default(PermissionPolicy::Ask));
        let (_, _, first) = answer_web(
            &gate,
            &bus,
            &pending,
            PERMISSION_KEY_FETCH_ANY_URL,
            WebTier::FetchAnyUrl,
            OPTION_ALLOW_ALWAYS,
        )
        .await;
        assert_eq!(first.len(), 1);

        let mut sub = bus.subscribe(16);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_FETCH_ANY_URL, None, WebTier::FetchAnyUrl)
                .await,
            PermissionDecision::Allowed
        );
        assert!(
            sub.try_recv().is_none(),
            "the replayed grant re-announced a decision nobody made"
        );
    }

    /// A policy row answers without a prompt and without a decision event: an
    /// `allow`/`deny` row is configuration, and nobody decided anything now.
    #[tokio::test]
    async fn a_policy_answer_publishes_no_consent_event() {
        let mut config = PermissionConfig::permissive();
        // `permissive()` deliberately leaves the web keys asking, so the row
        // this test is about has to be set explicitly.
        config.apply_web_permission(&[WebTier::FetchAnyUrl]);
        let (bus, _pending, gate) = gate(config);
        let mut sub = bus.subscribe(16);
        assert_eq!(
            gate.authorize_web(PERMISSION_KEY_FETCH_ANY_URL, None, WebTier::FetchAnyUrl)
                .await,
            PermissionDecision::Allowed
        );
        assert!(sub.try_recv().is_none());
    }

    /// **`permissive()` is about the local, jailed tool set — which web is not.**
    ///
    /// The constructor's own doc is the justification for it, and a web lookup
    /// satisfies neither half. Pre-approving the web keys there made "allow
    /// every tool" quietly mean "and talk to the internet without asking", on
    /// the one path that exists precisely because nothing should leave the
    /// machine.
    #[test]
    fn permissive_allows_the_local_tools_and_still_asks_about_the_web() {
        let config = PermissionConfig::permissive();
        for tool in ["shell", "edit", "read", "grep", "glob", "anything-else"] {
            assert_eq!(config.policy_for(tool), PermissionPolicy::Allow, "{tool}");
        }
        for key in WEB_PERMISSION_KEYS {
            assert_eq!(
                config.policy_for(key),
                PermissionPolicy::Ask,
                "`{key}` was pre-approved by a constructor that means \"local and jailed\""
            );
        }
    }

    /// **`[web] permission_allow` maps one member onto one key** (REQ-563 BR-3).
    ///
    /// The narrowness is the requirement, not an implementation detail: a member
    /// sets *its* key, leaves the other two web keys asking, and touches no
    /// non-web tool at all. The predecessor of this mapping was a two-valued
    /// `[web] permission` that fanned onto all three keys, which made one durable
    /// answer about a pasted URL a durable answer about model-composed URLs and
    /// searches as well.
    ///
    /// **Base changed from `deny` to `ask` by REQ-560 (ADR-C).** The `deny` base
    /// was a synthetic sentinel for "this row was not written", never a claim
    /// that config may lift a denial — and since REQ-560 a standing consent
    /// relaxes an `ask` and leaves a `deny` alone, so the sentinel had to become
    /// a policy the mapping actually acts on. `ask` is also what every real
    /// composition holds for these keys, so the test now starts from the
    /// production precondition rather than from one that only existed here.
    /// Every assertion about the *narrowness* — one member, one key, no non-web
    /// tool touched — is unchanged, which is what this test is for. The new
    /// deny-is-not-config's-to-lift rule is pinned separately by
    /// [`a_config_web_consent_relaxes_an_ask_but_never_a_deny`].
    #[test]
    fn the_web_permission_config_maps_each_member_onto_exactly_its_own_key() {
        for tier in [WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search] {
            let listed = permission_key_for(tier).expect("every tier above off has a key");
            let mut config = PermissionConfig::with_default(PermissionPolicy::Ask);
            config.apply_web_permission(&[tier]);
            for key in WEB_PERMISSION_KEYS {
                let expected = if key == listed {
                    PermissionPolicy::Allow
                } else {
                    // Untouched — which for an `Ask` default means `Ask`, and is
                    // the point: this mapping writes one row, not three.
                    PermissionPolicy::Ask
                };
                assert_eq!(config.policy_for(key), expected, "{tier:?} -> {key}");
            }
            assert_eq!(
                config.policy_for("shell"),
                PermissionPolicy::Ask,
                "a web config value reached a non-web tool"
            );
        }

        // An empty list — the shipped default — changes nothing at all.
        let mut untouched = PermissionConfig::permissive();
        untouched.apply_web_permission(&[]);
        for key in WEB_PERMISSION_KEYS {
            assert_eq!(untouched.policy_for(key), PermissionPolicy::Ask, "{key}");
        }

        // Every member is honoured when several are listed.
        let mut all = PermissionConfig::with_default(PermissionPolicy::Ask);
        all.apply_web_permission(&[WebTier::FetchUserUrl, WebTier::FetchAnyUrl, WebTier::Search]);
        for key in WEB_PERMISSION_KEYS {
            assert_eq!(all.policy_for(key), PermissionPolicy::Allow, "{key}");
        }

        // `Off` has no key, so it cannot borrow a neighbour's. Config validation
        // refuses it as a member; this is the second line of that defence.
        let mut off = PermissionConfig::permissive();
        off.apply_web_permission(&[WebTier::Off]);
        for key in WEB_PERMISSION_KEYS {
            assert_eq!(
                off.policy_for(key),
                PermissionPolicy::Ask,
                "`off` granted `{key}`"
            );
        }
    }
}
