//! Streaming session rendering and the permission round-trip.
//!
//! This is the client's hot path: it turns the daemon's event stream into what
//! the user sees. [`render_event`] is a pure function of one [`EventEnvelope`]
//! plus the session's running [`SessionState`] — assistant text streams as
//! fragments, tool calls and diffs render as lines, and every control event
//! (`route_decided`, `privacy_block`, `provider_degraded`, `phase_transition`,
//! `model_lifecycle`) becomes a one-line notice (the BR-5 legibility promise).
//! Routing notices are diagnostic chrome, not warnings, so they render only when
//! [`SessionState::verbose`] is set; privacy and degradation notices always
//! render. That flag has two mutators and one meaning: the `--verbose` flag
//! initialises it at startup, and the in-session `/verbose` command toggles it
//! live (REQ-555 BR-5, D-5). The turn-end line reads the same flag, so both
//! surfaces move together rather than one following the flag and the other the
//! command.
//!
//! Permission requests are handled separately by [`resolve_permission`], which
//! needs an input source: it renders the prompt, reads a decision, and returns
//! the [`PermissionRespondParams`] the caller sends back. "Allow/deny for this
//! session" grants are remembered in [`SessionGrants`] and auto-applied to later
//! requests for the same tool — session-scoped, never persisted.
//!
//! Everything here is driven in tests by scripted event streams and scripted
//! prompts, with no socket and no daemon.

use std::collections::{HashMap, HashSet};

use teton_protocol::events;
use teton_protocol::events::{
    AttachConsentRequested, BlockCause, BudgetBound, CapabilityDeadEnd, ConsentScope,
    ContextPressure, ContextPressureKind, DaemonClientAttach, DaemonLifetimeStage, DynamicOutcome,
    DynamicOutcomeView, Event, EventEnvelope, EvictionReason, FailureClass, ModelLifecycle,
    ModelSelectionProposed, NotRunReason, PermissionOption, PermissionOptionKind,
    PermissionRequest, PermissionSubject, PhaseTransition, PrefixCache, PrefixCacheMiss,
    PrefixCacheOutcome, PrivacyAction, PrivacyBlock, ProvenanceRejected, ProvenanceRejection,
    ProviderDegraded, ProviderSetupCompleted, ProviderSetupRejected, ProviderTested, RouteDecided,
    SessionGrantMinted, SessionUpdatePayload, SkillInvoked, TierWarming, ToolCallStatus,
    TurnQueued, WebCapabilityState, WebConsentDecided, WebConsentScope, WebLookup, WebLookupKind,
    WebLookupOutcome, WebSetupCompleted, WebSetupRejected, WebTier, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{
    AttachConsentOutcome, AttachConsentParams, PermissionOutcome, PermissionRespondParams,
    RefusalReason, SessionRoot, SkillSource,
};
use teton_protocol::{Phase, RequestId, SessionId};

use crate::banner;
use crate::cost_ui::CostMeter;
use crate::firstrun;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};
use crate::slash;

/// Session-scoped permission memory (never written to disk).
#[derive(Debug, Default)]
pub struct SessionGrants {
    allow_always: HashSet<String>,
    reject_always: HashSet<String>,
}

impl SessionGrants {
    /// True when `tool` was allowed for the whole session.
    #[must_use]
    pub fn is_allow_always(&self, tool: &str) -> bool {
        self.allow_always.contains(tool)
    }

    /// True when `tool` was denied for the whole session.
    #[must_use]
    pub fn is_reject_always(&self, tool: &str) -> bool {
        self.reject_always.contains(tool)
    }

    /// Remember an allow-for-session grant.
    pub fn allow_always(&mut self, tool: &str) {
        self.allow_always.insert(tool.to_owned());
    }

    /// Forget every grant a **session root move** invalidates (ADR-6,
    /// REQ-587 ASSUME-017).
    ///
    /// The other half of ADR-6, and the half a daemon-side test cannot see.
    /// `PermissionGate::drop_project_skill_grants` (a REQ-585 name its own doc
    /// records as narrower than what it now sweeps) forgets the daemon's copy on
    /// `/cd`; this store holds the same keys, is consulted *before* any prompt is
    /// drawn, and answers `allow_always` on its own. Without this, the daemon
    /// re-asks after a root move and the client auto-answers from a grant the
    /// user gave in a different repo — one `auto-allow` line, no commands
    /// shown, and the daemon then re-remembers it under the new root. That is
    /// exactly the harm ADR-6 exists to prevent, one hop across the seam.
    ///
    /// **Two families expire, and the predicate is shared rather than spelled.**
    /// [`teton_protocol::methods::expires_on_session_root_change`] is the one
    /// invalidation rule, above both crates: a project skill's dynamic-context
    /// grant *and* REQ-587 BR-4's project-skill acknowledgment. A copy here that
    /// knew only the first would keep the acknowledgment across a `/cd` while
    /// the daemon dropped it — and the acknowledgment is the one grant whose
    /// auto-answer costs a whole repository's skills reaching the model as
    /// instructions with nobody shown anything. That is REQ-585's finding 2 on
    /// the key ASSUME-017 was written for, which is why the rule lives in
    /// `teton_protocol` and not in either store.
    ///
    /// User skills are kept: `~/.claude/skills/<name>` is the same file whatever
    /// the root is.
    pub fn forget_root_scoped_grants(&mut self) -> usize {
        let before = self.allow_always.len() + self.reject_always.len();
        self.allow_always
            .retain(|key| !teton_protocol::methods::expires_on_session_root_change(key));
        self.reject_always
            .retain(|key| !teton_protocol::methods::expires_on_session_root_change(key));
        before - (self.allow_always.len() + self.reject_always.len())
    }

    /// Remember a deny-for-session grant.
    pub fn reject_always(&mut self, tool: &str) {
        self.reject_always.insert(tool.to_owned());
    }
}

/// Mutable state a rendered session carries across events.
#[derive(Debug, Default)]
pub struct SessionState {
    /// Tool-call id → human title, so a later `tool_call_update` can be named.
    tool_titles: HashMap<String, String>,
    /// Session-scoped permission grants.
    pub grants: SessionGrants,
    /// Cost accumulated from `cost_recorded` events.
    pub cost: CostMeter,
    /// Model proposals this client has already taken up (REQ-547).
    model_seen: HashSet<RequestId>,
    /// Show routing notices (`route [...]`) and the turn-end line. Off by
    /// default so the transcript is just the conversation. Two mutators, one
    /// flag: `--verbose` initialises it when the session starts, and the
    /// in-session `/verbose` command toggles it mid-session (REQ-555 BR-5).
    /// Session-scoped either way — nothing here is persisted.
    pub verbose: bool,
    /// The local tier's loading indicator (REQ-556).
    ///
    /// It lives in session state, beside `cost`, for the same reason `cost`
    /// does: the fold happens in [`render_event`], which is the **one** place
    /// every `model_lifecycle` event passes through — whether it arrives while
    /// the session is idle at the prompt or is drained by a turn's own pump. An
    /// indicator fed only from the idle path would keep animating "model
    /// starting" after a `Ready` that landed mid-turn.
    pub loading: crate::loading::LoadingIndicator,
    /// This session's permission level, once the daemon has been asked
    /// (REQ-560).
    ///
    /// `None` means "not known" — before the first read, or against a daemon too
    /// old to serve `session/permissions` — and a level nobody knows renders no
    /// status row rather than a guess. Guessing `guarded` here would be the
    /// worst available answer: it would show a stricter posture than the session
    /// might actually be in.
    ///
    /// The daemon is authoritative; this is a render cache, refreshed at session
    /// start and by `/permissions` itself. Session-scoped like everything else
    /// here — nothing is persisted (BR-6).
    pub permission_level: Option<teton_protocol::permissions::PermissionLevel>,
    /// The daemon's reasoning-effort view, for the status row (REQ-559 / REQ-560).
    ///
    /// A render cache like [`Self::permission_level`], and `None` for the same
    /// two reasons: nobody has asked yet, or the daemon predates the setting. In
    /// either case the status row shows the permission field alone rather than
    /// inventing an effort value.
    ///
    /// The view itself is the daemon's, computed with the same `resolve_effort`
    /// the router calls — this holds it rather than a derived string so
    /// [`crate::status::effort_field`] can apply REQ-559 BR-6's
    /// reaches-no-model rule to real data.
    pub effort: Option<teton_protocol::methods::EffortView>,
    /// This session's web-lookup capability, as the event stream reports it
    /// (REQ-563 BR-7/BR-13).
    ///
    /// Folded here for the reason `loading` is: [`render_event`] is the one place
    /// every web event passes through, so the status field and the notice lines
    /// are two readings of one fold and cannot disagree about whether the session
    /// is restricted.
    pub web: WebState,
    /// The session this client is *in*, once `session/create` (or
    /// `session/attach`) has answered — `None` before that.
    ///
    /// The event bus is daemon-wide: every attached client receives every
    /// session's events, and the envelope's `session_id` is the only thing that
    /// says whose. Most arms need not care, because the events they render are
    /// already scoped by which client owns the prompt. `context_cleared` is not:
    /// it is a fact about one session's conversation that every client would
    /// otherwise draw as though it were its own ("context cleared; 12 retained
    /// blocks dropped" in a session where nothing was cleared). So the arm names
    /// the other session, following `client.rs`'s "in another session" precedent
    /// for a permission request that is not ours to answer.
    pub session_id: Option<SessionId>,
    /// The assistant text of the turn in flight, kept so the surface can
    /// guarantee the `/provider setup` hand-off (REQ-579 ADR-9).
    ///
    /// Only [`SessionUpdatePayload::AgentMessageChunk`] appends here, and that
    /// is the whole point: the check downstream is a match on **the model's own
    /// words**, so the user's typed prompt, a tool title, a plan entry and
    /// `/help`'s output are all structurally incapable of triggering it. A
    /// second writer would turn a deterministic nudge into a line that fires
    /// when the user themselves mentions a shell command.
    ///
    /// Cleared by [`SessionState::begin_turn`] when the *next* prompt is sent
    /// rather than when a turn ends, so a turn that never reached
    /// [`hand_off_after_turn`] — an interrupted one, or one the daemon
    /// refused — cannot leak its text into the turn after it.
    turn_reply: String,
    /// This session's root — the directory its tools are scoped to — as the
    /// daemon last described it (REQ-583 ADR-4).
    ///
    /// A cache of a daemon fact, not client state: filled from
    /// `SessionCreateResult.root` when the session starts and replaced by every
    /// `session_root_changed` for this session, so `/cd`'s bare form can print
    /// the root without an RPC. `None` before the session exists, on a passive
    /// context, or against a daemon older than the field — and a root nobody
    /// knows renders as a notice saying so, never as a guess: the client does
    /// not derive kind (ADR-1).
    pub root: Option<SessionRoot>,
    /// The user's typed prompt for the turn in flight (REQ-581 ADR-4).
    ///
    /// REQ-579's nudge could read the reply alone because the failure it
    /// corrects is *in* the reply — a recited shell recipe. The connection
    /// question's failure is not: the model runs `teton provider list` through
    /// the shell tool, misreads it, and answers in prose that recites nothing.
    /// What identifies that turn is what the **user asked**, so the question is
    /// kept beside the answer for the one predicate that needs both.
    ///
    /// Written by [`SessionState::begin_turn`] only, from the same bytes the
    /// prompt RPC carries — never from the event stream — so no model output and
    /// no other session can put words here.
    turn_prompt: String,
    /// The titles of the tool calls this turn made (REQ-581 ADR-4).
    ///
    /// The daemon's own `<tool>: <command>` title, exactly as the renderer drew
    /// it (`shell: teton provider list`). It is the record of what the turn
    /// *did*, which is the half of the observed failure the reply text does not
    /// carry.
    ///
    /// Scoped to this session's updates for [`Self::turn_reply`]'s reason, and
    /// cleared at [`SessionState::begin_turn`] beside it: a turn that never
    /// reached [`hand_off_after_turn`] must not lend its tool calls to the next
    /// one.
    turn_tools: Vec<String>,
    /// The provider ids the daemon's config snapshot reported (REQ-581 ADR-4).
    ///
    /// A render cache like [`Self::permission_level`] and [`Self::effort`],
    /// filled from the same `config/get` the status row reads. It exists so
    /// "is kimi working?" reads as a connection question without a vendor list
    /// hard-coded into this crate — the registered ids are the user's own
    /// vocabulary for their providers.
    ///
    /// Empty is a legitimate state (no snapshot yet, or a daemon that answered
    /// with no providers) and costs only the extra half of the predicate: the
    /// fixed subject words still recognise "test the connection".
    pub provider_ids: Vec<String>,
    /// True while **this connection's own** `provider/test` call is out
    /// (REQ-581 verify G2).
    ///
    /// The `provider_tested` notice is suppressed for the client that ran the
    /// command, because that client prints the whole report and the notice would
    /// be the same news twice. The question the arm has to answer is therefore
    /// "did *I* ask for this?" — and the session id cannot answer it: a second
    /// client attached to the same session is precisely the audience the notice
    /// exists for (LESSON-505), and gating on the session alone silences it for
    /// them too.
    ///
    /// Written by [`crate::provider_test_ui::DaemonIo::provider_test`] around
    /// its `conn.call`, on both the `Ok` and the `Err` path, because the call is
    /// where the pump runs and the pump is what renders the event. Nothing else
    /// sets it: a flag any other flow could raise would be a way to silence a
    /// notice about a test this client never ran.
    pub(crate) provider_test_in_flight: bool,
    /// Whether this session's stdout is a terminal (REQ-583 BR-5 / ADR-5).
    ///
    /// The not-a-project notice is content the surface draws only at a
    /// terminal: launch prints it under the banner inside the same `if
    /// interactive` gate as the banner, and BR-8's re-fire after `/cd` — drawn
    /// from the `session_root_changed` arm, which has only this state to ask —
    /// takes the same gate from here, so a pipe sees the root line and nothing
    /// more (byte parity, ADR-007's TTY clause). Set once by `run_session` from
    /// the same `is_terminal` read the banner uses; `false` by default, which is
    /// the piped posture and the safe one — a passive context that was never
    /// told draws no notice rather than one nobody asked for.
    pub interactive: bool,
    /// This session's root moved, so its skill snapshot is out of date
    /// (REQ-585 ADR-2).
    ///
    /// A one-bit fold of `session_root_changed`, raised here rather than acted
    /// on there because refreshing the snapshot means **an RPC**, and
    /// [`render_event`] runs inside the event pump — where a blocking `call`
    /// would re-enter the pump from inside an event dispatch, the same
    /// re-entrancy the permission and proposal replies are fire-and-forget to
    /// avoid. So the arm that *sees* the move records it, and the entry loop —
    /// which owns the connection — is what asks.
    ///
    /// Raised only for **this client's own** session, under the same condition
    /// the root cache itself is written under: another session's `/cd`
    /// re-derives nothing here.
    ///
    /// [`SessionState::take_skills_stale`] is the only reader, and it clears as
    /// it reads — a flag two places could clear would be a refresh that stopped
    /// happening the first time one of them ran.
    skills_stale: bool,
}

/// What the session's web capability currently is, for the status row.
///
/// Three of its four fields are derived entirely from the event stream rather
/// than from config, and that is the point: the status row's job is to say what
/// *this session* can do now, and a config read at startup would keep saying
/// `fetch` through a taint trip that disabled it. Each of those is written by
/// exactly one event kind.
///
/// [`Self::capability`] is the exception and the reason is symmetric: what the
/// **machine** is configured for is not observable from this session's events at
/// all — a session that has never looked anything up produces none — so it comes
/// from the daemon's own derivation on the config snapshot (REQ-572 BR-3/BR-10)
/// and is refreshed when a setup commit announces itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WebState {
    /// The highest tier this session has been observed to hold, from consent
    /// decisions and from lookups that actually ran.
    ///
    /// `None` until something proves otherwise, which renders as `off` — the
    /// honest reading of "nothing in this session has used the web", and the
    /// default state on every machine (BR-1).
    granted: Option<WebTier>,
    /// This session read privacy-boundary content and a model-composed lookup
    /// has been refused for it (BR-13).
    restricted: bool,
    /// The user lifted that restriction with `/web allow` (BR-13, AC-12).
    overridden: bool,
    /// What the machine's `[web]` table amounts to, as the daemon derived it
    /// (REQ-572).
    ///
    /// `None` means nobody has read a config snapshot yet, or the daemon
    /// predates the field — both of which are "no answer", and neither is a
    /// reason to invent one. It carries the wire state rather than a projection
    /// of it so the one classifier that governs tool exposure is also the one
    /// this row reads (BR-3, LESSON-456).
    pub capability: Option<WebCapabilityState>,
}

impl WebState {
    /// The status-row field: `web: …`.
    ///
    /// A pure function of the four fields, so it is testable with no terminal —
    /// which matters because the row it belongs to is drawn only at a TTY.
    ///
    /// Order is precedence, not preference, and REQ-572 did not disturb it. The
    /// restricted and overridden states are *about* the tiers rather than
    /// alternatives to them, and a row can show one field: a session that is
    /// restricted has had a capability taken away, and saying `web: search`
    /// while search is refused would be the status row contradicting the notice
    /// that preceded it.
    ///
    /// The capability is consulted **last**, in the arm that used to be a flat
    /// `web: off`, because it answers a different question from the three above
    /// it: what this session has done is what the others report, and what the
    /// machine is configured for is only interesting when the session has not
    /// done anything yet. "Off" and "off but one command away" are the two
    /// states this REQ exists to tell apart (BR-10), so they no longer share a
    /// spelling.
    #[must_use]
    pub fn status_field(&self) -> &'static str {
        if self.overridden {
            return "web: overridden";
        }
        if self.restricted {
            return "web: restricted (taint)";
        }
        match self.granted {
            Some(WebTier::FetchUserUrl | WebTier::FetchAnyUrl) => "web: fetch",
            Some(WebTier::Search) => "web: search",
            None | Some(WebTier::Off) => self.configured_field(),
        }
    }

    /// What the machine is configured for, when the session itself has nothing
    /// to report.
    ///
    /// The `(configured)` and `(available)` suffixes are doing work: this row is
    /// about what *this session* may do, and a bare `web: search` here would
    /// claim a session-level grant the user has not given — consent is still
    /// asked per lookup (BR-3 of REQ-563). What these say is that a ceiling
    /// exists, and what it is.
    ///
    /// Every arm is reachable through this function, which is where the AC-4
    /// assertions read it; [`Self::is_engaged`] records why the *painted* row
    /// does not reach them yet and what changing that would mean.
    fn configured_field(&self) -> &'static str {
        match &self.capability {
            None => "web: off",
            Some(WebCapabilityState::OffAvailable) => "web: off (available)",
            Some(WebCapabilityState::SearchUnavailable { .. }) => "web: search (unavailable)",
            Some(WebCapabilityState::Ready { tier }) => match tier {
                // `Ready` never carries `Off` (the wire type says so); if a
                // daemon ever sends one, "off but available" is the honest
                // reading of a ceiling of nothing.
                WebTier::Off => "web: off (available)",
                WebTier::FetchUserUrl | WebTier::FetchAnyUrl => "web: fetch (configured)",
                WebTier::Search => "web: search (configured)",
            },
        }
    }

    /// Whether the status row has anything to say.
    ///
    /// False on a machine that never opted in — which is every machine by
    /// default (BR-1) — so the interactive layout an existing session sees is
    /// byte-identical until the user turns the capability on. `web: off` is
    /// still a rendered string ([`Self::status_field`]) because a caller that
    /// draws a full status row needs the field for it; what is suppressed is the
    /// row, not the vocabulary.
    ///
    /// **REQ-572 deliberately left this predicate alone.** The capability enters
    /// what the row *says* and not whether it is *drawn*, so the row keeps
    /// meaning "what this session has done" — which is what REQ-563's pty
    /// acceptance test pins, and pins non-vacuously by first observing an absent
    /// row on a configured machine.
    ///
    /// Making a configured or an available capability engage the row is a
    /// one-arm change here, and it is a **product** decision rather than a
    /// mechanical one: it would put a permanent row above the prompt of every
    /// interactive session on every machine, and it would change what the row
    /// reports from "what this session did" to "what this machine can do". This
    /// REQ's discoverability is carried by the per-state prompt clause and the
    /// refusal text (architecture, Half 1), neither of which needs the row, so
    /// the layout of a session that has not touched the web is unchanged.
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        self.overridden || self.restricted || matches!(self.granted, Some(t) if t != WebTier::Off)
    }

    /// Raise the observed ceiling; never lowers it.
    fn observe_tier(&mut self, tier: WebTier) {
        if tier != WebTier::Off && self.granted.is_none_or(|held| tier > held) {
            self.granted = Some(tier);
        }
    }
}

impl SessionState {
    /// Fresh session state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new turn: the accumulated reply text belongs to the turn about to
    /// start, not the one before it (REQ-579 ADR-9).
    ///
    /// Called from the entry loop immediately before the prompt goes on the
    /// wire. Clearing here rather than at turn end is deliberate — see
    /// [`SessionState::turn_reply`].
    ///
    /// `prompt` is the text the RPC is about to carry (REQ-581 ADR-4): the turn
    /// is opened *with* the question, so the one place that clears the turn's
    /// record is also the one place that starts it, and the three fields cannot
    /// come to describe different turns.
    pub(crate) fn begin_turn(&mut self, prompt: &str) {
        self.turn_reply.clear();
        self.turn_prompt.clear();
        self.turn_prompt.push_str(prompt);
        self.turn_tools.clear();
    }

    /// Claim a model proposal, returning `true` the first time only.
    ///
    /// A client can meet the same proposal twice — once as a broadcast event and
    /// once through `model/status`'s `pending_proposal` on the late-attach path
    /// (the daemon does not replay the event, so the client must ask). Both must
    /// not prompt, so the id is claimed once and the second sighting is dropped.
    /// Both carry the same `request_id`, which is what makes the two sightings
    /// recognisable as one.
    pub fn claim_model_proposal(&mut self, request_id: &RequestId) -> bool {
        self.model_seen.insert(request_id.clone())
    }

    /// Whether this session's skill snapshot needs re-fetching, clearing the
    /// flag (REQ-585 ADR-2).
    ///
    /// Read by the entry loop before it classifies a line, which is the last
    /// moment at which a stale snapshot could still dispatch a skill the
    /// session no longer has. Clearing as it reads is what keeps one `/cd` to
    /// one `skills/list` rather than one per typed line.
    pub fn take_skills_stale(&mut self) -> bool {
        std::mem::take(&mut self.skills_stale)
    }
}

/// What a rendered event needs from the caller afterwards.
#[derive(Debug)]
pub enum EventOutcome {
    /// Fully handled; nothing more to do.
    Rendered,
    /// A permission decision is required. The caller resolves it with
    /// [`resolve_permission`] and sends the result.
    Permission(Box<PermissionRequest>),
    /// A local-model proposal needs an answer (REQ-547 BR-1). The caller renders
    /// and resolves it with [`crate::model_ui::resolve_proposal`] and sends the
    /// resulting `model/confirm` — or sends nothing, leaving the proposal open.
    ModelProposal(Box<ModelSelectionProposed>),
    /// Somebody is asking to attach to a session, or to watch every session on
    /// this daemon (REQ-570 BR-4, AC-4). The caller resolves it with
    /// [`resolve_attach_consent`] and sends the resulting `attach/consent`.
    ///
    /// Before REQ-570 this event was rendered as a notice the client could not
    /// act on, so **every** consent path ended in the daemon's 30-second
    /// timeout: nothing in this crate sent `attach/consent`, and REQ-569's own
    /// acceptance evidence for the grant flow leaned on a test-harness
    /// auto-consent no shipped client had. The tested flow and the shipped flow
    /// diverged until this existed.
    AttachConsent(Box<AttachConsentRequested>),
}

/// Render one event, updating `state`, and report whether follow-up is needed.
pub fn render_event(
    env: &EventEnvelope,
    surface: &mut dyn Surface,
    state: &mut SessionState,
) -> EventOutcome {
    match &env.event {
        Event::SessionUpdate(su) => {
            // The envelope's session travels with the update because one thing
            // downstream keeps a *record* of it rather than merely drawing it
            // (the ADR-9 accumulator), and a record is where "whose session was
            // this" starts to matter — the same reason `context_cleared` reads
            // it below.
            render_session_update(&su.update, env.session_id.as_ref(), surface, state);
            EventOutcome::Rendered
        }
        // REQ-585 BR-12 / ADR-15. Never verbose-gated: *every* invocation
        // echoes one line, because the line is the only record that a `/name`
        // the user typed became a turn at all — the body is deliberately not
        // printed, so without this the transcript would show a prompt turn
        // nobody can see the question for. `/verbose` adds the detail under it.
        Event::SkillInvoked(invoked) => {
            render_skill_invoked(invoked, surface, state.verbose);
            EventOutcome::Rendered
        }
        // BUG-189: a call refused before any file resolved. One line, always —
        // not behind `/verbose`, because BR-9's rule is one line per typed
        // refusal and the whole defect was that these two produced none.
        Event::SkillRefused(refused) => {
            surface.line(LineKind::Notice, &skill_name_refusal_line(refused));
            EventOutcome::Rendered
        }
        // REQ-584 BR-11: the hand-off, drawn from the daemon's record rather
        // than hoped for in the model's prose (REQ-579 ADR-9, LESSON-532). Not
        // behind `/verbose`: it is the answer to the question the user asked.
        Event::ProjectMatch(matched) => {
            surface.line(LineKind::Notice, &project_handoff_line(matched));
            EventOutcome::Rendered
        }
        Event::RouteDecided(rd) => {
            if state.verbose {
                surface.line(LineKind::Notice, &format_route(rd));
            }
            EventOutcome::Rendered
        }
        Event::PrivacyBlock(pb) => {
            surface.line(LineKind::Notice, &format_privacy(pb));
            EventOutcome::Rendered
        }
        // Never verbose-gated, for the same reason `privacy_block` is not: this
        // is a refusal that changed what the session may do, and LESSON-505 is
        // that an audit signal only a daemon log carries is a weak control. A
        // user who cannot see it cannot act on it.
        Event::ProvenanceRejected(pr) => {
            surface.line(LineKind::Notice, &format_provenance_rejected(pr));
            EventOutcome::Rendered
        }
        Event::ProviderDegraded(pd) => {
            surface.line(LineKind::Notice, &format_degraded(pd));
            EventOutcome::Rendered
        }
        Event::CostRecorded(cr) => {
            state.cost.record(cr.record.clone());
            EventOutcome::Rendered
        }
        Event::ModelLifecycle(ModelLifecycle { model_id, stage }) => {
            // The line and the indicator are two presentations of one event,
            // folded at one place so they cannot disagree (REQ-556 BR-10,
            // LESSON-456). `render_lifecycle` keeps sole ownership of the
            // per-stage line; the indicator only ever draws motion for the
            // window that has no line at all.
            state.loading.observe(model_id, stage);
            firstrun::render_lifecycle(model_id, stage, surface);
            EventOutcome::Rendered
        }
        Event::PhaseTransition(pt) => {
            surface.line(LineKind::Notice, &format_phase(pt));
            EventOutcome::Rendered
        }
        Event::DaemonClientAttach(a) => {
            surface.line(LineKind::Info, &format_attach(a));
            EventOutcome::Rendered
        }
        // REQ-565's lifetime stages. Only the two connection-count stages can
        // ever reach a *live* client, and then only when a second client comes
        // or goes — the three shutdown stages fire when the count is already
        // zero, so nobody is attached to receive them and their audience is the
        // daemon log (which is where the acceptance suite reads them).
        //
        // Verbose-gated, like `route_decided`: another session opening or
        // closing is diagnostic detail, not something a default session should
        // interrupt itself to report.
        Event::DaemonLifetime(lifetime) => {
            if state.verbose {
                match &lifetime.stage {
                    DaemonLifetimeStage::ClientConnected {
                        live_connection_count,
                    } => surface.line(
                        LineKind::Notice,
                        &format!("a client attached ({live_connection_count} connected)"),
                    ),
                    DaemonLifetimeStage::ClientDisconnected {
                        live_connection_count,
                    } => surface.line(
                        LineKind::Notice,
                        &format!("a client detached ({live_connection_count} connected)"),
                    ),
                    // Unreachable while this client is attached; consumed rather
                    // than rendered so a future change that does deliver one
                    // cannot print a shutdown notice into a working session.
                    DaemonLifetimeStage::ShutdownArmed { .. }
                    | DaemonLifetimeStage::ShutdownDeferred { .. }
                    | DaemonLifetimeStage::Shutdown { .. } => {}
                }
            }
            EventOutcome::Rendered
        }
        // REQ-561 BR-9a ships the title as data and stops there: the event is
        // what makes `SessionSummary.title` observable, and it commits no
        // surface to drawing it. The CLI's session view has nowhere a title
        // belongs today — the status line is REQ-560's — so this arm handles the
        // event by consuming it, and a client that wants the title reads the
        // stream or `session/list`. Rendering it here would be this REQ deciding
        // another one's layout.
        Event::SessionTitled(_) => EventOutcome::Rendered,
        Event::PermissionRequest(pr) => EventOutcome::Permission(Box::new(pr.clone())),
        // REQ-547: the consent round-trip. The proposal is *not* rendered here —
        // it is handed back so the caller can decide whether this client owns the
        // prompt, and the client that owns it renders and answers in one step
        // (like a permission request). The decision, by contrast, is pure
        // information: every attached client shows it.
        Event::ModelSelectionProposed(proposed) => {
            EventOutcome::ModelProposal(Box::new(proposed.clone()))
        }
        Event::ModelSelectionDecided(decided) => {
            surface.line(LineKind::Notice, &firstrun::format_decided(decided));
            EventOutcome::Rendered
        }
        // REQ-563's three web events. Each folds into `state.web` first — the
        // status row reads that fold — and then decides whether it also has a
        // line to draw.
        Event::WebLookup(lookup) => {
            if lookup.outcome == WebLookupOutcome::TaintRestricted {
                state.web.restricted = true;
            }
            // A lookup that actually ran proves the tier it needed was held —
            // including a cache hit, which needed the grant even though nothing
            // left the machine (BR-12). Refusals prove nothing, so they raise
            // nothing.
            if matches!(
                lookup.outcome,
                WebLookupOutcome::Completed | WebLookupOutcome::CacheHit
            ) {
                state.web.observe_tier(match lookup.kind {
                    WebLookupKind::Fetch => WebTier::FetchUserUrl,
                    WebLookupKind::Search => WebTier::Search,
                });
            }
            if let Some(line) = format_web_lookup(lookup, state.verbose) {
                // Never `LineKind::Error`, for any outcome (BUG-152): a refusal
                // is this capability working, an unreachable host is transient,
                // and the turn continues in both cases (BR-9). The line class is
                // the same one every other control event uses.
                surface.line(LineKind::Notice, &line);
            }
            EventOutcome::Rendered
        }
        Event::WebConsentDecided(decided) => {
            if decided.granted {
                state.web.observe_tier(decided.tier);
            }
            // Never verbose-gated: a consent decision is the user's own answer
            // coming back, and the persistent one changed a file on disk.
            surface.line(LineKind::Notice, &format_web_consent(decided));
            EventOutcome::Rendered
        }
        Event::WebTaintOverridden(overridden) => {
            state.web.overridden = true;
            // Verbose-gated *only* because the client that issued `/web allow`
            // renders the RPC's own answer, which is authoritative about what was
            // restored and about whether anything was restricted at all. This
            // line exists for the other attached clients, which saw no command
            // and would otherwise watch the restriction lift in silence.
            if state.verbose {
                surface.line(
                    LineKind::Notice,
                    &format_web_taint_overridden(&overridden.tiers_restored),
                );
            }
            EventOutcome::Rendered
        }
        Event::ContextCleared(cleared) => {
            // REQ-567 BR-8. Never verbose-gated: a clear changes what every
            // later prompt starts from, and a second attached client that did
            // not type the command would otherwise watch the conversation reset
            // in silence — the case `web_taint_overridden`'s notice exists for,
            // without its gate, because there the issuing client's own RPC
            // answer is the authoritative line and here the reset is the news.
            //
            // This is also the **only** line the client that typed `/clear`
            // draws: `slash::handle_clear` renders nothing on success precisely
            // so that one clear is one line, drawn by this code, on every
            // attached client including the issuer. The count it needs is here,
            // so a second rendering from the RPC's answer would say the same
            // thing twice to the one person who already knew.
            //
            // It says the conversation and nothing else, because that is all a
            // clear drops (OQ-4): the session's taint pin, pasted URLs and
            // permission grants survive, and a line that read "session reset"
            // would be telling the user their consent had been re-asked for.
            //
            // And it says *whose* conversation when it was not this client's.
            // The bus is daemon-wide, so an unqualified line would tell every
            // attached client that its own context had just been dropped — the
            // one thing this notice must never get wrong, since the user's next
            // move depends on what their next prompt starts from.
            surface.line(
                LineKind::Notice,
                &format_context_cleared(
                    cleared.blocks_dropped,
                    other_session(state.session_id.as_ref(), env.session_id.as_ref()),
                ),
            );
            EventOutcome::Rendered
        }
        Event::PrefixCache(cache) => {
            // Diagnostic chrome, not news: prefix reuse is a pure latency
            // optimization and BR-1 makes it unobservable in output, so a user
            // who did not ask has nothing to act on. It renders under the same
            // `verbose` flag the routing notices use — an *eviction* included,
            // because "your cache went away" only matters to someone already
            // watching why a turn was slow.
            if state.verbose {
                surface.line(LineKind::Notice, &format_prefix_cache(cache));
            }
            EventOutcome::Rendered
        }
        Event::AttachConsentRequested(request) => {
            // REQ-570 BR-4: this client can now answer. Handing it back to the
            // caller rather than rendering a notice here is what closes Gap 2 —
            // until this existed, nothing in this crate sent `attach/consent`
            // and every consent path ended in the daemon's 30-second timeout.
            //
            // The rendering moved into `resolve_attach_consent` so the question
            // and the answer live together: a prompt drawn here and a decision
            // taken elsewhere is how the two drift apart.
            EventOutcome::AttachConsent(Box::new(request.clone()))
        }
        Event::AttachRefused(_) => {
            // Nothing to draw. The connection that was refused is told by its
            // own RPC error, and this client rendered a notice rather than a
            // modal, so there is no prompt on screen to retire.
            EventOutcome::Rendered
        }
        Event::SessionGrantMinted(minted) => {
            // REQ-569 verify (F6). Daemon-scoped, so every connected client
            // gets it — which is the point: the user whose session was just
            // opened up to somebody else is told by a surface the requester
            // cannot suppress. Never verbose-gated, for
            // `attach_consent_requested`'s reason: a widened permission is
            // news, not chrome.
            surface.line(LineKind::Notice, &format_grant_minted(minted));
            EventOutcome::Rendered
        }
        // REQ-572 BR-14 / OQ-2. Never verbose-gated, for `web_consent_decided`'s
        // reason: the config on disk changed and the machine can now reach the
        // network in a way it could not a moment ago. A second client attached
        // to this session watched that happen and is owed the news (LESSON-505).
        //
        // The client that typed `/web setup` renders nothing of its own on a
        // successful commit, so this line is the completion notice for the
        // issuer and the bystander alike — one change, one line, drawn by one
        // piece of code, which is the arrangement `context_cleared` settled on.
        Event::WebSetupCompleted(completed) => {
            // The machine's ceiling just moved, and this session's status row
            // reads it. Folded from the event rather than re-read from config,
            // because the event is the one thing that arrives *at* the change.
            state.web.capability = Some(WebCapabilityState::Ready {
                tier: completed.tier,
            });
            surface.line(LineKind::Notice, &format_web_setup_completed(completed));
            EventOutcome::Rendered
        }
        // BR-4/AC-4's defense in depth, announced rather than logged: something
        // that was not this session's user tried to preview or commit a config
        // change and was refused. The user is the only one who can act on that,
        // so it is never verbose-gated either.
        Event::WebSetupRejected(rejected) => {
            surface.line(LineKind::Notice, &format_web_setup_rejected(rejected));
            EventOutcome::Rendered
        }
        // REQ-579 BR-15, and the web pair's arrangement applied to the second
        // flow: never verbose-gated, because the config on disk changed and this
        // session can now route somewhere it could not a moment ago. A second
        // client attached to the same session watched that happen and is owed
        // the news (LESSON-505).
        Event::ProviderSetupCompleted(completed) => {
            let elsewhere = other_session(state.session_id.as_ref(), env.session_id.as_ref());
            // REQ-581 ADR-4's cache, kept current. `provider_ids` is filled once
            // by `read_config_view` at session start, so a provider registered
            // *during* the session — which is REQ-579's whole flow, and the
            // motivating one for this REQ — was invisible to the connection
            // predicate until the next run of the CLI. "is kimi working?" one
            // minute after registering `kimi` is the exact turn the hand-off
            // exists for.
            //
            // Only for this session's own registration: the ids are this user's
            // vocabulary for the providers *they* just set up, and another
            // session's registration is news (the notice below says so) rather
            // than a word to start reading their prompts for.
            //
            // "Ours" has to be *known*, not merely unrefuted (verify G6).
            // `other_session` folds "unknown" into `None` on purpose — a notice
            // that guessed "in another session" before `session/create` answered
            // would be wrong in the common case — but that fold is wrong for a
            // cache: events are pumped during `session/create` itself, and a
            // foreign session's registration arriving in that window would be
            // filed as our own vocabulary. So the id is required to exist,
            // which costs nothing real (the events that matter arrive long
            // after) and closes the one window where the two readings differ.
            if state.session_id.is_some() && elsewhere.is_none() {
                let id = completed.provider_id.0.as_str();
                if !state.provider_ids.iter().any(|known| known == id) {
                    state.provider_ids.push(id.to_owned());
                }
            }
            surface.line(
                LineKind::Notice,
                &format_provider_setup_completed(completed, elsewhere),
            );
            EventOutcome::Rendered
        }
        // BR-12's defense in depth, announced rather than logged: something that
        // was not this session's user tried to commit a provider registration
        // and was refused. The user is the only one who can act on that, so it
        // is never verbose-gated either.
        Event::ProviderSetupRejected(rejected) => {
            surface.line(
                LineKind::Notice,
                &format_provider_setup_rejected(
                    rejected,
                    other_session(state.session_id.as_ref(), env.session_id.as_ref()),
                ),
            );
            EventOutcome::Rendered
        }
        // Diagnostic chrome, and deliberately thin. The turn that dead-ended
        // has already told the user what it could not do and what would fix it
        // — the unserved-turn sentence and the web tool's own refusal both name
        // their remedy — so an ungated line here would be that fact twice. What
        // this adds for someone already watching is the capability *id*, which
        // is what a bug report and the `/web setup` path key on.
        //
        // Rendered from the id without branching on it: a client that has never
        // heard of a capability must still be able to report a dead end in it
        // (the reason the field is a string at all), and a match arm per id
        // would be a vocabulary the two ends have to agree on.
        Event::CapabilityDeadEnd(dead_end) => {
            if state.verbose {
                surface.line(LineKind::Notice, &format_capability_dead_end(dead_end));
            }
            EventOutcome::Rendered
        }
        // REQ-580 BR-2/BR-5: the user's message is being held for the local
        // tier, and the screen would otherwise sit still until the tier opened.
        // Never verbose-gated — this is the answer to "did my message go?" —
        // and a notice rather than an error, the class of the startup lifecycle
        // lines it continues (BUG-152's `TIER_WARMING` precedent): nothing
        // broke, nothing needs fixing, and the state ends by itself.
        // REQ-581 BR-3/BR-4: the health map the router reads at decision time
        // just moved, and a second client attached to the session watched that
        // happen (LESSON-505). Published on every outcome, not only the good one
        // — the test either spent or failed, and either way the next turn's
        // routing changed.
        //
        // Suppressed for **the connection that issued the test**, and for that
        // one only. That client has the whole `ProviderTestResult` and prints
        // the report — model, dial host, remedy, routing — so a notice here
        // would be the same news twice on the one surface that already said it
        // at length, in two wordings a reader has to reconcile. The spec's
        // example transcript and the CHANGELOG show the report alone.
        //
        // The gate is the in-flight flag rather than the session id, and the
        // difference is a whole audience. `other_session` returns `None` for
        // *any* event on our own session, which includes the case this notice
        // was written for: a second client attached to the same session, which
        // ran no command, has no report, and watched the health its own turns
        // route by move (LESSON-505). Keyed on the flag, that client renders —
        // without the "in another session" clause, because the test really was
        // in theirs.
        Event::ProviderTested(tested) => {
            if !state.provider_test_in_flight {
                let elsewhere = other_session(state.session_id.as_ref(), env.session_id.as_ref());
                surface.line(LineKind::Notice, &format_provider_tested(tested, elsewhere));
            }
            EventOutcome::Rendered
        }
        Event::TurnQueued(queued) => {
            surface.line(LineKind::Notice, &format_turn_queued(queued));
            EventOutcome::Rendered
        }
        Event::SessionRootChanged(changed) => {
            // REQ-583 BR-7/BR-8. The `context_cleared` event that precedes this
            // one has already drawn its line — the disposition of a move is a
            // clear, in the existing shape — so this arm draws only the news
            // that is new: where the session is now, and (when that is not a
            // project) the same one-line notice launch would have printed. Never
            // verbose-gated, for `context_cleared`'s reason: the jail every
            // later tool call runs under just moved.
            //
            // And it says *whose* root when it was not this client's session
            // (the bus is daemon-wide) — without redrawing that root here, since
            // this client's cache is about this client's session.
            match other_session(state.session_id.as_ref(), env.session_id.as_ref()) {
                Some(session) => {
                    surface.line(LineKind::Notice, &format_root_moved_elsewhere(session));
                }
                None => {
                    // The cache is *this* session's root, so it follows the
                    // event only when this client knows which session it is
                    // in. With no session of its own (a passive context, or the
                    // window before `session/create` answers) unknown is not
                    // evidence of elsewhere — the line still draws — but it is
                    // not evidence of *here* either, and caching another
                    // session's root would make a later bare `/cd` describe a
                    // root this client never had.
                    if state.session_id.is_some() {
                        state.root = Some(changed.root.clone());
                        // REQ-585 ADR-2: the project half of the registry is
                        // derived from the root, so a move re-derives it. Under
                        // the *same* condition as the root cache, and for the
                        // same reason: a client that does not know which
                        // session it is in has no registry of its own to
                        // refresh, and re-fetching on another session's move
                        // would ask for a root this client never had.
                        state.skills_stale = true;
                        // …and the answers the user gave about *this* root:
                        // its skills' commands, and REQ-587 BR-4's
                        // acknowledgment that the model may run them at all.
                        // The daemon drops its copy inside `set_session_cwd`;
                        // this is the same moment on this side of the wire,
                        // under the same own-session condition, because a grant
                        // consulted before the prompt is drawn would otherwise
                        // auto-answer for a repo the user never approved.
                        state.grants.forget_root_scoped_grants();
                    }
                    surface.line(
                        LineKind::Notice,
                        &format_session_root_changed(&changed.root),
                    );
                    // BR-8, under BR-5's gate: the notice's bytes are for a
                    // terminal, at launch and here alike — a pipe gets the
                    // root line above and nothing more.
                    if state.interactive {
                        if let Some(notice) = banner::root_notice(&changed.root) {
                            surface.line(LineKind::Notice, &notice);
                        }
                    }
                }
            }
            EventOutcome::Rendered
        }
        Event::ContextPressure(pressure) => {
            // REQ-586 BR-7, on the `context_cleared` precedent and for the same
            // reason: never verbose-gated. What was dropped, elided or re-fitted
            // is not diagnostic chrome about *how* a turn ran — it is a change
            // to what the model was given to answer with, and a user reading an
            // answer that forgot the first half of the conversation is owed the
            // sentence that says so. "Nothing is clamped in silence" is the
            // requirement; a `/verbose` gate would be silence by default.
            surface.line(LineKind::Notice, &format_context_pressure(pressure));
            EventOutcome::Rendered
        }
    }
}

/// The line a `session_root_changed` event draws for the session it is about
/// (REQ-583 BR-7): the new root and its kind, in the one spelling
/// [`banner::root_line`] gives every surface.
fn format_session_root_changed(root: &SessionRoot) -> String {
    format!("session root is now {}", banner::root_line(root))
}

// ---------------------------------------------------------------------------
// REQ-585: the invocation echo line (BR-12, ADR-15)
// ---------------------------------------------------------------------------

/// BR-12's one line per invocation, plus the detail `/verbose` adds under it.
///
/// **The body is never printed** — it is in the file, and BR-12 says so. What
/// reaches the surface is a summary of the file and, under `/verbose`, where it
/// lives, what its frontmatter flags did, what of its frontmatter was inert,
/// what became of each dynamic command, and what this turn has spent of the
/// per-turn cap (REQ-587 BR-9).
///
/// **The shadowing fact is not repeated here.** BR-9 lists it among what
/// `/verbose` adds, and the echo line above already carries it in the source
/// slot — `skill validate (project — shadows your user skill, …)` — on **every**
/// invocation, verbose or not. A second line under it would be one fact in two
/// spellings on one screen (LESSON-528), and the event carries nothing further
/// to say: which user file lost the name is not on it.
///
/// **A refused record is one of these events too, and it is not a duplicate.**
/// BR-9 asks for one line per invocation *and* one line per typed refusal, and
/// the daemon publishes accordingly: a call the loop refuses before dispatch
/// (Stage A) publishes only a refusal record, while one refused after its
/// expansion came back (Stage B) publishes an invocation record — whose commands
/// really did run, which is why it exists — and then a refusal record for the
/// same call. This renderer is stateless per event and deliberately stays that
/// way: it prints the pair as the two lines it is. Folding them would require
/// remembering the previous event to guess at a relationship the wire does not
/// state, and would drop the very line BR-9 asks for.
///
/// Everything rendered here is either the daemon's own typed value or
/// file-supplied text the daemon already bounded; `Surface::line` defuses it
/// again at the frame it is drawn into (ADR-009's two-layer shape).
///
/// One [`Surface::line`] per command, never one string with newlines in it:
/// `line` neutralizes newlines, so a joined list would arrive as one run-on
/// line — the same mechanical reason the *consent* lists commands one per line
/// (ADR-7).
fn render_skill_invoked(invoked: &SkillInvoked, surface: &mut dyn Surface, verbose: bool) {
    surface.line(LineKind::Notice, &skill_echo_line(invoked));
    if !verbose {
        return;
    }
    // **A refusal gets none of the file detail below.** Every line of it reports
    // what the invocation *did* — where the body it expanded came from, what its
    // frontmatter did on the way, what became of each command — and a refused
    // call did none of that. Printing the block anyway would put a paragraph of
    // true-sounding provenance under a line saying nothing happened, and in the
    // Stage B shape (an invocation record, then a refusal record for the same
    // call) it would print that paragraph twice.
    //
    // The turn's count is the exception and is rendered below for both, because
    // it is the one line here about the **turn** rather than about the file —
    // and on a `per_turn_cap` refusal it is the evidence for the refusal.
    if invoked.refused.is_none() {
        render_invocation_detail(invoked, surface);
    }
    if let Some(turn) = invoked.turn_invocations {
        surface.line(LineKind::Info, &turn_invocations_line(turn));
    }
}

/// The `/verbose` block under a *successful* invocation: where the body came
/// from, what its frontmatter did, and what became of each dynamic command.
fn render_invocation_detail(invoked: &SkillInvoked, surface: &mut dyn Surface) {
    surface.line(LineKind::Info, &format!("  {}", invoked.path_display));
    // Only when there were any: BR-5's ignored keys are news about *this* file,
    // and "ignored frontmatter: " with nothing after it is a line about nothing.
    if let Some(note) = &invoked.name_note {
        surface.line(LineKind::Notice, &format!("  {note}"));
    }
    // The keys this build **honored**, above the ones it did not: BR-3 took two
    // out of BR-5's inert list, and a reader comparing the two lines is reading
    // the same file's frontmatter sorted by what happened to it.
    if let Some(line) = declared_flags_line(invoked) {
        surface.line(LineKind::Info, &line);
    }
    if !invoked.ignored_keys.is_empty() {
        surface.line(
            LineKind::Info,
            &format!("  ignored frontmatter: {}", invoked.ignored_keys.join(", ")),
        );
    }
    for view in &invoked.outcomes {
        surface.line(LineKind::Info, &dynamic_outcome_line(view));
    }
}

/// What BR-3's two frontmatter flags did to this file, for `/verbose` (BR-9),
/// or `None` when the file declared neither.
///
/// **The words are [`slash::model_only_words`]'s**, which is `/help`'s mark for
/// the same file. Composing them again here would put "model-only" one line
/// under an echo line for a skill no roster contains, on exactly the file where
/// `/help` says `invocable by nobody` — one product, two answers, and the
/// disagreement only in the case that matters (LESSON-528).
///
/// Nothing renders for the ordinary file, on the `ignored frontmatter` line's
/// own rule: this block reports what *this file wrote*, and a line reading
/// "invocable by the user and the model" over a file that declared no flag at
/// all would report an absence as a declaration.
///
/// The key is named because the actionable half of "model-only" is which line
/// of which file said so — and a **value** this build could not read is worded
/// apart from one it could ([`model_flag_clause`]).
fn declared_flags_line(invoked: &SkillInvoked) -> Option<String> {
    match (invoked.user_invocable, invoked.model_invocable) {
        (true, true) => None,
        // The user's door is open and the model's is shut. `/help` marks this
        // row not at all — it answers "may you type this?", and the answer is
        // yes — so this line is the only place the flag is ever named, and the
        // two surfaces are not in disagreement about anything.
        (true, false) => Some(format!(
            "  hidden from the model ({})",
            model_flag_clause(invoked)
        )),
        // `user-invocable` needs no such split: its safe reading is the
        // *unchanged* one (the user keeps `/name`), so a `false` here can only
        // have come from the literal the clause quotes.
        (false, model_invocable) => Some(format!(
            "  {} (`{USER_INVOCATION_KEY}: false`{})",
            slash::model_only_words(model_invocable),
            if model_invocable {
                String::new()
            } else {
                format!(", {}", model_flag_clause(invoked))
            },
        )),
    }
}

/// BR-3's negative flag, as this build read it: the literal the file wrote, or
/// — when the value was not a boolean — what it was read as instead.
///
/// **A typo must not be quoted back as a declaration.** BR-3's safe reading
/// hides a file whose `disable-model-invocation` value is neither `true` nor
/// `false`, so an author who wrote `yes` is hidden from the model *and* was
/// shown, until this split existed, a line quoting `disable-model-invocation:
/// true` — a line their file does not contain — one line above
/// `ignored frontmatter: disable-model-invocation`, which on its own reads as
/// "this key did nothing". Two lines, contradicting each other, and the one
/// that was true was the one that sounded harmless.
///
/// The malformed case is recognized from [`SkillInvoked::ignored_keys`] rather
/// than from a new wire field, because that list is already exactly the daemon's
/// answer to "which keys did this file write that I did not honor": its parser
/// names the flag there precisely when the value was unreadable, and an honored
/// `true` never appears in it. The file's raw value is not on the wire, so the
/// clause names the key and the reading rather than quoting bytes it does not
/// have.
fn model_flag_clause(invoked: &SkillInvoked) -> String {
    if invoked
        .ignored_keys
        .iter()
        .any(|key| key == MODEL_INVOCATION_KEY)
    {
        format!("`{MODEL_INVOCATION_KEY}` was not `true` or `false`, so the safe reading hid it")
    } else {
        format!("`{MODEL_INVOCATION_KEY}: true`")
    }
}

/// The frontmatter key that hides a skill from the model (BR-3).
const MODEL_INVOCATION_KEY: &str = "disable-model-invocation";

/// The frontmatter key that keeps a skill out of the user's `/name` (BR-3).
const USER_INVOCATION_KEY: &str = "user-invocable";

/// BR-9's `/verbose` count: what this turn has spent of the per-turn cap.
///
/// **A count against the ceiling, never a bare number.** "3" is unreadable
/// without the cap and unfalsifiable with it — a reader cannot tell a turn
/// halfway through its budget from one at the last call it will be allowed, and
/// the next refusal (`per_turn_cap`) would arrive as a surprise. The cap travels
/// with the count for the same reason it travels on the wire: a client that
/// hardcoded 12 would print a stale ceiling the day the daemon moves it.
///
/// Never rendered for a `None`, which is the typed path — see
/// [`render_skill_invoked`].
fn turn_invocations_line(turn: events::TurnInvocations) -> String {
    format!("  invocation {} of {} this turn", turn.count, turn.cap)
}

/// BR-12's echo line: `/status → skill status (user, 5.3 KiB, 4 dynamic commands)`,
/// or REQ-587 BR-9's `skill status (user, 5.3 KiB, 4 dynamic commands) — invoked
/// by the model`.
///
/// The **source** slot names BR-9's swap where there is one — `skill validate
/// (project — shadows your user skill, …)` — read off the event's own
/// `shadows_user_skill` and never re-derived from the session's snapshot: the
/// registry lives on `UiContext`, `render_event` sees only `SessionState`, and a
/// snapshot may have moved under a `/cd` since the invocation it would be
/// answering about. It applies to a typed invocation as readily as to a model
/// one — `/validate` in a repository that defines its own reaches the
/// repository's file, and that is the same surprise BR-4 asks about.
///
/// **The `/name →` prefix is the user's typed line, so a model invocation does
/// not carry one.** Nobody typed `/status`; printing it would put a line in the
/// transcript that reads exactly like the user's own, which is the one
/// distinction BR-9's suffix exists to draw. The parenthetical is identical in
/// both, deliberately: same facts, same order, same units, so the two lines are
/// comparable at a glance and only the attribution differs.
///
/// The count is how many dynamic commands the invocation **had**, not how many
/// succeeded: `outcomes` carries one entry per `` !`…` `` in the body whatever
/// became of it, an empty list is the honest "0 dynamic commands" of a skill
/// with no dynamic context, and what each one did is `/verbose`'s line rather
/// than a number that would have to summarize four different endings.
///
/// The size is [`teton_protocol::format_bytes`] — the product's single byte
/// formatter, the one the daemon's own skip reasons (`over 64 KiB (67,184 B)`)
/// and the first-run sentences already speak. The spec writes the example as
/// `5.3 KB`; two spellings of a file size in one feature would be worse than
/// one that differs from an illustration by a unit suffix.
fn skill_echo_line(invoked: &SkillInvoked) -> String {
    // BR-9's *other* sentence, and it is a different line rather than this one
    // with a flag on it — see [`skill_refusal_line`].
    if let Some(reason) = &invoked.refused {
        return skill_refusal_line(invoked, reason);
    }
    let count = invoked.outcomes.len();
    // How many of them actually started. A command that was declined, refused
    // for want of a terminal, denied by the level, or never spawned leaves a
    // placeholder in the prompt rather than output — and the count alone cannot
    // say so. Reporting only `4 dynamic commands` after a decline would put the
    // one line the user *sees* at odds with what the model actually got, while
    // the record that resolves it sits behind `/verbose`. So the line says both
    // numbers whenever they differ, and stays a single count when they do not
    // (BR-12: observable, not noisy).
    let ran = invoked
        .outcomes
        .iter()
        .filter(|view| !matches!(view.outcome, DynamicOutcome::NotRun { .. }))
        .count();
    let dynamic = if count == 0 {
        "0 dynamic commands".to_owned()
    } else if ran == count {
        format!(
            "{count} dynamic command{}",
            if count == 1 { "" } else { "s" }
        )
    } else if ran == 0 {
        format!(
            "{count} dynamic command{}, none run",
            if count == 1 { "" } else { "s" }
        )
    } else {
        format!("{count} dynamic commands, {ran} run")
    };
    let name = &invoked.name;
    let body = format!(
        "skill {name} ({source}, {size}, {dynamic})",
        source = slash::source_words(invoked.source, invoked.shadows_user_skill),
        size = teton_protocol::format_bytes(invoked.body_bytes),
    );
    match invoked.invoked_by {
        events::InvokedBy::User => format!("/{name} → {body}"),
        events::InvokedBy::Model => format!("{body} — invoked by the model"),
    }
}

/// BR-9's second sentence: **one line per typed refusal**, naming the reason.
///
/// # It is not the invocation line with a flag on it
///
/// A refused record and a skill with no dynamic context are the same bytes
/// apart from one field — same name, same source, same size, the same empty
/// `outcomes` — so a refusal rendered as "the invocation line, plus something"
/// fails in the one direction that matters: at a glance it reads as a skill that
/// ran. This line therefore opens with the **verdict**, where the successful
/// line opens with the skill, and it drops every figure that would claim an
/// expansion happened. The body's size and the dynamic-command count are true of
/// the *file* and false of this turn — nothing of that file entered the context —
/// and printing them under the word "refused" is the same lie told quietly.
///
/// What is kept is what identifies the call: the name, the source, and BR-9's
/// shadowing clause, because "which `validate`?" is a question a refusal raises
/// more sharply than a success does.
///
/// # No invoker suffix
///
/// The successful line carries `— invoked by the model` because its two invokers
/// produce two shapes and the typed one opens with a `/name →` prefix this one
/// never has. Every record carrying `refused` on this build comes from the model
/// path — a typed refusal is composed client-side by `slash::dispatch` and
/// publishes no event at all — so the suffix would be a constant on every
/// refusal line, which BR-9 calls noise rather than observability. If a daemon
/// ever publishes a user-side refusal the line is still true, and
/// [`invoker_clause`] is where those words already live.
fn skill_refusal_line(invoked: &SkillInvoked, reason: &str) -> String {
    format!(
        "refused: skill {name} ({source}) — {words}",
        name = invoked.name,
        source = slash::source_words(invoked.source, invoked.shadows_user_skill),
        words = refusal_reason_words(reason),
    )
}

/// REQ-584 BR-11's hand-off: the one line that answers "where is my X repo".
///
/// Deliberately an **imperative recipe**, not a report. The user asked where
/// something is; the useful reply is the command that takes them there, and the
/// arrow is what makes it read as an offer rather than as another fact among
/// the model's prose.
fn project_handoff_line(matched: &events::ProjectMatch) -> String {
    format!("→ /cd {}  ({})", matched.name, matched.display)
}

/// BR-9's refusal line for a call that never reached a file (BUG-189).
///
/// Deliberately **shaped like** [`skill_refusal_line`] and deliberately not
/// identical: it opens with the same `refused: skill` verdict, so the two read
/// as one vocabulary, but it carries no `(source)` clause because there is no
/// file and therefore no source. Inventing one would be the hollow record this
/// event exists to avoid.
///
/// A nameless call — `invalid_arguments`, where the parse is what failed — says
/// so rather than rendering an empty gap where a name goes.
fn skill_name_refusal_line(refused: &events::SkillRefused) -> String {
    match &refused.name {
        Some(name) => format!(
            "refused: skill {name} — {}",
            refusal_reason_words(&refused.reason)
        ),
        None => format!(
            "refused: skill call — {}",
            refusal_reason_words(&refused.reason)
        ),
    }
}

/// The daemon's stable refusal id, in words a person reads (REQ-587 BR-9).
///
/// **The id keys the record; it is not the sentence.** It is the same token the
/// model is given at the head of its refusal, so the two audiences are told the
/// same fact — but `per_turn_cap` spliced into a line for a human is a token,
/// not a reason, and this client already maps every other typed outcome it
/// renders ([`not_run_words`], [`dynamic_outcome_words`], `BudgetBound::words`)
/// rather than printing the wire spelling.
///
/// # The set is open, and the unknown arm is the load-bearing one
///
/// `unknown_skill` and `invalid_arguments` reach this map through
/// [`skill_name_refusal_line`] rather than through [`skill_refusal_line`]:
/// neither names a registry row, so the daemon publishes them as
/// [`events::SkillRefused`] — a record whose subject is a *name* — instead of
/// forcing them onto `SkillInvoked`, whose subject is a file (BUG-189). They
/// were worded here before anything published them, on the reasoning that
/// "which reasons publish" is the daemon's to change and a complete map costs
/// nothing; that turned out to be exactly right, and closing BUG-189 needed no
/// change here at all. Every other id it raises does publish, and more will
/// exist than this build knows, exactly as `PermissionSubject::Unrecognized`
/// anticipates for subjects. An id this build cannot word must still produce a
/// **readable line** — BUG-186 is open against dropping an event a client does
/// not fully understand, and a refusal that renders blank is the worst of the three
/// outcomes here (worse than an awkward line, and much worse than a wrong one,
/// because nothing on the surface says the call happened at all).
///
/// So the unknown arm frames the id rather than either hiding it or emitting it
/// bare: the daemon's own word for what it did is the only information there
/// is, and quoting it inside a sentence is how a user finds the refusal in a
/// log — `refusal_line`'s rule for a request whose subject this build cannot
/// name. It is rendered rather than re-bounded: the value is daemon-authored
/// and bounded at the publish site, and `Surface::line` defuses it here, which
/// is the same two-layer treatment the skipped-skill reason already gets.
fn refusal_reason_words(reason: &str) -> String {
    match reason {
        "over_budget" => "the expansion did not fit this turn's context budget".to_owned(),
        "per_turn_cap" => "this turn has already made as many skill calls as it may".to_owned(),
        "repeated" => "the same skill and arguments were invoked twice in a row".to_owned(),
        "unknown_skill" => "this session has no skill of that name".to_owned(),
        "not_model_invocable" => "its frontmatter says `disable-model-invocation: true`".to_owned(),
        "reserved_name" => "a built-in command owns that name".to_owned(),
        "invalid_arguments" => "the call's arguments were not usable".to_owned(),
        "project_not_acknowledged" => {
            "this repository's skills have not been acknowledged for this session".to_owned()
        }
        other => format!("the daemon reported `{other}`"),
    }
}

/// One `/verbose` line for one dynamic command and what became of it.
fn dynamic_outcome_line(view: &DynamicOutcomeView) -> String {
    format!(
        "  {} — {}",
        dynamic_command_text(&view.command),
        dynamic_outcome_words(&view.outcome)
    )
}

/// A dynamic command as both surfaces spell it — the consent and the
/// `/verbose` line — in the body's own `` !`…` `` form.
///
/// One speller, because the whole point of showing the command at consent time
/// is that the user recognizes the same thing afterwards in the record.
fn dynamic_command_text(command: &str) -> String {
    format!("!`{command}`")
}

/// The typed outcome, in words (BR-6's four endings).
///
/// A projection of [`DynamicOutcome`], never a re-parse of the daemon's own
/// placeholder sentence: the daemon composes what the *model* reads and this
/// composes what the *user* reads, and a client that recovered "declined" by
/// scanning `[dynamic context not run: … — declined]` would be a second parser
/// of that sentence (LESSON-529).
fn dynamic_outcome_words(outcome: &DynamicOutcome) -> String {
    match outcome {
        DynamicOutcome::Ran {
            output_bytes,
            truncated,
        } => {
            let cut = if *truncated { ", truncated" } else { "" };
            format!("ran ({}{cut})", teton_protocol::format_bytes(*output_bytes))
        }
        DynamicOutcome::NotRun { reason } => format!("not run: {}", not_run_words(*reason)),
        DynamicOutcome::Failed {
            exit_status: Some(code),
        } => format!("failed (exit {code})"),
        DynamicOutcome::Failed { exit_status: None } => "failed (killed by a signal)".to_owned(),
        DynamicOutcome::TimedOut => "timed out".to_owned(),
        // A `kind` this build does not know (BUG-186). Naming it as unknown
        // keeps the invocation's echo line — the alternative was the whole
        // `skill_invoked` frame failing to parse and no line at all.
        DynamicOutcome::Unknown => "outcome unknown to this build".to_owned(),
    }
}

/// Why a dynamic command never started, in words — four doors, four sentences.
///
/// Distinct on purpose and asserted as such: "the user declined" and "no human
/// could be asked" are different facts about the same missing output, and BR-6
/// exists because collapsing them would tell a user their answer decided
/// something they were never asked.
fn not_run_words(reason: NotRunReason) -> &'static str {
    match reason {
        NotRunReason::Declined => "the user declined",
        NotRunReason::Level => "this session's permission level does not run them",
        NotRunReason::NoTerminal => "no human could be asked",
        NotRunReason::UnrecognizedSubject => "this client did not recognize the request",
        NotRunReason::CouldNotStart => "it could not be started",
        // A fifth door this build has no sentence for (BUG-186). The fact that
        // it did not run is still true and still worth saying; only the reason
        // is lost.
        NotRunReason::Unknown => "it did not run",
    }
}

/// The line drawn when *another* session's root moved: named, and nothing
/// else — a root this client is not in is not a root it should describe.
fn format_root_moved_elsewhere(session: &SessionId) -> String {
    format!("session root moved in another session ({session})")
}

/// The line a connection test renders for a client that did **not** run it
/// (REQ-581 BR-3).
///
/// Deliberately **terse**, and that is the difference between this and the
/// report [`crate::provider_test_ui`] prints. The client that ran the test
/// already has the whole [`teton_protocol::methods::ProviderTestResult`] — the
/// model, the dial host, the routing that follows — and renders it; this notice
/// is for the *other* clients, whose news is only what came back and where
/// health landed. A full second copy of the report would be the same facts twice
/// on the surface that ran the command, which is why the caller suppresses this
/// line for exactly one connection: the one whose own `provider/test` call is
/// out ([`SessionState::provider_test_in_flight`]).
///
/// The gate is that flag and **not** the session, which is the correction the
/// verify pass made: a second client attached to the same session is the reader
/// this notice was written for, and a session-keyed gate silenced it for them.
///
/// The outcome is worded by [`crate::provider_test_ui::outcome_sentence`], the
/// same function the report uses, because the event's `outcome` is byte-identical
/// to the RPC answer's: two renderers would be two spellings of one value for a
/// reader to find subtly different, which is what the protocol's own note on
/// nesting rather than flattening asks to avoid.
///
/// It names no key and no endpoint — the event carries neither (BR-2's rule,
/// `format_provider_setup_completed`'s audience reason). What it *may* carry is
/// the credential **reference**, inside a `reason` the daemon composed, which is
/// exactly what AC-2 asserts is safe to show.
///
/// `elsewhere` names the session the test ran in when it was not this client's,
/// and the clause is worth the branch: the bus is daemon-wide, and "the provider
/// your turns use just came back healthy" and "some other session's did" are not
/// the same news. `None` is our own session — a sibling client on the very
/// session that ran the test — where the qualification would be a lie.
fn format_provider_tested(tested: &ProviderTested, elsewhere: Option<&SessionId>) -> String {
    let whose = match elsewhere {
        Some(elsewhere) => format!(" in another session ({elsewhere})"),
        None => String::new(),
    };
    format!(
        "provider `{}` tested{whose}: {}; provider health: {}.",
        tested.provider_id,
        crate::provider_test_ui::outcome_sentence(&tested.outcome),
        crate::provider_test_ui::health_name(tested.health_after),
    )
}

/// The line a held turn renders (REQ-580 BR-5).
///
/// It says three things: the message is queued rather than lost or refused,
/// what it is waiting on — the model by name, and *which* of its two transient
/// states, branched on the event's typed value and never on a sentence — and
/// that nothing more is asked of the user: the turn runs by itself when the
/// tier opens. No countdown and no ETA (REQ-556 BR-5's rule, for the same
/// reason: the load window publishes nothing to derive one from, and a figure
/// would be fabricated). The lifecycle stream's own `benchmark` and `ready`
/// lines follow this one as they happen, and then the reply.
fn format_turn_queued(queued: &TurnQueued) -> String {
    let doing = match queued.waiting_on {
        TierWarming::Installing => "installing",
        TierWarming::Loading => "loading",
    };
    format!(
        "message queued until {} finishes {doing} — it will run as soon as the local tier opens.",
        queued.model_id
    )
}

/// The line a completed `/web setup` renders (REQ-572 BR-14, OQ-2).
///
/// It says three things, and the third is the settled answer to OQ-2: the
/// capability is on, that it was written to config rather than held in memory,
/// and that **nothing has been looked up**. No lookup is auto-offered — the flow
/// performs no egress (BR-13), and the next question that needs the web raises
/// the ordinary per-lookup consent. A notice that stopped after "enabled" would
/// leave a user expecting their last question to be answered now, which it will
/// not be.
///
/// The event's `config_path` is deliberately **not** rendered. This notice goes
/// to every open session on the machine, not only to the one that ran the
/// walkthrough, and an absolute path is a home directory and therefore a
/// username on somebody else's screen and in somebody else's scrollback. The
/// field stays on the wire — a surface that has a reason to show it still can —
/// and `teton status` is where the path a session is using is asked for.
fn format_web_setup_completed(completed: &WebSetupCompleted) -> String {
    format!(
        "web lookup enabled (`{}`) — written to your Teton config. Nothing has been looked up \
         yet: the next web-needing question will ask before anything leaves the machine.",
        web_tier_name(completed.tier),
    )
}

/// The line a refused setup call renders (REQ-572 BR-4 / AC-4).
///
/// The daemon's `origin` names a *kind* of caller and never an identity, and it
/// is rendered rather than branched on — the client's only job with it is to
/// show it. What this adds is the part the user cares about: nothing happened.
fn format_web_setup_rejected(rejected: &WebSetupRejected) -> String {
    // "tried to change": since the verify pass, this event fires only for a
    // refused COMMIT (previews refuse silently), so the one audit line the
    // user gets must describe an attempted write, not minimize it as a read.
    format!(
        "web setup refused: {} tried to change this session's web configuration, and was not \
         allowed to. Nothing was written.",
        rejected.origin
    )
}

/// The line a completed `/provider setup` renders (REQ-579 BR-15).
///
/// Four facts and no more: which id was registered, which model it is pinned
/// to, **where it will be dialed**, and what now routes to it — including the
/// honest "nothing yet" for the registered-but-unrouted outcome BR-7 permits,
/// which a line that simply omitted the tiers would leave a user guessing about.
///
/// The host is the fourth because the other three cannot answer the question a
/// bystander actually has. "`kimi` registered; `think` now routes to it" names
/// the id and never the destination, so a second client attached to this session
/// — the audience this event exists for — watched routing move and could not
/// tell where turns now go. It is the daemon's dial-time reading, a host and
/// never the endpoint, so it carries no userinfo, path or query into a
/// transcript (LESSON-529). Empty means an older daemon did not say, and the
/// clause is dropped rather than rendered blank.
///
/// It still names **no key reference**. The event carries none (BR-2), and this
/// notice reaches every session attached to the one that ran the walkthrough —
/// the same audience reason `format_web_setup_completed` keeps the config path
/// off screen.
///
/// `elsewhere` names the session when the registration was not this client's:
/// the bus is daemon-wide, and "your session's routing just changed" and "some
/// other session's did" are not the same news (`format_context_cleared`'s rule).
fn format_provider_setup_completed(
    completed: &ProviderSetupCompleted,
    elsewhere: Option<&SessionId>,
) -> String {
    let routing = if completed.bindings.is_empty() {
        "nothing routes to it yet".to_owned()
    } else {
        let tiers: Vec<&str> = completed
            .bindings
            .iter()
            .map(|binding| binding.tier.as_str())
            .collect();
        format!("`{}` now routes to it", tiers.join("`, `"))
    };
    let dialed = if completed.dial_host.is_empty() {
        String::new()
    } else {
        format!(", dialed at `{}`", completed.dial_host)
    };
    let whose = match elsewhere {
        Some(session) => format!(" in another session ({session})"),
        None => String::new(),
    };
    format!(
        "provider `{}` registered{whose} (model `{}`{dialed}) — {routing}.",
        completed.provider_id, completed.model,
    )
}

/// The line a refused provider-setup commit renders (REQ-579 BR-12).
///
/// The daemon names the *method* and never the caller, and it is rendered rather
/// than branched on. What this adds is the part the user cares about: nothing
/// was written, and no key was stored.
///
/// `elsewhere` for [`format_provider_setup_completed`]'s reason, with a sharper
/// edge: this line accuses something of trying to change the machine's
/// configuration, and an unqualified copy of it on every attached client tells
/// each of them that *their* session was the target.
fn format_provider_setup_rejected(
    rejected: &ProviderSetupRejected,
    elsewhere: Option<&SessionId>,
) -> String {
    match elsewhere {
        Some(session) => format!(
            "provider setup refused in another session ({session}): something that is not that \
             session's user tried to run `{}`, and was not allowed to. Nothing was written and no \
             key was stored.",
            rejected.method
        ),
        None => format!(
            "provider setup refused: something that is not this session's user tried to run `{}`, \
             and was not allowed to. Nothing was written and no key was stored.",
            rejected.method
        ),
    }
}

/// The verbose line a capability dead end renders (REQ-572, architecture ADR-4).
fn format_capability_dead_end(dead_end: &CapabilityDeadEnd) -> String {
    format!(
        "capability dead end: `{}` is not configured on this machine, so the turn had nowhere \
         to go.",
        dead_end.capability
    )
}

/// Put an attach/monitor consent question to the user and build the answer
/// (REQ-570 BR-4, AC-4).
///
/// Always returns an answer. There is deliberately no "no reply" arm: an
/// unanswered consent costs the requester the daemon's full 30-second window,
/// and a client that has decided to decline should say so immediately rather
/// than make a user wait out a timeout for a decision already taken.
///
/// # It never auto-answers, and that is the point
///
/// Every other decision surface in this client has some form of "yes to
/// everything": `--yes` accepts a model proposal, a session grant auto-allows a
/// tool. **None of them may reach this one.** They are all consent to *the
/// user's own* pending action; this is consent to admit a **different
/// connection** into the user's session, and a flag the user set once to skip
/// download prompts must never become standing authority to hand their session
/// to whatever asks for it next.
///
/// So there is deliberately no `auto_accept` parameter here to wire one into.
/// A non-interactive invocation — piped stdin, no TTY, EOF — **declines**, and
/// says so on screen. Silence is not consent, and the fail-closed direction is
/// the one that refuses.
///
/// The daemon does not rely on this: it runs its own OS presence check before
/// minting anything (REQ-570 BR-1), so a client that lied here would still be
/// refused. This is the surface being honest, not the control.
pub fn resolve_attach_consent(
    request: &AttachConsentRequested,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
) -> AttachConsentParams {
    // The question first, as a notice: it is news whether or not the user is in
    // a position to answer, and it must be legible before the prompt line.
    surface.line(LineKind::Notice, &format_attach_consent_notice(request));

    let question = match request.scope {
        ConsentScope::Attach => "Allow this client to attach to your session? [y/N] ",
        // Deliberately a different sentence, not the same one with a noun
        // swapped: a monitor grant is sight of *every* session on the machine,
        // and a user skimming a familiar prompt would answer the smaller
        // question they have answered before.
        ConsentScope::Monitor => "Allow this client to watch EVERY session on this daemon? [y/N] ",
    };

    let Some(answer) = prompter.ask(question) else {
        // EOF — a pipe, a redirect, no TTY. Nobody is there to consent.
        surface.line(
            LineKind::Notice,
            "no interactive input available, so the request was declined. \
             Answer it from an interactive `teton` session.",
        );
        return AttachConsentParams {
            request_id: request.request_id.clone(),
            outcome: AttachConsentOutcome::Denied,
        };
    };

    // Default-deny on anything that is not an explicit yes, which is the
    // opposite of `confirm_model`'s empty-is-yes. The asymmetry is deliberate:
    // there the default action is the one the user asked for, here it is
    // admitting somebody else.
    let granted = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
    surface.line(
        LineKind::Prompt,
        if granted {
            "granted — the daemon will ask you to confirm you are present."
        } else {
            "denied."
        },
    );

    AttachConsentParams {
        request_id: request.request_id.clone(),
        outcome: if granted {
            AttachConsentOutcome::Granted
        } else {
            AttachConsentOutcome::Denied
        },
    }
}

/// The one-line notice an `attach_consent_requested` event draws (REQ-569 BR-6).
///
/// It says what would be granted rather than repeating the wire scope name,
/// because the two scopes are wildly different asks and a user deciding between
/// them needs the difference in the sentence.
///
/// `requester` is a peer-chosen string. The daemon already bounds it and strips
/// its control characters before publishing, which is where that has to happen —
/// this is one renderer of several, and a guard that lived here would protect
/// only this one.
pub fn format_attach_consent_notice(request: &AttachConsentRequested) -> String {
    let what = match request.scope {
        ConsentScope::Attach => "attach to this session",
        ConsentScope::Monitor => "watch every session on this daemon",
    };
    format!("{} asked to {what}.", request.requester)
}

/// The one-line notice a `session_grant_minted` event draws (REQ-569 verify,
/// F6).
///
/// **Every grant names who approved it** (REQ-569 re-verify, R1). The first cut
/// branched on `self_approved` alone and rendered everything else as a bare "the
/// daemon granted … permission to attach" — which is precisely the benign
/// reading an attacker holding two connections earns for free, since having its
/// first connection approve its second sets `self_approved: false`. A reader
/// cannot act on a flag that is false in both the good case and the bad one, so
/// the notice states the relation instead: who asked, and who answered.
///
/// Three shapes, and the middle one is why this exists:
///
/// - self-approved — nobody was attached, so the requester answered its own
///   prompt. The accepted ADR-A residual, named as what happened.
/// - approved by a connection giving the **same name** as the requester. Said
///   plainly and without a verdict: two honest clients may well spell themselves
///   identically, and the daemon cannot tell that from one actor working both
///   ends. The reader is handed the coincidence, not a conclusion.
/// - approved by a differently-named connection — the ordinary case, which still
///   names the approver rather than leaving "somebody" implied.
///
/// Both descriptors are peer-chosen text the daemon already bounded and
/// stripped; [`format_attach_consent`]'s note applies to each unchanged. A
/// trailing clause reports announcements the daemon's own rate limit dropped
/// (R3), so a quieted burst is still legible as a burst.
fn format_grant_minted(minted: &SessionGrantMinted) -> String {
    let what = match minted.scope {
        ConsentScope::Attach => "attach to a session",
        ConsentScope::Monitor => "watch every session on this daemon",
    };
    let line = if minted.self_approved {
        format!(
            "the daemon granted {} permission to {what} — approved by the \
             connection that asked, because nothing was attached to that \
             session.",
            minted.requester
        )
    } else if minted.approver == minted.requester {
        format!(
            "the daemon granted {} permission to {what} — approved by a second \
             connection giving that same name. Both sides of this decision call \
             themselves {}.",
            minted.requester, minted.approver
        )
    } else {
        format!(
            "the daemon granted {} permission to {what} — approved by {}.",
            minted.requester, minted.approver
        )
    };
    match minted.suppressed {
        0 => line,
        1 => format!(
            "{line} (1 further grant announcement was held back by the daemon's rate limit.)"
        ),
        n => format!(
            "{line} ({n} further grant announcements were held back by the \
             daemon's rate limit.)"
        ),
    }
}

/// The session an event belongs to when it is **not** the one this client is in
/// — `None` when it is ours, or when either side is unknown.
///
/// Unknown counts as ours on purpose. A client that has not yet learned its own
/// session id, or an event that names none, gives no evidence that the event
/// came from elsewhere, and a notice that guessed "in another session" would be
/// wrong in precisely the common case (a single-session client, before
/// `session/create` answers).
fn other_session<'a>(
    ours: Option<&SessionId>,
    theirs: Option<&'a SessionId>,
) -> Option<&'a SessionId> {
    match (ours, theirs) {
        (Some(ours), Some(theirs)) if ours != theirs => Some(theirs),
        _ => None,
    }
}

/// The one-line notice a `context_cleared` event draws (REQ-567 BR-8).
///
/// The count is stated even when it is zero, and singular/plural is worth the
/// branch: "cleared 0 blocks" and "there was nothing to clear" are the same
/// fact, and only the second reads as an answer to a command the user just
/// typed.
///
/// `elsewhere` names the session when the clear was not this client's — the
/// difference between "your next prompt starts from nothing" and "some other
/// session's does", which is not a nuance the reader can recover from a line
/// that omits it.
fn format_context_cleared(blocks_dropped: u64, elsewhere: Option<&SessionId>) -> String {
    let what = match blocks_dropped {
        0 => "there was nothing retained to drop".to_owned(),
        1 => "1 retained block dropped".to_owned(),
        n => format!("{n} retained blocks dropped"),
    };
    match elsewhere {
        Some(session) => format!("context cleared in another session ({session}); {what}."),
        None => format!("context cleared; {what}."),
    }
}

/// The one line a `context_pressure` event draws (REQ-586 BR-7).
///
/// Pure, and the only place the three shapes are worded, so the never-gated
/// rendering above and the tests over it read the same sentence — the
/// [`format_context_cleared`] arrangement, for a sibling fact.
///
/// Every arm names the budget that was fitted to **and** what bound it, in that
/// order, because the two answer different questions: the number says how much
/// room the turn had, the bound says which knob would change it (BR-8 — the
/// bound is read off the event, never re-derived here). The words are the
/// bound's, spelled for a person rather than as the wire's snake_case.
fn format_context_pressure(pressure: &ContextPressure) -> String {
    let budget = format!("{}-word budget", thousands(pressure.budget_tokens));
    let bound = bound_clause(pressure.bound, pressure.bound_floored);
    match pressure.kind {
        ContextPressureKind::BlocksDropped => format!(
            "context: {} dropped to fit the {budget} {bound}",
            older_blocks(pressure.dropped_blocks)
        ),
        // Which block it was is the whole point of the distinction: the newest
        // user block is the case where the model answers a prompt the user did
        // not send (BR-7), and it is the one the daemon additionally reports as
        // a turn notice. An older one is a smaller fact and says so.
        ContextPressureKind::BlockElided => format!(
            "context: {} middle-elided by {} to fit the {budget} {bound}",
            if pressure.newest_user_elided {
                "newest message"
            } else {
                "an older message"
            },
            bytes_figure(pressure.elided_bytes),
        ),
        // A reroute moved the turn to a route with a different budget, so the
        // retained conversation was re-fitted to it. The drop count trails
        // rather than leads because the news is the re-fit; the count is how
        // much it cost, and it is stated even when it is zero — "nothing
        // dropped" is the reassurance, and its absence would read as an
        // unfinished sentence.
        ContextPressureKind::RefitOnReroute => format!(
            "context: re-fitted to the {budget} after a reroute {bound} — {}",
            match pressure.dropped_blocks {
                0 => "nothing dropped".to_owned(),
                n => format!("{} dropped", older_blocks(n)),
            }
        ),
        // The gate ran and could not finish the job: it will neither drop its
        // last block nor clamp it to nothing, so the turn goes out over budget.
        // The other three lines all end "to fit the …", and this one must not —
        // it is the case where the fitting failed (TASK-194 2a). What the gate
        // *did* manage trails the fact that it was not enough.
        ContextPressureKind::DidNotFit => format!(
            "context: could not be fitted to the {budget} {bound} — the turn was sent over \
             budget{}",
            match (pressure.dropped_blocks, pressure.elided_bytes) {
                (0, 0) => String::new(),
                (0, bytes) => format!(" after eliding {}", bytes_figure(bytes)),
                (blocks, 0) => format!(" after dropping {}", older_blocks(blocks)),
                (blocks, bytes) => format!(
                    " after dropping {} and eliding {}",
                    older_blocks(blocks),
                    bytes_figure(bytes)
                ),
            }
        ),
        // REQ-588 BR-4: a kind this build does not know. It says the true
        // part — the context was changed to fit the budget — and does not
        // invent the part it cannot know. Every other arm names WHAT happened;
        // this one deliberately does not, because a guess here would be the
        // mis-rendering `DidNotFit`'s doc calls worse than silence.
        ContextPressureKind::Unknown => {
            format!(
                "context: adjusted to fit the {budget} {bound} (this build does not recognise how)"
            )
        }
    }
}

/// `(bound: user cap)`, or the same with the floor named when the bound could
/// not be honored (REQ-586 BR-8, TASK-194 2b).
///
/// One clause, read by the `/verbose` route line and by every pressure line,
/// for the reason [`bound_words`] is one table: the bound is one fact with one
/// source, and a budget that is *larger* than the bound the same line names is
/// the one place a reader would conclude the surface is broken. The floor is
/// the smallest budget that can still hold the harness's own system prompt; a
/// window or cap deriving below it is raised to it, so the declaration is
/// recorded but not in force.
///
/// The daemon decides `floored` where it derives the budget — this never
/// compares the pair against a floor of its own (BR-8, AC-12).
fn bound_clause(bound: BudgetBound, floored: bool) -> String {
    if floored {
        return format!(
            "(bound: {} — floored: below the smallest budget that holds the system prompt)",
            bound_words(bound)
        );
    }
    format!("(bound: {})", bound_words(bound))
}

/// `1 older block` / `3 older blocks` — the phrase every pressure line counts
/// with, so singular and plural are decided once.
fn older_blocks(blocks: u64) -> String {
    match blocks {
        1 => "1 older block".to_owned(),
        n => format!("{} older blocks", thousands(n)),
    }
}

/// The words a [`BudgetBound`] is said in.
///
/// The wire spelling is `default_unknown`; a person reading a turn's line is
/// told `unknown window`, which names the thing they would go and set. One
/// table, read by the route line and by every pressure line — the bound is one
/// fact with one source (BR-8), and two tables of adjectives for it would be
/// the mirrored-predicate shape LESSON-528 is about.
///
/// The table itself is [`BudgetBound::words`], in the protocol crate, because
/// the daemon words the same bound in its refusals (REQ-585 BR-8) and cannot
/// reach into this one. This function stays as the name the rendering here
/// reads and as a `fn` item `map` can be handed.
fn bound_words(bound: BudgetBound) -> &'static str {
    bound.words()
}

/// A count with thousands separators: `4096` → `4,096`.
///
/// Budgets are five- and six-digit numbers that a reader compares at a glance
/// ("did that turn really only get 4k?"), and an ungrouped `132650` is the one
/// shape that cannot be read at a glance.
fn thousands(n: u64) -> String {
    events::thousands(n)
}

/// A byte figure for a budget line: `900 B`, `33 KB`, `4.2 MB`.
///
/// Named for what it *is* rather than for its first caller: `budget_bytes` is
/// the wire field's name (and one call site here hands it `elided_bytes`, which
/// is not a budget at all), so a formatter wearing it read as an accessor.
///
/// **Decimal** units, and labelled as such. `firstrun`'s [`firstrun::format_bytes`]
/// is the other byte formatter in this crate and stays where it is: it renders
/// an *exact* download size in the binary units the daemon's own sentences use,
/// where the tenth of a GiB is a fact about a file. A budget is an approximation
/// with a safety ratio already baked into it, so it is rounded to whole KB and
/// never claims a precision the number has not got — and rounding a 1024-based
/// number under a `KB` label is the exact confusion that formatter's doc warns
/// about, which is why this one divides by 1000.
fn bytes_figure(bytes: u64) -> String {
    events::bytes_figure(bytes)
}

/// The one-line verbose notice a `prefix_cache` event draws.
fn format_prefix_cache(cache: &PrefixCache) -> String {
    match &cache.outcome {
        PrefixCacheOutcome::Hit {
            cached_tokens,
            new_tokens,
            divergent,
        } => {
            // A divergent hit says why the prefill was bigger than the turn's
            // delta: history was rewritten past the reuse point, and the
            // rewritten tail was re-prefilled (BR-2 as amended).
            let note = if *divergent {
                " after a history change"
            } else {
                ""
            };
            format!(
                "context: reused {cached_tokens} tokens{note}, prefilled {new_tokens} ({})",
                cache.model
            )
        }
        PrefixCacheOutcome::Miss {
            reason,
            processed_tokens,
        } => {
            // The reason is spelled out rather than folded into "cache miss":
            // `divergent` means history was rewritten, `session_switch` means
            // another session took the slot, and a user chasing latency needs
            // to tell those apart (BR-8).
            let reason = match reason {
                PrefixCacheMiss::Cold => "no resident context",
                PrefixCacheMiss::SessionSwitch => "another session held the context",
                PrefixCacheMiss::Divergent => "conversation history changed",
                PrefixCacheMiss::Evicted => "context was released",
            };
            format!("context: prefilled {processed_tokens} tokens — {reason}")
        }
        PrefixCacheOutcome::Evicted { reason } => {
            let reason = match reason {
                EvictionReason::MemoryPressure => "memory pressure",
                EvictionReason::EngineUnload => "model unloaded",
                EvictionReason::GenerationFailed => "a generation failed",
            };
            format!("context: released — {reason}")
        }
    }
}

/// The notice a `web_lookup` draws, or `None` when it is quiet chrome.
///
/// The split is which endings a user needs to know about without asking:
///
/// - a **completed** or **cache-served** lookup is routine, and its per-lookup
///   line (host + outcome) is diagnostic chrome behind the same `verbose` flag
///   the routing notices use (BR-7's `/verbose` clause);
/// - every **refusal, block, and unreachable host** always renders. Each one is
///   a thing the model asked for and did not get, so the answer that follows was
///   composed without it — BR-13's "never silent" rule, and BR-9's for offline.
fn format_web_lookup(lookup: &WebLookup, verbose: bool) -> Option<String> {
    let routine = matches!(
        lookup.outcome,
        WebLookupOutcome::Completed | WebLookupOutcome::CacheHit
    );
    if routine && !verbose {
        return None;
    }
    let kind = match lookup.kind {
        WebLookupKind::Fetch => "fetch",
        WebLookupKind::Search => "search",
    };
    // The taint refusal is the one ending that gets a sentence rather than a
    // phrase, because BR-13 requires it to name both the cause and the effect.
    if lookup.outcome == WebLookupOutcome::TaintRestricted {
        return Some(format!("web {kind} {} — {TAINT_RESTRICTION}", lookup.host));
    }
    let ending = match lookup.outcome {
        WebLookupOutcome::Completed => format!("completed ({} bytes)", lookup.bytes_in),
        WebLookupOutcome::CacheHit => format!(
            "served from the local cache, nothing left this machine ({} bytes)",
            lookup.bytes_in
        ),
        WebLookupOutcome::BlockedPrivacy => {
            "blocked: the outgoing text derived from privacy-boundary content".to_owned()
        }
        // BR-14's honesty half. `blocked_redact` folds two facts that send a
        // user to two different places, and the event's `cause` is what tells
        // them apart: a scan that *ran* and refused the text, and a scan that
        // could not run at all. The second is the ordinary state of a build with
        // no local model loaded — and told the first, a user goes hunting for a
        // secret in a query that contained none while the actual fix is never
        // named.
        WebLookupOutcome::BlockedRedact => match lookup.cause {
            Some(BlockCause::ScanUnavailable) => scan_unavailable(lookup.kind).to_owned(),
            _ => "blocked: the redaction scan refused the outgoing text".to_owned(),
        },
        WebLookupOutcome::RefusedDomain => {
            "refused: outside the configured `[web] allowed_domains`".to_owned()
        }
        WebLookupOutcome::RefusedTier => {
            "refused: above the `[web] tier` this machine granted".to_owned()
        }
        // Handled above — kept as its own arm so a reordering cannot silently
        // drop the sentence and fall back to a phrase.
        WebLookupOutcome::TaintRestricted => TAINT_RESTRICTION.to_owned(),
        WebLookupOutcome::Offline => "unavailable: offline".to_owned(),
    };
    Some(format!("web {kind} {} — {ending}", lookup.host))
}

/// What a `blocked_redact` whose cause is `scan_unavailable` actually means
/// (REQ-563 BR-14).
///
/// The search tier exists **because** every query is scanned first, and the scan
/// is pinned to the local model — so a machine with no local model loaded has no
/// scanner, and BR-14's coupling turns that into a refusal rather than an
/// unscanned query leaving. That is the capability working exactly as specified,
/// and the sentence has to say so: the previous wording ("the redaction scan
/// refused the outgoing text") described a scan that ran and found something,
/// which is a different problem with a different fix, and sent the user looking
/// for a secret in a query that contained none.
///
/// One function, like [`TAINT_RESTRICTION`] beside it, so the copy cannot drift
/// between this surface and any other that has to state the same thing.
///
/// **Two sentences, because the two tiers have two different remedies.** A
/// search is coupled to the scan unconditionally (BR-14: the tier exists because
/// every query is scanned), so the only way out is to make the scanner
/// available. A *fetch* is scanned at provider parity (BR-2), under `[privacy]
/// redact` — so a user who does not want that coupling has a switch, and telling
/// them the search story would send them to install a model they may not need.
fn scan_unavailable(kind: WebLookupKind) -> &'static str {
    match kind {
        WebLookupKind::Search => {
            "blocked: web search scans every query before it leaves, and that scan runs on \
             the local model — which is not loaded on this machine, so the query was not \
             sent. Install or start the local model to use search."
        }
        WebLookupKind::Fetch => {
            "blocked: `[privacy] redact` scans the outgoing URL before it leaves, and that \
             scan runs on the local model — which is not loaded on this machine, so nothing \
             was sent. Install or start the local model, or turn `[privacy] redact` off."
        }
    }
}

/// The BR-13 restriction sentence: the cause, then the effect, then the way out.
///
/// Both halves are required by the rule and neither is inferable from the other
/// — "boundary content was read" without the consequence reads as an FYI, and
/// "web lookup disabled" without the cause reads as a bug. Kept as one constant
/// so the copy cannot drift between the per-lookup line and any other surface
/// that has to state it.
const TAINT_RESTRICTION: &str = "restricted: this session read privacy-boundary content, so \
                                 model-composed web lookups (searches, and URLs the model \
                                 chose) are disabled for the rest of it. URLs you paste \
                                 yourself still work; `/web allow` lifts the restriction for \
                                 this session.";

/// The notice a `web_consent_decided` draws.
fn format_web_consent(decided: &WebConsentDecided) -> String {
    let tier = web_tier_name(decided.tier);
    match (decided.granted, decided.scope) {
        (true, WebConsentScope::Once) => format!("web consent: `{tier}` allowed for this lookup"),
        (true, WebConsentScope::Session) => {
            format!("web consent: `{tier}` allowed for the rest of this session")
        }
        // The only answer that changed a file, and it says so: BR-4 makes this
        // the sole consent path that writes config, and a user who picked it
        // should not have to check the file to learn whether it took.
        //
        // The key named here is the key that was written. It used to name `[web]
        // tier`, which is the raise-only ceiling and is a no-op for every prompt
        // a user can actually reach; the durable effect is the per-tier consent
        // list, and naming the wrong key sent anyone who went looking to a line
        // that had not changed. The `+=` is deliberate: this answer adds one
        // tier and leaves the other two asking (BR-3).
        (true, WebConsentScope::Persistent) => format!(
            "web consent: `{tier}` enabled permanently — written to your config as \
             `[web] permission_allow += \"{tier}\"`"
        ),
        (false, WebConsentScope::Session) => {
            format!("web consent: `{tier}` declined for the rest of this session")
        }
        (false, _) => format!("web consent: `{tier}` declined"),
    }
}

/// The notice a `web_taint_overridden` draws.
fn format_web_taint_overridden(tiers: &[WebTier]) -> String {
    if tiers.is_empty() {
        return "web taint restriction lifted for this session; no tiers were granted to \
                restore."
            .to_owned();
    }
    let named = tiers
        .iter()
        .map(|t| format!("`{}`", web_tier_name(*t)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "web taint restriction lifted for this session; model-composed lookups resume at: \
         {named}."
    )
}

/// A tier's config spelling, for a line that has to name one.
///
/// The same vocabulary the daemon writes to `[web] tier` — a user reading this
/// notice and then their config must see one word, not two. Which is exactly why
/// there is one of these and it is shared with [`crate::slash`]: two copies of a
/// vocabulary are two places for it to drift, and the drift would show up as the
/// event stream and the command output naming the same tier differently.
pub(crate) fn web_tier_name(tier: WebTier) -> &'static str {
    match tier {
        WebTier::Off => "off",
        WebTier::FetchUserUrl => "fetch_user_url",
        WebTier::FetchAnyUrl => "fetch_any_url",
        WebTier::Search => "search",
    }
}

/// Render a streaming turn update.
///
/// `session` is the envelope's — whose turn this chunk belongs to. The bus is
/// daemon-wide, so it is not necessarily this client's.
fn render_session_update(
    update: &SessionUpdatePayload,
    session: Option<&SessionId>,
    surface: &mut dyn Surface,
    state: &mut SessionState,
) {
    match update {
        SessionUpdatePayload::AgentMessageChunk { text } => {
            // The one writer of the turn accumulator (REQ-579 ADR-9). It is fed
            // from the same chunk that reaches the screen, so what the hand-off
            // check reads is exactly what the user was shown — not a second
            // reading of the turn taken somewhere else.
            //
            // ...but only for **this** session's turn. The accumulator is read
            // once per typed prompt of ours, so a chunk from another session
            // would not be a line drawn in the wrong place: it would be another
            // session's words deciding whether our next prompt earns a notice,
            // and words we never showed this user at that. `other_session`'s
            // "unknown counts as ours" is deliberate and is the same reading
            // `context_cleared` takes — a client that has not yet learned its
            // own id has no evidence a chunk came from elsewhere, and the
            // single-session case is the common one.
            if other_session(state.session_id.as_ref(), session).is_none() {
                state.turn_reply.push_str(text);
            }
            surface.fragment(text);
        }
        SessionUpdatePayload::ToolCall {
            tool_call_id,
            title,
            status,
        } => {
            state
                .tool_titles
                .insert(tool_call_id.clone(), title.clone());
            // REQ-581 ADR-4's second reader of the turn. Recorded from the same
            // payload that reaches the screen, and only for **our** session, for
            // the reasons the reply accumulator above states: what the predicate
            // reads must be what the user was shown, and another session's tool
            // calls must not arm this session's line.
            //
            // The title, not the raw arguments, because the title is what the
            // daemon composed (`shell: <command>`) and what the user read — a
            // second reading of the arguments here would be a second opinion
            // about what the turn did (LESSON-456).
            if other_session(state.session_id.as_ref(), session).is_none() {
                state.turn_tools.push(title.clone());
            }
            surface.line(
                LineKind::Tool,
                &format!("{title} [{}]", status_label(*status)),
            );
        }
        SessionUpdatePayload::ToolCallUpdate {
            tool_call_id,
            status,
        } => {
            let title = state
                .tool_titles
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| tool_call_id.clone());
            surface.line(
                LineKind::Tool,
                &format!("{title} [{}]", status_label(*status)),
            );
        }
        SessionUpdatePayload::Diff {
            path,
            old_text,
            new_text,
        } => render_diff(path, old_text.as_deref(), new_text, surface),
        SessionUpdatePayload::Plan { entries } => {
            surface.line(LineKind::Info, "plan:");
            for entry in entries {
                surface.line(
                    LineKind::Info,
                    &format!("  [{:?}] {}", entry.status, entry.content),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The `/provider setup` hand-off (REQ-579 ADR-9)
// ---------------------------------------------------------------------------
//
// Three live rounds against the shipped local model proved it will not
// volunteer the in-session command from the guide (verification.md §1–§24):
// 0/9 replies named it, while the endpoint and model transferred every time.
// So the guarantee moves off the prompt and onto the surface. When a turn's
// reply reached for the *shell* recipe, the harness — in its own voice, not the
// model's — says the session has a command for it.
//
// Everything here is deliberately dumb: a substring match on text the model
// already emitted, no model call, no daemon round-trip, no state beyond the
// turn. It fires only when the model reached for the CLI, so a session that
// never discusses providers never sees it, and a model that one day volunteers
// the command makes it dormant with no code change.

/// The shell recipes whose appearance means the model answered with the
/// out-of-session path.
///
/// These are Teton's own command names, which a reply can only have got from
/// the bundled guide or the recipe catalog — which is what keeps the match
/// honest rather than a guess at intent. Case-sensitive: the commands are typed
/// in lowercase and a capitalised prose mention ("Teton Provider Add") is not a
/// recipe.
const PROVIDER_CLI_RECIPES: [&str; 2] = ["teton provider add", "teton policy set-tier"];

/// The in-session command the hand-off names.
///
/// A reply that names it **and recites no shell recipe** makes the hand-off
/// dormant: the model volunteered the command, so repeating it would be the
/// harness talking over an answer that was already right (ADR-9).
///
/// The "and" is load-bearing, and was not there at first. Presence alone made
/// the line suppressible by a reply that named the command *and* told the user
/// to paste their key into the chat — one sentence that mentions
/// `/provider setup` and the harness's only correction goes quiet, on the exact
/// turn it is most needed. A reply that offers both paths has still pointed at
/// the CLI and still earns the correction.
const GUIDED_COMMAND: &str = "/provider setup";

/// ADR-9's sentence, and the only thing this module prints for it.
///
/// Plain text with no escape of its own (LESSON-517: styling belongs to
/// [`LineKind`], never to the caller's string), and shaped by BUG-168's rules —
/// stated outright, imperative, one sentence, no em-dash aside. The model may
/// quote it back verbatim, which is a further reason it has to read as an
/// instruction rather than as an aside about one.
const HAND_OFF_LINE: &str =
    "in this session, /provider setup <vendor> [tier] does this without leaving it; no key in chat.";

/// The reply with its markdown fences taken out.
///
/// A model that has read the guide reproduces the command inside markdown and
/// does not always put the fence around the whole of it — `` `teton` provider
/// add `` and `` `teton provider add` `` are the same answer, and only one of
/// them survives a naive `contains`. Stripping the character is cheaper and
/// more predictable than teaching the matcher markdown.
///
/// It exists as its own function because **both** halves of ADR-9's predicate
/// have to be asked of the same characters. When only the recital half stripped
/// backticks, `` `/provider setup` `` and `/provider setup` were two different
/// answers to the dormancy question, which is precisely the kind of asymmetry
/// nobody notices until a reply lands in the gap.
fn without_backticks(text: &str) -> String {
    text.chars().filter(|c| *c != '`').collect()
}

/// True when the reply reached for one of the shell recipes.
///
/// Takes text whose backticks are **already gone** — see [`without_backticks`]
/// for why that is the caller's job and not this function's: the stripping
/// happens once, above, so that this half and the dormancy half below cannot be
/// asked about different characters.
fn recites_provider_cli(plain: &str) -> bool {
    PROVIDER_CLI_RECIPES
        .iter()
        .any(|recipe| plain.contains(recipe))
}

/// ADR-9's predicate, over one backtick-stripped reading of the turn.
///
/// Stated as the ADR states it, in two named terms rather than as the one term
/// they reduce to. The reduction is real — dormancy can only fire on a reply
/// that recites nothing, and such a reply never passes `recites` — and that
/// *is* the fix: dormancy is the complement of the recital, not a second,
/// independent veto over it. Writing it out is what keeps a later edit to
/// either half from restoring the veto by accident.
fn earns_hand_off(plain: &str) -> bool {
    let recites = recites_provider_cli(plain);
    let dormant = plain.contains(GUIDED_COMMAND) && !recites;
    recites && !dormant
}

/// The one sentence the hand-off prints.
pub(crate) fn hand_off_line() -> &'static str {
    HAND_OFF_LINE
}

// ---------------------------------------------------------------------------
// The `/provider test` hand-off (REQ-581 ADR-4)
// ---------------------------------------------------------------------------
//
// A second line through the same seam, for a failure the one above cannot see.
// Asked "can you test the Kimi connection?", the shipped local model does not
// recite a recipe — it *runs* `teton provider list` and `teton policy show`
// through the shell tool, reads registration as connectivity, and answers that
// the provider is fine without a byte having left the machine. The reply text
// the REQ-579 nudge reads recites nothing, so nothing fires.
//
// So this predicate keys on the **turn**: what the user asked, and what the
// turn did. Everything else is unchanged — a substring match on text the
// session already has in hand, no model call, no daemon round-trip, at most one
// line per turn, TTY only.
//
// The word lists below are v1 heuristics and are labelled as such deliberately
// (LESSON-532): the deterministic half of AC-8b is the unit table in this
// module, and the claim that the line fires on the phrasings a real user types
// is only made after the live A/B recorded in `docs/manual-verification.md`.

/// The verbs a connection question is asked with — half of ADR-4's predicate.
///
/// Matched case-insensitively and **as whole words** ([`contains_word`]). The
/// substring reading this list started with was too wide to mean anything: it
/// fired on "the la*test* run", "con*test*", "*reach*" inside "research". Whole
/// words cost the inflections a substring got for free, so the ones that were
/// being relied on are listed rather than implied — `reachable` beside `reach`.
///
/// A v1 word list, not a classifier: the cost of a miss is one line that does
/// not print, and the cost of a hit on prose that merely uses a verb is nothing
/// at all, because the other three conditions still have to hold.
const CONNECTION_VERBS: [&str; 7] = [
    "test",
    "check",
    "verify",
    "working",
    "connected",
    "reach",
    "reachable",
];

/// The subjects that make such a question about a *provider* — the other half.
///
/// The registered provider ids join this list at match time
/// ([`SessionState::provider_ids`]), which is what lets "is kimi working?" count
/// without a vendor list hard-coded here. These four are the fallback, and they
/// carry the common phrasings on their own: the screenshot's "test the Kimi
/// connection" matches on `connection` even when no snapshot has been read.
const CONNECTION_SUBJECTS: [&str; 4] = ["provider", "connection", "connectivity", "api"];

/// The diagnostics a reply reaches for when it answers the question by
/// inspecting configuration instead of dialling.
///
/// All three report what the machine is *configured* to do; none of them sends
/// anything. That is precisely the confusion this line exists to correct, and it
/// is why `teton provider` is listed as a prefix — `list`, `show` and a bare
/// mention are the same mistake.
const PROVIDER_DIAGNOSTICS: [&str; 3] = ["teton provider", "teton policy", "teton doctor"];

/// The two spellings of the **right** answer (BR-6): the in-session command and
/// its shell form.
///
/// A reply that named either has answered the question, and the harness stays
/// quiet — [`HAND_OFF_LINE`]'s dormancy rule. Both are needed because both are
/// correct: `/provider test kimi` is what a user types here, and `teton provider
/// test kimi` is the non-interactive answer this REQ shipped alongside it. A
/// list that knew only the slash form printed the nudge at a reply that had
/// already given the user the right command, which is the shape of correction a
/// reader learns to ignore.
///
/// The shell form is also why [`improvised_a_probe`] cannot read
/// [`PROVIDER_DIAGNOSTICS`] naively: `teton provider test` starts with `teton
/// provider`, so the correct answer looked exactly like the mistake.
const CONNECTION_COMMANDS: [&str; 2] = ["/provider test", "teton provider test"];

/// ADR-4's sentence, and the only thing this half prints.
///
/// Shaped like [`HAND_OFF_LINE`] and for the same reasons: plain text with no
/// escape of its own (LESSON-517), stated outright rather than as an aside about
/// an instruction (BUG-168), because the model may quote it back. It says what
/// the command *does* — one call, and what came back — since the failure it
/// follows is a turn that reported a connection it never made.
const CONNECTION_TEST_LINE: &str = "in this session, /provider test <id> makes one consented call \
                                    and reports what came back; that is the connection test.";

/// The one sentence the connection hand-off prints.
pub(crate) fn connection_test_line() -> &'static str {
    CONNECTION_TEST_LINE
}

/// True when `needle` occurs in `haystack` as a whole word.
///
/// "Whole" means the byte on each side is not ASCII alphanumeric, so `test`
/// matches "test the connection" and "test?" and not "latest" or "contest", and
/// a short provider id cannot match the middle of a longer word. Both arguments
/// are expected lowercase; the caller lowercases once rather than per candidate.
///
/// A hand-rolled scan rather than a regex, and rather than a token set, for two
/// reasons: this crate has no regex dependency and one word list does not earn
/// it, and the needles are not all single tokens — `teton provider` is a needle
/// with a space in it, which a token set cannot express. Non-ASCII neighbours
/// count as boundaries, which is the right answer for the only case that arises
/// (a provider id beside punctuation or CJK text) and costs nothing elsewhere.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let open = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let close = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if open && close {
            return true;
        }
        // Past the first *character* of this match, so the walk stays on a UTF-8
        // boundary even for a needle that is not ASCII.
        from = start + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// True when the user's prompt reads as a question about whether a provider
/// works (ADR-4).
///
/// Both halves must hold: a verb of testing **and** a provider-shaped subject,
/// where the subjects are [`CONNECTION_SUBJECTS`] plus whatever ids the config
/// snapshot reported. "check the tests pass" carries a verb and no subject;
/// "which provider is on think?" carries a subject and no verb; neither is this
/// question.
///
/// Every match is on a whole word ([`contains_word`]). Substring matching made
/// both halves fire on prose that was not about a connection at all — "run the
/// la**test** provider **test**s" carried a verb twice over — and it made an id
/// dangerous in proportion to how short it was: a two-character id matched
/// inside any word containing those two letters. Hence the length floor as well:
/// an id of one or two characters is not distinctive enough to be read as the
/// subject of a sentence, so it contributes nothing rather than everything.
///
/// Case-insensitive, because a typed prompt is prose — unlike the reply-side
/// match below, which is case-sensitive precisely because a *command* is
/// lowercase and a capitalised mention of one is not a command.
fn asks_about_a_connection(prompt: &str, provider_ids: &[String]) -> bool {
    let asked = prompt.to_lowercase();
    let verb = CONNECTION_VERBS
        .iter()
        .any(|word| contains_word(&asked, word));
    let subject = CONNECTION_SUBJECTS
        .iter()
        .any(|word| contains_word(&asked, word))
        || provider_ids
            .iter()
            .filter(|id| id.chars().count() >= MIN_ID_SUBJECT_CHARS)
            .any(|id| contains_word(&asked, &id.to_lowercase()));
    verb && subject
}

/// How short a provider id may be and still count as a prompt's subject.
///
/// Three characters, because the ids this reads are arbitrary user strings and a
/// one- or two-letter one ("k", "ds") appears inside ordinary English constantly.
/// The whole-word match makes that far less likely than a substring did; the
/// floor is the second guard, and the cost of it is that a user with a
/// two-letter id says "provider" or "connection" like everybody else.
const MIN_ID_SUBJECT_CHARS: usize = 3;

/// True when the turn answered that question by inspecting rather than dialling
/// (ADR-4).
///
/// Two readings, because the observed failure came in two shapes and only one of
/// them is in the reply: the model *recites* a diagnostic, or it *ran* one
/// through the shell tool and reported what it made of the output.
///
/// Both readings first take out every mention of the **right** answer, and that
/// is the fix this predicate most needed: `teton provider test kimi` contains
/// `teton provider`, so a reply that gave the user exactly the command BR-6
/// names — or a turn that *ran* it — read as a provider diagnostic and drew a
/// line correcting a correction.
///
/// The tool half reads the `<tool>: <command>` title the renderer already drew.
/// It requires the tool to be `shell` — a `read` of a file whose path contains
/// `teton` is not a probe — and requires `teton` to be the command's **first
/// word**: `cargo test -p tetond` is a build, not a probe of the user's
/// provider, and a substring match on "teton" called it one.
///
/// `plain_reply` arrives backtick-stripped, for [`without_backticks`]'s reason:
/// every question this module asks of a turn is asked of the same characters.
fn improvised_a_probe(plain_reply: &str, turn_tools: &[String]) -> bool {
    let recited = without_the_connection_test(plain_reply);
    if PROVIDER_DIAGNOSTICS
        .iter()
        .any(|command| recited.contains(command))
    {
        return true;
    }
    turn_tools.iter().any(|title| shell_probed_teton(title))
}

/// `text` with every mention of the connection test itself removed.
///
/// So that the right answer cannot be counted as the mistake it corrects: the
/// shell form of this REQ's own command is a prefix-match for `teton provider`,
/// which is the first entry in [`PROVIDER_DIAGNOSTICS`].
fn without_the_connection_test(text: &str) -> String {
    CONNECTION_COMMANDS
        .iter()
        .fold(text.to_owned(), |stripped, command| {
            stripped.replace(command, " ")
        })
}

/// True when one tool-call title is a `shell` call whose command **starts** with
/// `teton` and is not the connection test.
fn shell_probed_teton(title: &str) -> bool {
    let Some((tool, command)) = title.split_once(':') else {
        return false;
    };
    if tool.trim() != "shell" {
        return false;
    }
    let mut words = command.split_whitespace();
    if words.next() != Some("teton") {
        return false;
    }
    // `teton provider test <id>` is the command this hand-off would have named:
    // a turn that ran it dialled, and has nothing to be corrected about.
    !(words.next() == Some("provider") && words.next() == Some("test"))
}

// ---------------------------------------------------------------------------
// The generic hand-off (REQ-582 ADR-6)
// ---------------------------------------------------------------------------
//
// The two lines above correct a *mistake*: a reply that would have the user
// paste a key into the chat, and a reply that reports a connection nothing
// dialled. This one corrects nothing. Ten commands now have a session row
// (BR-1), and a reply that names one of them by its shell spelling is right —
// `teton policy show` works, and it is what the bundled guide and every page
// ever written about a CLI teaches. It is only the longer way round from inside
// a session that has the same command one slash away.
//
// So the line is a spelling, not an argument, and the rest follows from that:
// it names the `/` forms and says nothing else, it goes last of the three, and
// it is built from the row table rather than from a list of its own — BR-7's
// rule ("`/help` is generated from the table") applied to the other surface
// that names commands. A row added to the table is nudged for with no second
// list to maintain, and this line cannot name a spelling that dispatches to
// nothing.
//
// Read off the same backtick-stripped text every predicate above reads, with
// [`contains_word`], **case-sensitively** — REQ-581's reply-side rule: a
// command is typed in lowercase, so "Teton Provider List" is prose about one
// and not one.

/// The generic line's opening, and the whole of its editorial content.
///
/// The two sentences above each carry a reason — "no key in chat", "makes one
/// consented call" — because each corrects a belief the reply left the user
/// with. This one has no belief to correct, so it states the equivalence and
/// stops. Anything longer would be the harness arguing with an answer that was
/// already true.
const GENERIC_HAND_OFF_PREFIX: &str = "in this session: ";

/// ADR-6's line for `names`, in the order given (which is the caller's table
/// order, never the order the reply happened to mention them in).
///
/// Plain text with no escape of its own (LESSON-517) and no em-dash aside
/// (BUG-168), like the two constants above. It is built rather than declared
/// only because its subject is whatever the reply recited.
fn generic_hand_off_line(names: &[&str]) -> String {
    let spellings: Vec<String> = names.iter().map(|name| format!("/{name}")).collect();
    format!("{GENERIC_HAND_OFF_PREFIX}{}", spellings.join(", "))
}

/// End a typed-prompt turn: print the hand-off if this turn earned one.
///
/// Called once per turn from the entry loop, and it **consumes** the turn's
/// record — so a second call in the same turn reads an empty reply, an empty
/// prompt and no tool calls, and prints nothing. That is the "at most once"
/// guarantee expressed as data rather than as a flag somebody has to remember to
/// reset, and it doubles as the reset on every path that reaches here.
///
/// Three lines can be earned and **at most one prints**: REQ-579's setup
/// hand-off (the reply reached for the shell recipe), REQ-581's connection
/// hand-off (the turn answered a connection question by inspecting
/// configuration), and REQ-582's generic line (the reply named a mirrored
/// command by its shell spelling). The setup line goes first because its subject
/// is the more basic one — a reply steering a user toward pasting a key into the
/// chat is corrected before a reply that merely tested the wrong thing — and
/// because a setup recipe recites `teton provider add`, which the connection
/// predicate would otherwise read as a probe.
///
/// The generic line goes last because it is the only one of the three that
/// carries no reason (BR-8, ADR-6). The older two say "no key in chat" and
/// "makes one consented call"; those sentences are why the turns that earn them
/// are worth interrupting, and a bare list of spellings printed in their place
/// would drop the part that mattered. `teton provider add` and `teton policy
/// set-tier` are mirrored rows *and* setup recipes, so this ordering is what
/// decides which of the two lines such a reply gets — and it decides for the one
/// that says something.
///
/// `tty` is the session's `typed_input`, the same world-fact
/// [`crate::web_setup_ui::gate`] turns on. A piped session prints nothing at
/// all: BR-11 already gives a script the shell recipe, and a script's output has
/// to stay byte-identical to what it was before REQ-579.
pub(crate) fn hand_off_after_turn(state: &mut SessionState, surface: &mut dyn Surface, tty: bool) {
    // Taken unconditionally, before any gate: whether a line prints or not, this
    // turn's record is finished with, and a `return` that left it behind would
    // hand it to the next turn.
    let reply = std::mem::take(&mut state.turn_reply);
    let prompt = std::mem::take(&mut state.turn_prompt);
    let tools = std::mem::take(&mut state.turn_tools);
    if !tty {
        return;
    }
    // One reading of the turn, stripped once, asked by every predicate below —
    // see [`without_backticks`].
    let plain = without_backticks(&reply);
    if earns_hand_off(&plain) {
        surface.line(LineKind::Notice, hand_off_line());
        return;
    }
    if asks_about_a_connection(&prompt, &state.provider_ids)
        && improvised_a_probe(&plain, &tools)
        && !CONNECTION_COMMANDS
            .iter()
            .any(|command| plain.contains(command))
    {
        surface.line(LineKind::Notice, connection_test_line());
        return;
    }
    // Neither correction was earned, so the only thing left to say about a reply
    // that named a `teton …` command is how this session spells it (ADR-6). The
    // candidates are the row table's, in table order; one the reply *also* named
    // in `/` form is dropped, because the model already said it — REQ-579 ADR-9's
    // dormancy, asked once per command rather than once per turn.
    //
    // The dormancy match is a **word** match too (verify m8): a substring
    // `/doctor` occurs inside `crates/teton/src/doctor.rs`, and a reply that
    // named that path while telling the user to run `teton doctor` has not
    // taught the session spelling. `contains_word` reads the byte before the
    // `/` — a path separator's neighbour is alphanumeric, a command's is a
    // space or the line's start.
    let named: Vec<&str> = slash::mirrored_rows()
        .filter(|(name, shell)| {
            contains_word(&plain, shell) && !contains_word(&plain, &format!("/{name}"))
        })
        .map(|(name, _)| name)
        .collect();
    if !named.is_empty() {
        surface.line(LineKind::Notice, &generic_hand_off_line(&named));
    }
}

/// Render a compact preview of a proposed file change.
fn render_diff(path: &str, old_text: Option<&str>, new_text: &str, surface: &mut dyn Surface) {
    match old_text {
        None => surface.line(LineKind::Diff, &format!("± {path} (new file)")),
        Some(_) => surface.line(LineKind::Diff, &format!("± {path}")),
    }
    if let Some(old) = old_text {
        for line in old.lines() {
            surface.line(LineKind::Diff, &format!("- {line}"));
        }
    }
    for line in new_text.lines() {
        surface.line(LineKind::Diff, &format!("+ {line}"));
    }
}

/// What [`consent_gate`] decides (REQ-585 ADR-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentGate {
    /// Render the request and ask — every prompt this client has ever shown.
    Answerable,
    /// Refuse with [`RefusalReason::NoTerminal`], asking nobody.
    RefuseNoTerminal,
    /// Refuse with [`RefusalReason::UnrecognizedSubject`], asking nobody.
    RefuseUnrecognized,
}

/// Whether a permission request may be put to a human at all, from the
/// request's own subject and where this session's input comes from
/// (REQ-585 BR-11, ADR-8).
///
/// **A pure two-input predicate, consulted before `prompter.ask`** —
/// [`crate::cli_rows::write_gate`]'s shape, and for a sharper version of its
/// reason. `StdinPrompter::ask` reads a line *unconditionally*: a refusal
/// computed after the call has already eaten one, and on a pipe the user's next
/// **prompt** line becomes the answer — a pasted `y` turning into consent for
/// shell commands, which is exactly LESSON-537's shape. Deciding first is what
/// makes "the commands were not run and your next line is still your next line"
/// true by construction rather than by care.
///
/// Three rules, and the polarity of each is deliberate:
///
/// - **No subject at all is answerable, terminal or not.** Every prompt this
///   client showed before REQ-585 arrives this way, and the piped shell consent
///   answering from the next stdin line is behaviour a shipped script depends
///   on. BR-11's narrowing is about the *skill* consent and nothing else; a gate
///   that generalized it would be a silent change to every tool prompt.
/// - **Every skill subject needs a terminal.** This is the narrowing, and it is
///   the whole of it: a skill's dynamic context (REQ-585 BR-6) and REQ-587
///   BR-4's project-skill acknowledgment, which is the same rule applied to the
///   question one step earlier. At `full` the daemon asks neither, so a piped
///   session still runs dynamic context and still expands a non-shadowing
///   project skill exactly as a TTY does — that is the automation posture, and
///   it is why this gate never sees those turns.
/// - **An unrecognized subject is refused, terminal or not.** A client that does
///   not know what it is being asked cannot render the question, so there is
///   nothing to put to the user even at a terminal; falling through to `ask`
///   would be reading a line to answer a question nobody was shown. Fail-closed
///   in the direction that can only cost a skill invocation, never a swallowed
///   prompt line.
///
/// Pure, so all eight rows are unit-tested without a terminal, a pipe or a
/// daemon: the rows that matter are the ones a test process cannot otherwise
/// reach.
#[must_use]
pub(crate) fn consent_gate(subject: Option<&PermissionSubject>, typed_input: bool) -> ConsentGate {
    match subject {
        None => ConsentGate::Answerable,
        Some(PermissionSubject::Unrecognized) => ConsentGate::RefuseUnrecognized,
        Some(PermissionSubject::SkillDynamicContext { .. }) => {
            if typed_input {
                ConsentGate::Answerable
            } else {
                ConsentGate::RefuseNoTerminal
            }
        }
        // REQ-587 BR-4, and the same row as the one above it: this client can
        // draw the question (`render_consent_subject` names the root and lists
        // the set), so at a terminal it is asked — and on a pipe it is refused
        // **without reading a line**, for the reason BR-11 gave the first skill
        // subject. Nothing about this question is more answerable from a pipe
        // than a command list is: it grants no effect, but what it grants is
        // repository text reaching the model labelled *instructions*, and a
        // pasted `y` becoming that consent is LESSON-537's shape on the one
        // question that has no human in the loop by construction.
        Some(PermissionSubject::ProjectSkillTrust { .. }) => {
            if typed_input {
                ConsentGate::Answerable
            } else {
                ConsentGate::RefuseNoTerminal
            }
        }
    }
}

/// Resolve a permission request: refuse what cannot be asked, apply any session
/// grant, else prompt.
///
/// Returns the [`PermissionRespondParams`] to send back to the daemon and, as a
/// side effect, records "always" decisions in `grants` so a later request for the
/// same tool needs no prompt.
///
/// `typed_input` is the session's own terminal fact, threaded from the one edge
/// that reads it (`main.rs`'s `IsTerminal` on **stdin**) rather than recomputed
/// here — the same discipline `UiContext::typed_input` documents, and the reason
/// is that a second reading is a second answer waiting to disagree.
///
/// [`consent_gate`] is consulted **first, before anything else in this
/// function**, and that ordering is the guarantee rather than a tidiness: it is
/// the only arrangement under which no path can reach `prompter.ask` with a
/// request that was never answerable (REQ-585 BR-11, ADR-8). The grant lookups
/// below consume no prompt and would be harmless above it — but "the gate is
/// first" is a property a reader can check in one glance, and "the gate is
/// somewhere before the loop" is not.
///
/// A refusal is [`PermissionOutcome::Refused`] and never
/// [`PermissionOutcome::Cancelled`]: `Cancelled` already means *the user
/// dismissed the prompt*, it is what EOF on a pipe returns two dozen lines
/// below, and AC-9 needs the daemon's placeholders to be able to say that no
/// human could be asked — which a dismissal cannot tell it.
pub fn resolve_permission(
    req: &PermissionRequest,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
    grants: &mut SessionGrants,
    typed_input: bool,
) -> PermissionRespondParams {
    let tool = req.tool_name.as_str();

    // REQ-585 BR-11: before the grants, before the render, and above all before
    // any call that could read a line.
    match consent_gate(req.subject.as_ref(), typed_input) {
        ConsentGate::Answerable => {}
        ConsentGate::RefuseNoTerminal => {
            render_consent_subject(req.subject.as_ref(), surface);
            surface.line(
                LineKind::Notice,
                &refusal_line(req, RefusalReason::NoTerminal),
            );
            return respond(
                req,
                PermissionOutcome::Refused {
                    reason: RefusalReason::NoTerminal,
                },
            );
        }
        ConsentGate::RefuseUnrecognized => {
            // No subject block: this is precisely the request whose contents
            // this build cannot read, so there is nothing of it to show.
            surface.line(
                LineKind::Notice,
                &refusal_line(req, RefusalReason::UnrecognizedSubject),
            );
            return respond(
                req,
                PermissionOutcome::Refused {
                    reason: RefusalReason::UnrecognizedSubject,
                },
            );
        }
    }

    // Session-scoped auto-decisions first — these consume no prompt.
    if grants.is_reject_always(tool) {
        surface.line(
            LineKind::Prompt,
            &format!("auto-deny {tool} (denied for this session)"),
        );
        return respond(req, deny_outcome(&req.options));
    }
    if grants.is_allow_always(tool) {
        surface.line(
            LineKind::Prompt,
            &format!("auto-allow {tool} (allowed for this session)"),
        );
        return respond(req, allow_outcome(&req.options, true));
    }

    // Render the request, then ask.
    let description = req
        .description
        .as_deref()
        .map_or_else(String::new, |d| format!(" — {d}"));
    surface.line(
        LineKind::Prompt,
        &format!("permission requested: {tool}{description}"),
    );
    // REQ-585 ADR-7: what is being consented to, from the typed subject rather
    // than from the key or the description. Nothing renders for a request that
    // carries no subject, so every prompt that existed before this REQ is
    // byte-identical.
    render_consent_subject(req.subject.as_ref(), surface);

    // REQ-563 BR-4: the persistent option exists only on prompts that offer it
    // (the web tiers), and the question must not advertise a key that answers
    // nothing. Both the prompt line and the retry hint read this one flag, so
    // they cannot disagree about what is on offer.
    let permanent = permanent_option(&req.options);
    let question = if permanent.is_some() {
        format!("  allow {tool}? [y]es / [n]o / [a]llow-always / [p]ermanently / [d]eny-always: ")
    } else {
        format!("  allow {tool}? [y]es / [n]o / [a]llow-always / [d]eny-always: ")
    };
    let retry = if permanent.is_some() {
        "  please answer y, n, a (allow-always), p (enable permanently), or d (deny-always)"
    } else {
        "  please answer y, n, a (allow-always), or d (deny-always)"
    };

    loop {
        let answer = prompter.ask(&question);
        let choice = match answer {
            Some(a) => a.trim().to_lowercase(),
            None => return respond(req, PermissionOutcome::Cancelled), // EOF = cancel
        };
        match choice.as_str() {
            "y" | "yes" => return respond(req, allow_outcome(&req.options, false)),
            "n" | "no" => return respond(req, reject_outcome(&req.options)),
            "a" | "always" => {
                grants.allow_always(tool);
                return respond(req, allow_outcome(&req.options, true));
            }
            // Only reachable when the option was offered: on any other prompt
            // `p` is an unrecognised answer and re-asks, rather than silently
            // meaning something the user did not pick.
            "p" | "permanently" => {
                if let Some(option_id) = permanent.clone() {
                    // Remembered locally too: the daemon's grant is per-turn, and
                    // the config write it performs governs the *ceiling*, not
                    // this session's answer. Without this the user would be
                    // re-asked on the next lookup despite having said the
                    // strongest possible yes.
                    grants.allow_always(tool);
                    return respond(req, PermissionOutcome::Selected { option_id });
                }
                surface.line(LineKind::Prompt, retry);
            }
            "d" | "deny" => {
                grants.reject_always(tool);
                return respond(req, deny_outcome(&req.options));
            }
            "" => return respond(req, PermissionOutcome::Cancelled),
            _ => surface.line(LineKind::Prompt, retry),
        }
    }
}

/// Render what a request is about, from its typed subject (REQ-585 ADR-7).
///
/// **One [`Surface::line`] per command.** `Surface::line` defuses its text, and
/// defusing destroys newlines — so BR-6's "every command of the invocation,
/// listed verbatim" cannot ride one joined string, and could not have ridden
/// `PermissionRequest::description` either. That mechanical fact is half of why
/// the subject is a structure at all.
///
/// A request with no subject renders nothing: every prompt that existed before
/// REQ-585 arrives that way, and its bytes do not move.
///
/// [`PermissionSubject::Unrecognized`] renders nothing either, and never
/// reaches here on the asking path — [`consent_gate`] refuses it first. It is
/// matched rather than wildcarded so that adding a subject variant is a
/// compile error here, where the question is drawn, and not a silently blank
/// prompt.
///
/// Both skill subjects are drawn on the **refusing** path too, above the
/// refusal line: a piped session is told what was refused, not merely that
/// something was. That is the one place the wildcard would have been invisible —
/// a blank arm renders nothing and asserts nothing, and the request would still
/// be refused with the right reason.
fn render_consent_subject(subject: Option<&PermissionSubject>, surface: &mut dyn Surface) {
    match subject {
        None | Some(PermissionSubject::Unrecognized) => {}
        Some(PermissionSubject::SkillDynamicContext {
            skill,
            source,
            commands,
            invoked_by,
        }) => {
            surface.line(
                LineKind::Prompt,
                &format!(
                    "  skill `{skill}` ({}){} wants to run {} dynamic-context command{}:",
                    slash::source_word(*source),
                    invoker_clause(*invoked_by),
                    commands.len(),
                    if commands.len() == 1 { "" } else { "s" },
                ),
            );
            for command in commands {
                surface.line(
                    LineKind::Prompt,
                    &format!("    {}", dynamic_command_text(command)),
                );
            }
        }
        // REQ-587 BR-4's acknowledgment: the root, then the named set, one
        // `Surface::line` per entry for the reason the command list is one per
        // line — `line` defuses, and defusing destroys newlines.
        Some(PermissionSubject::ProjectSkillTrust { root, skills, more }) => {
            surface.line(
                LineKind::Prompt,
                &format!(
                    "  the model wants to run this repository's skills as instructions: {root}",
                ),
            );
            for entry in skills {
                surface.line(
                    LineKind::Prompt,
                    &format!("    {}", project_skill_entry(entry)),
                );
            }
            // Only when the daemon left some out. The tail is the *count* it
            // sent, never a re-count of a list this side bounded: the bound is
            // the daemon's, at the door that mints the subject, and "and some
            // more" is a different fact from "+5 more" to someone being asked
            // to trust the whole set.
            if *more > 0 {
                surface.line(LineKind::Prompt, &format!("    +{more} more"));
            }
        }
    }
}

/// How the acknowledgment prompt names one project skill (REQ-587 BR-4, AC-6).
///
/// A shadowing entry reads `validate (project — shadows your user skill)`, which
/// is the spelling the daemon's expansion frame uses for the same fact on the
/// other side of the wire. Both are *rendered* from
/// [`events::ProjectSkillTrustEntry::shadows_user_skill`]; neither is a re-parse
/// of the other's prose (LESSON-529), which is why the wire carries a flag and
/// not a pre-marked name.
///
/// An ordinary entry is its bare name. Every entry in this list is a project
/// skill — the line above says so — so `(project)` on each of them would be the
/// same word twenty times over, and the source is only worth saying where it
/// contrasts with the user skill this one is taking the name from.
fn project_skill_entry(entry: &events::ProjectSkillTrustEntry) -> String {
    if entry.shadows_user_skill {
        format!(
            "{} ({})",
            entry.name,
            slash::source_words(SkillSource::Project, true)
        )
    } else {
        entry.name.clone()
    }
}

/// The clause BR-5 adds to a skill consent: **who asked**.
///
/// "You asked for `deploy`" and "the model decided to run `deploy`" are
/// different questions carrying the same command list, and the human at
/// `guarded` is entitled to know which one is on the screen.
///
/// A user invocation adds nothing, so REQ-585's prompt keeps its bytes exactly —
/// the words it renders are the words `pty_e2e` already pins. The model's clause
/// is the vocabulary [`skill_echo_line`] uses for the same fact, so a user who
/// answered a prompt reads the same phrase back in the echo line that follows.
fn invoker_clause(invoked_by: events::InvokedBy) -> &'static str {
    match invoked_by {
        events::InvokedBy::User => "",
        events::InvokedBy::Model => ", invoked by the model,",
    }
}

/// The one line a refused request gets, naming what was refused and why
/// (REQ-585 BR-11).
///
/// It reports exactly what was checked — "this session's input is not a
/// terminal" — never a claim to have identified anyone, and it names the remedy,
/// because a refusal without one is a dead end
/// ([`crate::cli_rows::typed_only_line`]'s rule). The remedy is BR-11's stated
/// automation posture: an unattended runner that wants a skill's dynamic
/// context chooses `full` for the session, the same choice it already makes for
/// every `shell` call.
///
/// The subject supplies the name when it has one. The fallbacks name the
/// request's key instead — *rendered*, never parsed: what a client may not do
/// is **select** on that string (ADR-7), and printing the thing the daemon
/// called this request is how a user finds it in a log.
fn refusal_line(req: &PermissionRequest, reason: RefusalReason) -> String {
    let subject = match &req.subject {
        Some(PermissionSubject::SkillDynamicContext { skill, .. }) => {
            format!("skill `{skill}`'s dynamic context")
        }
        // REQ-587 BR-4. Named from the subject like the row above it: the key
        // would say `project_skill_trust:~/dev/teton`, which names the same root
        // in the vocabulary of a log rather than of a question. The fallback
        // below still renders the key for a subject this build cannot read, and
        // that remains the right answer there — it is the only thing such a
        // request is known by.
        Some(PermissionSubject::ProjectSkillTrust { root, .. }) => {
            format!("running `{root}`'s skills as instructions")
        }
        _ => format!("`{}`", req.tool_name),
    };
    match reason {
        RefusalReason::NoTerminal => format!(
            "{subject} was refused without asking: this session's input is not a terminal, so \
             nobody could be asked — send `/permissions full` ahead of it, or set \
             `[permissions] default_level`, to allow it unattended."
        ),
        RefusalReason::UnrecognizedSubject => format!(
            "{subject} was refused without asking: this build does not recognize what it is \
             asking to do, and a question it cannot show is not one it may answer."
        ),
    }
}

/// The offered persistent-enable option's id, when the prompt carries one.
///
/// Selected **by id**, not by [`PermissionOptionKind`]: the ACP kind enum has no
/// variant for "and write it down", so this option travels as `AllowAlways` and
/// is indistinguishable from the plain session grant by kind alone. Picking it
/// by kind would let [`allow_outcome`] reach it by accident — a user answering
/// "allow for this session" would have edited their config.
fn permanent_option(options: &[PermissionOption]) -> Option<String> {
    options
        .iter()
        .find(|o| o.option_id == OPTION_ID_ENABLE_PERMANENT)
        .map(|o| o.option_id.clone())
}

/// Build a response for `req` with the chosen `outcome`.
fn respond(req: &PermissionRequest, outcome: PermissionOutcome) -> PermissionRespondParams {
    PermissionRespondParams {
        request_id: req.request_id.clone(),
        outcome,
    }
}

/// Pick an allow option; `session` prefers the allow-always option when offered.
fn allow_outcome(options: &[PermissionOption], session: bool) -> PermissionOutcome {
    let preferred: &[PermissionOptionKind] = if session {
        &[
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce,
        ]
    } else {
        &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    };
    select_option(options, preferred)
}

/// Pick a reject-once option.
fn reject_outcome(options: &[PermissionOption]) -> PermissionOutcome {
    select_option(
        options,
        &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ],
    )
}

/// Pick a reject-always option (falling back to reject-once).
fn deny_outcome(options: &[PermissionOption]) -> PermissionOutcome {
    select_option(
        options,
        &[
            PermissionOptionKind::RejectAlways,
            PermissionOptionKind::RejectOnce,
        ],
    )
}

/// Select the first option matching one of `preferred` kinds, in order; if none
/// of the offered options match, cancel rather than guess.
fn select_option(
    options: &[PermissionOption],
    preferred: &[PermissionOptionKind],
) -> PermissionOutcome {
    for kind in preferred {
        if let Some(opt) = options.iter().find(|o| o.kind == *kind) {
            return PermissionOutcome::Selected {
                option_id: opt.option_id.clone(),
            };
        }
    }
    PermissionOutcome::Cancelled
}

/// Short label for a tool-call status.
fn status_label(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
    }
}

/// Human name for a lifecycle phase.
///
/// There is no `freeform` arm: ADR-G retired the variant. A freeform turn now
/// reaches the display path with a *category*, which is what
/// [`format_route`] renders; only [`format_phase_transition`] still needs a
/// phase name, and a transition always has one.
fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Spec => "spec",
        Phase::Architect => "architect",
        Phase::Implement => "implement",
        Phase::Review => "review",
        Phase::Io => "io",
    }
}

fn format_route(rd: &RouteDecided) -> String {
    // REQ-558: the category is what drove the decision, so it is the label.
    //
    // A decision with **no** category consulted no binding, and there is
    // exactly one of those: the BR-7 taint pin, which overrides every binding
    // because the session has touched `local-only` content. So the label is
    // `pinned` — not the phase, which did not drive this route (BR-11 keeps
    // `phase` for cost attribution, not for explaining one), and certainly not
    // `freeform`, which is the value ADR-G retired and which was being printed
    // for *structured* sessions: the taint arm returns before the phase is
    // stamped, so a pinned `implement` turn rendered `route [freeform] → local`
    // — wrong about the mode, and silent about the only thing worth saying,
    // which is why the turn is on the local tier at all.
    let key = match (rd.category, rd.tier) {
        (Some(category), Some(tier)) => format!("{category}/{tier}"),
        (Some(category), None) => category.to_string(),
        _ => "pinned".to_owned(),
    };
    let model = rd.model.as_deref().unwrap_or("(model tbd)");
    format!(
        "route [{key}] → {} {model} — {}{}",
        rd.provider_id,
        rd.reason,
        budget_clause(rd).unwrap_or_default()
    )
}

/// The budget clause a route line carries, or `None` from a daemon that states
/// no budget (REQ-586 BR-9, ADR-9).
///
/// **Both currencies**, because on a remote route it is the byte guard that
/// binds in practice — the word figure alone would tell a user they have room
/// they have not got — and a route line that says one number is a route line
/// that will be argued with. The bound closes it, in the same words every
/// pressure line uses.
///
/// All three fields or none: they are stamped together by the router, so a
/// partial set is a daemon that predates them and gets the pre-REQ-586 line
/// byte-for-byte rather than a half-rendered clause (the `RouteDecided::effort`
/// rule).
fn budget_clause(rd: &RouteDecided) -> Option<String> {
    let tokens = rd.budget_tokens?;
    let bytes = rd.budget_bytes?;
    let bound = rd.bound?;
    Some(format!(
        " · budget {} words / {} {}",
        thousands(tokens),
        bytes_figure(bytes),
        // `false` from a daemon that predates the field: it floored nothing it
        // could report, and the clause is today's byte for byte.
        bound_clause(bound, rd.bound_floored.unwrap_or(false))
    ))
}

/// Renders a block as the sentence its cause earns.
///
/// The three causes are three different problems with three different fixes, so
/// they get three different sentences (REQ-562 BR-3). Two rules hold across all
/// of them: nothing here interpolates payload content — `path`, `kind` and
/// `span` are the whole vocabulary, and there is no matched text on the event to
/// print even if a line wanted to (BR-6) — and the scan-unavailable line says
/// the scan could not run, never that it found something.
fn format_privacy(pb: &PrivacyBlock) -> String {
    let action = match pb.action {
        PrivacyAction::Stripped => "stripped from the outbound payload",
        PrivacyAction::ReroutedToLocal => "call re-routed to the local tier",
    };
    match &pb.cause {
        BlockCause::Boundary => format!(
            "privacy: {} would have reached {} — {action}",
            pb.path, pb.provider_id
        ),
        BlockCause::Redaction { kind, span } => format!(
            "privacy: the redaction scan detected {} at bytes {}–{} of {}, bound for {} — {action}",
            kind.user_label(),
            span.start,
            span.end,
            pb.path,
            pb.provider_id
        ),
        BlockCause::ScanUnavailable => format!(
            "privacy: the redaction scan could not run on {}, bound for {} — blocked unscanned; {action}",
            pb.path, pb.provider_id
        ),
    }
}

/// The `provenance_rejected` notice (REQ-571 ADR-D).
///
/// ## The source is rendered with `{:?}`, deliberately
///
/// `source` is attacker-influenced text: a remote MCP server chose it. The
/// daemon already strips control characters and truncates before it goes on the
/// wire, and this is the second half of that posture rather than a duplicate of
/// it — `{:?}` escapes any control byte that reached here anyway, so nothing in
/// a hostile source can move the cursor, colour the terminal, or fake a second
/// notice line. It also makes the value visibly a quoted string rather than
/// something the reader might take for a path the daemon endorses.
fn format_provenance_rejected(pr: &ProvenanceRejected) -> String {
    let reason = match pr.reason {
        ProvenanceRejection::Absolute => "it is absolute, and boundaries are repo-relative",
        ProvenanceRejection::ParentTraversal => {
            "it retains a `..` segment, which only the filesystem could resolve"
        }
        ProvenanceRejection::NotCanonical => "it is not in canonical form",
        ProvenanceRejection::Empty => "it names no file",
    };
    match &pr.tool {
        // `tool` is `mcp__<server>__<tool>`, whose `<tool>` component is supplied
        // verbatim by a remote MCP server's `tools/list` and never validated. It
        // gets the same `{:?}` escaping as `source` so a hostile tool name cannot
        // smuggle newlines or ANSI escapes to forge or erase this refusal line —
        // the exact anti-forgery the source field already has (REQ-571, LESSON-505).
        Some(tool) => format!(
            "privacy: {tool:?} claimed the source {:?} — refused because {reason}; \
             that result is treated as unknown-origin and held local",
            pr.source
        ),
        None => format!(
            "privacy: the source {:?} reached the egress check un-minted — refused \
             because {reason}; the call was blocked",
            pr.source
        ),
    }
}

fn format_degraded(pd: &ProviderDegraded) -> String {
    let class = match pd.failure_class {
        FailureClass::ToolCallFailure => "tool-call failure",
        FailureClass::Timeout => "timeout",
        FailureClass::RateLimited => "rate-limited",
        FailureClass::ConnectionError => "connection error",
        FailureClass::InvalidResponse => "invalid response",
    };
    match &pd.fallback_id {
        Some(fallback) => format!(
            "degraded: {} ({class}) → fell back to {fallback}",
            pd.provider_id
        ),
        None => format!(
            "degraded: {} ({class}) — no fallback configured",
            pd.provider_id
        ),
    }
}

fn format_phase(pt: &PhaseTransition) -> String {
    let from = pt.from_phase.map_or("start", phase_name);
    format!(
        "phase: {from} → {} ({} artifact(s))",
        phase_name(pt.to_phase),
        pt.artifacts.len()
    )
}

fn format_attach(a: &DaemonClientAttach) -> String {
    let kind = match a.client_kind {
        teton_protocol::ClientKind::Cli => "CLI",
        teton_protocol::ClientKind::Extension => "extension",
    };
    format!("a {kind} client attached (protocol {})", a.protocol_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use teton_protocol::events::{
        ByteSpan, ContextCleared, CostRecord, CostRecorded, FindingKind, ModelSelectionDecided,
        PlanEntry, PlanEntryStatus, SelectionSource, SessionRootChanged, SessionUpdate,
        WebTaintOverridden,
    };
    use teton_protocol::methods::{ProviderHealth, ProviderTestOutcome, RootKind, TierBinding};
    use teton_protocol::{ProviderId, ProviderKind, RequestId, SessionId, Tier};

    /// A consent request as the daemon would publish it.
    fn consent_request(scope: ConsentScope) -> AttachConsentRequested {
        AttachConsentRequested {
            request_id: teton_protocol::RequestId::from("consent-0"),
            scope,
            requester: "cli client \"teton\"".to_owned(),
        }
    }

    /// **REQ-570 AC-4.** The CLI renders the request, takes a decision, and
    /// answers — the capability whose absence made every consent path time out.
    #[test]
    fn the_cli_renders_a_consent_request_and_sends_the_users_decision() {
        for (case, typed, expected) in [
            ("an explicit yes grants", "y", AttachConsentOutcome::Granted),
            (
                "the long form works too",
                "yes",
                AttachConsentOutcome::Granted,
            ),
            ("an explicit no denies", "n", AttachConsentOutcome::Denied),
        ] {
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&[typed]);
            let reply = resolve_attach_consent(
                &consent_request(ConsentScope::Attach),
                &mut surface,
                &mut prompter,
            );

            assert_eq!(reply.outcome, expected, "{case}");
            assert_eq!(reply.request_id.to_string(), "consent-0", "{case}");
            assert!(
                surface
                    .lines_of(LineKind::Notice)
                    .iter()
                    .any(|l| l.contains("asked to")),
                "{case}: the user must see who asked and for what"
            );
        }
    }

    /// **AC-4, the half that matters.** It never auto-answers.
    ///
    /// A non-interactive invocation — piped stdin, no TTY, EOF — **declines**.
    /// Silence is not consent, and this is the path nobody exercises by hand, so
    /// it is the one most likely to rot into an accidental approval.
    #[test]
    fn a_non_interactive_cli_declines_rather_than_auto_approving() {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&[]); // EOF immediately
        let reply = resolve_attach_consent(
            &consent_request(ConsentScope::Attach),
            &mut surface,
            &mut prompter,
        );

        assert_eq!(
            reply.outcome,
            AttachConsentOutcome::Denied,
            "no input must never mean yes"
        );
        assert!(
            surface
                .lines_of(LineKind::Notice)
                .iter()
                .any(|l| l.contains("no interactive input")),
            "and it says why, so a scripted user is not left guessing"
        );
    }

    /// Anything that is not an explicit yes is a no.
    ///
    /// Deliberately the opposite of `confirm_model`'s empty-is-yes: there the
    /// default action is the one the user asked for, here it is admitting
    /// somebody else.
    #[test]
    fn only_an_explicit_yes_grants_consent() {
        for typed in ["", " ", "sure", "ok", "yep", "Y E S", "1", "true", "d"] {
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&[typed]);
            let reply = resolve_attach_consent(
                &consent_request(ConsentScope::Attach),
                &mut surface,
                &mut prompter,
            );
            assert_eq!(
                reply.outcome,
                AttachConsentOutcome::Denied,
                "{typed:?} is not an explicit yes and must not grant"
            );
        }
    }

    /// A monitor ask is a different sentence, not the same one with a noun
    /// swapped — a user skimming a familiar prompt would otherwise answer the
    /// smaller question they have answered before.
    #[test]
    fn a_monitor_request_asks_a_visibly_bigger_question() {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["n"]);
        let _ = resolve_attach_consent(
            &consent_request(ConsentScope::Monitor),
            &mut surface,
            &mut prompter,
        );
        let asked = prompter.questions.join(" ");
        assert!(
            asked.contains("EVERY"),
            "the monitor prompt must not read like the attach one: {asked}"
        );
    }

    fn envelope(event: Event) -> EventEnvelope {
        EventEnvelope::new(1, Some(SessionId::from("s1")), event)
    }

    fn chunk(text: &str) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::AgentMessageChunk {
                text: text.to_owned(),
            },
        })
    }

    #[test]
    fn streamed_chunks_render_as_fragments_in_order() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        for text in ["Hello", ", ", "world"] {
            render_event(&envelope(chunk(text)), &mut surface, &mut state);
        }
        assert_eq!(surface.fragments(), "Hello, world");
    }

    /// REQ-558: a decision that came through the category chain is labelled by
    /// the thing that drove it. The phase is not what explains a route any more,
    /// so it does not appear in the label when a category does.
    #[test]
    fn a_route_notice_is_labelled_by_its_category_and_tier() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;

        render_event(
            &envelope(Event::RouteDecided(RouteDecided {
                category: Some(teton_protocol::Category::Design),
                tier: Some(teton_protocol::Tier::Think),
                phase: None,
                provider_id: ProviderId::from("anthropic"),
                model: Some("claude-opus-4".to_owned()),
                reason: "Routing the 'design' category to 'anthropic' through its 'think' tier \
                         binding."
                    .to_owned(),
                effort: None,
                budget_tokens: None,
                budget_bytes: None,
                bound: None,
                bound_floored: None,
            })),
            &mut surface,
            &mut state,
        );

        assert!(surface.any_line_contains(LineKind::Notice, "route [design/think]"));
        assert!(!surface.any_line_contains(LineKind::Notice, "route [freeform]"));
    }

    /// **A taint-pinned route is labelled `pinned`, in either session mode.**
    ///
    /// The pin consults no binding — that is what makes it a privacy guarantee
    /// rather than a routing decision — so it arrives with no category and no
    /// tier. The label used to fall through to the phase, defaulting to
    /// `freeform`; and because `dispatch_route`'s taint arm returns *before* the
    /// phase is stamped, a pinned `implement` turn rendered
    /// `route [freeform] → local`. Wrong about the mode, naming the value ADR-G
    /// retired, and silent about the only thing worth saying: why this turn is
    /// on the local tier.
    #[test]
    fn a_taint_pinned_route_is_labelled_pinned_not_freeform() {
        // Both shapes the pin can arrive in: a freeform session (no phase) and
        // a structured one (the phase never stamped, because the taint arm
        // returns first). Neither may say `freeform`.
        for phase in [None, Some(Phase::Implement)] {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = true;

            render_event(
                &envelope(Event::RouteDecided(RouteDecided {
                    category: None,
                    tier: None,
                    phase,
                    provider_id: ProviderId::from("local"),
                    model: Some("qwen2.5-coder-7b".to_owned()),
                    // The daemon's own `taint_pin_reason` sentence, verbatim. It
                    // names no specific cause since REQ-562 — the pin has three
                    // sources and only one of them is boundary content — and
                    // this renderer keys on the absent category/tier rather than
                    // on the wording, which is what the assertions below check.
                    reason: "an earlier privacy decision in this session; this turn is \
                             pinned to the local tier (BR-1 backstop)"
                        .to_owned(),
                    effort: None,
                    budget_tokens: None,
                    budget_bytes: None,
                    bound: None,
                    bound_floored: None,
                })),
                &mut surface,
                &mut state,
            );

            assert!(
                surface.any_line_contains(LineKind::Notice, "route [pinned]"),
                "a route that consulted no binding must say so ({phase:?})"
            );
            assert!(
                !surface.any_line_contains(LineKind::Notice, "freeform"),
                "`freeform` is the value ADR-G retired, and this session may not \
                 even be one ({phase:?})"
            );
        }
    }

    #[test]
    fn control_events_render_as_one_line_notices() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;

        let events = [
            Event::RouteDecided(RouteDecided {
                category: Some(teton_protocol::Category::Design),
                tier: Some(teton_protocol::Tier::Think),
                phase: Some(Phase::Architect),
                provider_id: ProviderId::from("anthropic"),
                model: Some("claude-opus-4".to_owned()),
                reason: "architecture routes to the frontier tier".to_owned(),
                effort: None,
                budget_tokens: None,
                budget_bytes: None,
                bound: None,
                bound_floored: None,
            }),
            Event::PrivacyBlock(PrivacyBlock {
                path: "secrets/prod.env".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause: BlockCause::Boundary,
            }),
            Event::ProviderDegraded(ProviderDegraded {
                provider_id: ProviderId::from("flaky"),
                failure_class: FailureClass::Timeout,
                fallback_id: Some(ProviderId::from("anthropic")),
            }),
        ];
        for event in events {
            render_event(&envelope(event), &mut surface, &mut state);
        }

        assert!(surface.any_line_contains(LineKind::Notice, "route [design/think]"));
        assert!(surface.any_line_contains(LineKind::Notice, "claude-opus-4"));
        assert!(surface.any_line_contains(LineKind::Notice, "privacy: secrets/prod.env"));
        assert!(surface.any_line_contains(LineKind::Notice, "re-routed to the local tier"));
        assert!(surface.any_line_contains(LineKind::Notice, "degraded: flaky"));
        assert!(surface.any_line_contains(LineKind::Notice, "fell back to anthropic"));
    }

    /// REQ-571 ADR-D: a rejected provenance source reaches the terminal, and it
    /// reaches it *without* the source being able to draw on the terminal.
    ///
    /// Two claims, and the second is the one worth a test: the source is
    /// attacker-influenced text, so a hostile spelling carrying an ANSI escape
    /// and a newline must render as escaped characters on one line rather than
    /// as cursor movement and a forged second notice.
    #[test]
    fn a_rejected_provenance_source_renders_as_inert_escaped_text() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::ProvenanceRejected(ProvenanceRejected {
                source: "/etc/passwd".to_owned(),
                tool: Some("mcp__fs__read_file".to_owned()),
                reason: ProvenanceRejection::Absolute,
            })),
            &mut surface,
            &mut state,
        );
        render_event(
            &envelope(Event::ProvenanceRejected(ProvenanceRejected {
                // A source that would like to end this line and start another.
                source: "\u{1b}[31m../evil\nprivacy: nothing to see here".to_owned(),
                tool: None,
                reason: ProvenanceRejection::ParentTraversal,
            })),
            &mut surface,
            &mut state,
        );

        let lines = surface.lines_of(LineKind::Notice);
        assert_eq!(lines.len(), 2, "one notice per rejection: {lines:?}");

        // The tool is named, the source is quoted, and the reason is a sentence
        // a user can act on.
        assert!(lines[0].contains("mcp__fs__read_file"), "{}", lines[0]);
        assert!(lines[0].contains("\"/etc/passwd\""), "{}", lines[0]);
        assert!(lines[0].contains("absolute"), "{}", lines[0]);

        // Nothing raw survives: no escape byte, no embedded newline.
        assert!(
            !lines[1].contains('\u{1b}'),
            "an escape byte reached the terminal: {:?}",
            lines[1]
        );
        assert!(
            !lines[1].contains('\n'),
            "a hostile source forged a second line: {:?}",
            lines[1]
        );
        // And the guard's line does not invent a tool it cannot know.
        assert!(!lines[1].contains("claimed the source"), "{}", lines[1]);
        assert!(lines[1].contains("`..`"), "{}", lines[1]);
    }

    #[test]
    fn a_hostile_mcp_tool_name_cannot_forge_the_rejection_line() {
        // The `tool` field is a separate attacker channel from `source`: its
        // `mcp__<server>__<tool>` value carries a remote server's tool name
        // verbatim. A prior version escaped `source` but rendered `tool` bare,
        // so a tool named with an ANSI escape + newline could erase this
        // refusal and forge an "all clear" (REQ-571 verify, LESSON-505).
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::ProvenanceRejected(ProvenanceRejected {
                source: "/etc/passwd".to_owned(),
                tool: Some("mcp__evil__\u{1b}[2K\rprivacy: all clear\nmcp__evil__x".to_owned()),
                reason: ProvenanceRejection::Absolute,
            })),
            &mut surface,
            &mut state,
        );

        let lines = surface.lines_of(LineKind::Notice);
        assert_eq!(lines.len(), 1, "one notice, not a forged second: {lines:?}");
        assert!(
            !lines[0].contains('\u{1b}'),
            "an escape byte in the tool name reached the terminal: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains('\n'),
            "a hostile tool name forged a second line: {:?}",
            lines[0]
        );
        // The refusal is still legible and still names its cause.
        assert!(lines[0].contains("refused because"), "{}", lines[0]);
        assert!(lines[0].contains("absolute"), "{}", lines[0]);
    }

    /// The three causes reach the terminal as three different sentences, and the
    /// scan-unavailable one is the sentence BR-3 is actually about: a guard that
    /// could not run is a different problem from a guard that fired, with a
    /// different fix, so the line may not read as a finding.
    #[test]
    fn the_three_block_causes_render_as_three_distinguishable_lines() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        let events = [
            Event::PrivacyBlock(PrivacyBlock {
                path: "secrets/prod.env".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause: BlockCause::Boundary,
            }),
            Event::PrivacyBlock(PrivacyBlock {
                path: "the outbound payload".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause: BlockCause::Redaction {
                    kind: teton_protocol::events::FindingKind::Credential,
                    span: ByteSpan {
                        start: 1400,
                        end: 1436,
                    },
                },
            }),
            Event::PrivacyBlock(PrivacyBlock {
                path: "the outbound payload".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause: BlockCause::ScanUnavailable,
            }),
        ];
        for event in events {
            render_event(&envelope(event), &mut surface, &mut state);
        }

        let lines = surface.lines_of(LineKind::Notice);
        assert_eq!(lines.len(), 3, "one notice per block: {lines:?}");
        let unique: HashSet<&str> = lines.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "the causes must not share a line: {lines:?}"
        );

        let (boundary, redaction, unavailable) = (lines[0], lines[1], lines[2]);

        // Boundary keeps the sentence it has always had.
        assert!(boundary.contains("secrets/prod.env would have reached anthropic"));

        // A redaction block reports kind and byte span — the whole vocabulary it
        // has, because there is no matched text on the event to print (BR-6).
        assert!(redaction.contains("detected a credential"), "{redaction}");
        assert!(redaction.contains("bytes 1400–1436"), "{redaction}");

        // And the unavailable line says the scan could not run, without ever
        // claiming something was detected.
        assert!(unavailable.contains("could not run"), "{unavailable}");
        assert!(unavailable.contains("unscanned"), "{unavailable}");
        assert!(
            !unavailable.contains("detected"),
            "a scan that never ran cannot have detected anything: {unavailable}"
        );
    }

    #[test]
    fn route_notices_are_suppressed_by_default_but_warnings_still_render() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        let events = [
            Event::RouteDecided(RouteDecided {
                category: None,
                tier: None,
                phase: None,
                provider_id: ProviderId::from("local"),
                model: None,
                reason: "coding turn goes to the default provider".to_owned(),
                effort: None,
                budget_tokens: None,
                budget_bytes: None,
                bound: None,
                bound_floored: None,
            }),
            Event::PrivacyBlock(PrivacyBlock {
                path: "secrets/prod.env".to_owned(),
                provider_id: ProviderId::from("anthropic"),
                action: PrivacyAction::ReroutedToLocal,
                cause: BlockCause::Boundary,
            }),
            Event::ProviderDegraded(ProviderDegraded {
                provider_id: ProviderId::from("flaky"),
                failure_class: FailureClass::Timeout,
                fallback_id: None,
            }),
        ];
        for event in events {
            render_event(&envelope(event), &mut surface, &mut state);
        }

        assert!(!surface.any_line_contains(LineKind::Notice, "route ["));
        assert!(surface.any_line_contains(LineKind::Notice, "privacy: secrets/prod.env"));
        assert!(surface.any_line_contains(LineKind::Notice, "degraded: flaky"));
    }

    #[test]
    fn tool_calls_render_and_updates_reuse_the_title() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::ToolCall {
                    tool_call_id: "c1".to_owned(),
                    title: "read src/main.rs".to_owned(),
                    status: ToolCallStatus::Pending,
                },
            })),
            &mut surface,
            &mut state,
        );
        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::ToolCallUpdate {
                    tool_call_id: "c1".to_owned(),
                    status: ToolCallStatus::Completed,
                },
            })),
            &mut surface,
            &mut state,
        );

        let tools = surface.lines_of(LineKind::Tool);
        assert_eq!(tools.len(), 2);
        assert!(tools[0].contains("read src/main.rs [pending]"));
        // The update reuses the remembered title rather than the raw id.
        assert!(tools[1].contains("read src/main.rs [done]"));
    }

    #[test]
    fn diff_renders_removed_and_added_lines() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::Diff {
                    path: "src/a.rs".to_owned(),
                    old_text: Some("fn a() {}".to_owned()),
                    new_text: "fn a() { 1 }".to_owned(),
                },
            })),
            &mut surface,
            &mut state,
        );
        let diff = surface.lines_of(LineKind::Diff);
        assert!(diff.iter().any(|l| l.contains("± src/a.rs")));
        assert!(diff.contains(&"- fn a() {}"));
        assert!(diff.contains(&"+ fn a() { 1 }"));
    }

    #[test]
    fn plan_entries_render() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::Plan {
                    entries: vec![PlanEntry {
                        content: "write tests".to_owned(),
                        status: PlanEntryStatus::InProgress,
                    }],
                },
            })),
            &mut surface,
            &mut state,
        );
        assert!(surface.any_line_contains(LineKind::Info, "plan:"));
        assert!(surface.any_line_contains(LineKind::Info, "write tests"));
    }

    #[test]
    fn cost_recorded_events_feed_the_meter_without_rendering_noise() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::CostRecorded(CostRecorded {
                record: CostRecord {
                    session_id: SessionId::from("s1"),
                    phase: Some(Phase::Review),
                    category: None,
                    provider_id: ProviderId::from("anthropic"),
                    model: "claude-opus-4".to_owned(),
                    input_tokens: 1000,
                    output_tokens: 500,
                    usd_micros: 45_000,
                    cached_tokens: None,
                    reasoning_tokens: None,
                    probe: false,
                },
            })),
            &mut surface,
            &mut state,
        );
        assert_eq!(state.cost.len(), 1);
    }

    fn permission_request(tool: &str) -> PermissionRequest {
        PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: tool.to_owned(),
            description: Some("run `cargo test`".to_owned()),
            subject: None,
            options: vec![
                PermissionOption {
                    option_id: "allow_once".to_owned(),
                    label: "Allow once".to_owned(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "allow_always".to_owned(),
                    label: "Allow for session".to_owned(),
                    kind: PermissionOptionKind::AllowAlways,
                },
                PermissionOption {
                    option_id: "reject_once".to_owned(),
                    label: "Reject once".to_owned(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
        }
    }

    #[test]
    fn a_model_proposal_is_handed_back_rather_than_rendered_here() {
        // The owning client renders and answers in one step (it must not be
        // painted twice, once by the pump and once by the prompt).
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(
            &envelope(Event::ModelSelectionProposed(
                crate::model_ui::testing::proposal(),
            )),
            &mut surface,
            &mut state,
        );
        match outcome {
            EventOutcome::ModelProposal(proposal) => {
                assert_eq!(proposal.request_id, RequestId::from("req-model-1"));
            }
            other => panic!("expected a model proposal, got {other:?}"),
        }
        assert!(surface.calls.is_empty(), "the pump renders nothing itself");
    }

    #[test]
    fn a_model_decision_renders_as_a_notice_for_every_attached_client() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(
            &envelope(Event::ModelSelectionDecided(ModelSelectionDecided {
                request_id: Some(RequestId::from("req-model-1")),
                model_name: Some("qwen2.5-coder-7b".to_owned()),
                declined_local: false,
                source: SelectionSource::UserOverride,
            })),
            &mut surface,
            &mut state,
        );
        assert!(matches!(outcome, EventOutcome::Rendered));
        assert!(surface.any_line_contains(LineKind::Notice, "qwen2.5-coder-7b"));
        assert!(surface.any_line_contains(LineKind::Notice, "user override"));
    }

    /// REQ-564: prefix-cache outcomes are verbose-only diagnostic chrome. A
    /// user who did not ask has nothing to act on — BR-1 makes reuse
    /// unobservable in output — so a quiet session must stay quiet.
    #[test]
    fn a_prefix_cache_event_is_silent_unless_verbose() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let event = || {
            envelope(Event::PrefixCache(PrefixCache {
                model: "qwen2.5-coder-3b".to_owned(),
                outcome: PrefixCacheOutcome::Hit {
                    cached_tokens: 15_000,
                    new_tokens: 84,
                    divergent: false,
                },
            }))
        };

        let outcome = render_event(&event(), &mut surface, &mut state);
        assert!(matches!(outcome, EventOutcome::Rendered));
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "a non-verbose session must not narrate cache hits"
        );

        state.verbose = true;
        render_event(&event(), &mut surface, &mut state);
        assert!(surface.any_line_contains(LineKind::Notice, "15000"));
        assert!(surface.any_line_contains(LineKind::Notice, "84"));
    }

    /// **BR-7's inverse of [`a_prefix_cache_event_is_silent_unless_verbose`].**
    ///
    /// A cache hit is chrome about *how* a turn ran; pressure is a change to
    /// *what the turn was given*, so it draws its line in a default session and
    /// draws exactly one. "Nothing is clamped in silence, on any tier" is not a
    /// property a `/verbose` gate can have, and this test is what fails if one
    /// is ever added — the `context_cleared` arrangement, one event over.
    #[test]
    fn a_context_pressure_event_is_never_silent() {
        let event = || {
            envelope(Event::ContextPressure(ContextPressure {
                kind: ContextPressureKind::BlocksDropped,
                dropped_blocks: 3,
                elided_bytes: 0,
                newest_user_elided: false,
                budget_tokens: 4_096,
                budget_bytes: 32_768,
                bound: BudgetBound::LocalEngine,
                bound_floored: false,
            }))
        };

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(&event(), &mut surface, &mut state);
        assert!(matches!(outcome, EventOutcome::Rendered));
        let quiet = surface.lines_of(LineKind::Notice);
        assert_eq!(
            quiet.len(),
            1,
            "a quiet session must still be told what was dropped: {:?}",
            surface.calls
        );
        assert!(
            quiet[0].starts_with("context: 3 older blocks dropped"),
            "{quiet:?}"
        );

        // And verbose does not double it — the line is the same line.
        let mut surface = RecordingSurface::new();
        state.verbose = true;
        render_event(&event(), &mut surface, &mut state);
        assert_eq!(surface.lines_of(LineKind::Notice), quiet);
    }

    /// The four shapes, each naming the budget it was fitted to and the bound
    /// that decided it — and the elision naming *which* block, because the
    /// newest user block is the case where the model answers a prompt the user
    /// did not send (BR-7).
    #[test]
    fn each_pressure_shape_names_the_budget_and_its_bound() {
        let pressure = |kind, dropped_blocks, elided_bytes, newest_user_elided, bound| {
            format_context_pressure(&ContextPressure {
                kind,
                dropped_blocks,
                elided_bytes,
                newest_user_elided,
                budget_tokens: 4_096,
                budget_bytes: 32_768,
                bound,
                bound_floored: false,
            })
        };

        assert_eq!(
            pressure(
                ContextPressureKind::BlocksDropped,
                3,
                0,
                false,
                BudgetBound::LocalEngine
            ),
            "context: 3 older blocks dropped to fit the 4,096-word budget (bound: local engine)"
        );
        // Singular is worth the branch for the same reason `context_cleared`'s
        // is: "1 older blocks" reads as a bug in the line, not in the count.
        assert!(pressure(
            ContextPressureKind::BlocksDropped,
            1,
            0,
            false,
            BudgetBound::Window
        )
        .contains("1 older block dropped"));
        assert_eq!(
            pressure(
                ContextPressureKind::BlockElided,
                0,
                12_288,
                true,
                BudgetBound::LocalEngine
            ),
            "context: newest message middle-elided by 12 KB to fit the 4,096-word budget \
             (bound: local engine)"
        );
        assert!(pressure(
            ContextPressureKind::BlockElided,
            0,
            12_288,
            false,
            BudgetBound::LocalEngine
        )
        .starts_with("context: an older message middle-elided"));
        assert_eq!(
            pressure(
                ContextPressureKind::RefitOnReroute,
                7,
                0,
                false,
                BudgetBound::LocalEngine
            ),
            "context: re-fitted to the 4,096-word budget after a reroute (bound: local engine) \
             — 7 older blocks dropped"
        );
        assert!(pressure(
            ContextPressureKind::RefitOnReroute,
            0,
            0,
            false,
            BudgetBound::UserCap
        )
        .ends_with("(bound: user cap) — nothing dropped"));
        // **TASK-194 2a.** The gate ran and could do nothing. It used to be
        // announced as an elision of zero bytes — "an older message
        // middle-elided by 0 B" — which described something that did not
        // happen, on the one surface BR-7 exists to keep honest.
        assert_eq!(
            pressure(
                ContextPressureKind::DidNotFit,
                0,
                0,
                false,
                BudgetBound::Window
            ),
            "context: could not be fitted to the 4,096-word budget (bound: window) — the turn \
             was sent over budget"
        );
        // What the gate managed trails the fact that it was not enough — the
        // one line that must never end "to fit the …", because the fitting is
        // exactly what failed.
        assert_eq!(
            pressure(
                ContextPressureKind::DidNotFit,
                3,
                512,
                false,
                BudgetBound::Window
            ),
            "context: could not be fitted to the 4,096-word budget (bound: window) — the turn \
             was sent over budget after dropping 3 older blocks and eliding 512 B"
        );
    }

    /// **TASK-194 2b.** A bound the floor overruled says so, on both surfaces
    /// that name a bound — and a bound that is genuinely in force renders
    /// exactly as it did before.
    ///
    /// The untruth this closes is small and complete: `bound: user cap` printed
    /// beside a budget *larger* than that cap. The daemon decides `floored`
    /// where it derives the budget; the clause below never compares a pair to a
    /// floor of its own (BR-8).
    #[test]
    fn a_bound_the_floor_overruled_is_rendered_as_overruled() {
        let line = |bound_floored| {
            format_context_pressure(&ContextPressure {
                kind: ContextPressureKind::BlocksDropped,
                dropped_blocks: 1,
                elided_bytes: 0,
                newest_user_elided: false,
                budget_tokens: 2_048,
                budget_bytes: 16_384,
                bound: BudgetBound::UserCap,
                bound_floored,
            })
        };
        assert_eq!(
            line(false),
            "context: 1 older block dropped to fit the 2,048-word budget (bound: user cap)"
        );
        assert_eq!(
            line(true),
            "context: 1 older block dropped to fit the 2,048-word budget (bound: user cap — \
             floored: below the smallest budget that holds the system prompt)"
        );

        let route = |bound_floored| {
            format_route(&RouteDecided {
                category: None,
                tier: None,
                phase: None,
                provider_id: ProviderId::from("kimi"),
                model: Some("kimi-k3".to_owned()),
                reason: "a reason.".to_owned(),
                effort: None,
                budget_tokens: Some(2_048),
                budget_bytes: Some(16_384),
                bound: Some(BudgetBound::UserCap),
                bound_floored,
            })
        };
        assert!(
            route(Some(false)).ends_with(" · budget 2,048 words / 16 KB (bound: user cap)"),
            "{}",
            route(Some(false))
        );
        assert!(
            route(Some(true)).ends_with(
                " · budget 2,048 words / 16 KB (bound: user cap — floored: below the smallest \
                 budget that holds the system prompt)"
            ),
            "{}",
            route(Some(true))
        );
        // A daemon predating the field states nothing, and states it as
        // "not floored" — today's line, byte for byte.
        assert_eq!(route(None), route(Some(false)));
    }

    /// Every bound has its own words, and the wire's `default_unknown` is said
    /// as the thing a user would go and fix (BR-8: one fact, one source — this
    /// table is the only place it is spelled for a person).
    #[test]
    fn every_bound_has_its_own_words() {
        let said: Vec<&str> = [
            BudgetBound::Window,
            BudgetBound::DefaultUnknown,
            BudgetBound::RedactScan,
            BudgetBound::UserCap,
            BudgetBound::LocalEngine,
        ]
        .into_iter()
        .map(bound_words)
        .collect();
        assert_eq!(
            said,
            [
                "window",
                "unknown window",
                "redact scan",
                "user cap",
                "local engine"
            ]
        );
        assert_eq!(said.iter().collect::<HashSet<_>>().len(), said.len());
    }

    /// **AC-4/AC-8's client half.** A route line under `/verbose` carries the
    /// budget in both currencies and names the bound; a `route_decided` from a
    /// daemon that states none renders the pre-REQ-586 line byte for byte.
    #[test]
    fn a_route_line_carries_the_budget_when_the_daemon_states_one() {
        let route = |budget: Option<(u64, u64, BudgetBound)>| {
            let (budget_tokens, budget_bytes, bound) = match budget {
                Some((t, b, bound)) => (Some(t), Some(b), Some(bound)),
                None => (None, None, None),
            };
            format_route(&RouteDecided {
                category: Some(teton_protocol::Category::Edit),
                tier: Some(teton_protocol::Tier::Build),
                phase: None,
                provider_id: ProviderId::from("kimi"),
                model: Some("kimi-k3".to_owned()),
                reason: "a reason.".to_owned(),
                effort: None,
                budget_tokens,
                budget_bytes,
                bound,
                bound_floored: None,
            })
        };

        let bare = route(None);
        assert_eq!(bare, "route [edit/build] → kimi kimi-k3 — a reason.");
        // A plausible redact-scan pair, rounded on purpose: the CLI renders the
        // numbers the daemon states and holds no copy of the scannable bound —
        // whose one home is the daemon's `REDACT_SCANNABLE_CONTEXT_BYTES`, a
        // constant this crate cannot even see (TASK-192's one-home grep).
        assert_eq!(
            route(Some((84_650, 89_000, BudgetBound::RedactScan))),
            format!("{bare} · budget 84,650 words / 89 KB (bound: redact scan)")
        );
        // AC-8: a cap below the window is what the line says bound the budget.
        assert!(route(Some((26_650, 79_952, BudgetBound::UserCap))).ends_with("(bound: user cap)"));
        // A partial set is a daemon mid-upgrade, not half a clause.
        assert_eq!(
            format_route(&RouteDecided {
                category: None,
                tier: None,
                phase: None,
                provider_id: ProviderId::from("kimi"),
                model: None,
                reason: "a reason.".to_owned(),
                effort: None,
                budget_tokens: Some(4_096),
                budget_bytes: None,
                bound: Some(BudgetBound::LocalEngine),
                bound_floored: None,
            }),
            "route [pinned] → kimi (model tbd) — a reason."
        );
    }

    /// The two figure formatters, at the boundaries that decide a unit — and
    /// that this crate's wrappers really do reach them.
    ///
    /// The golden table itself lives beside the implementations, in
    /// `teton_protocol::events` (verify: it stayed here when they moved, so
    /// dropping the KB rounding survived `cargo test -p teton-protocol`). What
    /// is asserted *here* is the delegation: a wrapper that grew a second
    /// implementation would pass the protocol crate's table and fail this.
    #[test]
    fn the_figure_wrappers_delegate_to_the_protocols_formatters() {
        for n in [0u64, 999, 4_096, 132_650, 1_050_000] {
            assert_eq!(thousands(n), events::thousands(n));
        }
        for bytes in [0u64, 999, 1_000, 32_768, 999_999, 1_000_000, 4_200_000] {
            assert_eq!(bytes_figure(bytes), events::bytes_figure(bytes));
        }
    }

    /// A divergent hit says so: the prefill was bigger than the turn's delta
    /// because history was rewritten, and a user chasing latency needs that
    /// distinction just as BR-8 demands it for misses.
    #[test]
    fn a_divergent_hit_names_the_history_change() {
        let plain = format_prefix_cache(&PrefixCache {
            model: "m".to_owned(),
            outcome: PrefixCacheOutcome::Hit {
                cached_tokens: 100,
                new_tokens: 8,
                divergent: false,
            },
        });
        let divergent = format_prefix_cache(&PrefixCache {
            model: "m".to_owned(),
            outcome: PrefixCacheOutcome::Hit {
                cached_tokens: 100,
                new_tokens: 8,
                divergent: true,
            },
        });
        assert!(!plain.contains("history change"));
        assert!(divergent.contains("history change"));
    }

    /// Every miss reason renders its own sentence. Folding them into one
    /// "cache miss" line would hide the difference between "history was
    /// rewritten" and "another session took the slot" — the two a user chasing
    /// a slow turn most needs to tell apart (BR-8).
    #[test]
    fn each_miss_reason_renders_a_distinguishable_sentence() {
        let mut rendered = Vec::new();
        for reason in [
            PrefixCacheMiss::Cold,
            PrefixCacheMiss::SessionSwitch,
            PrefixCacheMiss::Divergent,
            PrefixCacheMiss::Evicted,
        ] {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = true;
            render_event(
                &envelope(Event::PrefixCache(PrefixCache {
                    model: "qwen2.5-coder-3b".to_owned(),
                    outcome: PrefixCacheOutcome::Miss {
                        reason,
                        processed_tokens: 2_048,
                    },
                })),
                &mut surface,
                &mut state,
            );
            let line = surface
                .lines_of(LineKind::Notice)
                .first()
                .map(|text| (*text).to_owned())
                .expect("a verbose miss renders a line");
            assert!(line.contains("2048"), "the line names the prefilled count");
            rendered.push(line);
        }
        rendered.sort();
        rendered.dedup();
        assert_eq!(
            rendered.len(),
            4,
            "two miss reasons rendered the same sentence: {rendered:?}"
        );
    }

    #[test]
    fn a_proposal_is_claimed_once_so_the_late_attach_path_cannot_double_prompt() {
        let mut state = SessionState::new();
        let id = RequestId::from("req-model-1");
        assert!(state.claim_model_proposal(&id), "first sighting wins");
        assert!(
            !state.claim_model_proposal(&id),
            "the same proposal seen again (event, then model/status) is dropped"
        );
        assert!(state.claim_model_proposal(&RequestId::from("req-model-2")));
    }

    #[test]
    fn permission_request_becomes_a_permission_outcome() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(
            &envelope(Event::PermissionRequest(permission_request("shell"))),
            &mut surface,
            &mut state,
        );
        assert!(matches!(outcome, EventOutcome::Permission(_)));
    }

    #[test]
    fn permission_yes_selects_allow_once() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(resp.request_id, RequestId::from("r1"));
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        );
        assert!(surface.any_line_contains(LineKind::Prompt, "permission requested: shell"));
    }

    #[test]
    fn permission_no_selects_reject_once() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["n"]);
        let mut grants = SessionGrants::default();
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "reject_once".to_owned()
            }
        );
    }

    #[test]
    fn permission_eof_cancels() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut grants = SessionGrants::default();
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(resp.outcome, PermissionOutcome::Cancelled);
    }

    #[test]
    fn always_grant_is_session_scoped_and_auto_applies() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        // Only ONE scripted answer ("a"). The second request must resolve from
        // the remembered grant, consuming no further prompt.
        let mut prompter = ScriptedPrompter::new(&["a"]);
        let mut grants = SessionGrants::default();

        let first = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            first.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned()
            }
        );
        assert!(grants.is_allow_always("shell"));

        let second = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            second.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned()
            }
        );
        // The auto-decision did not consume a second scripted answer.
        assert_eq!(prompter.asked, 1);
        assert!(surface.any_line_contains(LineKind::Prompt, "auto-allow shell"));
    }

    #[test]
    fn deny_always_is_session_scoped_and_auto_applies() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["d"]);
        let mut grants = SessionGrants::default();

        let first = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            first.outcome,
            PermissionOutcome::Selected {
                option_id: "reject_once".to_owned() // no reject_always offered → falls back
            }
        );
        assert!(grants.is_reject_always("shell"));

        let second = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert!(matches!(second.outcome, PermissionOutcome::Selected { .. }));
        assert_eq!(prompter.asked, 1);
    }

    #[test]
    fn invalid_answer_reprompts_then_accepts() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["huh?", "y"]);
        let mut grants = SessionGrants::default();
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        );
        assert_eq!(prompter.asked, 2);
    }

    // ------------------------------------------------------------------
    // REQ-563: web state, web notices, and the persistent consent key
    // ------------------------------------------------------------------

    /// A web consent prompt as the daemon raises it: the grant key is one of
    /// the **per-tier** web keys (REQ-563 BR-3), never a single `web_fetch`.
    fn web_permission_request() -> PermissionRequest {
        let mut req = permission_request("web_fetch_any_url");
        req.options.insert(
            2,
            PermissionOption {
                option_id: OPTION_ID_ENABLE_PERMANENT.to_owned(),
                label: "Enable permanently".to_owned(),
                kind: PermissionOptionKind::AllowAlways,
            },
        );
        req
    }

    fn lookup(kind: WebLookupKind, outcome: WebLookupOutcome) -> Event {
        Event::WebLookup(WebLookup {
            kind,
            host: "docs.rs".to_owned(),
            outcome,
            bytes_in: 4_096,
            cause: None,
        })
    }

    /// AC-6 / AC-12: the four status strings, and the precedence between them.
    #[test]
    fn the_status_field_renders_every_web_state() {
        let off = WebState::default();
        assert_eq!(off.status_field(), "web: off");
        assert!(
            !off.is_engaged(),
            "an opted-out session draws no row (BR-1)"
        );

        let mut fetch = WebState::default();
        fetch.observe_tier(WebTier::FetchUserUrl);
        assert_eq!(fetch.status_field(), "web: fetch");
        fetch.observe_tier(WebTier::FetchAnyUrl);
        assert_eq!(fetch.status_field(), "web: fetch");
        assert!(fetch.is_engaged());

        let mut search = fetch;
        search.observe_tier(WebTier::Search);
        assert_eq!(search.status_field(), "web: search");

        // A taint trip outranks the tier: saying `web: search` while search is
        // refused would contradict the notice the user just read.
        let mut restricted = search;
        restricted.restricted = true;
        assert_eq!(restricted.status_field(), "web: restricted (taint)");
        assert!(restricted.is_engaged());

        // And the override outranks the restriction it lifted.
        let mut overridden = restricted;
        overridden.overridden = true;
        assert_eq!(overridden.status_field(), "web: overridden");
        assert!(overridden.is_engaged());
    }

    /// REQ-572 AC-4: the two states this REQ exists to tell apart no longer
    /// share a spelling — "off" (no answer from the daemon) and "off, and one
    /// command away" are different strings, and a configured ceiling says so
    /// without claiming a grant this session does not hold.
    #[test]
    fn the_capability_field_tells_off_from_off_but_available() {
        // No answer: a daemon that predates the field, or a snapshot nobody has
        // read yet. The field says what it always said.
        let unknown = WebState::default();
        assert_eq!(unknown.status_field(), "web: off");

        let available = WebState {
            capability: Some(WebCapabilityState::OffAvailable),
            ..WebState::default()
        };
        assert_eq!(available.status_field(), "web: off (available)");

        for (tier, expected) in [
            (WebTier::FetchUserUrl, "web: fetch (configured)"),
            (WebTier::FetchAnyUrl, "web: fetch (configured)"),
            (WebTier::Search, "web: search (configured)"),
        ] {
            let ready = WebState {
                capability: Some(WebCapabilityState::Ready { tier }),
                ..WebState::default()
            };
            assert_eq!(ready.status_field(), expected, "{tier:?}");
        }

        let gap = WebState {
            capability: Some(WebCapabilityState::SearchUnavailable {
                reason: "search needs the local model".to_owned(),
            }),
            ..WebState::default()
        };
        assert_eq!(gap.status_field(), "web: search (unavailable)");
    }

    /// The row's **visibility** rule is REQ-563's and this REQ did not move it:
    /// a session that has not touched the web draws no row, whatever the machine
    /// is configured for. It is asserted rather than left implicit because the
    /// alternative — a permanent capability row above every prompt — is a
    /// one-arm change away, and it should be a decision somebody takes rather
    /// than one that arrives with an edit.
    #[test]
    fn the_capability_alone_never_makes_the_row_appear() {
        for capability in [
            WebCapabilityState::OffAvailable,
            WebCapabilityState::Ready {
                tier: WebTier::Search,
            },
            WebCapabilityState::SearchUnavailable {
                reason: "search needs the local model".to_owned(),
            },
        ] {
            let state = WebState {
                capability: Some(capability.clone()),
                ..WebState::default()
            };
            assert!(
                !state.is_engaged(),
                "the machine's configuration is not something this session did: {capability:?}"
            );
        }

        // Non-vacuity: the things that *are* this session's still engage it.
        let mut used = WebState {
            capability: Some(WebCapabilityState::Ready {
                tier: WebTier::Search,
            }),
            ..WebState::default()
        };
        used.observe_tier(WebTier::FetchUserUrl);
        assert!(used.is_engaged());
        assert_eq!(used.status_field(), "web: fetch");
    }

    /// The REQ-563 precedence is untouched: what the **session** did outranks
    /// what the machine is configured for, in all three directions. A row that
    /// announced a configured ceiling over a taint restriction would contradict
    /// the notice the user had just read.
    #[test]
    fn the_configured_capability_never_outranks_what_the_session_did() {
        let configured = || {
            Some(WebCapabilityState::Ready {
                tier: WebTier::Search,
            })
        };

        let mut granted = WebState {
            capability: configured(),
            ..WebState::default()
        };
        granted.observe_tier(WebTier::FetchUserUrl);
        assert_eq!(
            granted.status_field(),
            "web: fetch",
            "an observed grant is what this session actually holds"
        );

        let restricted = WebState {
            restricted: true,
            capability: configured(),
            ..WebState::default()
        };
        assert_eq!(restricted.status_field(), "web: restricted (taint)");

        let overridden = WebState {
            overridden: true,
            restricted: true,
            capability: configured(),
            ..WebState::default()
        };
        assert_eq!(overridden.status_field(), "web: overridden");
    }

    /// The ceiling only ever rises: a refusal after a grant must not read as a
    /// downgrade, and `off` is never "observed".
    #[test]
    fn the_observed_tier_never_falls() {
        let mut state = WebState::default();
        state.observe_tier(WebTier::Search);
        state.observe_tier(WebTier::FetchUserUrl);
        assert_eq!(state.status_field(), "web: search");
        state.observe_tier(WebTier::Off);
        assert_eq!(state.status_field(), "web: search");
    }

    /// BR-13's never-silent rule, and BUG-152's class rule in the same test: the
    /// taint refusal renders without `--verbose`, names the cause and the
    /// effect, and is a Notice rather than an error.
    #[test]
    fn a_taint_restriction_names_cause_and_effect_and_is_not_an_error() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        assert!(!state.verbose, "the default session, deliberately");

        render_event(
            &envelope(lookup(
                WebLookupKind::Search,
                WebLookupOutcome::TaintRestricted,
            )),
            &mut surface,
            &mut state,
        );

        let notices = surface.lines_of(LineKind::Notice).join("\n");
        // Cause.
        assert!(
            notices.contains("read privacy-boundary content"),
            "the notice must name what caused the restriction: {notices}"
        );
        // Effect.
        assert!(
            notices.contains("model-composed web lookups"),
            "the notice must name what was disabled: {notices}"
        );
        assert!(
            notices.contains("disabled"),
            "the effect must read as a capability lost: {notices}"
        );
        // The way out, so the notice is not a dead end.
        assert!(notices.contains("/web allow"), "{notices}");
        // BUG-152: nothing broke, so nothing wears an `error:` prefix.
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a refusal is this capability working: {:?}",
            surface.calls
        );
        // And the status row reflects it for the rest of the session.
        assert_eq!(state.web.status_field(), "web: restricted (taint)");
    }

    /// BR-7's `/verbose` clause: the routine per-lookup line is chrome behind
    /// the same flag the routing notices use, and every refusal is not.
    #[test]
    fn routine_lookups_are_verbose_gated_and_refusals_always_render() {
        let routine = [WebLookupOutcome::Completed, WebLookupOutcome::CacheHit];

        for outcome in WebLookupOutcome::ALL {
            let mut quiet = RecordingSurface::new();
            let mut state = SessionState::new();
            render_event(
                &envelope(lookup(WebLookupKind::Fetch, outcome)),
                &mut quiet,
                &mut state,
            );
            let drew = !quiet.lines_of(LineKind::Notice).is_empty();
            assert_eq!(
                drew,
                !routine.contains(&outcome),
                "{outcome:?} rendered the wrong way in a default session: {:?}",
                quiet.calls
            );

            // With `--verbose` every outcome has a line, and it names the host.
            let mut loud = RecordingSurface::new();
            let mut verbose = SessionState::new();
            verbose.verbose = true;
            render_event(
                &envelope(lookup(WebLookupKind::Fetch, outcome)),
                &mut loud,
                &mut verbose,
            );
            assert!(
                loud.any_line_contains(LineKind::Notice, "docs.rs"),
                "{outcome:?} drew no verbose line naming the host: {:?}",
                loud.calls
            );
            assert!(
                loud.lines_of(LineKind::Error).is_empty(),
                "{outcome:?} rendered as an error (BUG-152): {:?}",
                loud.calls
            );
        }
    }

    /// Every outcome gets a line of its own — a renderer that folded two of them
    /// onto one sentence would make the ledger and the transcript disagree about
    /// what happened.
    #[test]
    fn the_eight_lookup_outcomes_render_as_eight_distinguishable_lines() {
        let seen: HashSet<String> = WebLookupOutcome::ALL
            .into_iter()
            .map(|outcome| {
                format_web_lookup(
                    &WebLookup {
                        kind: WebLookupKind::Fetch,
                        host: "docs.rs".to_owned(),
                        outcome,
                        bytes_in: 0,
                        cause: None,
                    },
                    true,
                )
                .expect("verbose renders every outcome")
            })
            .collect();
        assert_eq!(
            seen.len(),
            WebLookupOutcome::ALL.len(),
            "two outcomes render identically: {seen:?}"
        );
    }

    /// **BR-14's honesty half: a scan that could not run is not a scan that
    /// refused the text.**
    ///
    /// `blocked_redact` folds two facts with two different fixes. Told "the
    /// redaction scan refused the outgoing text" when the truth is "no local
    /// model is loaded", the user goes hunting for a secret in a query that
    /// contained none — and the actual remedy, which is the one thing they can
    /// act on, is never named.
    #[test]
    fn a_blocked_search_names_the_missing_local_model_rather_than_a_refusal() {
        let line = |kind, cause| {
            format_web_lookup(
                &WebLookup {
                    kind,
                    host: "search.example".to_owned(),
                    outcome: WebLookupOutcome::BlockedRedact,
                    bytes_in: 0,
                    cause,
                },
                false,
            )
            .expect("a block always renders, verbose or not")
        };

        let unavailable = line(WebLookupKind::Search, Some(BlockCause::ScanUnavailable));
        assert!(
            unavailable.contains("local model"),
            "the cause the user can act on is unnamed: {unavailable}"
        );
        assert!(
            !unavailable.contains("refused the outgoing text"),
            "the scan did not refuse anything — it never ran: {unavailable}"
        );

        // The other reading keeps its own sentence, so the two do not collapse.
        let found = line(
            WebLookupKind::Search,
            Some(BlockCause::Redaction {
                kind: FindingKind::Secret,
                span: ByteSpan { start: 0, end: 8 },
            }),
        );
        assert!(found.contains("refused the outgoing text"), "{found}");
        assert_ne!(unavailable, found, "the two causes read identically");

        // A daemon that sends no cause (an older build, or a path with none)
        // falls back to the general sentence rather than claiming a diagnosis.
        assert_eq!(line(WebLookupKind::Search, None), found);

        // A **fetch** whose scan could not run has a different remedy: it is
        // scanned at provider parity under `[privacy] redact`, not because of
        // BR-14's search coupling, so the search sentence would send the user to
        // install a model they may not need.
        let fetch = line(WebLookupKind::Fetch, Some(BlockCause::ScanUnavailable));
        assert!(fetch.contains("local model"), "{fetch}");
        assert!(
            fetch.contains("[privacy] redact"),
            "a fetch block must name the switch that turned the scan on: {fetch}"
        );
        assert!(
            !fetch.contains("web search"),
            "a fetch was explained as a search: {fetch}"
        );
    }

    /// Only a lookup that ran proves a tier was held. A refusal proves the
    /// **REQ-584 BR-11 / AC-12.** A matched `projects` call ends the turn with
    /// the recipe — from the daemon's record, not the model's prose.
    ///
    /// Driven through `render_event` rather than through the formatter, for the
    /// reason BUG-189's sibling test records: "the line reaches the surface" is
    /// a claim about the dispatch, and a test that called
    /// `project_handoff_line` directly would stay green with the arm deleted.
    #[test]
    fn a_matched_project_ends_the_turn_with_the_cd_recipe() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::ProjectMatch(events::ProjectMatch {
                name: "teton-code".to_owned(),
                display: "~/Documents/GitHub/teton-code".to_owned(),
            })),
            &mut surface,
            &mut state,
        );

        assert!(
            surface.any_line_contains(LineKind::Notice, "→ /cd teton-code"),
            "the hand-off must name the command that moves the session: {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert!(
            surface.any_line_contains(LineKind::Notice, "~/Documents/GitHub/teton-code"),
            "and where it goes: {:?}",
            surface.lines_of(LineKind::Notice)
        );
    }

    /// **AC-12's negative half.** No event, no line.
    ///
    /// The daemon publishes only on a match, so "a turn that did not call the
    /// tool, or called it and found nothing, prints nothing" is the absence of
    /// this event — which is what this asserts, over an unrelated event so the
    /// surface is not merely empty for want of anything happening.
    #[test]
    fn a_turn_without_a_project_match_prints_no_hand_off() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::ContextCleared(events::ContextCleared {
                blocks_dropped: 1,
            })),
            &mut surface,
            &mut state,
        );
        assert!(
            !surface.fragments().contains("→ /cd "),
            "a turn with no project match must print no hand-off: {}",
            surface.fragments()
        );
    }

    /// opposite, and must never raise the status row's reading.
    #[test]
    fn only_a_lookup_that_ran_raises_the_status_field() {
        for outcome in WebLookupOutcome::ALL {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            render_event(
                &envelope(lookup(WebLookupKind::Search, outcome)),
                &mut surface,
                &mut state,
            );
            let ran = matches!(
                outcome,
                WebLookupOutcome::Completed | WebLookupOutcome::CacheHit
            );
            if ran {
                assert_eq!(state.web.status_field(), "web: search", "{outcome:?}");
            } else if outcome == WebLookupOutcome::TaintRestricted {
                assert_eq!(state.web.status_field(), "web: restricted (taint)");
            } else {
                assert_eq!(
                    state.web.status_field(),
                    "web: off",
                    "{outcome:?} is a refusal and grants nothing"
                );
            }
        }
    }

    /// A consent decision always renders — it is the user's own answer coming
    /// back — and the permanent one says that it wrote config.
    #[test]
    fn consent_decisions_render_and_the_permanent_one_names_the_write() {
        for (scope, granted, expect) in [
            (WebConsentScope::Once, true, "for this lookup"),
            (WebConsentScope::Session, true, "rest of this session"),
            (WebConsentScope::Persistent, true, "written to your config"),
            (WebConsentScope::Once, false, "declined"),
            (WebConsentScope::Session, false, "declined for the rest"),
        ] {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            assert!(!state.verbose, "consent is never verbose-gated");
            render_event(
                &envelope(Event::WebConsentDecided(WebConsentDecided {
                    scope,
                    tier: WebTier::FetchAnyUrl,
                    granted,
                })),
                &mut surface,
                &mut state,
            );
            assert!(
                surface.any_line_contains(LineKind::Notice, expect),
                "{scope:?}/{granted} rendered wrongly: {:?}",
                surface.calls
            );
            assert!(
                surface.any_line_contains(LineKind::Notice, "fetch_any_url"),
                "a decision must name the tier it concerns: {:?}",
                surface.calls
            );
            assert_eq!(
                state.web.status_field(),
                if granted { "web: fetch" } else { "web: off" },
                "only a grant raises the status field"
            );
            // And it names the key that was written. It used to name `[web]
            // tier`, the raise-only ceiling — which is checked *before* any
            // prompt exists and so is a no-op for every prompt a user can
            // reach, sending anyone who went looking to a line that had not
            // changed. The durable effect is the per-tier consent list.
            if scope == WebConsentScope::Persistent && granted {
                assert!(
                    surface.any_line_contains(LineKind::Notice, "permission_allow"),
                    "the notice must name the key the answer wrote: {:?}",
                    surface.calls
                );
                assert!(
                    !surface.any_line_contains(LineKind::Notice, "tier ="),
                    "the notice points at a key this answer does not change: {:?}",
                    surface.calls
                );
            }
        }
    }

    /// **REQ-569 re-verify, R1.** A grant notice names who approved it, and a
    /// peer approval never renders as the bare, benign sentence.
    ///
    /// The middle case is the one this exists for. One actor holding two
    /// connections has the first approve the second, which sets
    /// `self_approved: false` — indistinguishable, on that flag, from a real
    /// second user's decision. Rendered off the flag alone it read "the daemon
    /// granted X permission to attach to a session." and stopped, which is the
    /// benign sentence. Both descriptors are on the line now, so the coincidence
    /// is visible to whoever is looking at the screen.
    ///
    /// Not verbose-gated, on purpose and asserted: a widened permission is news.
    #[test]
    fn a_grant_notice_names_both_parties_and_never_hides_a_peer_approval() {
        /// One announcement shape and the phrases its notice must carry.
        struct Case {
            what: &'static str,
            requester: &'static str,
            approver: &'static str,
            self_approved: bool,
            suppressed: u32,
            expected: &'static [&'static str],
        }
        let cases = [
            Case {
                what: "the resume flow: one connection, both roles",
                requester: "cli \"resume\"",
                approver: "cli \"resume\"",
                self_approved: true,
                suppressed: 0,
                expected: &["cli \"resume\"", "the connection that asked"],
            },
            Case {
                what: "one actor, two connections, one name",
                requester: "cli \"attacker\"",
                approver: "cli \"attacker\"",
                self_approved: false,
                suppressed: 0,
                expected: &[
                    "cli \"attacker\"",
                    "a second connection giving that same name",
                ],
            },
            Case {
                what: "a real second party",
                requester: "cli \"newcomer\"",
                approver: "cli \"holder\"",
                self_approved: false,
                suppressed: 0,
                expected: &["cli \"newcomer\"", "approved by cli \"holder\""],
            },
            Case {
                what: "a burst the daemon quieted still says how much it stands for",
                requester: "cli \"flooder\"",
                approver: "cli \"flooder\"",
                self_approved: false,
                suppressed: 9,
                expected: &["9 further grant announcements were held back"],
            },
        ];

        for Case {
            what: case,
            requester,
            approver,
            self_approved,
            suppressed,
            expected,
        } in cases
        {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = false;

            render_event(
                &envelope(Event::SessionGrantMinted(SessionGrantMinted {
                    scope: ConsentScope::Attach,
                    requester: requester.to_owned(),
                    approver: approver.to_owned(),
                    self_approved,
                    suppressed,
                    attestation: "os_biometric".to_owned(),
                })),
                &mut surface,
                &mut state,
            );

            let notices = surface.lines_of(LineKind::Notice);
            assert_eq!(
                notices.len(),
                1,
                "{case}: a widened permission is news, not chrome — it renders \
                 without `verbose`: {:?}",
                surface.calls
            );
            for needle in expected {
                assert!(
                    notices[0].contains(needle),
                    "{case}: notice must contain {needle:?}: {}",
                    notices[0]
                );
            }
        }
    }

    /// The override folds into the status row on every client, and draws its
    /// line for the ones that did not issue the command.
    #[test]
    fn a_taint_override_updates_the_status_row_and_announces_itself_when_verbose() {
        for verbose in [false, true] {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = verbose;
            state.web.restricted = true;

            render_event(
                &envelope(Event::WebTaintOverridden(WebTaintOverridden {
                    tiers_restored: vec![WebTier::FetchUserUrl, WebTier::FetchAnyUrl],
                })),
                &mut surface,
                &mut state,
            );

            assert_eq!(
                state.web.status_field(),
                "web: overridden",
                "the fold happens whether or not the line is drawn"
            );
            assert_eq!(
                surface.any_line_contains(LineKind::Notice, "lifted"),
                verbose,
                "the line is verbose-gated: the issuing client renders the RPC's \
                 own answer"
            );
            assert!(surface.lines_of(LineKind::Error).is_empty());
        }
    }

    /// The tier vocabulary these lines use is the config vocabulary, so a user
    /// reading a notice and then their config sees one word. Asserted here
    /// because this is now the only definition — `slash.rs` had a byte-identical
    /// copy, which is exactly how two spellings of one vocabulary get started.
    #[test]
    fn tiers_are_named_the_way_the_config_names_them() {
        assert_eq!(web_tier_name(WebTier::Off), "off");
        assert_eq!(web_tier_name(WebTier::FetchUserUrl), "fetch_user_url");
        assert_eq!(web_tier_name(WebTier::FetchAnyUrl), "fetch_any_url");
        assert_eq!(web_tier_name(WebTier::Search), "search");
    }

    /// A restore list that is empty says so, rather than trailing an empty
    /// "resume at: ".
    #[test]
    fn an_empty_restore_list_reads_as_nothing_restored() {
        let line = format_web_taint_overridden(&[]);
        assert!(line.contains("no tiers"), "{line}");
        assert!(!line.contains("resume at:"), "{line}");
    }

    /// BR-4's fifth option reaches the daemon by **id**, and only on prompts
    /// that offered it.
    #[test]
    fn the_permanent_key_selects_the_persistent_option_when_offered() {
        let req = web_permission_request();
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["p"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: OPTION_ID_ENABLE_PERMANENT.to_owned()
            }
        );
        // The strongest possible yes must not leave the user re-asked next
        // lookup: the daemon's gate is per-turn, so the client remembers too.
        assert!(grants.is_allow_always("web_fetch_any_url"));
    }

    /// On a prompt without the option, `p` is an unrecognised answer: it
    /// re-asks rather than quietly meaning something else.
    #[test]
    fn the_permanent_key_is_not_offered_on_an_ordinary_prompt() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["p", "y"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        );
        assert_eq!(
            prompter.asked, 2,
            "`p` must have been rejected and re-asked"
        );
        assert!(!grants.is_allow_always("shell"));
    }

    /// "Allow for this session" must never reach the option that writes config —
    /// the two share a [`PermissionOptionKind`], and only the id tells them
    /// apart.
    #[test]
    fn allow_always_never_selects_the_persistent_option() {
        let req = web_permission_request();
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["a"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned()
            },
            "a session grant must not have edited the user's config"
        );
    }

    /// The question offers the key exactly when the option exists.
    #[test]
    fn the_prompt_advertises_the_permanent_key_only_when_it_is_on_offer() {
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        resolve_permission(
            &web_permission_request(),
            &mut surface,
            &mut prompter,
            &mut SessionGrants::default(),
            true,
        );
        assert!(
            prompter.any_question_contains("[p]ermanently"),
            "the web prompt must advertise the key: {:?}",
            prompter.questions
        );

        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        resolve_permission(
            &permission_request("shell"),
            &mut surface,
            &mut prompter,
            &mut SessionGrants::default(),
            true,
        );
        assert!(
            !prompter.any_question_contains("[p]ermanently"),
            "a prompt must not advertise a key that answers nothing: {:?}",
            prompter.questions
        );
    }

    // -- REQ-567 BR-8: the clear notice ------------------------------------

    /// A `context_cleared` envelope for `session`, carrying `dropped` blocks.
    fn cleared(session: &str, dropped: u64) -> EventEnvelope {
        EventEnvelope::new(
            1,
            Some(SessionId::from(session)),
            Event::ContextCleared(ContextCleared {
                blocks_dropped: dropped,
            }),
        )
    }

    /// The client that is *in* the cleared session gets the plain notice, and
    /// the count reads as an answer to the command the user just typed —
    /// including the singular and the nothing-to-drop branches, which exist
    /// because "cleared 0 blocks" and "there was nothing to clear" are the same
    /// fact stated one usefully and one uselessly.
    #[test]
    fn a_clear_in_this_session_reports_what_it_dropped() {
        for (dropped, expected) in [
            (0u64, "context cleared; there was nothing retained to drop."),
            (1, "context cleared; 1 retained block dropped."),
            (12, "context cleared; 12 retained blocks dropped."),
        ] {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.session_id = Some(SessionId::from("s1"));

            render_event(&cleared("s1", dropped), &mut surface, &mut state);

            assert!(
                surface.any_line_contains(LineKind::Notice, expected),
                "{dropped} dropped blocks rendered as {:?}",
                surface.lines_of(LineKind::Notice)
            );
            assert!(
                !surface.any_line_contains(LineKind::Notice, "another session"),
                "this client's own clear must not be attributed elsewhere"
            );
        }
    }

    /// **The bus is daemon-wide.** A clear in a *different* session must not read
    /// as this session's: the user's next prompt starts from an untouched
    /// conversation, and a bare "context cleared; 12 retained blocks dropped"
    /// tells them the opposite. It names the other session, the way `client.rs`
    /// names one for a permission request that is not ours to answer.
    #[test]
    fn a_clear_in_another_session_says_so_and_names_it() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));

        render_event(&cleared("s2", 12), &mut surface, &mut state);

        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "context cleared in another session (s2); 12 retained blocks dropped."
            ),
            "a clear elsewhere rendered as {:?}",
            surface.lines_of(LineKind::Notice)
        );
    }

    /// A client that does not yet know its own session id — a passive
    /// subcommand context, or the window before `session/create` answers —
    /// renders the plain line. Unknown is not evidence of elsewhere, and
    /// guessing "another session" would be wrong in the single-session case
    /// that is almost every case.
    #[test]
    fn a_clear_is_not_attributed_elsewhere_when_this_client_has_no_session() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(&cleared("s2", 3), &mut surface, &mut state);

        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "context cleared; 3 retained blocks dropped."
            ),
            "rendered as {:?}",
            surface.lines_of(LineKind::Notice)
        );
    }

    // -- REQ-583 BR-7 / BR-8: the session-root line and its notice ----------

    fn session_root(kind: RootKind, display: &str) -> SessionRoot {
        SessionRoot {
            display: display.to_owned(),
            kind,
            project_name: (kind == RootKind::Project).then(|| "teton-code".to_owned()),
            vcs_branch: (kind == RootKind::Project).then(|| "main".to_owned()),
        }
    }

    /// A `session_root_changed` envelope for `session`, moving it to `root`.
    fn root_changed(session: &str, root: SessionRoot) -> EventEnvelope {
        EventEnvelope::new(
            1,
            Some(SessionId::from(session)),
            Event::SessionRootChanged(SessionRootChanged {
                previous_display: "~/before".to_owned(),
                root,
            }),
        )
    }

    /// The client that is *in* the moved session reads where it is now — root
    /// and kind, in the one spelling every surface uses — and its cache of the
    /// root moves with it, so `/cd`'s bare form answers from what the daemon
    /// last said. A project draws that one line and **no** notice (BR-8 fires
    /// only when the new kind is not a project).
    #[test]
    fn a_root_move_in_this_session_states_the_new_root_and_kind() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));
        state.root = Some(session_root(RootKind::Home, "~"));

        let root = session_root(RootKind::Project, "~/Documents/GitHub/teton-code");
        render_event(&root_changed("s1", root.clone()), &mut surface, &mut state);

        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "session root is now ~/Documents/GitHub/teton-code (project teton-code, branch main)"
            ),
            "rendered as {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert!(
            !surface.any_line_contains(LineKind::Notice, "Not inside a project"),
            "a project root needs no notice: {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert!(
            !surface.any_line_contains(LineKind::Notice, "another session"),
            "this client's own move must not be attributed elsewhere"
        );
        // The disposition line is the `context_cleared` event's to draw, not
        // this arm's — one clear, one line, drawn once (BR-7's "existing shape").
        assert!(!surface.any_line_contains(LineKind::Notice, "context cleared"));
        assert_eq!(state.root, Some(root), "the cache follows the daemon");
    }

    /// **BR-8 / AC-11.** A `/cd ~` from a project fires the BR-5 notice again:
    /// the user gets the same one line they would have gotten at launch, drawn
    /// by the same function, right after the root line.
    #[test]
    fn a_root_move_to_home_refires_the_launch_notice() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.interactive = true;
        state.session_id = Some(SessionId::from("s1"));
        state.root = Some(session_root(
            RootKind::Project,
            "~/Documents/GitHub/teton-code",
        ));

        let home = session_root(RootKind::Home, "~");
        render_event(&root_changed("s1", home.clone()), &mut surface, &mut state);

        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(
            notices.len(),
            2,
            "the root line, then the notice: {notices:?}"
        );
        assert_eq!(notices[0], "session root is now ~ (your home folder)");
        assert_eq!(
            notices[1],
            banner::root_notice(&home).expect("home is announced"),
            "the re-fired notice is launch's own, not a second wording"
        );
        assert!(notices[1].contains("`/cd <path>`"), "{}", notices[1]);
        assert_eq!(state.root, Some(home));
    }

    /// **BR-5's gate applies to the re-fire (TASK-180).** The same move on a
    /// pipe — stdout not a terminal, `interactive` false as `run_session` sets
    /// it — draws the root line and **no** notice: the notice's content is
    /// pure and its bytes are the terminal's, at launch and after `/cd` alike,
    /// so piped output moves only by the one line the move itself adds.
    #[test]
    fn a_root_move_to_home_on_a_pipe_draws_the_root_line_and_no_notice() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        assert!(!state.interactive, "the default is the piped posture");
        state.session_id = Some(SessionId::from("s1"));
        state.root = Some(session_root(
            RootKind::Project,
            "~/Documents/GitHub/teton-code",
        ));

        let home = session_root(RootKind::Home, "~");
        render_event(&root_changed("s1", home.clone()), &mut surface, &mut state);

        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["session root is now ~ (your home folder)".to_owned()],
            "on a pipe the move is the root line alone"
        );
        assert_eq!(state.root, Some(home), "the cache still follows the daemon");
    }

    /// **The bus is daemon-wide.** A move in a *different* session names that
    /// session and describes nothing — this client's root did not move, its
    /// cache stays, and no notice about someone else's root fires here.
    #[test]
    fn a_root_move_in_another_session_says_so_and_names_it() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));
        let ours = session_root(RootKind::Project, "~/Documents/GitHub/teton-code");
        state.root = Some(ours.clone());

        render_event(
            &root_changed("s2", session_root(RootKind::Home, "~")),
            &mut surface,
            &mut state,
        );

        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(
            notices,
            vec!["session root moved in another session (s2)"],
            "rendered as {notices:?}"
        );
        assert_eq!(state.root, Some(ours), "another session's move is not ours");
    }

    /// A client with no session of its own (a passive context, or the window
    /// before `session/create` answers) renders the plain line — unknown is not
    /// evidence of elsewhere, exactly as for `context_cleared` — but it does
    /// **not** cache the root: unknown is not evidence of *here* either, and a
    /// later bare `/cd` must not describe a root this client never had.
    #[test]
    fn a_root_move_is_not_attributed_elsewhere_when_this_client_has_no_session() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.interactive = true;
        assert_eq!(state.session_id, None);

        render_event(
            &root_changed("s2", session_root(RootKind::Plain, "/opt/scratch")),
            &mut surface,
            &mut state,
        );

        assert!(
            surface.any_line_contains(
                LineKind::Notice,
                "session root is now /opt/scratch (not a project)"
            ),
            "rendered as {:?}",
            surface.lines_of(LineKind::Notice)
        );
        assert!(surface.any_line_contains(LineKind::Notice, "Not inside a project"));
        assert_eq!(
            state.root, None,
            "with no session of its own this client must not cache another session's root"
        );
    }

    /// The root cache starts empty: nothing is known until the daemon says.
    #[test]
    fn the_root_cache_starts_unknown() {
        assert_eq!(SessionState::new().root, None);
    }

    // ------------------------------------------------------------------
    // REQ-572: the setup events (BR-14 / AC-4, and OQ-2's settled answer)
    // ------------------------------------------------------------------

    /// **OQ-2, in one line.** A completed setup says the capability is on, where
    /// it was written, and — the half that keeps the promise — that nothing has
    /// been looked up and the next web-needing question will ask first. No
    /// lookup is fired: the flow performs no egress (BR-13).
    ///
    /// Not verbose-gated, asserted: the machine's configuration changed.
    #[test]
    fn a_completed_setup_announces_the_tier_and_that_nothing_has_gone_out_yet() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        assert!(!state.verbose, "the default session, deliberately");
        assert_eq!(
            state.web.status_field(),
            "web: off",
            "non-vacuity: nothing knows about the capability before the commit"
        );

        render_event(
            &envelope(Event::WebSetupCompleted(WebSetupCompleted {
                tier: WebTier::Search,
                config_path: "/Users/x/.config/teton/config.toml".to_owned(),
            })),
            &mut surface,
            &mut state,
        );

        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("web lookup enabled"), "{notice}");
        assert!(notice.contains("`search`"), "{notice}");
        assert!(
            notice.contains("written to your Teton config"),
            "the user must learn the change is durable and not session-local: {notice}"
        );
        // The path is on the wire and off the screen. This notice reaches every
        // open session, and an absolute config path is a home directory — a
        // username on a screen that is not the one that ran the walkthrough.
        assert!(
            !notice.contains("/Users/x"),
            "the completion notice must not print an absolute path: {notice}"
        );
        assert!(
            notice.contains(
                "the next web-needing question will ask before anything leaves the \
                           machine"
            ),
            "OQ-2's answer is this clause: {notice}"
        );
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "an enablement is not an error"
        );

        // The status field learns the new ceiling from the event itself — no
        // second config read, and no waiting for the next session.
        assert_eq!(
            state.web.capability,
            Some(WebCapabilityState::Ready {
                tier: WebTier::Search
            })
        );
        assert_eq!(state.web.status_field(), "web: search (configured)");
    }

    /// AC-4's client leg: a setup call the daemon refused is announced to the
    /// session's own user, never merely logged (LESSON-505). It names a kind of
    /// caller — the daemon's own word — and says that nothing happened.
    #[test]
    fn a_refused_setup_call_is_announced_and_says_nothing_was_written() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::WebSetupRejected(WebSetupRejected {
                origin: "an unattached connection".to_owned(),
            })),
            &mut surface,
            &mut state,
        );

        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("web setup refused"), "{notice}");
        assert!(notice.contains("an unattached connection"), "{notice}");
        assert!(notice.contains("Nothing was written"), "{notice}");
        // The event fires only for a refused COMMIT since the verify pass, so
        // the line must describe an attempted write — not minimize it as a
        // read (re-verify security finding, wording drift).
        assert!(notice.contains("tried to change"), "{notice}");
        assert!(
            state.web.capability.is_none(),
            "a refusal changes no capability state"
        );
    }

    /// REQ-579 BR-15's client leg: a committed registration is announced to
    /// every session attached, naming the id, the model, **where it will be
    /// dialed**, and what now routes to it — and naming no key reference, which
    /// the event does not carry (BR-2).
    #[test]
    fn a_completed_provider_setup_is_announced_with_what_now_routes_to_it() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::ProviderSetupCompleted(ProviderSetupCompleted {
                provider_id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: "kimi-k2-turbo-preview".to_owned(),
                bindings: vec![TierBinding {
                    tier: Tier::Think,
                    provider_id: ProviderId::from("kimi"),
                }],
                dial_host: "api.moonshot.ai".to_owned(),
            })),
            &mut surface,
            &mut state,
        );

        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("provider `kimi` registered"), "{notice}");
        assert!(notice.contains("`kimi-k2-turbo-preview`"), "{notice}");
        assert!(notice.contains("`think` now routes to it"), "{notice}");
        // The destination, so a client that watched routing move under it can
        // tell where turns now go (the audience this event exists for).
        assert!(notice.contains("dialed at `api.moonshot.ai`"), "{notice}");
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a registration is not an error"
        );

        // BR-7's declined-every-binding outcome says so plainly rather than
        // trailing off after the model name.
        let mut surface = RecordingSurface::new();
        render_event(
            &envelope(Event::ProviderSetupCompleted(ProviderSetupCompleted {
                provider_id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: "kimi-k2-turbo-preview".to_owned(),
                bindings: vec![],
                dial_host: "api.moonshot.ai".to_owned(),
            })),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("nothing routes to it yet"), "{notice}");

        // A daemon built before the field says nothing, and the line renders no
        // empty clause for it.
        let mut surface = RecordingSurface::new();
        render_event(
            &envelope(Event::ProviderSetupCompleted(ProviderSetupCompleted {
                provider_id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: "kimi-k2-turbo-preview".to_owned(),
                bindings: vec![],
                dial_host: String::new(),
            })),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(
            notice.contains("provider `kimi` registered (model `kimi-k2-turbo-preview`)"),
            "{notice}"
        );
        assert!(!notice.contains("dialed at"), "{notice}");
    }

    /// REQ-579 BR-12's client leg: a refused commit is announced to the
    /// session's own user, names the method the daemon named, and says that
    /// nothing was written and no key was stored.
    #[test]
    fn a_refused_provider_setup_commit_says_nothing_was_written_or_stored() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        render_event(
            &envelope(Event::ProviderSetupRejected(ProviderSetupRejected {
                method: "provider/setup_commit".to_owned(),
            })),
            &mut surface,
            &mut state,
        );

        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("provider setup refused"), "{notice}");
        assert!(notice.contains("provider/setup_commit"), "{notice}");
        assert!(notice.contains("Nothing was written"), "{notice}");
        assert!(notice.contains("no key was stored"), "{notice}");
        assert!(
            !notice.contains("another session"),
            "an unqualified event is this session's: {notice}"
        );
    }

    /// The bus is daemon-wide, so both provider-setup notices say *whose*
    /// session they are about when it is not this one.
    ///
    /// `context_cleared`'s rule, applied to the two events that change what the
    /// machine will dial: an unqualified copy on every attached client tells
    /// each of them that their own session's routing just moved, or that their
    /// own session was the target of a refused write. Neither is a nuance the
    /// reader can recover from the line.
    #[test]
    fn provider_setup_notices_name_the_other_session_when_it_is_not_ours() {
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("ours"));
        let theirs = |event| EventEnvelope::new(7, Some(SessionId::from("theirs")), event);

        let mut surface = RecordingSurface::new();
        render_event(
            &theirs(Event::ProviderSetupCompleted(ProviderSetupCompleted {
                provider_id: ProviderId::from("kimi"),
                kind: ProviderKind::OpenaiCompatible,
                model: "kimi-k3".to_owned(),
                bindings: vec![TierBinding {
                    tier: Tier::Think,
                    provider_id: ProviderId::from("kimi"),
                }],
                dial_host: "api.moonshot.ai".to_owned(),
            })),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("in another session (theirs)"), "{notice}");
        assert!(notice.contains("dialed at `api.moonshot.ai`"), "{notice}");

        let mut surface = RecordingSurface::new();
        render_event(
            &theirs(Event::ProviderSetupRejected(ProviderSetupRejected {
                method: "provider/setup_commit".to_owned(),
            })),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(
            notice.contains("refused in another session (theirs)"),
            "{notice}"
        );
        assert!(notice.contains("Nothing was written"), "{notice}");

        // Our own session is unqualified — the qualification is news, and news
        // on every line is noise.
        let mut surface = RecordingSurface::new();
        render_event(
            &EventEnvelope::new(
                8,
                Some(SessionId::from("ours")),
                Event::ProviderSetupRejected(ProviderSetupRejected {
                    method: "provider/setup_commit".to_owned(),
                }),
            ),
            &mut surface,
            &mut state,
        );
        assert!(
            !surface
                .lines_of(LineKind::Notice)
                .join("\n")
                .contains("another session"),
            "{:?}",
            surface.calls
        );
    }

    /// REQ-581 BR-3/BR-4's client leg: a connection test run **elsewhere** is
    /// announced here, naming what came back and where health landed — including
    /// on the outcomes that failed, because the health map this session's router
    /// reads moved either way.
    ///
    /// It is asserted against the *same* wording the report uses, which is the
    /// point of sharing `outcome_sentence`: an event that spelled `reachable`
    /// differently from the command that produced it would read as two different
    /// facts.
    #[test]
    fn a_provider_test_run_elsewhere_is_announced_with_its_outcome_and_health() {
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("ours"));
        let theirs = |seq: u64, event: Event| {
            EventEnvelope::new(seq, Some(SessionId::from("theirs")), event)
        };

        let mut surface = RecordingSurface::new();
        render_event(
            &theirs(
                7,
                Event::ProviderTested(ProviderTested {
                    provider_id: ProviderId::from("kimi"),
                    outcome: ProviderTestOutcome::Reached {
                        latency_ms: 1_400,
                        input_tokens: 2_040,
                        output_tokens: 21,
                        usd_micros: Some(6_400),
                    },
                    health_after: ProviderHealth::Healthy,
                }),
            ),
            &mut surface,
            &mut state,
        );

        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(
            notice.contains("provider `kimi` tested in another session (theirs):"),
            "{notice}"
        );
        assert!(notice.contains("reachable — answered in 1.4 s"), "{notice}");
        assert!(notice.contains("provider health: healthy."), "{notice}");
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "a completed test is not an error, whatever it found"
        );

        // A failure is announced too, and its `reason` — the daemon's own
        // sentence — is carried verbatim, credential *reference* and all (AC-2).
        let mut surface = RecordingSurface::new();
        render_event(
            &theirs(
                8,
                Event::ProviderTested(ProviderTested {
                    provider_id: ProviderId::from("kimi"),
                    outcome: ProviderTestOutcome::Refused {
                        status: 401,
                        reason: "HTTP 401 from api.moonshot.ai — the vendor did not accept the \
                                 credential at keychain://teton/kimi"
                            .to_owned(),
                    },
                    health_after: ProviderHealth::Unavailable,
                }),
            ),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("refused — HTTP 401"), "{notice}");
        assert!(notice.contains("keychain://teton/kimi"), "{notice}");
        assert!(notice.contains("provider health: unavailable."), "{notice}");
    }

    /// **The connection that ran the test renders nothing here — and it is the
    /// only one that renders nothing** (REQ-581 verify G2).
    ///
    /// Three rows, because the first shape of this gate satisfied one of them by
    /// silencing all three. The client that ran the command already printed the
    /// full report — the outcome, the model, the dial host's sentence, the
    /// remedy and what now routes there — so a notice on that surface is the
    /// same news twice, in two wordings a reader has to reconcile. But the gate
    /// was `other_session(...).is_none()`, which is true for *any* event on our
    /// own session, and that swept up the reader the notice was written for: a
    /// second client attached to the same session, which ran nothing, holds no
    /// report, and watched the health its own turns route by move (LESSON-505).
    ///
    /// So the discriminator is `provider_test_in_flight` — "this connection
    /// issued the call" — and the three rows are the whole truth table it
    /// decides: our session with the flag up (the caller: nothing), our session
    /// with it down (the sibling: the notice, *unqualified*), another session
    /// (the notice, qualified). Row two fails against the session-keyed gate,
    /// which is the only reason it is worth writing down.
    #[test]
    fn only_the_connection_that_ran_the_test_renders_no_notice() {
        let tested = || {
            Event::ProviderTested(ProviderTested {
                provider_id: ProviderId::from("kimi"),
                outcome: ProviderTestOutcome::Unreachable {
                    reason: "could not reach api.moonshot.ai: timeout".to_owned(),
                },
                health_after: ProviderHealth::Unavailable,
            })
        };

        // (a) The caller: its own `provider/test` is out, so the report is on
        // the way and the notice would duplicate it.
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("ours"));
        state.provider_test_in_flight = true;
        let mut surface = RecordingSurface::new();
        render_event(
            &EventEnvelope::new(8, Some(SessionId::from("ours")), tested()),
            &mut surface,
            &mut state,
        );
        assert!(
            surface.calls.is_empty(),
            "the surface that is about to print the report must not print the \
             notice too: {:?}",
            surface.calls
        );

        // (b) A second client on the *same* session. Same event, same session
        // id, no call of its own — the audience this notice exists for. It reads
        // the news plainly: the test was in this session, so there is no other
        // one to name.
        state.provider_test_in_flight = false;
        let mut surface = RecordingSurface::new();
        render_event(
            &EventEnvelope::new(9, Some(SessionId::from("ours")), tested()),
            &mut surface,
            &mut state,
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(
            notice.contains("provider `kimi` tested:"),
            "a client attached to the session the test ran in is owed the \
             outcome and the health it now routes by: {:?}",
            surface.calls
        );
        assert!(
            !notice.contains("another session"),
            "…and it was *this* session's test, so the qualification would be \
             false: {notice}"
        );
        assert!(notice.contains("provider health: unavailable."), "{notice}");

        // (c) Another session's test, with the flag down as it always is there:
        // the same news, qualified, because "the provider your turns use" and
        // "some other session's" are not the same fact.
        let mut surface = RecordingSurface::new();
        render_event(
            &EventEnvelope::new(10, Some(SessionId::from("theirs")), tested()),
            &mut surface,
            &mut state,
        );
        assert!(
            surface
                .lines_of(LineKind::Notice)
                .join("\n")
                .contains("tested in another session (theirs)"),
            "{:?}",
            surface.calls
        );
    }

    /// A dead end is chrome: the turn that hit it already said what it could not
    /// do and what would fix it, so this renders only for a session that asked
    /// for the diagnostic detail — and then it names the capability id, which is
    /// the part a bug report needs.
    #[test]
    fn a_capability_dead_end_is_verbose_only_and_names_the_capability() {
        let dead_end = || {
            envelope(Event::CapabilityDeadEnd(CapabilityDeadEnd {
                capability: CapabilityDeadEnd::WEB_SEARCH.to_owned(),
            }))
        };

        let mut quiet = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(&dead_end(), &mut quiet, &mut state);
        assert!(
            quiet.calls.is_empty(),
            "a default session renders nothing for it: {:?}",
            quiet.calls
        );

        let mut loud = RecordingSurface::new();
        state.verbose = true;
        render_event(&dead_end(), &mut loud, &mut state);
        let notice = loud.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("web_search"), "{notice}");
        assert!(notice.contains("dead end"), "{notice}");

        // An id this build has never heard of still renders — the field is a
        // string precisely so a client cannot fail to report one.
        let mut later = RecordingSurface::new();
        render_event(
            &envelope(Event::CapabilityDeadEnd(CapabilityDeadEnd {
                capability: "a_capability_from_the_future".to_owned(),
            })),
            &mut later,
            &mut state,
        );
        assert!(
            later.any_line_contains(LineKind::Notice, "a_capability_from_the_future"),
            "{:?}",
            later.calls
        );
    }

    /// REQ-580 BR-5: a held turn renders as a **notice** — not an error, and
    /// not verbose-gated — that says the message is queued, names the model,
    /// says which of the two transient states it is waiting out (branched on
    /// the typed value, so the two spellings differ exactly there), and
    /// promises nothing more of the user. And it never invents an ETA.
    #[test]
    fn a_held_turn_renders_as_a_queued_notice_naming_the_model_and_the_wait() {
        let queued = |waiting_on| {
            envelope(Event::TurnQueued(TurnQueued {
                turn_id: teton_protocol::TurnId::from("turn-3"),
                model_id: "qwen3-coder-30b-a3b".to_owned(),
                waiting_on,
            }))
        };

        let mut state = SessionState::new();
        assert!(!state.verbose, "the premise: a default, quiet session");
        let mut surface = RecordingSurface::new();
        render_event(&queued(TierWarming::Loading), &mut surface, &mut state);
        assert!(
            surface.lines_of(LineKind::Error).is_empty(),
            "nothing broke, so nothing renders as an error: {:?}",
            surface.calls
        );
        let notice = surface.lines_of(LineKind::Notice).join("\n");
        assert!(notice.starts_with("message queued"), "{notice}");
        assert!(
            notice.contains("qwen3-coder-30b-a3b"),
            "names the model: {notice}"
        );
        assert!(notice.contains("finishes loading"), "{notice}");
        assert!(
            notice.contains("as soon as the local tier opens"),
            "says the turn runs by itself: {notice}"
        );
        assert!(
            !notice.contains("retry") && !notice.contains("Retry"),
            "a held turn asks nothing of the user: {notice}"
        );
        assert!(
            !notice.contains("second") && !notice.contains("minute") && !notice.contains('%'),
            "no countdown, no ETA, no percentage (REQ-556 BR-5's rule): {notice}"
        );

        let mut installing = RecordingSurface::new();
        render_event(
            &queued(TierWarming::Installing),
            &mut installing,
            &mut state,
        );
        let notice = installing.lines_of(LineKind::Notice).join("\n");
        assert!(notice.contains("finishes installing"), "{notice}");
        assert!(!notice.contains("finishes loading"), "{notice}");
    }

    // -----------------------------------------------------------------------
    // The `/provider setup` hand-off (REQ-579 ADR-9)
    // -----------------------------------------------------------------------

    /// Drive one whole turn: open it, stream `chunks` as assistant text, end it.
    ///
    /// The turn is driven through `render_event` rather than by writing the
    /// accumulator directly, so what these tests assert on is the real path —
    /// including that the chunk actually reaches the accumulator on its way to
    /// the screen.
    ///
    /// The turn opens with **no prompt**, which is what keeps every assertion
    /// below about REQ-579's line alone: a turn nobody asked a connection
    /// question in cannot earn REQ-581's, so these tests answer the same
    /// question after ADR-4 as before it.
    fn hand_off_turn(chunks: &[&str], tty: bool) -> RecordingSurface {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");
        for text in chunks {
            render_event(&envelope(chunk(text)), &mut surface, &mut state);
        }
        hand_off_after_turn(&mut state, &mut surface, tty);
        surface
    }

    /// **AC-1, the deterministic half.** A reply that reached for the shell
    /// recipe is followed by exactly one Notice naming the in-session command.
    ///
    /// This is the criterion three live rounds could not obtain from the model
    /// itself (verification.md §1–§24): the guarantee is the surface's now.
    #[test]
    fn a_reply_that_recites_the_cli_earns_exactly_one_hand_off_line() {
        for (case, chunks) in [
            (
                "the registration command",
                &["run `teton provider add kimi --kind openai-compatible`."][..],
            ),
            (
                "the routing command on its own",
                &["you would then run teton policy set-tier think kimi."][..],
            ),
            // The chunk boundary is the daemon's, not the sentence's. A check
            // that read one chunk at a time would miss a command split across
            // two frames, which is why the accumulator exists at all.
            (
                "a command split across two chunks",
                &[
                    "to register it, run teton prov",
                    "ider add kimi from a shell.",
                ][..],
            ),
            // Backtick-agnostic: a model that has read the guide reproduces the
            // command inside markdown and does not always fence the whole of it.
            (
                "markdown that fences only part of the command",
                &["run `teton` provider add kimi."][..],
            ),
        ] {
            let surface = hand_off_turn(chunks, true);
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec![hand_off_line()],
                "{case}: exactly one hand-off, and it is ADR-9's sentence; got {:?}",
                surface.calls
            );
        }
    }

    /// The model volunteered the command **and nothing else**, so the surface
    /// stays quiet.
    ///
    /// ADR-9's nudge exists because the model will not name `/provider setup`.
    /// A model that one day does makes it dormant with no code change, and this
    /// is the test that says so — repeating the command over an answer that was
    /// already right is the harness talking over the model.
    #[test]
    fn a_reply_that_names_only_the_command_earns_nothing() {
        for (case, reply) in [
            (
                "it named the command instead",
                "run `/provider setup kimi think` and it will walk you through it.",
            ),
            // Backtick-agnostic on this half too, since the verify pass: the
            // dormancy question is asked of the same stripped characters the
            // recital question is.
            (
                "unfenced, as prose",
                "type /provider setup kimi think at the prompt.",
            ),
            (
                "fenced whole",
                "type `/provider setup kimi think` at the prompt.",
            ),
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "{case}: {:?}",
                surface.calls
            );
        }
    }

    /// **The dormancy hole.** A reply that names the command *and* recites the
    /// CLI still earns the line.
    ///
    /// Naming `/provider setup` used to silence the harness outright, whatever
    /// else the reply said — so one sentence containing the command was enough
    /// to suppress the only line that says "no key in chat", on the exact turn a
    /// reply was steering the user toward pasting one. A reply that offers both
    /// paths has still pointed at the CLI, and the correction is still true.
    #[test]
    fn a_reply_that_names_the_command_but_still_recites_the_cli_earns_the_line() {
        for (case, reply) in [
            (
                "it offered both paths",
                "in-session: `/provider setup kimi think`. From a shell: `teton provider add kimi`.",
            ),
            // The shape the suppression was worth having: the command named as
            // cover, and the actually-dangerous instruction alongside it.
            (
                "it named the command and then asked for the key in chat",
                "you can use /provider setup, but it is easier if you paste your API key here and \
                 I will run teton provider add kimi for you.",
            ),
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec![hand_off_line()],
                "{case}: {:?}",
                surface.calls
            );
        }
    }

    /// Both halves of the predicate read the **same** characters.
    ///
    /// Before the verify pass the recital half stripped backticks and the
    /// dormancy half did not, so `` `/provider setup` `` and `/provider setup`
    /// were two different answers to one question. Fencing is a markdown
    /// accident, and a matcher that treats it as meaning is a matcher whose
    /// behaviour depends on how the model felt about code spans.
    #[test]
    fn dormancy_and_recital_read_the_same_backtick_stripped_text() {
        // One reply, written four ways: fenced or not, on either half. Every
        // spelling recites the CLI, so every spelling earns the line.
        for reply in [
            "use `/provider setup`, or run `teton provider add kimi`.",
            "use /provider setup, or run teton provider add kimi.",
            "use `/provider setup`, or run teton provider add kimi.",
            "use /provider setup, or run `teton provider add kimi`.",
            // And the fence landing mid-command, which is the case that made
            // stripping necessary in the first place.
            "use `/provider` setup, or run `teton` provider add kimi.",
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec![hand_off_line()],
                "{reply:?} earned nothing; got {:?}",
                surface.calls
            );
        }

        // The dormant reply, likewise written both ways, earns nothing either
        // way. Together the two loops are the symmetry claim: fencing changes
        // no answer on either half.
        for reply in [
            "use `/provider setup` — it does the whole thing.",
            "use /provider setup — it does the whole thing.",
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "{reply:?} must stay dormant; got {:?}",
                surface.calls
            );
        }
    }

    /// The accumulator is fed by **this** session's chunks only.
    ///
    /// The bus is daemon-wide: every attached client receives every session's
    /// updates. An accumulator that took them all would let another session's
    /// turn decide whether this user's next prompt earns a notice — a line
    /// drawn about words this user was never shown, and one that fires on a
    /// prompt that reached for nothing.
    #[test]
    fn another_sessions_chunks_do_not_feed_the_hand_off() {
        let recital = "run teton provider add kimi --kind openai-compatible.";
        let elsewhere =
            |text: &str| EventEnvelope::new(3, Some(SessionId::from("theirs")), chunk(text));

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("ours"));
        state.begin_turn("");
        render_event(&elsewhere(recital), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "another session's reply may not arm this session's hand-off; got {:?}",
            surface.calls
        );

        // The control, so this is a test about the *session* and not about the
        // text: the same words on our own envelope do earn the line.
        let mut surface = RecordingSurface::new();
        state.begin_turn("");
        render_event(
            &EventEnvelope::new(4, Some(SessionId::from("ours")), chunk(recital)),
            &mut surface,
            &mut state,
        );
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![hand_off_line()],
            "{:?}",
            surface.calls
        );

        // And an event that names no session, or a client that has not yet
        // learned its own id, still counts as ours — `other_session`'s reading,
        // and the one a single-session client depends on.
        let mut surface = RecordingSurface::new();
        state.begin_turn("");
        render_event(
            &EventEnvelope::new(5, None, chunk(recital)),
            &mut surface,
            &mut state,
        );
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![hand_off_line()],
            "{:?}",
            surface.calls
        );
    }

    /// A session that never reached for the CLI never sees the line.
    ///
    /// The match is on the **command**, not on the topic — prose about
    /// providers is not something a user can paste, so it earns nothing.
    #[test]
    fn a_reply_about_anything_else_earns_nothing() {
        for reply in [
            "the file you want is crates/teton/src/main.rs.",
            "teton supports several providers, and you can add one.",
            // Case-sensitive by design: this is prose, not a command line.
            "Teton Provider Add is not how it is spelled.",
            "",
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "{reply:?} must earn nothing; got {:?}",
                surface.calls
            );
        }
    }

    /// Once per turn: two recitals are one turn, and the line does not repeat.
    ///
    /// Three claims in the order they can be established — both commands in one
    /// reply print one line; a second call inside the same turn prints nothing,
    /// because the first consumed the turn's words; and the next turn does not
    /// inherit them.
    #[test]
    fn the_hand_off_is_once_per_turn_even_when_both_commands_appear() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");
        for text in [
            "first, teton provider add kimi --kind openai-compatible.\n",
            "then, teton policy set-tier think kimi.\n",
        ] {
            render_event(&envelope(chunk(text)), &mut surface, &mut state);
        }

        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![hand_off_line()],
            "two recitals are still one turn; got {:?}",
            surface.calls
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![hand_off_line()],
            "a second call in the same turn adds nothing; got {:?}",
            surface.calls
        );

        // The turn after it recites nothing, and must not inherit the line.
        state.begin_turn("");
        render_event(&envelope(chunk("done.")), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![hand_off_line()],
            "a quiet turn must not reprint the previous turn's line; got {:?}",
            surface.calls
        );
    }

    /// **BR-11's byte-identity.** A piped session gets nothing.
    ///
    /// A script already receives the shell recipe, and its output has to be
    /// what it was before this REQ. The second half is the part worth pinning:
    /// the gate does not skip the reset, so a turn whose line was suppressed
    /// still leaves no words behind.
    #[test]
    fn the_hand_off_never_prints_on_a_non_tty_surface() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");
        render_event(
            &envelope(chunk(
                "run teton provider add kimi --kind openai-compatible.",
            )),
            &mut surface,
            &mut state,
        );

        hand_off_after_turn(&mut state, &mut surface, false);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "a pipe must see no hand-off; got {:?}",
            surface.calls
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "the suppressed turn's words must not survive the gate; got {:?}",
            surface.calls
        );
    }

    /// Only the model's own output arms it.
    ///
    /// The user's typed line, a tool title and a plan entry all reach the same
    /// screen, and any of them can carry the command's characters — a user
    /// pasting the recipe to ask about it, a shell tool call that runs it. None
    /// of them is the model answering, so none may trigger the nudge. This is
    /// what makes "do not match the user's prompt" structural rather than a
    /// rule somebody has to keep obeying.
    #[test]
    fn the_users_own_text_and_help_output_do_not_trigger_it() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");

        // A line on the surface — what `/help`, a command's output, and the
        // echo of a typed prompt all are.
        surface.line(
            LineKind::Info,
            "teton provider add kimi --kind openai-compatible",
        );
        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::ToolCall {
                    tool_call_id: "c1".to_owned(),
                    title: "shell: teton policy set-tier think kimi".to_owned(),
                    status: ToolCallStatus::Pending,
                },
            })),
            &mut surface,
            &mut state,
        );
        render_event(
            &envelope(Event::SessionUpdate(SessionUpdate {
                update: SessionUpdatePayload::Plan {
                    entries: vec![PlanEntry {
                        content: "run teton provider add kimi".to_owned(),
                        status: PlanEntryStatus::InProgress,
                    }],
                },
            })),
            &mut surface,
            &mut state,
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "only an assistant chunk may arm the hand-off; got {:?}",
            surface.calls
        );
    }

    /// **The recipes have to be real commands.** Each matched string is built
    /// out of the CLI's own clap tree rather than compared against a second
    /// hand-written copy of it.
    ///
    /// Without this the array is two literals with nothing tying them to
    /// anything: rename `provider add`, and the hand-off quietly stops firing on
    /// the recipe the model now recites, with every existing test still green
    /// because they all feed it the same stale strings. Walking the tree makes
    /// the rename fail here.
    #[test]
    fn every_matched_recipe_is_a_path_through_the_cli_itself() {
        use clap::CommandFactory as _;

        /// `teton provider add`, spelled by clap from the derive.
        fn path(root: &clap::Command, steps: &[&str]) -> String {
            let mut node = root;
            let mut rendered = root.get_name().to_owned();
            for step in steps {
                node = node
                    .get_subcommands()
                    .find(|sub| sub.get_name() == *step)
                    .unwrap_or_else(|| {
                        panic!(
                            "`{}` has no subcommand `{step}` — the recipe names a command this \
                             binary does not have",
                            node.get_name()
                        )
                    });
                rendered.push(' ');
                rendered.push_str(node.get_name());
            }
            rendered
        }

        let cli = crate::Cli::command();
        assert_eq!(
            PROVIDER_CLI_RECIPES.to_vec(),
            vec![
                path(&cli, &["provider", "add"]),
                path(&cli, &["policy", "set-tier"]),
            ],
            "the strings the hand-off matches on are no longer the commands the CLI parses"
        );

        // The match is case-sensitive on purpose, and the tree agrees: these are
        // typed in lowercase, so a capitalised prose mention is not a recipe.
        for recipe in PROVIDER_CLI_RECIPES {
            assert_eq!(recipe.to_ascii_lowercase(), recipe, "{recipe}");
        }
    }

    /// The sentence itself: plain, imperative, and it names the command.
    ///
    /// LESSON-517 puts styling in [`LineKind`] and never in the caller's
    /// string, and BUG-168 asks for the thing stated outright — one sentence,
    /// no em-dash aside. The model may quote this back, which is a further
    /// reason it has to read as an instruction rather than as an aside.
    #[test]
    fn the_hand_off_line_carries_no_ansi_and_names_the_command_verbatim() {
        let line = hand_off_line();
        assert!(
            line.contains("/provider setup <vendor> [tier]"),
            "it must name the command with its arguments: {line}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "no escape may be baked into the text (LESSON-517): {line:?}"
        );
        assert!(
            !line.contains('\u{2014}'),
            "BUG-168: no em-dash aside: {line}"
        );
        assert_eq!(
            line.matches('.').count(),
            1,
            "one sentence, stated outright: {line}"
        );
        assert!(
            line.contains("no key in chat"),
            "it must say the part that makes it safe to take: {line}"
        );

        // And it is exactly what reaches the surface — the constant and the
        // rendered line cannot drift.
        let surface = hand_off_turn(&["teton provider add kimi"], true);
        assert_eq!(surface.lines_of(LineKind::Notice), vec![line]);
    }

    // -----------------------------------------------------------------------
    // The `/provider test` hand-off (REQ-581 ADR-4)
    // -----------------------------------------------------------------------

    /// A tool call as the daemon composes it: `<tool>: <command>` for `shell`,
    /// `<tool> <argument>` for the rest (`harness::turn_loop::describe_call`).
    fn tool_call(id: &str, title: &str) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::ToolCall {
                tool_call_id: id.to_owned(),
                title: title.to_owned(),
                status: ToolCallStatus::Pending,
            },
        })
    }

    /// Drive one whole turn the way the entry loop does: open it **with the
    /// prompt**, render the tool calls it made, stream its reply, end it.
    ///
    /// Everything goes through `begin_turn` and `render_event` rather than being
    /// written into the state directly, so these tests exercise the real path —
    /// including that a tool-call title reaches the turn record on its way to
    /// the screen, which is the half of ADR-4 that the reply text cannot carry.
    fn connection_turn(
        prompt: &str,
        tools: &[&str],
        chunks: &[&str],
        provider_ids: &[&str],
        tty: bool,
    ) -> RecordingSurface {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.provider_ids = provider_ids.iter().map(|id| (*id).to_owned()).collect();
        state.begin_turn(prompt);
        for (index, title) in tools.iter().enumerate() {
            render_event(
                &envelope(tool_call(&format!("c{index}"), title)),
                &mut surface,
                &mut state,
            );
        }
        for text in chunks {
            render_event(&envelope(chunk(text)), &mut surface, &mut state);
        }
        hand_off_after_turn(&mut state, &mut surface, tty);
        surface
    }

    /// **AC-8b, the deterministic half — the predicate table.**
    ///
    /// The positive row is the observed failure, turn for turn: the user's own
    /// words from the screenshot, the `shell: teton provider list` the model ran
    /// instead of dialling, and a reply that reports a healthy connection while
    /// naming no command. Every negative row is one condition of ADR-4's
    /// predicate withdrawn, so a row that starts passing for the wrong reason
    /// shows up as the wrong row.
    #[test]
    fn the_connection_hand_off_fires_only_on_a_probed_connection_question() {
        let asked = "alright, I followed your instructions. Can you test the Kimi connection?";
        let probe = "shell: teton provider list";
        let improvised =
            "kimi is registered and routed to think, so the connection is working fine.";

        for (case, prompt, tools, reply, tty, expected) in [
            (
                "the screenshot's turn: asked, probed, answered without the command",
                asked,
                &[probe][..],
                improvised,
                true,
                Some(connection_test_line()),
            ),
            // The model recited the diagnostic rather than running it. Same
            // mistake, different evidence — which is why the predicate reads
            // both the reply and the tool calls.
            (
                "recited a diagnostic instead of running one",
                asked,
                &[][..],
                "I ran teton provider list and it is all registered correctly.",
                true,
                Some(connection_test_line()),
            ),
            // Dormancy: the model named the command, so the harness has nothing
            // to add. The whole line exists to say this sentence once.
            (
                "the reply named the command",
                asked,
                &[probe][..],
                "registration looks right; run /provider test kimi to actually dial it.",
                true,
                None,
            ),
            // The turn ran a Teton diagnostic, but nobody asked about a
            // connection — a `teton doctor` during a build failure is not this.
            (
                "an unrelated prompt that runs a diagnostic",
                "why is my build failing?",
                &["shell: teton doctor"][..],
                "the failure is in crates/teton/src/main.rs; the toolchain is fine.",
                true,
                None,
            ),
            // A subject with no verb of testing. "which provider is on think?"
            // is a configuration question and is answered correctly by reading
            // configuration.
            (
                "a provider question that is not a connection question",
                "which provider is on think?",
                &[probe][..],
                "think routes to kimi.",
                true,
                None,
            ),
            // The tool half is `shell`-only: reading a file whose path contains
            // `teton` is not probing.
            (
                "a read of a Teton file is not a probe",
                asked,
                &["read crates/teton/src/main.rs"][..],
                "your provider list looks right to me.",
                true,
                None,
            ),
            // BR-11's byte-identity, inherited: a pipe sees nothing.
            (
                "the same turn on a pipe",
                asked,
                &[probe][..],
                improvised,
                false,
                None,
            ),
            // Precedence: a reply that reached for the setup recipe gets the
            // REQ-579 line and only it, even though the connection predicate
            // would also have matched (`teton provider add` is a diagnostic
            // recital as far as the substring is concerned).
            (
                "a setup-recipe reply still earns the setup line, not both",
                asked,
                &[probe][..],
                "run teton provider add kimi --kind openai-compatible first.",
                true,
                Some(hand_off_line()),
            ),
            // BR-6's *other* correct answer. `teton provider test kimi` is the
            // non-interactive form of the very command this line would name, and
            // it begins with `teton provider` — so the reply-side diagnostic
            // scan read the right answer as the mistake and corrected a model
            // that had just told the user exactly what to run.
            (
                "the reply named the shell form of the command",
                asked,
                &[probe][..],
                "registration looks right; run teton provider test kimi to actually dial it.",
                true,
                None,
            ),
            // The same command, *run* rather than recited: the turn dialled, so
            // there is nothing to correct. Before the tool half matched on a
            // first-word `teton`, this was a probe like any other.
            (
                "the turn ran the connection test itself",
                asked,
                &["shell: teton provider test kimi"][..],
                "kimi answered in 1.4 s.",
                true,
                None,
            ),
            // C10's width: `test` inside "latest", `teton` inside "tetond", and
            // a build command that mentions neither the user's providers nor
            // their configuration. Every substring reading of this turn found
            // something; no word-boundary reading does.
            (
                "a cargo run over the daemon's own tests is not a probe",
                "run the latest provider tests",
                &["shell: cargo test -p tetond"][..],
                "all green.",
                true,
                None,
            ),
        ] {
            let surface = connection_turn(prompt, tools, &[reply], &[], tty);
            let expected: Vec<&str> = expected.into_iter().collect();
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                expected,
                "{case}: got {:?}",
                surface.calls
            );
        }
    }

    /// **A provider registered mid-session joins the vocabulary at once.**
    ///
    /// `provider_ids` is filled by `read_config_view` when the session opens and
    /// never again, so a provider registered *during* the session — REQ-579's
    /// `/provider setup`, which is the flow this REQ was written to follow — was
    /// invisible to the connection predicate until the next run of the CLI. "is
    /// kimi working?" a minute after registering `kimi` is the exact turn the
    /// hand-off exists for, and it was the one turn that could not earn it.
    ///
    /// Another session's registration deliberately does *not* join: the ids are
    /// this user's words for the providers they just set up.
    #[test]
    fn a_provider_registered_this_session_becomes_a_connection_subject() {
        let completed = |id: &str| {
            Event::ProviderSetupCompleted(ProviderSetupCompleted {
                provider_id: ProviderId::from(id),
                kind: ProviderKind::OpenaiCompatible,
                model: "kimi-k3".to_owned(),
                bindings: Vec::new(),
                dial_host: "api.moonshot.ai".to_owned(),
            })
        };

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));
        assert!(
            state.provider_ids.is_empty(),
            "the session opened with none"
        );

        render_event(&envelope(completed("kimi")), &mut surface, &mut state);
        assert_eq!(state.provider_ids, vec!["kimi".to_owned()]);

        // Twice is once: the daemon may replay, and a list that grew a duplicate
        // per attach would be a longer list saying the same thing.
        render_event(&envelope(completed("kimi")), &mut surface, &mut state);
        assert_eq!(state.provider_ids, vec!["kimi".to_owned()]);

        // Another session's registration is news, not vocabulary.
        render_event(
            &EventEnvelope::new(9, Some(SessionId::from("theirs")), completed("deepseek")),
            &mut surface,
            &mut state,
        );
        assert_eq!(state.provider_ids, vec!["kimi".to_owned()]);

        // …and it is still news before this client knows its own id (verify
        // G6). Events are pumped during `session/create` itself, so this window
        // is real: `other_session` folds "unknown" into `None` — the right
        // reading for a *notice*, which must not guess "in another session" —
        // and a cache that shared that fold would file a stranger's provider as
        // this user's own vocabulary, in the one window where nothing can
        // contradict it.
        let mut blind = SessionState::new();
        assert!(blind.session_id.is_none(), "the premise: no id yet");
        render_event(
            &EventEnvelope::new(10, Some(SessionId::from("theirs")), completed("deepseek")),
            &mut surface,
            &mut blind,
        );
        assert!(
            blind.provider_ids.is_empty(),
            "a session that does not yet know its own id has no evidence the \
             registration was its own: {:?}",
            blind.provider_ids
        );

        // And the point of all of it: the very next turn's question counts.
        state.begin_turn("is kimi working?");
        render_event(
            &envelope(tool_call("c1", "shell: teton provider list")),
            &mut surface,
            &mut state,
        );
        let mut turn = RecordingSurface::new();
        render_event(&envelope(chunk("it is registered.")), &mut turn, &mut state);
        hand_off_after_turn(&mut state, &mut turn, true);
        assert_eq!(
            turn.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "{:?}",
            turn.calls
        );
    }

    /// The registered ids are the user's own vocabulary for their providers.
    ///
    /// "is kimi working?" names no fixed subject word at all — it is a provider
    /// question only because `kimi` is a provider *on this machine*, which is a
    /// fact the config snapshot holds and this crate must not hard-code (ADR-4).
    /// The second half is the honest cost of an empty snapshot: the same turn
    /// earns nothing, because the id was the only subject it had.
    #[test]
    fn a_registered_provider_id_makes_a_bare_vendor_question_count() {
        let surface = connection_turn(
            "is kimi working?",
            &["shell: teton provider list"],
            &["it is registered and routed to think."],
            &["kimi"],
            true,
        );
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "{:?}",
            surface.calls
        );

        let surface = connection_turn(
            "is kimi working?",
            &["shell: teton provider list"],
            &["it is registered and routed to think."],
            &[],
            true,
        );
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "with no snapshot the vendor word is not a subject; got {:?}",
            surface.calls
        );
    }

    /// **Words, not substrings — the reading every half of the predicate uses.**
    ///
    /// The failures this closes were all one bug wearing different clothes: a
    /// verb found inside "la*test*" and "con*test*", `reach` inside "research",
    /// and — worst, because it scales with how short the id is — a registered id
    /// found inside any word containing its letters. The floor on id length is
    /// the second guard for the same class.
    #[test]
    fn the_connection_predicate_matches_whole_words_only() {
        // The helper itself, at its edges.
        assert!(contains_word("test the connection", "test"));
        assert!(contains_word("is it working?", "working"));
        assert!(contains_word("kimi, is it up", "kimi"));
        assert!(contains_word("test", "test"));
        assert!(!contains_word("the latest run", "test"));
        assert!(!contains_word("a contest", "test"));
        assert!(!contains_word("some research", "reach"));
        assert!(!contains_word("tetond is building", "teton"));
        assert!(!contains_word("anything", ""));

        // And through the predicate: a prompt that only *contains* the letters
        // is not a connection question.
        assert!(!asks_about_a_connection(
            "run the latest provider tests",
            &[]
        ));
        assert!(asks_about_a_connection("test the provider", &[]));

        // A short id contributes nothing rather than everything: `ds` inside
        // "roads" would otherwise make any sentence a provider question.
        let short = vec!["ds".to_owned()];
        assert!(!asks_about_a_connection("check the roads", &short));
        let long = vec!["deep".to_owned()];
        assert!(asks_about_a_connection("check deep", &long));
        assert!(
            !asks_about_a_connection("check deeply", &long),
            "an id must not match the head of a longer word"
        );
    }

    /// The tool half reads a `shell` call whose command **starts** with `teton`,
    /// and never the connection test itself.
    #[test]
    fn only_a_teton_shell_command_counts_as_a_probe() {
        for (title, expected) in [
            ("shell: teton provider list", true),
            ("shell: teton doctor", true),
            ("shell: teton policy show", true),
            // The right answer, run.
            ("shell: teton provider test kimi", false),
            // Not a shell call at all.
            ("read crates/teton/src/main.rs", false),
            ("read: crates/teton/src/main.rs", false),
            // `teton` somewhere in the middle of somebody else's command.
            ("shell: cargo test -p tetond", false),
            ("shell: grep -r teton .", false),
            ("shell: ls ~/.teton", false),
        ] {
            assert_eq!(
                shell_probed_teton(title),
                expected,
                "{title:?} was read wrongly"
            );
        }
    }

    /// Once per turn, and the turn's record does not outlive it.
    ///
    /// The same "consume the record" guarantee REQ-579's line has, asserted over
    /// the two fields ADR-4 added: a second call in the same turn has no prompt
    /// and no tool calls left to read, and the turn after it inherits neither.
    #[test]
    fn the_connection_hand_off_is_once_per_turn_and_does_not_outlive_it() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("can you check the kimi connection?");
        render_event(
            &envelope(tool_call("c1", "shell: teton policy show")),
            &mut surface,
            &mut state,
        );
        render_event(
            &envelope(chunk("everything is configured correctly.")),
            &mut surface,
            &mut state,
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "{:?}",
            surface.calls
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "a second call in the same turn adds nothing; got {:?}",
            surface.calls
        );

        // The next turn asks nothing and runs nothing, and must not inherit
        // either half of the previous turn's evidence.
        state.begin_turn("thanks");
        render_event(&envelope(chunk("no problem.")), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "a quiet turn must not reprint the previous turn's line; got {:?}",
            surface.calls
        );
    }

    /// The tool record is fed by **this** session's updates only.
    ///
    /// The bus is daemon-wide, so without the scope check another session's
    /// `shell: teton …` would arm a line about a turn this user never saw —
    /// the reason [`SessionState::turn_reply`] is scoped, applied to the second
    /// reader of the turn.
    #[test]
    fn another_sessions_tool_call_does_not_arm_the_connection_hand_off() {
        let ask = "can you test the provider connection?";
        let reply = "it looks fine to me.";

        // Every event carries a session id here, ours or theirs, so the only
        // difference between this half and the control below is *whose* tool
        // call it was.
        let ours =
            |seq: u64, event: Event| EventEnvelope::new(seq, Some(SessionId::from("ours")), event);

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("ours"));
        state.begin_turn(ask);
        render_event(
            &EventEnvelope::new(
                7,
                Some(SessionId::from("theirs")),
                tool_call("c1", "shell: teton provider list"),
            ),
            &mut surface,
            &mut state,
        );
        render_event(&ours(8, chunk(reply)), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "another session's tool call may not arm this session's line; got {:?}",
            surface.calls
        );

        // The control: the same tool call on our own envelope does arm it, so
        // this is a test about the session and not about the title.
        let mut surface = RecordingSurface::new();
        state.begin_turn(ask);
        render_event(
            &ours(9, tool_call("c2", "shell: teton provider list")),
            &mut surface,
            &mut state,
        );
        render_event(&ours(10, chunk(reply)), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "{:?}",
            surface.calls
        );
    }

    /// The sentence itself: it names the command, says what the command does,
    /// and carries no styling of its own.
    ///
    /// LESSON-517 keeps escapes in [`LineKind`]; BUG-168 asks for the thing
    /// stated outright rather than in an em-dash aside. It has to say *one call*
    /// and *what came back*, because the failure it follows is a turn that
    /// reported a connection nothing had dialled.
    #[test]
    fn the_connection_test_line_names_the_command_and_says_what_it_does() {
        let line = connection_test_line();
        assert!(
            line.contains("/provider test <id>"),
            "it must name the command with its argument: {line}"
        );
        assert!(
            line.contains("one consented call"),
            "it must say a call is made, and that it is consented: {line}"
        );
        assert!(
            line.contains("reports what came back"),
            "it must say what the user gets for it: {line}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "no escape may be baked into the text (LESSON-517): {line:?}"
        );
        assert!(
            !line.contains('\u{2014}'),
            "BUG-168: no em-dash aside: {line}"
        );

        // And it is exactly what reaches the surface — the constant and the
        // rendered line cannot drift.
        let surface = connection_turn(
            "can you verify the provider connection?",
            &["shell: teton doctor"],
            &["it all looks healthy."],
            &[],
            true,
        );
        assert_eq!(surface.lines_of(LineKind::Notice), vec![line]);
    }

    // -----------------------------------------------------------------------
    // The generic hand-off (REQ-582 ADR-6)
    // -----------------------------------------------------------------------

    /// **AC-9's first case.** A reply that recites two shell twins earns exactly
    /// one line, naming both `/` spellings.
    ///
    /// The second half is the ordering claim, and it is the reason the arm reads
    /// the table instead of the reply: the same two commands mentioned the other
    /// way round produce the same line. A line whose order came from the prose
    /// would be a different line each time the model rephrased the same answer,
    /// and a user who learns "the providers one comes first" would be learning
    /// something that is not true.
    #[test]
    fn a_reply_that_recites_shell_twins_names_their_session_spellings() {
        for (case, reply) in [
            (
                "AC-9's sentence, fenced",
                "run `teton provider list` and `teton policy show`.",
            ),
            (
                "the same two, in the other order",
                "start with teton policy show, then teton provider list.",
            ),
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec!["in this session: /provider list, /policy show"],
                "{case}: one line, both spellings, table order; got {:?}",
                surface.calls
            );
        }

        // One row is the ordinary case, and the one the line has to read well
        // as: no list, no comma, just the spelling.
        let surface = hand_off_turn(&["run `teton doctor` and paste the output."], true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /doctor"],
            "{:?}",
            surface.calls
        );
    }

    /// **AC-9's second case, and dormancy per command.** A reply that already
    /// names the `/` spelling earns nothing.
    ///
    /// REQ-579 ADR-9's rule, asked once per command rather than once per turn:
    /// the model said it, so the harness has nothing to add *about that command*
    /// — and still has something to add about the one it spelled only as a shell
    /// call. A turn-wide dormancy would let one correct mention silence every
    /// other row in the same reply, which is the shape of suppression the
    /// REQ-579 line had to have taken back out of it.
    #[test]
    fn a_reply_that_already_names_the_session_spelling_earns_nothing() {
        for (case, reply) in [
            (
                "it named the session spelling and nothing else",
                "run `/provider list` to see what is registered.",
            ),
            (
                "it named both spellings, so it already taught the mapping",
                "run `/provider list` (from a shell: `teton provider list`).",
            ),
            (
                "unfenced, as prose",
                "type /doctor at the prompt, or teton doctor from a shell.",
            ),
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "{case}: {:?}",
                surface.calls
            );
        }

        // Per command: one row named correctly does not cover for the other.
        let surface = hand_off_turn(&["run `/provider list`, then `teton policy show`."], true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /policy show"],
            "the row the reply spelled only as a shell command is still named; got {:?}",
            surface.calls
        );

        // Dormancy is a **word** match (verify m8): a `/doctor` inside a file
        // path is not the session spelling, so a reply that named the path and
        // told the user to run `teton doctor` still gets the nudge.
        let surface = hand_off_turn(
            &["the check lives in crates/teton/src/doctor.rs; run `teton doctor` to see it."],
            true,
        );
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /doctor"],
            "a path containing `/doctor` silenced the nudge; got {:?}",
            surface.calls
        );
    }

    /// A capitalised mention is prose about a command, not a command.
    ///
    /// The reply-side rule REQ-581 chose, inherited here rather than re-decided:
    /// a command is typed in lowercase, so the match is case-sensitive and this
    /// arm never lowercases the reply. Asserted on [`contains_word`] directly as
    /// well as through the turn, because the case-sensitivity is a property of
    /// *this* caller passing untouched text — the prompt-side callers lowercase
    /// first — and nothing in the helper itself would stop a later edit here.
    #[test]
    fn a_capitalised_mention_of_a_command_is_not_one() {
        assert!(contains_word(
            "run teton provider list now",
            "teton provider list"
        ));
        assert!(!contains_word(
            "Teton Provider List is how the marketing page spells it",
            "teton provider list"
        ));

        let surface = hand_off_turn(&["Teton Provider List is not how it is spelled."], true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "{:?}",
            surface.calls
        );
    }

    /// **AC-9's fourth case.** Prose that merely contains the word `teton` earns
    /// nothing, and neither does a command with no session row.
    ///
    /// LESSON-535: a false positive on a turn that was not about running a
    /// command is a finding, so the trigger is the exact `teton <sub>` token
    /// sequence and not a keyword. The rows without a `mirror` are the other
    /// half of the same claim — `teton provider test` is a real command, and
    /// naming its `/` spelling here would be REQ-581's line said badly.
    #[test]
    fn a_reply_that_names_no_mirrored_command_earns_nothing() {
        for reply in [
            "the teton binary is slow to start on this machine.",
            "teton is slow today.",
            // Not a mirrored row: `provider test` and `provider setup` are the
            // two commands whose session form is its own line above.
            "registration looks right; run teton provider test kimi to dial it.",
            // The daemon's crate is not the CLI — `contains_word`'s boundary,
            // relied on by this arm as much as by REQ-581's.
            "tetond provider list is not a command anybody can type.",
            // The subcommand without the binary is prose about output.
            "the provider list shows what is registered.",
            "",
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert!(
                surface.lines_of(LineKind::Notice).is_empty(),
                "{reply:?} must earn nothing; got {:?}",
                surface.calls
            );
        }
    }

    /// **AC-9's third case — precedence.** A setup recipe earns REQ-579's
    /// sentence and not the generic list.
    ///
    /// `teton provider add` and `teton policy set-tier` are mirrored rows *and*
    /// setup recipes, so every reply reciting one could earn either line. It gets
    /// the older one because that one carries a reason — "no key in chat" — and
    /// the generic line would replace it with a spelling on the exact turn the
    /// reason is worth reading (BR-8).
    #[test]
    fn the_setup_hand_off_wins_over_the_generic_line() {
        for (case, reply) in [
            (
                "the registration recipe",
                "run `teton provider add kimi --kind openai-compatible` first.",
            ),
            (
                "the routing recipe",
                "then run teton policy set-tier build kimi.",
            ),
            // Both kinds of row in one reply: still one line, and still the one
            // that says something.
            (
                "a recipe beside a plain mirrored row",
                "run teton provider add kimi, then teton policy show to check it.",
            ),
        ] {
            let surface = hand_off_turn(&[reply], true);
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec![hand_off_line()],
                "{case}: the setup line, alone; got {:?}",
                surface.calls
            );
        }
    }

    /// Precedence, the other side: a turn that earned REQ-581's line does not
    /// also get the generic one.
    ///
    /// The observed failure recites `teton provider list`, which is a mirrored
    /// row — so without the ordering this turn would print the spelling of the
    /// command that answered the *wrong* question, in place of the sentence
    /// saying which command answers the right one.
    #[test]
    fn the_connection_hand_off_wins_over_the_generic_line() {
        let surface = connection_turn(
            "can you test the kimi connection?",
            &["shell: teton provider list"],
            &["I ran teton provider list and kimi is registered, so it is working."],
            &["kimi"],
            true,
        );
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![connection_test_line()],
            "the connection line, alone; got {:?}",
            surface.calls
        );
    }

    /// The two guarantees the older lines have, inherited unchanged: a pipe sees
    /// nothing, and the turn's record is consumed whether a line printed or not.
    ///
    /// BR-11's byte-identity is the reason for the first — a script already has
    /// the shell command, and its output has to be what it was — and the `take`s
    /// at the top of [`hand_off_after_turn`] are the reason for the second, which
    /// is why "at most one line per turn" needs no flag to hold for a third arm.
    #[test]
    fn the_generic_line_is_tty_only_and_prints_once_per_turn() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");
        render_event(
            &envelope(chunk("run teton policy show to see the routing.")),
            &mut surface,
            &mut state,
        );

        hand_off_after_turn(&mut state, &mut surface, false);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "a pipe must see no hand-off; got {:?}",
            surface.calls
        );
        hand_off_after_turn(&mut state, &mut surface, true);
        assert!(
            surface.lines_of(LineKind::Notice).is_empty(),
            "the suppressed turn's words must not survive the gate; got {:?}",
            surface.calls
        );

        // On a TTY: one line, and a second call in the same turn adds nothing
        // because the first consumed the reply.
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.begin_turn("");
        render_event(
            &envelope(chunk("run teton policy show to see the routing.")),
            &mut surface,
            &mut state,
        );

        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /policy show"],
            "{:?}",
            surface.calls
        );
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /policy show"],
            "a second call in the same turn adds nothing; got {:?}",
            surface.calls
        );

        // And the next turn does not inherit the previous turn's words.
        state.begin_turn("");
        render_event(&envelope(chunk("done.")), &mut surface, &mut state);
        hand_off_after_turn(&mut state, &mut surface, true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["in this session: /policy show"],
            "a quiet turn must not reprint the previous turn's line; got {:?}",
            surface.calls
        );
    }

    /// **The candidates are the table's mirrored rows.**
    ///
    /// Driven from [`slash::mirrored_rows`] itself, so a row added to the table
    /// is covered here the day it lands rather than the day somebody remembers
    /// to add a case — which is the whole reason the arm reads the table (BR-7's
    /// rule extended to the hand-off). The slash module pins the other end of
    /// the same claim: that `mirrored_rows` is exactly the `mirror` rows of
    /// `COMMANDS`, each named after the command it mirrors.
    ///
    /// Two rows expect the *setup* line rather than the generic one, and that is
    /// the precedence above stated as a property of the table: a mirrored row
    /// which is also a `PROVIDER_CLI_RECIPES` entry is answered by the sentence
    /// with the reason in it.
    #[test]
    fn every_mirrored_row_is_a_candidate_of_the_generic_line() {
        let rows: Vec<(&str, &str)> = slash::mirrored_rows().collect();
        assert!(
            !rows.is_empty(),
            "the table has mirrored rows; a vacuous loop would prove nothing"
        );

        for (name, shell) in &rows {
            let recital = format!("run {shell} and read what it prints.");
            let surface = hand_off_turn(&[recital.as_str()], true);
            let expected = if PROVIDER_CLI_RECIPES.contains(shell) {
                hand_off_line().to_owned()
            } else {
                format!("in this session: /{name}")
            };
            assert_eq!(
                surface.lines_of(LineKind::Notice),
                vec![expected],
                "{shell:?} is a mirrored row and must be nudged for; got {:?}",
                surface.calls
            );
        }

        // All of them at once — minus the setup recipes, which the arm above
        // never sees — is the ordering claim over the whole table rather than
        // over the pair AC-9 names.
        let plain: Vec<(&str, &str)> = rows
            .iter()
            .copied()
            .filter(|(_, shell)| !PROVIDER_CLI_RECIPES.contains(shell))
            .collect();
        let reply: String = plain
            .iter()
            .map(|(_, shell)| format!("{shell}\n"))
            .collect();
        let expected = format!(
            "in this session: {}",
            plain
                .iter()
                .map(|(name, _)| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let surface = hand_off_turn(&[reply.as_str()], true);
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![expected],
            "every mirrored row the arm can reach, once each, in table order; got {:?}",
            surface.calls
        );
    }

    /// The line itself: plain, and it names commands the session can dispatch.
    ///
    /// Shaped like the two sentences above and for their reasons — no escape
    /// baked into the text (LESSON-517), no em-dash aside (BUG-168) — and one
    /// claim of its own: every spelling it prints is a `/` form of a real row,
    /// because it is built from the row table and from nothing else.
    #[test]
    fn the_generic_line_names_only_spellings_the_session_dispatches() {
        let names: Vec<&str> = slash::mirrored_rows().map(|(name, _)| name).collect();
        let line = generic_hand_off_line(&names);

        assert!(
            line.starts_with(GENERIC_HAND_OFF_PREFIX),
            "it opens with the phrase that makes it about this session: {line}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "no escape may be baked into the text (LESSON-517): {line:?}"
        );
        assert!(
            !line.contains('\u{2014}'),
            "BUG-168: no em-dash aside: {line}"
        );
        for spelling in line[GENERIC_HAND_OFF_PREFIX.len()..].split(", ") {
            let name = spelling.strip_prefix('/').unwrap_or_else(|| {
                panic!("every spelling is a slash command: {spelling:?} in {line}")
            });
            assert!(
                names.contains(&name),
                "{spelling:?} is not a row of the table: {line}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// REQ-585: the pipe rule, the consent subject, and the echo line
// ---------------------------------------------------------------------------

#[cfg(test)]
mod skill_tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use crate::render::RecordingSurface;
    use teton_protocol::events::{SessionRootChanged, SkillInvoked};
    use teton_protocol::methods::{RootKind, SkillSource};
    use teton_protocol::{RequestId, SessionId};

    /// An envelope for this client's own session, as the module above spells
    /// one.
    fn envelope(event: Event) -> EventEnvelope {
        EventEnvelope::new(1, Some(SessionId::from("s1")), event)
    }

    /// The subject a skill consent carries: three commands, already substituted,
    /// in document order (ADR-7), for a skill the **user** typed.
    fn skill_subject(skill: &str) -> PermissionSubject {
        skill_subject_from(skill, events::InvokedBy::User)
    }

    /// [`skill_subject`], with who asked (REQ-587 BR-5).
    fn skill_subject_from(skill: &str, invoked_by: events::InvokedBy) -> PermissionSubject {
        PermissionSubject::SkillDynamicContext {
            skill: skill.to_owned(),
            source: SkillSource::User,
            commands: vec![
                "ls -1 .adlc/specs".to_owned(),
                "git status --short".to_owned(),
                "grep -c '' README.md".to_owned(),
            ],
            invoked_by,
        }
    }

    /// BR-4's acknowledgment subject, as the daemon mints one: the root
    /// home-relative, the named set in registry order with the shadowing entry
    /// marked, and the tail as a count.
    fn trust_subject(more: u32) -> PermissionSubject {
        PermissionSubject::ProjectSkillTrust {
            root: "~/dev/teton".to_owned(),
            skills: vec![
                events::ProjectSkillTrustEntry {
                    name: "validate".to_owned(),
                    shadows_user_skill: true,
                },
                events::ProjectSkillTrustEntry {
                    name: "canary".to_owned(),
                    shadows_user_skill: false,
                },
            ],
            more,
        }
    }

    /// The acknowledgment as the daemon raises it: under
    /// `project_skill_trust:<root>`, never a skill's key and never a tool's
    /// name, and with no `description` — the subject carries the facts.
    fn trust_permission_request(subject: Option<PermissionSubject>) -> PermissionRequest {
        PermissionRequest {
            request_id: RequestId::from("r-trust"),
            tool_name: teton_protocol::methods::project_skill_trust_key("~/dev/teton"),
            ..skill_permission_request(subject)
        }
    }

    /// A skill consent as the daemon raises it: the grant key is ADR-6's
    /// `skill:<source>:<name>`, and nothing in this crate reads its shape.
    fn skill_permission_request(subject: Option<PermissionSubject>) -> PermissionRequest {
        PermissionRequest {
            request_id: RequestId::from("r-skill"),
            tool_name: "skill:user:status".to_owned(),
            description: None,
            subject,
            options: vec![
                PermissionOption {
                    option_id: "allow_once".to_owned(),
                    label: "Allow once".to_owned(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "allow_always".to_owned(),
                    label: "Allow for session".to_owned(),
                    kind: PermissionOptionKind::AllowAlways,
                },
                PermissionOption {
                    option_id: "reject_once".to_owned(),
                    label: "Reject once".to_owned(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
        }
    }

    /// A subject whose `kind` this build has never heard of, arriving through
    /// serde exactly as a future daemon's would — never constructed by hand,
    /// because the variant only exists as `#[serde(other)]`'s output and a
    /// hand-built one would prove nothing about the wire (LESSON-544).
    fn unrecognized_subject() -> PermissionSubject {
        let wire = serde_json::json!({ "kind": "something_invented_later", "detail": 7 });
        let subject: PermissionSubject =
            serde_json::from_value(wire).expect("an unknown kind degrades, never errors");
        assert_eq!(subject, PermissionSubject::Unrecognized);
        subject
    }

    // ----------------------------------------------------------------- gate

    /// **ADR-8's truth table.** All eight rows of a two-input predicate, pinned
    /// the way `cli_rows::write_gate`'s are — because the two rows that matter
    /// (a pipe, and a subject from the future) are the ones a test process
    /// cannot reach through a real terminal.
    #[test]
    fn the_consent_gate_is_a_truth_table_over_the_subject_and_the_terminal() {
        let skill = skill_subject("status");
        let trust = trust_subject(0);
        let unknown = unrecognized_subject();
        for (case, subject, typed_input, expected) in [
            (
                "no subject, at a terminal: every prompt before REQ-585",
                None,
                true,
                ConsentGate::Answerable,
            ),
            (
                "no subject, on a pipe: the shell consent still answers from \
                 the next line, and BR-11 narrows nothing else",
                None,
                false,
                ConsentGate::Answerable,
            ),
            (
                "a skill's dynamic context, at a terminal: ask",
                Some(&skill),
                true,
                ConsentGate::Answerable,
            ),
            (
                "a skill's dynamic context, on a pipe: refuse (BR-11)",
                Some(&skill),
                false,
                ConsentGate::RefuseNoTerminal,
            ),
            (
                "the project-skill acknowledgment, at a terminal: ask (BR-4)",
                Some(&trust),
                true,
                ConsentGate::Answerable,
            ),
            (
                "the project-skill acknowledgment, on a pipe: refuse — and \
                 `RefuseNoTerminal`, not `RefuseUnrecognized`: this build knows \
                 exactly what it is being asked and there is simply nobody to \
                 ask (the placeholder arm TASK-210 left fails here)",
                Some(&trust),
                false,
                ConsentGate::RefuseNoTerminal,
            ),
            (
                "an unknown subject, at a terminal: still refused — a question \
                 this build cannot show is not one it may answer",
                Some(&unknown),
                true,
                ConsentGate::RefuseUnrecognized,
            ),
            (
                "an unknown subject, on a pipe: refused",
                Some(&unknown),
                false,
                ConsentGate::RefuseUnrecognized,
            ),
        ] {
            assert_eq!(consent_gate(subject, typed_input), expected, "{case}");
        }
    }

    // ------------------------------------------------- the negative pin

    /// **BR-11 / AC-9, written as the negative pin it is.** A piped session at
    /// a level that would ask refuses the skill consent **without reading a
    /// line** — the prompter is scripted with the `y` a paste would have left
    /// sitting there, and the assertion is that it is still sitting there.
    ///
    /// This is LESSON-537's shape: `StdinPrompter::ask` reads unconditionally,
    /// so a refusal computed *after* the call has already eaten the user's next
    /// prompt line and turned a pasted `y` into consent for shell commands.
    /// Moving `prompter.ask` above the gate fails here.
    #[test]
    fn a_piped_skill_consent_is_refused_without_reading_a_line() {
        let req = skill_permission_request(Some(skill_subject("status")));
        let mut surface = RecordingSurface::new();
        // The line a paste would have queued behind the invocation. It must
        // survive as the *next prompt line*, not become an answer.
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false);

        assert_eq!(
            prompter.asked, 0,
            "the gate ran before `ask`: {:?}",
            prompter.questions
        );
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Refused {
                reason: RefusalReason::NoTerminal
            }
        );
        assert!(
            !grants.is_allow_always("skill:user:status")
                && !grants.is_reject_always("skill:user:status"),
            "a refusal nobody answered records no session grant"
        );
    }

    /// **REQ-587 BR-4's pipe rule, and the negative pin is the one that
    /// matters.** A piped session at a level that would ask refuses the
    /// acknowledgment **without reading a line** — the `y` a paste would have
    /// left queued is still queued afterwards, and arrives as the user's next
    /// *prompt* line rather than as consent for a repository's skills to reach
    /// the model as instructions.
    ///
    /// **Mutation.** Treat the new subject as answerable on a pipe — the
    /// `Answerable` row, or `prompter.ask` moved above the gate — and
    /// `prompter.asked` is 1 here. Answer it `RefuseUnrecognized` instead, as
    /// TASK-210's placeholder did, and the outcome assertion fails: the reason
    /// is what tells the daemon whether anybody could have been asked, and
    /// "this build does not know what it is asking" is false of a build that
    /// draws the question two tests below.
    #[test]
    fn a_piped_project_skill_acknowledgment_is_refused_without_reading_a_line() {
        let req = trust_permission_request(Some(trust_subject(0)));
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false);

        assert_eq!(
            prompter.asked, 0,
            "the gate ran before `ask`: {:?}",
            prompter.questions
        );
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Refused {
                reason: RefusalReason::NoTerminal
            },
            "a refusal is never the cancellation a dismissed prompt produces, \
             and never `UnrecognizedSubject` for a question this build can draw"
        );
        assert!(
            !grants.is_allow_always(&req.tool_name) && !grants.is_reject_always(&req.tool_name),
            "a refusal nobody answered records no session grant"
        );
        // Refused, and *said* — with the root named from the subject rather
        // than from the key, and the unattended remedy BR-4 gives.
        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(notices.len(), 1, "one line, not a paragraph: {notices:?}");
        assert!(
            notices[0].contains("`~/dev/teton`") && notices[0].contains("not a terminal"),
            "{}",
            notices[0]
        );
        assert!(
            notices[0].contains("/permissions full"),
            "the model is to be told the user must acknowledge or run at full: {}",
            notices[0]
        );
        // What was refused is still shown: a piped session is told which
        // repository and which skills, not merely that something was refused.
        let shown = surface.lines_of(LineKind::Prompt);
        assert!(
            shown.iter().any(|line| line.contains("~/dev/teton"))
                && shown.iter().any(|line| line.contains("validate")),
            "the subject block did not render on the refusing path: {shown:?}"
        );
    }

    /// **BR-4's prompt bytes.** The root, the named set in registry order, the
    /// shadowing entry marked in the spelling the daemon's expansion frame uses
    /// for the same fact, and the bounded tail as `+N more`.
    ///
    /// One `Surface::line` per entry, for the command list's mechanical reason:
    /// `line` defuses, and defusing destroys newlines, so a joined list would
    /// arrive as one run-on line.
    #[test]
    fn the_acknowledgment_names_the_root_marks_shadowing_and_counts_the_tail() {
        let req = trust_permission_request(Some(trust_subject(5)));
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);

        assert_eq!(prompter.asked, 1, "at a terminal it is asked");
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        );
        let lines = surface.lines_of(LineKind::Prompt);
        let at = lines
            .iter()
            .position(|line| line.contains("this repository's skills"))
            .unwrap_or_else(|| panic!("no acknowledgment block: {lines:?}"));
        assert_eq!(
            &lines[at..at + 4],
            [
                "  the model wants to run this repository's skills as instructions: ~/dev/teton",
                "    validate (project — shadows your user skill)",
                "    canary",
                "    +5 more",
            ],
            "{lines:?}"
        );
        // The root is home-relative wherever it is rendered: BR-1's entity
        // table, and the reason the daemon sends a display and not a path.
        assert!(
            !lines.iter().any(|line| line.contains("/Users/")),
            "a username reached the prompt: {lines:?}"
        );
    }

    /// A complete list has no tail line: `+0 more` is a line about nothing, and
    /// the count is the daemon's fact rather than a re-count of what this side
    /// was handed.
    #[test]
    fn a_complete_acknowledgment_list_prints_no_tail() {
        let req = trust_permission_request(Some(trust_subject(0)));
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();
        resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);

        assert!(
            !surface
                .lines_of(LineKind::Prompt)
                .iter()
                .any(|line| line.contains("more")),
            "{:?}",
            surface.lines_of(LineKind::Prompt)
        );
    }

    /// **BR-11's fail-closed half.** A subject this build does not recognize is
    /// refused rather than falling through to `prompter.ask`, and it is refused
    /// **at a terminal too**: there is nothing to show, so there is nothing to
    /// ask. Treating `Unrecognized` as answerable fails here.
    #[test]
    fn an_unrecognized_subject_is_refused_without_reading_a_line_even_at_a_terminal() {
        for typed_input in [true, false] {
            let req = skill_permission_request(Some(unrecognized_subject()));
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&["y"]);
            let mut grants = SessionGrants::default();

            let resp =
                resolve_permission(&req, &mut surface, &mut prompter, &mut grants, typed_input);

            assert_eq!(prompter.asked, 0, "typed_input={typed_input}");
            assert_eq!(
                resp.outcome,
                PermissionOutcome::Refused {
                    reason: RefusalReason::UnrecognizedSubject
                },
                "typed_input={typed_input}"
            );
        }
    }

    /// **ADR-7's naming rule.** A refusal is `Refused`, never `Cancelled` —
    /// `Cancelled` already means *the user dismissed the prompt*, and it is what
    /// EOF on a pipe returns for an ordinary request. The two outcomes are
    /// asserted side by side so that collapsing them is a failure rather than a
    /// silent re-spelling: AC-9's placeholder can only say "no human could be
    /// asked" if the daemon is told which of the two happened.
    #[test]
    fn a_refusal_is_never_the_cancellation_a_dismissed_prompt_produces() {
        let refused = {
            let req = skill_permission_request(Some(skill_subject("status")));
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            let mut grants = SessionGrants::default();
            resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false).outcome
        };
        let dismissed = {
            // The same session, the same empty stdin — but an ordinary request,
            // which is read to EOF and cancels.
            let req = skill_permission_request(None);
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&[]);
            let mut grants = SessionGrants::default();
            resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false).outcome
        };

        assert_eq!(
            refused,
            PermissionOutcome::Refused {
                reason: RefusalReason::NoTerminal
            }
        );
        assert_eq!(dismissed, PermissionOutcome::Cancelled);
        assert_ne!(
            refused, dismissed,
            "a refusal nobody was asked for is not a dismissal"
        );
    }

    /// **The narrowing is exactly one case wide.** An ordinary tool prompt on a
    /// pipe still answers from the next stdin line, as every shipped script
    /// depends on: a gate that generalized BR-11 to every request would be a
    /// silent change to the shell consent's piped behaviour.
    #[test]
    fn an_ordinary_prompt_on_a_pipe_still_answers_from_the_next_line() {
        let req = skill_permission_request(None);
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false);

        assert_eq!(prompter.asked, 1);
        assert_eq!(
            resp.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        );
    }

    // ------------------------------------------------- consent rendering

    /// **ADR-7's mechanical half.** Three commands reach the surface as three
    /// lines, each carrying its command verbatim. `Surface::line` defuses, and
    /// defusing destroys newlines, so a joined string could not have carried
    /// them — which is why the subject is a structure and not a description.
    #[test]
    fn a_skill_consent_lists_every_command_on_its_own_line() {
        let subject = skill_subject("status");
        let PermissionSubject::SkillDynamicContext { commands, .. } = &subject else {
            panic!("the fixture is a skill subject");
        };
        let commands = commands.clone();

        let req = skill_permission_request(Some(subject));
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["y"]);
        let mut grants = SessionGrants::default();
        resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);

        let lines = surface.lines_of(LineKind::Prompt);
        for command in &commands {
            let hits = lines.iter().filter(|line| line.contains(command)).count();
            assert_eq!(hits, 1, "{command:?} rides exactly one line: {lines:?}");
        }
        assert!(
            lines.iter().any(|line| line.contains("skill `status`")
                && line.contains("(user)")
                && line.contains("3 dynamic-context commands")),
            "the block names the skill, its source and the count: {lines:?}"
        );
    }

    /// **REQ-587 BR-5: the consent says who asked.** "You asked for `status`"
    /// and "the model decided to run `status`" are different questions carrying
    /// the same command list, and the human at `guarded` is entitled to know
    /// which one is on the screen.
    ///
    /// The user's line is asserted **verbatim and unchanged**, because
    /// `pty_e2e` pins those bytes against a real terminal: BR-9's attribution is
    /// an inserted clause on the model's line, never a rewording of REQ-585's.
    #[test]
    fn a_skill_consent_says_when_the_model_was_the_one_that_asked() {
        let ask = |invoked_by| {
            let req = skill_permission_request(Some(skill_subject_from("status", invoked_by)));
            let mut surface = RecordingSurface::new();
            let mut prompter = ScriptedPrompter::new(&["y"]);
            let mut grants = SessionGrants::default();
            resolve_permission(&req, &mut surface, &mut prompter, &mut grants, true);
            surface
                .lines_of(LineKind::Prompt)
                .iter()
                .find(|line| line.contains("dynamic-context command"))
                .expect("the block names the skill and the count")
                .to_string()
        };

        assert_eq!(
            ask(events::InvokedBy::User),
            "  skill `status` (user) wants to run 3 dynamic-context commands:",
            "REQ-585's prompt keeps its bytes; `pty_e2e` pins them",
        );
        assert_eq!(
            ask(events::InvokedBy::Model),
            "  skill `status` (user), invoked by the model, wants to run 3 \
             dynamic-context commands:",
        );
    }

    /// The refusal says what was checked and names the remedy, because a
    /// refusal without one is a dead end — and the remedy is BR-11's stated
    /// automation posture, not an invitation to type at a terminal.
    #[test]
    fn the_no_terminal_refusal_names_the_skill_and_the_unattended_remedy() {
        let req = skill_permission_request(Some(skill_subject("status")));
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&[]);
        let mut grants = SessionGrants::default();
        resolve_permission(&req, &mut surface, &mut prompter, &mut grants, false);

        let notices = surface.lines_of(LineKind::Notice);
        assert_eq!(notices.len(), 1, "one line, not a paragraph: {notices:?}");
        assert!(notices[0].contains("skill `status`"), "{}", notices[0]);
        assert!(
            notices[0].contains("not a terminal"),
            "it reports what was checked: {}",
            notices[0]
        );
        // The remedy has to be something that exists. `--permissions` is not a
        // flag — `teton`'s globals are `--yes` and `--verbose` — so a line
        // naming one would send an unattended user to a parse error at the one
        // moment they cannot be asked anything. Both spellings below are real:
        // a `/permissions full` line piped ahead of the invocation, and the
        // config key a runner sets once.
        assert!(
            notices[0].contains("/permissions full"),
            "it names the unattended remedy: {}",
            notices[0]
        );
        assert!(
            notices[0].contains("[permissions] default_level"),
            "it names the durable remedy too: {}",
            notices[0]
        );
        assert!(
            !notices[0].contains("--permissions"),
            "the remedy named a flag that does not exist: {}",
            notices[0]
        );
    }

    // --------------------------------------------------------- echo line

    /// One invocation, as the daemon publishes it — the **user**-typed one,
    /// which is REQ-585's whole world and every byte of it is still pinned.
    fn invoked(outcomes: Vec<DynamicOutcomeView>) -> SkillInvoked {
        invoked_by(outcomes, events::InvokedBy::User)
    }

    /// [`invoked`], with who issued it (REQ-587 BR-9).
    fn invoked_by(
        outcomes: Vec<DynamicOutcomeView>,
        invoked_by: events::InvokedBy,
    ) -> SkillInvoked {
        SkillInvoked {
            name: "status".to_owned(),
            source: SkillSource::User,
            path_display: "~/.claude/skills/status/SKILL.md".to_owned(),
            body_bytes: 5_432,
            ignored_keys: vec!["allowed-tools".to_owned(), "model".to_owned()],
            name_note: None,
            outcomes,
            invoked_by,
            // The ordinary user-typed row: nobody's name was taken, both doors
            // are open, and no per-turn budget was spent. Every REQ-585
            // assertion below runs against exactly this, which is what makes
            // "those bytes did not move" a claim about the shipped line rather
            // than about a fixture that happens to avoid the new branches. The
            // varied ones are built from it by the three helpers under this.
            shadows_user_skill: false,
            model_invocable: true,
            user_invocable: true,
            turn_invocations: None,
            // Not refused: this is the record of a skill that ran. The refused
            // shape is `refusal` below, and it is deliberately built from this
            // one — a refused record differs from a command-free successful
            // record in exactly this field, which is the whole reason the field
            // exists.
            refused: None,
        }
    }

    /// A record of a call the loop **refused**, as either loop stage publishes
    /// one: no commands, no outcomes, the turn's count, and the reason id.
    fn refusal(reason: &str, count: u32) -> SkillInvoked {
        SkillInvoked {
            outcomes: Vec::new(),
            turn_invocations: Some(events::TurnInvocations { count, cap: 12 }),
            refused: Some(reason.to_owned()),
            ..invoked_by(Vec::new(), events::InvokedBy::Model)
        }
    }

    /// [`invoked`] of a **project** skill that took a user skill's name — the
    /// swap BR-9's echo line names and BR-4's acknowledgment asks about.
    fn shadowing(outcomes: Vec<DynamicOutcomeView>, by: events::InvokedBy) -> SkillInvoked {
        SkillInvoked {
            name: "validate".to_owned(),
            source: SkillSource::Project,
            path_display: "~/dev/teton/.claude/skills/validate/SKILL.md".to_owned(),
            shadows_user_skill: true,
            ..invoked_by(outcomes, by)
        }
    }

    /// A model invocation carrying BR-6a's count, as the tool publishes one.
    fn counted(count: u32, cap: u32) -> SkillInvoked {
        SkillInvoked {
            turn_invocations: Some(events::TurnInvocations { count, cap }),
            ..invoked_by(vec![ran("date", 8)], events::InvokedBy::Model)
        }
    }

    /// [`invoked`] with BR-3's two frontmatter flags set as a file wrote them.
    fn flagged(user_invocable: bool, model_invocable: bool) -> SkillInvoked {
        SkillInvoked {
            user_invocable,
            model_invocable,
            ..invoked(vec![ran("date", 8)])
        }
    }

    /// [`flagged`] for a file whose `disable-model-invocation` value this build
    /// could **not** read — `disable-model-invocation: yes`, say.
    ///
    /// The wire shape is BR-3's safe reading exactly as the daemon publishes it:
    /// the model is shut out (`model_invocable: false`), the user's door is
    /// whatever the other flag said, and the key is named on `ignored_keys`
    /// because the value was not honored. That list is the *only* signal the
    /// event carries that a typo rather than a declaration produced this row —
    /// the raw value is not on the wire.
    fn flagged_unreadable(user_invocable: bool) -> SkillInvoked {
        SkillInvoked {
            ignored_keys: vec!["disable-model-invocation".to_owned()],
            ..flagged(user_invocable, false)
        }
    }

    fn ran(command: &str, bytes: u64) -> DynamicOutcomeView {
        DynamicOutcomeView {
            command: command.to_owned(),
            outcome: DynamicOutcome::Ran {
                output_bytes: bytes,
                truncated: false,
            },
        }
    }

    /// **BR-12 / AC-19.** Every invocation echoes exactly one line naming the
    /// skill, its source, its size and how many dynamic commands it had — and
    /// the **body is never printed**, which is the half a byte assertion can
    /// only make by counting what was drawn.
    #[test]
    fn an_invocation_echoes_one_line_and_never_the_body() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(
            &envelope(Event::SkillInvoked(invoked(vec![
                ran("ls -1", 120),
                ran("git status --short", 40),
                ran("date", 30),
                ran("grep -c '' README.md", 4),
            ]))),
            &mut surface,
            &mut state,
        );

        assert!(matches!(outcome, EventOutcome::Rendered));
        assert_eq!(
            surface.calls.len(),
            1,
            "one line, and nothing else: {:?}",
            surface.calls
        );
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["/status → skill status (user, 5.3 KiB, 4 dynamic commands)"]
        );
    }

    /// **REQ-587 BR-9 / AC-10, in the shipped spellings.** A model invocation
    /// echoes one line saying so — `teton_protocol::format_bytes`, so `KiB` and
    /// not the spec's illustrative `KB`, and **both** counts whenever they
    /// differ, which AC-5's declined path produces routinely.
    ///
    /// It carries **no `/status →` prefix**: nobody typed that line, and a
    /// transcript that showed one would read exactly like the user's own — the
    /// single distinction the suffix exists to draw. The parenthetical is
    /// byte-identical to the user line's, which is the other half of the claim.
    #[test]
    fn a_model_invocation_says_so_and_carries_no_typed_prefix() {
        let outcomes = vec![
            ran("ls -1", 120),
            DynamicOutcomeView {
                command: "date".to_owned(),
                outcome: DynamicOutcome::NotRun {
                    reason: NotRunReason::Declined,
                },
            },
            DynamicOutcomeView {
                command: "git status".to_owned(),
                outcome: DynamicOutcome::NotRun {
                    reason: NotRunReason::Declined,
                },
            },
        ];
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(
            &envelope(Event::SkillInvoked(invoked_by(
                outcomes.clone(),
                events::InvokedBy::Model,
            ))),
            &mut surface,
            &mut state,
        );

        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec!["skill status (user, 5.3 KiB, 3 dynamic commands, 1 run) — invoked by the model"],
        );
        assert_eq!(surface.calls.len(), 1, "still one line, and never the body");

        // The same invocation typed by the user: the prefix returns, the suffix
        // goes, and everything between them is the same bytes.
        let user = skill_echo_line(&invoked(outcomes));
        assert_eq!(
            user,
            "/status → skill status (user, 5.3 KiB, 3 dynamic commands, 1 run)",
        );
    }

    /// **BR-9's shadowing clause, in the source slot.** A project skill that
    /// took a user skill's name says so on the one line every invocation
    /// prints: the user asked for `validate` and a file the repository
    /// substituted answered, which is the same swap BR-4's acknowledgment asks
    /// about and the daemon's expansion frame names to the model.
    ///
    /// It is on the **typed** line too, and that is the case worth having:
    /// `/validate` in a repository that defines its own reaches the
    /// repository's file with no prompt at any level, so the echo line is the
    /// only notice the user gets.
    ///
    /// **Mutation.** Drop the clause — `source_word` in place of
    /// `source_words`, or a `source_words` that ignores its second argument —
    /// and both assertions here fail.
    #[test]
    fn a_shadowing_invocation_names_the_swap_in_the_source_slot() {
        assert_eq!(
            skill_echo_line(&shadowing(vec![ran("date", 8)], events::InvokedBy::User)),
            "/validate → skill validate (project — shadows your user skill, 5.3 KiB, \
             1 dynamic command)",
        );
        assert_eq!(
            skill_echo_line(&shadowing(vec![ran("date", 8)], events::InvokedBy::Model)),
            "skill validate (project — shadows your user skill, 5.3 KiB, 1 dynamic command) \
             — invoked by the model",
        );
        // An ordinary project skill takes nobody's name and says nothing about
        // it: the clause is news, not decoration.
        let plain = SkillInvoked {
            shadows_user_skill: false,
            ..shadowing(Vec::new(), events::InvokedBy::Model)
        };
        assert_eq!(
            skill_echo_line(&plain),
            "skill validate (project, 5.3 KiB, 0 dynamic commands) — invoked by the model",
        );
    }

    /// **The `/verbose` flags line speaks `/help`'s words.** BR-3's two states
    /// have one home (`slash::model_only_words`), so a file both flags deny
    /// cannot be `invocable by nobody` in `/help` and `model-only` here — the
    /// only case where two spellings of that precedence would differ, and the
    /// case where the difference is a claim that the model is running a skill
    /// no roster contains.
    ///
    /// The literals are pinned in **both** files deliberately: the code has one
    /// home, and re-spelling it reddens `/help`'s row goldens and this test
    /// together rather than leaving one surface to drift.
    ///
    /// The ordinary file gets **no line**, on the `ignored frontmatter` rule:
    /// this block reports what the file wrote, and a file that declared no flag
    /// declared nothing to report.
    #[test]
    fn verbose_names_the_flags_in_the_same_words_help_marks_them_with() {
        let flags_line = |user_invocable, model_invocable| -> Option<String> {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = true;
            render_event(
                &envelope(Event::SkillInvoked(flagged(
                    user_invocable,
                    model_invocable,
                ))),
                &mut surface,
                &mut state,
            );
            surface
                .lines_of(LineKind::Info)
                .iter()
                .find(|line| line.contains("invocable") || line.contains("hidden"))
                .map(|line| (*line).to_owned())
        };

        assert_eq!(
            flags_line(false, true).as_deref(),
            Some("  model-only (`user-invocable: false`)"),
        );
        assert_eq!(
            flags_line(false, false).as_deref(),
            Some(
                "  invocable by nobody (`user-invocable: false`, \
                 `disable-model-invocation: true`)"
            ),
        );
        // The user's door open, the model's shut: `/help` marks this row not at
        // all, because the user may type it — so this line is the only place
        // the flag is named, and the two surfaces contradict nothing.
        assert_eq!(
            flags_line(true, false).as_deref(),
            Some("  hidden from the model (`disable-model-invocation: true`)"),
        );
        assert_eq!(
            flags_line(true, true),
            None,
            "a default is not a declaration"
        );
    }

    /// **BR-3's named diagnostic must not name the opposite of what happened.**
    ///
    /// A file writing `disable-model-invocation: yes` is hidden from the model —
    /// that is BR-3's safe reading of a value the parser cannot read — *and* has
    /// its key on the `ignored frontmatter` line, because the daemon did not
    /// honor the value it was given. Quoting `disable-model-invocation: true`
    /// back at that author shows them a line their file does not contain, one
    /// line above a line that on its own reads "this key had no effect"; of the
    /// two, the harmless-sounding one is the false one, and the author of the
    /// typo is exactly the reader this block exists for.
    ///
    /// Both `/verbose` lines are asserted **together**, because the failure was
    /// never in either line alone — it was in what they say side by side.
    ///
    /// **Mutation.** Return the literal unconditionally from
    /// `model_flag_clause` (the shape before this split) and the first two
    /// assertions fail; drop the malformed branch's reference to the reading it
    /// took and the third does.
    #[test]
    fn verbose_tells_an_unreadable_flag_value_apart_from_the_literal_that_hid_the_file() {
        let block = |invoked: SkillInvoked| -> Vec<String> {
            let mut surface = RecordingSurface::new();
            let mut state = SessionState::new();
            state.verbose = true;
            render_event(
                &envelope(Event::SkillInvoked(invoked)),
                &mut surface,
                &mut state,
            );
            surface
                .lines_of(LineKind::Info)
                .iter()
                .map(|line| (*line).to_owned())
                .collect()
        };

        let typo = block(flagged_unreadable(true));
        assert!(
            typo.iter().any(|line| line
                == "  hidden from the model (`disable-model-invocation` was not `true` or \
                    `false`, so the safe reading hid it)"),
            "the file wrote `yes`; the line must say the value was not a boolean \
             and name the reading it took: {typo:?}"
        );
        assert!(
            !typo
                .iter()
                .any(|line| line.contains("`disable-model-invocation: true`")),
            "a value the file never wrote was quoted back at its author: {typo:?}"
        );
        // And the key is still on the ignored line — the daemon's own
        // diagnostic, which is now explained rather than contradicted.
        assert!(
            typo.iter()
                .any(|line| line == "  ignored frontmatter: disable-model-invocation"),
            "the unhonored key must still be named: {typo:?}"
        );

        // The two-flag file, whose model flag is the unreadable one: the
        // `user-invocable` half is a literal (its safe reading is the unchanged
        // one, so `false` can only have been written) and the other is not.
        let both = block(flagged_unreadable(false));
        assert!(
            both.iter().any(|line| line
                == "  invocable by nobody (`user-invocable: false`, \
                    `disable-model-invocation` was not `true` or `false`, so the safe \
                    reading hid it)"),
            "the two flags must be quoted as each was written: {both:?}"
        );

        // The honored value still reads as the declaration it is — the reverse
        // mutation, which would tell every author their `true` was a typo.
        let honored = block(flagged(true, false));
        assert!(
            honored
                .iter()
                .any(|line| line == "  hidden from the model (`disable-model-invocation: true`)"),
            "a file that wrote the literal must be quoted verbatim: {honored:?}"
        );
    }

    /// **BR-9's `/verbose` count, against the cap.** A bare "3" cannot tell a
    /// turn halfway through its budget from one at its last permitted call, and
    /// the `per_turn_cap` refusal would then arrive as a surprise. The ceiling
    /// rides with the count rather than being hardcoded here, so a daemon that
    /// moves it does not leave this client printing a stale one.
    ///
    /// **Mutation.** Drop the line, or render the count without the cap, and
    /// this fails.
    #[test]
    fn verbose_counts_the_turns_invocations_against_the_cap() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(counted(3, 12))),
            &mut surface,
            &mut state,
        );

        let detail = surface.lines_of(LineKind::Info);
        assert_eq!(
            detail.last().copied(),
            Some("  invocation 3 of 12 this turn"),
            "the turn's count closes the block, and names the ceiling: {detail:?}"
        );
        // The ceiling is the daemon's, not this crate's: a different cap reads
        // back as a different line.
        let mut other = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(counted(1, 25))),
            &mut other,
            &mut state,
        );
        assert_eq!(
            other.lines_of(LineKind::Info).last().copied(),
            Some("  invocation 1 of 25 this turn"),
        );
    }

    /// **`None` is a fact, and it renders as nothing at all.** The per-turn cap
    /// bounds the *model's* calls inside one prompt turn; a human typing
    /// `/name` spends none of it, and the daemon publishes `None` there.
    ///
    /// **Mutation.** Render the count for a `None` — `unwrap_or_default()`, a
    /// `0 of 12`, or an em-dash placeholder — and this fails. Any of them would
    /// invent a budget the user is not drawing on, and "0 of 12" would read as
    /// a turn that has spent nothing rather than as a turn with no cap.
    #[test]
    fn a_typed_invocation_prints_no_turn_count_because_it_spends_none_of_the_cap() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(invoked(vec![ran("date", 8)]))),
            &mut surface,
            &mut state,
        );

        let lines = surface.lines_of(LineKind::Info);
        assert!(
            !lines.iter().any(|line| line.contains("this turn")
                || line.contains(" of ")
                || line.contains("invocation")),
            "a typed invocation was given a per-turn budget it does not draw \
             on: {lines:?}"
        );
        // Non-vacuity: the same session *does* print the rest of the block, so
        // this is the absence of one line rather than of the whole thing.
        assert!(
            lines.iter().any(|line| line.contains("SKILL.md")),
            "{lines:?}"
        );
    }

    /// **BR-9's second sentence: a refusal is its own line, and it is not the
    /// invocation line wearing a flag.**
    ///
    /// The fixture is the point. `refusal(…)` differs from a *successful*
    /// command-free invocation in exactly one field, which is the shape the
    /// wire has and the reason `refused` exists — so a renderer that ignored it
    /// would print "skill status (user, 5.3 KiB, 0 dynamic commands)" for a
    /// call that put nothing in the turn, and the surface would be reporting
    /// the opposite of what happened.
    ///
    /// Three claims: the line **opens with the verdict**, so a glance at the
    /// left edge tells the two apart; it carries **neither figure** that would
    /// imply an expansion; and it names the reason in **words**, never the id.
    ///
    /// **Mutation.** Drop the refusal branch from `skill_echo_line` and the
    /// first three assertions fail; render the refusal as the invocation line
    /// plus a suffix and the size and count assertions fail.
    #[test]
    fn a_refused_call_is_not_rendered_as_an_invocation() {
        let line = skill_echo_line(&refusal("over_budget", 4));

        assert_eq!(
            line,
            "refused: skill status (user) — the expansion did not fit this turn's context budget",
        );
        assert!(
            line.starts_with("refused:"),
            "the verdict has to be the first thing read, not a suffix: {line}"
        );
        // Neither figure: both are true of the file and false of this turn.
        assert!(
            !line.contains("KiB") && !line.contains("dynamic command"),
            "a refusal reported a body size or a command count, which would \
             describe an expansion that never happened: {line}"
        );
        // The id keys the record; the sentence is what a person reads.
        assert!(!line.contains("over_budget"), "{line}");

        // And the same record, but successful, is the line it has always been —
        // which is what makes the comparison above a claim about the field
        // rather than about two unrelated fixtures.
        let ran_instead = SkillInvoked {
            refused: None,
            ..refusal("over_budget", 4)
        };
        assert_eq!(
            skill_echo_line(&ran_instead),
            "skill status (user, 5.3 KiB, 0 dynamic commands) — invoked by the model",
        );

        // Through `render_event`, because "a refusal is never silent" is a claim
        // about what reaches the surface and not about what a formatter returns:
        // a renderer that swallowed the record would leave the pure function
        // above green and the session with nothing to show.
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        let outcome = render_event(
            &envelope(Event::SkillInvoked(refusal("over_budget", 4))),
            &mut surface,
            &mut state,
        );
        assert!(matches!(outcome, EventOutcome::Rendered));
        assert_eq!(surface.lines_of(LineKind::Notice), vec![line.as_str()]);
    }

    /// **The Stage B pair is two records on purpose, and the session prints two
    /// lines.**
    ///
    /// The tool publishes its invocation record at the end of the expansion —
    /// correct, because those dynamic commands really did run and `/verbose`
    /// renders their outcomes — and the loop then refuses to fold the result and
    /// publishes a second record carrying the reason. That is BR-9's two
    /// sentences about one call, not a duplicate.
    ///
    /// **Mutation.** Read the pair as two invocations (ignore `refused`) and the
    /// second line's assertion fails; dedupe them to one line — by name, or by
    /// remembering the previous event — and the count fails. The renderer is
    /// stateless per event precisely so that neither is expressible without
    /// adding state that the wire does not justify.
    #[test]
    fn the_stage_b_pair_prints_an_invocation_line_and_then_a_refusal_line() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();

        // What the tool published when the expansion came back: its commands ran.
        let expanded = SkillInvoked {
            turn_invocations: Some(events::TurnInvocations { count: 4, cap: 12 }),
            ..invoked_by(vec![ran("git status", 96)], events::InvokedBy::Model)
        };
        render_event(
            &envelope(Event::SkillInvoked(expanded)),
            &mut surface,
            &mut state,
        );
        // …and what the loop published when it declined to fold it.
        render_event(
            &envelope(Event::SkillInvoked(refusal("over_budget", 4))),
            &mut surface,
            &mut state,
        );

        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![
                "skill status (user, 5.3 KiB, 1 dynamic command) — invoked by the model",
                "refused: skill status (user) — the expansion did not fit this turn's context \
                 budget",
            ],
            "one call, two records, two lines — in the order the daemon published \
             them, and neither folded into the other"
        );
    }

    /// **A reason id this build has never heard of still reads.** Six of the
    /// daemon's ids are unpublished today and the set is open; a client that
    /// rendered a blank line, or dropped the event, would leave nothing on the
    /// surface saying the call happened at all — BUG-186's shape, and the
    /// failure `PermissionSubject::Unrecognized` exists to prevent for subjects.
    ///
    /// The id is quoted **inside a sentence**, which is the same answer
    /// [`refusal_line`] gives for a request whose subject it cannot name: the
    /// daemon's own word for what it did is the only information there is, and
    /// it is how a user finds the refusal in a log.
    #[test]
    fn an_unrecognized_refusal_id_still_reads_as_a_sentence() {
        let line = skill_echo_line(&refusal("some_reason_invented_later", 2));

        assert_eq!(
            line,
            "refused: skill status (user) — the daemon reported \
             `some_reason_invented_later`",
        );
        assert!(
            line.starts_with("refused:") && line.len() > "refused: skill status (user) — ".len(),
            "an unknown id must not render a blank tail: {line:?}"
        );
    }

    /// Every published id this build knows reads as a distinct sentence, and
    /// none of them is its own wire spelling.
    ///
    /// Listed exhaustively rather than sampled, on
    /// [`the_not_run_reasons_read_as_different_sentences`]'s rule: an id added
    /// to the daemon and forgotten here reaches a user wearing the fallback
    /// sentence, which is readable but says less than it could.
    #[test]
    fn the_refusal_reasons_read_as_different_sentences() {
        let ids = [
            "over_budget",
            "per_turn_cap",
            "repeated",
            "unknown_skill",
            "not_model_invocable",
            "reserved_name",
            "invalid_arguments",
            "project_not_acknowledged",
        ];
        let mut seen: Vec<String> = ids.iter().map(|id| refusal_reason_words(id)).collect();
        for (id, words) in ids.iter().zip(seen.iter()) {
            assert!(
                !words.contains(id),
                "`{id}` reached a user as its own wire spelling: {words}"
            );
            assert!(
                !words.contains("the daemon reported"),
                "`{id}` fell through to the unknown arm: {words}"
            );
        }
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), total, "two reasons share a sentence: {seen:?}");
    }

    /// **`/verbose` under a refusal: the turn's count, and none of the file
    /// detail.**
    ///
    /// The detail block reports what the invocation did — the file its body came
    /// from, what its frontmatter did on the way, what each command did — and a
    /// refused call did none of it. The count is the exception because it is
    /// about the *turn*, and on a `per_turn_cap` refusal it is the evidence for
    /// the refusal itself.
    #[test]
    fn verbose_under_a_refusal_adds_the_turn_count_and_no_file_detail() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(refusal("per_turn_cap", 12))),
            &mut surface,
            &mut state,
        );

        assert_eq!(
            surface.lines_of(LineKind::Info),
            vec!["  invocation 12 of 12 this turn"],
            "the count is the evidence for this refusal, and the file detail \
             describes an expansion that did not happen"
        );
        assert!(
            !surface.any_line_contains(LineKind::Info, "SKILL.md"),
            "a refusal claimed a body came from a file"
        );
        assert_eq!(
            surface.lines_of(LineKind::Notice),
            vec![
                "refused: skill status (user) — this turn has already made as many skill \
                  calls as it may"
            ],
        );
    }

    /// A skill with no dynamic context says so honestly, and singular reads as
    /// singular: "1 dynamic command", never "1 dynamic commands".
    #[test]
    fn the_echo_line_counts_zero_and_one_in_words_that_read() {
        let declined = |command: &str| DynamicOutcomeView {
            command: command.to_owned(),
            outcome: DynamicOutcome::NotRun {
                reason: NotRunReason::Declined,
            },
        };
        for (outcomes, expected) in [
            (Vec::new(), "0 dynamic commands"),
            (vec![ran("ls -1", 12)], "1 dynamic command"),
            (vec![ran("ls -1", 12), ran("date", 8)], "2 dynamic commands"),
            // The count alone cannot say a command never started, and after a
            // decline every one of them is a placeholder in the prompt rather
            // than output. The line the user *sees* has to agree with what the
            // model actually got.
            (vec![declined("ls -1")], "1 dynamic command, none run"),
            (
                vec![declined("ls -1"), declined("date")],
                "2 dynamic commands, none run",
            ),
            (
                vec![ran("ls -1", 12), declined("date"), declined("git status")],
                "3 dynamic commands, 1 run",
            ),
            // A command that started and failed still ran: the model has its
            // placeholder *and* the fact that it was attempted.
            (
                vec![
                    ran("ls -1", 12),
                    DynamicOutcomeView {
                        command: "false".to_owned(),
                        outcome: DynamicOutcome::Failed {
                            exit_status: Some(1),
                        },
                    },
                ],
                "2 dynamic commands",
            ),
        ] {
            let line = skill_echo_line(&invoked(outcomes));
            assert!(line.ends_with(&format!("{expected})")), "{line}");
        }
    }

    /// **BR-12's `/verbose` clause.** The path, the ignored frontmatter keys and
    /// one line per command's typed outcome — added under the echo line, and
    /// only under `/verbose`.
    ///
    /// `allowed-tools` and `model` are still inert under REQ-587: BR-3 shrank
    /// REQ-585 BR-5's list by exactly two keys, and neither of them is one of
    /// these.
    #[test]
    fn verbose_adds_the_path_the_ignored_keys_and_one_line_per_outcome() {
        let event = Event::SkillInvoked(invoked(vec![
            ran("ls -1 .adlc/specs", 2_048),
            DynamicOutcomeView {
                command: "git status --short".to_owned(),
                outcome: DynamicOutcome::NotRun {
                    reason: NotRunReason::NoTerminal,
                },
            },
            DynamicOutcomeView {
                command: "false".to_owned(),
                outcome: DynamicOutcome::Failed {
                    exit_status: Some(1),
                },
            },
            DynamicOutcomeView {
                command: "sleep 600".to_owned(),
                outcome: DynamicOutcome::TimedOut,
            },
        ]));

        let mut quiet = RecordingSurface::new();
        let mut state = SessionState::new();
        render_event(&envelope(event.clone()), &mut quiet, &mut state);
        assert_eq!(quiet.calls.len(), 1, "no detail without /verbose");

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(&envelope(event), &mut surface, &mut state);

        let detail = surface.lines_of(LineKind::Info);
        assert_eq!(
            surface.lines_of(LineKind::Notice).len(),
            1,
            "still exactly one echo line"
        );
        assert!(
            detail[0].contains("~/.claude/skills/status/SKILL.md"),
            "the path is home-relative, as the daemon spelled it: {detail:?}"
        );
        assert!(
            !detail[0].contains("/Users/"),
            "BR-1's entity table: never an absolute path: {detail:?}"
        );
        assert!(
            detail[1] == "  ignored frontmatter: allowed-tools, model",
            "BR-5's inert keys are named: {detail:?}"
        );
        assert_eq!(
            &detail[2..],
            [
                "  !`ls -1 .adlc/specs` — ran (2.0 KiB)",
                "  !`git status --short` — not run: no human could be asked",
                "  !`false` — failed (exit 1)",
                "  !`sleep 600` — timed out",
            ],
            "one line per command, in document order"
        );
    }

    /// A file that declared no inert keys gets no line about them: a header with
    /// nothing after it is a line about nothing.
    #[test]
    fn verbose_says_nothing_about_ignored_keys_when_there_were_none() {
        let mut event = invoked(vec![ran("date", 8)]);
        event.ignored_keys.clear();
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(event)),
            &mut surface,
            &mut state,
        );

        assert!(
            !surface.any_line_contains(LineKind::Info, "ignored frontmatter"),
            "{:?}",
            surface.lines_of(LineKind::Info)
        );
    }

    /// **Which keys a build honors is the daemon's answer, and this line is a
    /// renderer.**
    ///
    /// REQ-587 BR-3 takes `disable-model-invocation` and `user-invocable` out
    /// of the inert list — *when the daemon could read their values*. A value
    /// it could not read leaves the key named here, which is how a user who
    /// wrote `user-invocable: yes` learns the line did nothing; the same bytes
    /// arrive from a REQ-585-vintage daemon, for which they are simply true.
    ///
    /// So the client filters nothing. A filter here would be a second home for
    /// a rule the daemon owns, and the stale one against a daemon of any other
    /// vintage (LESSON-528) — it would swallow exactly the diagnostic BR-3
    /// promises instead of a silent ignore.
    #[test]
    fn verbose_names_the_keys_the_daemon_called_ignored_and_filters_none_of_them() {
        let mut event = invoked(vec![ran("date", 8)]);
        event.ignored_keys = vec![
            "user-invocable".to_owned(),
            "disable-model-invocation".to_owned(),
        ];
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.verbose = true;
        render_event(
            &envelope(Event::SkillInvoked(event)),
            &mut surface,
            &mut state,
        );

        assert!(
            surface.any_line_contains(
                LineKind::Info,
                "  ignored frontmatter: user-invocable, disable-model-invocation"
            ),
            "the daemon named two keys it did not honor and both must reach the \
             user, in the daemon's order: {:?}",
            surface.lines_of(LineKind::Info)
        );
    }

    /// **BR-6's four doors, four sentences.** "The user declined" and "no human
    /// could be asked" are different facts about the same missing output;
    /// collapsing any two of them would tell a user their answer decided
    /// something they were never asked.
    #[test]
    fn the_not_run_reasons_read_as_different_sentences() {
        // Every arm, listed exhaustively rather than sampled: a reason added
        // later that this crate forgets to word would otherwise reach a user
        // wearing another reason's sentence.
        let mut seen: Vec<&str> = [
            NotRunReason::Declined,
            NotRunReason::Level,
            NotRunReason::NoTerminal,
            NotRunReason::UnrecognizedSubject,
            NotRunReason::CouldNotStart,
        ]
        .into_iter()
        .map(not_run_words)
        .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "two reasons share a sentence: {seen:?}");
    }

    /// A truncated run says so: the model is reading a prefix, and the record
    /// the user reads says which.
    #[test]
    fn a_truncated_run_is_marked_as_one() {
        assert_eq!(
            dynamic_outcome_words(&DynamicOutcome::Ran {
                output_bytes: 4_096,
                truncated: true,
            }),
            "ran (4.0 KiB, truncated)"
        );
        assert_eq!(
            dynamic_outcome_words(&DynamicOutcome::Failed { exit_status: None }),
            "failed (killed by a signal)"
        );
    }

    // ------------------------------------------------- snapshot lifetime

    fn root_moved(session: &str) -> EventEnvelope {
        EventEnvelope::new(
            1,
            Some(SessionId::from(session)),
            Event::SessionRootChanged(SessionRootChanged {
                previous_display: "~/before".to_owned(),
                root: SessionRoot {
                    display: "~/Documents/GitHub/teton-code".to_owned(),
                    kind: RootKind::Project,
                    project_name: Some("teton-code".to_owned()),
                    vcs_branch: Some("main".to_owned()),
                },
            }),
        )
    }

    /// **ADR-2 / AC-14.** A `/cd` in *this* session marks the snapshot stale, so
    /// the entry loop re-fetches before it classifies the next line — and the
    /// flag clears as it is read, so one move costs one `skills/list` rather
    /// than one per typed line.
    #[test]
    fn a_root_move_in_this_session_marks_the_snapshot_stale_exactly_once() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));
        assert!(
            !state.take_skills_stale(),
            "a fresh session has already fetched"
        );

        render_event(&root_moved("s1"), &mut surface, &mut state);

        assert!(state.take_skills_stale(), "the move is news");
        assert!(
            !state.take_skills_stale(),
            "reading clears it: one move, one fetch"
        );
    }

    /// **ADR-6's client half.** A root move forgets the answers the user gave
    /// about *this* root's skills, and keeps the ones that are still about the
    /// same file.
    ///
    /// This store is consulted *before* any prompt is drawn, so a grant that
    /// outlived its root does not merely linger — it silently answers. The
    /// daemon drops its copy inside `set_session_cwd` and then re-asks; without
    /// this, the re-ask is auto-answered from a different repo's approval, one
    /// `auto-allow` line goes by, the commands are never shown, and the daemon
    /// re-remembers the grant under the new root. The daemon-side test cannot
    /// see any of that, because by then the request never reaches a human.
    #[test]
    fn a_root_move_forgets_the_project_skill_grants_and_keeps_the_others() {
        use teton_protocol::methods::{
            expires_on_session_root_change, project_skill_trust_key, skill_permission_key,
            SkillSource,
        };

        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));

        let project = skill_permission_key(SkillSource::Project, "deploy");
        let user = skill_permission_key(SkillSource::User, "status");
        // REQ-587 BR-4 / ASSUME-017: the acknowledgment is the second family a
        // root move invalidates, and the one whose survival costs most.
        let acknowledgment = project_skill_trust_key("~/dev/before");
        state.grants.allow_always(&project);
        state.grants.allow_always(&user);
        state.grants.allow_always("shell");
        state.grants.allow_always(&acknowledgment);
        state
            .grants
            .reject_always(&skill_permission_key(SkillSource::Project, "canary"));

        render_event(&root_moved("s1"), &mut surface, &mut state);

        // **The two stores expire the same keys at the same moment.** This
        // client's memo is consulted *before* any prompt is drawn, so a key it
        // keeps and the daemon drops is not a stale entry — it is an
        // `auto-allow` line answering the new root's question with the old
        // root's answer, with no human shown anything. Asserted against the
        // shared predicate rather than against a list of keys, so a family
        // added to one side cannot be forgotten on this one.
        for key in [&project, &acknowledgment] {
            assert!(
                expires_on_session_root_change(key),
                "`{key}` is one of the keys a `/cd` invalidates",
            );
            assert!(
                !state.grants.is_allow_always(key),
                "`{key}` outlived the root that gave it meaning, and this store \
                 answers before a prompt is drawn",
            );
        }
        assert!(
            !expires_on_session_root_change(&user) && !expires_on_session_root_change("shell"),
            "the kept half is kept because the rule says so, not by coincidence",
        );
        assert!(
            !state.grants.is_allow_always(&project),
            "a project skill's grant outlived the root that gave its name meaning",
        );
        assert!(
            !state
                .grants
                .is_reject_always(&skill_permission_key(SkillSource::Project, "canary")),
            "a project skill's refusal outlived it too — the same key names another file now",
        );
        // Kept: `~/.claude/skills/status` is the same file whatever the root
        // is, and `shell` is not root-scoped at all. Forgetting these would
        // re-ask questions whose answers are still true.
        assert!(
            state.grants.is_allow_always(&user),
            "a user skill's grant is still about its file"
        );
        assert!(
            state.grants.is_allow_always("shell"),
            "`shell` is not root-scoped"
        );
    }

    /// Another session's `/cd` re-derives nothing here — the bus is daemon-wide,
    /// and this client's registry is about this client's root. Under the same
    /// condition the root cache itself is written under, so the two cannot come
    /// to describe different sessions.
    #[test]
    fn another_sessions_root_move_leaves_this_snapshot_alone() {
        let mut surface = RecordingSurface::new();
        let mut state = SessionState::new();
        state.session_id = Some(SessionId::from("s1"));

        render_event(&root_moved("s2"), &mut surface, &mut state);

        assert!(!state.take_skills_stale());
        assert_eq!(state.root, None, "and the root cache is untouched too");
    }
}

// ---------------------------------------------------------------------------
// REQ-585 ADR-7: the client selects on the subject, never on the key string
// ---------------------------------------------------------------------------

/// A source-level scan of the whole `teton` crate, in the style of
/// `tetond/tests/boundary_coverage.rs`'s.
///
/// The claim ADR-7 makes is a **negative** one — neither shape `req.tool_name`
/// can take for this feature (`skill:<source>:<name>` and REQ-587's
/// `project_skill_trust:<root>`) is parsed or composed anywhere in this crate —
/// and a negative claim about code that does not exist cannot be asserted by
/// running anything. So it is asserted about the source itself, embedded with
/// `include_str!` at compile time rather than read from disk, which is BUG-159's
/// trap: a scan that opens files at runtime passes vacuously from a directory
/// that is not the crate.
///
/// The rule this guards is not "the key is unimportant" — each is a grant key,
/// and [`SessionGrants`] uses it as an **opaque** one, whole, hashed, never
/// split. What a client may not do is *select behaviour* by its shape: BR-11
/// says the key is an implementation detail, and a client sniffing an unstable
/// string mis-fires in the one direction that costs a swallowed stdin line.
/// `OPTION_ID_ENABLE_PERMANENT` is the shipped precedent for the single value a
/// client may match by string; everything else is matched by typed kind, and
/// [`PermissionSubject`] is that kind for the subject of a request.
#[cfg(test)]
mod key_scan {
    use std::collections::BTreeSet;
    use teton_protocol::methods::{
        skill_permission_key_prefix, SkillSource, PROJECT_SKILL_TRUST_KEY_PREFIX,
    };

    /// Every production source file of this crate. A fixed list, because
    /// `include_str!` takes a literal path — and
    /// [`every_module_of_this_crate_is_scanned`] fails the day one is added and
    /// not listed here, so the list cannot silently stop covering the crate.
    const CRATE_SOURCES: &[(&str, &str)] = &[
        ("main.rs", include_str!("main.rs")),
        ("banner.rs", include_str!("banner.rs")),
        ("cli_rows.rs", include_str!("cli_rows.rs")),
        ("client.rs", include_str!("client.rs")),
        ("cost_ui.rs", include_str!("cost_ui.rs")),
        ("effort_ui.rs", include_str!("effort_ui.rs")),
        ("firstrun.rs", include_str!("firstrun.rs")),
        ("keychain.rs", include_str!("keychain.rs")),
        ("loading.rs", include_str!("loading.rs")),
        ("model_ui.rs", include_str!("model_ui.rs")),
        ("prompt.rs", include_str!("prompt.rs")),
        ("provider_setup_ui.rs", include_str!("provider_setup_ui.rs")),
        ("provider_test_ui.rs", include_str!("provider_test_ui.rs")),
        ("render.rs", include_str!("render.rs")),
        ("service.rs", include_str!("service.rs")),
        ("session_ui.rs", include_str!("session_ui.rs")),
        ("slash.rs", include_str!("slash.rs")),
        ("status.rs", include_str!("status.rs")),
        ("uninstall.rs", include_str!("uninstall.rs")),
        ("web_setup_ui.rs", include_str!("web_setup_ui.rs")),
    ];

    /// The string literals a client would have to write in order to take one of
    /// this feature's grant keys apart **or to build one**: an opening quote
    /// followed by a key family's prefix, which is what `starts_with`,
    /// `strip_prefix`, `split` and `format!` all need.
    ///
    /// **Both families, and neither of them spelled here.** REQ-587 minted a
    /// second key beside REQ-585's `skill:<source>:<name>` — the project-skill
    /// acknowledgment's `project_skill_trust:<root>` (ADR-7) — and a scan that
    /// knew only the first would pass a client that matched the second by
    /// string. The [`DECOMPOSITIONS`] half catches a `starts_with` on either,
    /// but not a `format!` that *builds* one, so the literal needle is the only
    /// guard on that half of the mutation and it has to cover both keys.
    ///
    /// The prefixes are read off the protocol's own definitions rather than
    /// re-typed, so a rename of either key reaches this scan through the
    /// compiler instead of through somebody's grep (LESSON-546).
    fn key_literals() -> Vec<String> {
        // `skill:` — the root both source prefixes share, taken off one of them
        // rather than written out: a client matching the bare family root is
        // the mutation this catches, and a needle of `"skill:user:` would miss
        // it.
        let source_prefix = skill_permission_key_prefix(SkillSource::User);
        let family = source_prefix
            .split_once(':')
            .expect("the skill permission key is `skill:<source>:<name>`")
            .0;
        vec![
            format!("\"{family}:"),
            format!("\"{PROJECT_SKILL_TRUST_KEY_PREFIX}"),
        ]
    }

    /// Ways to decompose the key once it is in hand. `as_str`, `clone` and a
    /// bare `{}` are absent on purpose: passing the key along whole, hashing it
    /// as a grant, and *printing* it are all fine — printing what the daemon
    /// called a request is how a user finds it in a log.
    const DECOMPOSITIONS: &[&str] = &[
        "tool_name.split",
        "tool_name.splitn",
        "tool_name.rsplit",
        "tool_name.strip_prefix",
        "tool_name.strip_suffix",
        "tool_name.starts_with",
        "tool_name.ends_with",
        "tool_name.contains",
        "tool_name.find",
    ];

    /// Everything in `text` before its first `#[cfg(test)] mod …`.
    ///
    /// A test fixture may of course spell a key — this module's own does — and a
    /// scan that counted them would be asserting about itself. The anchor is the
    /// attribute *and* the `mod` line together; a file that loses the pair is
    /// scanned whole, which can only make the scan **stricter**, never blinder.
    fn production_half(text: &str) -> &str {
        match text.find("\n#[cfg(test)]\nmod ") {
            Some(at) => &text[..at],
            None => text,
        }
    }

    /// Source lines that are not comments — the scan is about code, and every
    /// ADR reference in this crate's prose spells the key on purpose.
    fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
        text.lines().enumerate().filter(|(_, line)| {
            let t = line.trim_start();
            !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
        })
    }

    /// **ADR-7 / BR-11.** No production line of this crate builds or takes apart
    /// either permission key this feature mints — `skill:<source>:<name>` or
    /// `project_skill_trust:<root>`. A client that sniffed one would mis-fire
    /// the one way that costs a stdin line, and the typed `PermissionSubject`
    /// exists precisely so that it never has to.
    #[test]
    fn no_production_source_parses_the_skill_permission_key() {
        let literals = key_literals();
        let mut offences: Vec<String> = Vec::new();
        for (file, text) in CRATE_SOURCES {
            for (index, line) in code_lines(production_half(text)) {
                if literals.iter().any(|needle| line.contains(needle)) {
                    offences.push(format!("{file}:{}: {}", index + 1, line.trim()));
                }
                for needle in DECOMPOSITIONS {
                    if line.contains(needle) {
                        offences.push(format!("{file}:{}: {}", index + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offences.is_empty(),
            "the client must select on `PermissionSubject`, never on the key string \
             (REQ-585 BR-11, ADR-7):\n{}",
            offences.join("\n")
        );
    }

    /// The scan covers the crate, not a list somebody forgot to grow.
    ///
    /// `include_str!` takes a literal path, so [`CRATE_SOURCES`] cannot be
    /// derived — but the module list in `main.rs` can be, and a module declared
    /// there and absent here fails this rather than silently going unscanned
    /// (LESSON-546: a one-home rule needs a test, not a grep).
    #[test]
    fn every_module_of_this_crate_is_scanned() {
        let main = CRATE_SOURCES
            .iter()
            .find(|(name, _)| *name == "main.rs")
            .expect("the crate root is scanned")
            .1;
        let declared: BTreeSet<String> = main
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter_map(|line| {
                let decl = line.strip_prefix("pub ").unwrap_or(line);
                let name = decl.strip_prefix("mod ")?.strip_suffix(';')?;
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    .then(|| format!("{name}.rs"))
            })
            .collect();
        let scanned: BTreeSet<String> = CRATE_SOURCES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();

        assert!(!declared.is_empty(), "main.rs declares modules");
        let missing: Vec<&String> = declared.difference(&scanned).collect();
        assert!(
            missing.is_empty(),
            "these modules are not scanned; add each `include_str!` to \
             `CRATE_SOURCES`: {missing:?}"
        );
    }

    /// The scan is not vacuous: it really would fire, **on both key families
    /// and on both shapes of the mutation**.
    ///
    /// Every needle is exercised against text shaped like the mutation it
    /// exists to catch — a client that recognized a consent by reading its key,
    /// and a client that composed one — so a later edit that broke
    /// `production_half`, `code_lines` or [`key_literals`] is a failure here
    /// rather than a green scan of nothing.
    ///
    /// The `format!` rows are the ones [`DECOMPOSITIONS`] cannot see: nothing is
    /// taken apart there, so the literal needle is the whole guard, and before
    /// REQ-587 extended it a client building a `project_skill_trust:` key passed
    /// this module untouched.
    #[test]
    fn the_scan_would_catch_a_client_that_sniffed_the_key() {
        let literals = key_literals();
        let offending_lines = |source: &str| -> Vec<String> {
            code_lines(production_half(source))
                .filter(|(_, line)| {
                    literals.iter().any(|needle| line.contains(needle))
                        || DECOMPOSITIONS.iter().any(|n| line.contains(n))
                })
                .map(|(_, line)| line.to_owned())
                .collect()
        };

        for mutation in [
            // Reading a skill key's shape …
            "    if req.tool_name.starts_with(\"skill:\") {\n        ask();\n    }\n",
            // … and the acknowledgment key's, which is the family REQ-587 added.
            "    if req.tool_name.starts_with(\"project_skill_trust:\") {\n        ask();\n    }\n",
            // Building either, which no decomposition needle can catch.
            "    let key = format!(\"skill:{source}:{name}\");\n",
            "    let key = format!(\"project_skill_trust:{root}\");\n",
        ] {
            let hits = offending_lines(mutation);
            assert_eq!(
                hits.len(),
                1,
                "the mutation is caught: {mutation:?} → {hits:?}"
            );
        }

        // And a comment saying the same words is not an offence.
        let prose = "    /// The key's `skill:<source>:<name>` shape (`\"skill:`) is not parsed.\n";
        assert_eq!(code_lines(production_half(prose)).count(), 0);
    }
}
