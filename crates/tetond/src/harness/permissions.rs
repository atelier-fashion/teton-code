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

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use teton_core::config::WebTier;
use teton_protocol::permissions::PermissionLevel;
use teton_protocol::events::{
    Event, PermissionOption, PermissionOptionKind, PermissionRequest, WebConsentDecided,
    WebConsentScope, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::PermissionOutcome;
use teton_protocol::{RequestId, SessionId};

use crate::broadcast::EventBus;
use crate::egress::to_protocol_web_tier;
use crate::harness::tools::web::{permission_key_for, tier_name, WEB_PERMISSION_KEYS};

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
const READ_ONLY_TOOLS: &[&str] = &["read", "glob", "grep"];

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
/// every level, rather than being silently unclassified. A new *read-only*
/// first-party tool that nobody adds to [`READ_ONLY_TOOLS`] merely asks — a
/// degradation, not a hole.
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
    fn register(&self, id: RequestId, owner: SessionId) -> oneshot::Receiver<PermissionOutcome> {
        let (tx, rx) = oneshot::channel();
        let mut waiters = self
            .waiters
            .lock()
            .expect("pending permissions mutex poisoned");
        match waiters.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(Waiter { owner, tx });
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
    pub fn resolve(&self, id: &RequestId, outcome: PermissionOutcome) -> bool {
        let waiter = self
            .waiters
            .lock()
            .expect("pending permissions mutex poisoned")
            .remove(id);
        match waiter {
            Some(waiter) => waiter.tx.send(outcome).is_ok(),
            None => false,
        }
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

/// The session-scoped permission authority.
///
/// Publishes prompts to the event bus, awaits answers via [`PendingPermissions`],
/// and remembers `*_always` answers for the life of the session.
pub struct PermissionGate {
    session_id: SessionId,
    config: PermissionConfig,
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
}

impl PermissionGate {
    /// A gate for `session_id` using `config`, publishing to `events` and
    /// awaiting answers on `pending`.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        config: PermissionConfig,
        events: Arc<EventBus>,
        pending: Arc<PendingPermissions>,
    ) -> Self {
        Self {
            session_id,
            config,
            grants: Mutex::new(HashMap::new()),
            events,
            pending,
            web_persistence: None,
        }
    }

    /// Wire the sink `enable_permanent` writes through (REQ-563 BR-4).
    #[must_use]
    pub fn with_web_persistence(mut self, sink: Arc<dyn WebTierPersistence>) -> Self {
        self.web_persistence = Some(sink);
        self
    }

    /// Decide whether `tool_name` may run, prompting the client if the policy is
    /// `ask` and no session grant already answers.
    ///
    /// A cancelled prompt, a `reject_*`, or a dropped client (channel closed) all
    /// resolve to [`PermissionDecision::Denied`] — the safe default.
    ///
    /// Web lookups do not come through here: they carry a tier, and
    /// [`Self::authorize_web`] is the entry point that takes one.
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
    async fn decide(
        &self,
        tool_name: &str,
        description: Option<String>,
        web: Option<WebTier>,
    ) -> PermissionDecision {
        // A remembered session grant short-circuits everything (asked once).
        //
        // No consent event here, deliberately: the decision this replays was
        // published when it was *made*, and re-announcing it per lookup would
        // turn one decision into a stream of them.
        if let Some(grant) = self.session_grant(tool_name) {
            return match grant {
                RememberedGrant::AllowAlways => PermissionDecision::Allowed,
                RememberedGrant::RejectAlways => PermissionDecision::Denied,
            };
        }

        // Likewise nothing is published for a policy answer: `allow` and `deny`
        // rows are configuration, and no one decided anything just now.
        match self.config.policy_for(tool_name) {
            PermissionPolicy::Allow => return PermissionDecision::Allowed,
            PermissionPolicy::Deny => return PermissionDecision::Denied,
            PermissionPolicy::Ask => {}
        }

        // Register the waiter, publish the prompt, then await — no lock is held
        // across the await.
        let request_id = self.pending.next_request_id();
        // The owning session travels with the waiter, so the answer that comes
        // back can be authorized against it (REQ-569 BR-9): this gate is the
        // only place that knows whose tool call is about to block.
        let rx = self
            .pending
            .register(request_id.clone(), self.session_id.clone());

        self.events.publish(
            Some(self.session_id.clone()),
            Event::PermissionRequest(PermissionRequest {
                request_id,
                tool_name: tool_name.to_owned(),
                description,
                options: options_for(web),
            }),
        );

        match rx.await {
            Ok(outcome) => self.interpret(tool_name, outcome, web),
            // Client disconnected before answering: deny (never run unapproved).
            // Not a consent decision — nobody decided it — so nothing is
            // published; a `web_consent_decided { granted: false }` here would
            // record a refusal the user never gave.
            Err(_) => PermissionDecision::Denied,
        }
    }

    /// Interpret a client's chosen option, recording any `*_always` grant and —
    /// for a web decision — publishing it at the scope it achieved.
    fn interpret(
        &self,
        tool_name: &str,
        outcome: PermissionOutcome,
        web: Option<WebTier>,
    ) -> PermissionDecision {
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
        decision
    }

    /// Write `tier` through the persistence seam, answering with the scope the
    /// attempt actually achieved.
    ///
    /// A failure is reported to the operator's stderr and downgraded to
    /// [`WebConsentScope::Session`] rather than turned into a denial: the user
    /// said yes, and the only thing that did not happen is the part that would
    /// have outlived the session. The line names the reason so "why am I being
    /// asked again next time" has an answer on this machine.
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

/// The options offered on a prompt: the four standard ones, plus the persistent
/// enable when `web` names the tier a decision could be written down at.
///
/// Keyed on the tier rather than on the tool name, so the option and the value
/// it would persist come from **one** source: an `enable_permanent` that could
/// be offered without a tier in hand is an option the daemon could not honour.
/// That is also why the fifth option is web-only rather than universal — there
/// is no `[shell] tier` to write, and an "always" that quietly edited config
/// would be a much larger promise than the one the prompt makes.
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
    fn expected_rows(level: PermissionLevel) -> (PermissionPolicy, Vec<(&'static str, PermissionPolicy)>) {
        use PermissionPolicy::{Allow, Ask, Deny};
        match level {
            PermissionLevel::Guarded => (
                Ask,
                vec![
                    ("read", Allow),
                    ("glob", Allow),
                    ("grep", Allow),
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
    #[test]
    fn an_unknown_server_supplied_tool_is_classified_by_the_levels_default() {
        let unknown = "mcp__some_server__some_tool_nobody_declared";
        assert_eq!(
            table_for(PermissionLevel::Guarded).policy_for(unknown),
            PermissionPolicy::Ask
        );
        assert_eq!(
            table_for(PermissionLevel::Edits).policy_for(unknown),
            PermissionPolicy::Ask
        );
        // Fail-closed at the level whose promise is that nothing changes.
        assert_eq!(
            table_for(PermissionLevel::Plan).policy_for(unknown),
            PermissionPolicy::Deny
        );
        assert_eq!(
            table_for(PermissionLevel::Full).policy_for(unknown),
            PermissionPolicy::Allow
        );
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
        let mut first = pending.register(id.clone(), SessionId::from("s1"));
        let mut second = pending.register(id.clone(), SessionId::from("s2"));

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
