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

use teton_protocol::events::{
    AttachConsentRequested, BlockCause, CapabilityDeadEnd, ConsentScope, DaemonClientAttach,
    DaemonLifetimeStage, Event, EventEnvelope, EvictionReason, FailureClass, ModelLifecycle,
    ModelSelectionProposed, PermissionOption, PermissionOptionKind, PermissionRequest,
    PhaseTransition, PrefixCache, PrefixCacheMiss, PrefixCacheOutcome, PrivacyAction, PrivacyBlock,
    ProvenanceRejected, ProvenanceRejection, ProviderDegraded, RouteDecided, SessionGrantMinted,
    SessionUpdatePayload, ToolCallStatus, WebCapabilityState, WebConsentDecided, WebConsentScope,
    WebLookup, WebLookupKind, WebLookupOutcome, WebSetupCompleted, WebSetupRejected, WebTier,
    OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{
    AttachConsentOutcome, AttachConsentParams, PermissionOutcome, PermissionRespondParams,
};
use teton_protocol::{Phase, RequestId, SessionId};

use crate::cost_ui::CostMeter;
use crate::firstrun;
use crate::prompt::Prompter;
use crate::render::{LineKind, Surface};

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
            render_session_update(&su.update, surface, state);
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
    }
}

/// The line a completed `/web setup` renders (REQ-572 BR-14, OQ-2).
///
/// It says three things, and the third is the settled answer to OQ-2: the
/// capability is on, the file it was written to, and that **nothing has been
/// looked up**. No lookup is auto-offered — the flow performs no egress (BR-13),
/// and the next question that needs the web raises the ordinary per-lookup
/// consent. A notice that stopped after "enabled" would leave a user expecting
/// their last question to be answered now, which it will not be.
fn format_web_setup_completed(completed: &WebSetupCompleted) -> String {
    format!(
        "web lookup enabled (`{}`) — written to {}. Nothing has been looked up yet: the next \
         web-needing question will ask before anything leaves the machine.",
        web_tier_name(completed.tier),
        completed.config_path
    )
}

/// The line a refused setup call renders (REQ-572 BR-4 / AC-4).
///
/// The daemon's `origin` names a *kind* of caller and never an identity, and it
/// is rendered rather than branched on — the client's only job with it is to
/// show it. What this adds is the part the user cares about: nothing happened.
fn format_web_setup_rejected(rejected: &WebSetupRejected) -> String {
    format!(
        "web setup refused: the request came from {} rather than from this session's user, so \
         nothing was previewed and nothing was written.",
        rejected.origin
    )
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
fn render_session_update(
    update: &SessionUpdatePayload,
    surface: &mut dyn Surface,
    state: &mut SessionState,
) {
    match update {
        SessionUpdatePayload::AgentMessageChunk { text } => surface.fragment(text),
        SessionUpdatePayload::ToolCall {
            tool_call_id,
            title,
            status,
        } => {
            state
                .tool_titles
                .insert(tool_call_id.clone(), title.clone());
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

/// Resolve a permission request: apply any session grant, else prompt.
///
/// Returns the [`PermissionRespondParams`] to send back to the daemon and, as a
/// side effect, records "always" decisions in `grants` so a later request for the
/// same tool needs no prompt.
pub fn resolve_permission(
    req: &PermissionRequest,
    surface: &mut dyn Surface,
    prompter: &mut dyn Prompter,
    grants: &mut SessionGrants,
) -> PermissionRespondParams {
    let tool = req.tool_name.as_str();

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
    format!("route [{key}] → {} {model} — {}", rd.provider_id, rd.reason)
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
        PlanEntry, PlanEntryStatus, SelectionSource, SessionUpdate, WebTaintOverridden,
    };
    use teton_protocol::{ProviderId, RequestId, SessionId};

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
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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

        let first = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
        assert_eq!(
            first.outcome,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned()
            }
        );
        assert!(grants.is_allow_always("shell"));

        let second = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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

        let first = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
        assert_eq!(
            first.outcome,
            PermissionOutcome::Selected {
                option_id: "reject_once".to_owned() // no reject_always offered → falls back
            }
        );
        assert!(grants.is_reject_always("shell"));

        let second = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
        assert!(matches!(second.outcome, PermissionOutcome::Selected { .. }));
        assert_eq!(prompter.asked, 1);
    }

    #[test]
    fn invalid_answer_reprompts_then_accepts() {
        let req = permission_request("shell");
        let mut surface = RecordingSurface::new();
        let mut prompter = ScriptedPrompter::new(&["huh?", "y"]);
        let mut grants = SessionGrants::default();
        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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

        let resp = resolve_permission(&req, &mut surface, &mut prompter, &mut grants);
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
            notice.contains("/Users/x/.config/teton/config.toml"),
            "the user must be able to go read what they agreed to: {notice}"
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
        assert!(notice.contains("nothing was written"), "{notice}");
        assert!(
            state.web.capability.is_none(),
            "a refusal changes no capability state"
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
}
