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
    BlockCause, DaemonClientAttach, Event, EventEnvelope, EvictionReason, FailureClass,
    ModelLifecycle, ModelSelectionProposed, PermissionOption, PermissionOptionKind,
    PermissionRequest, PhaseTransition, PrefixCache, PrefixCacheMiss, PrefixCacheOutcome,
    PrivacyAction, PrivacyBlock, ProviderDegraded, RouteDecided, SessionUpdatePayload,
    ToolCallStatus, WebConsentDecided, WebConsentScope, WebLookup, WebLookupKind, WebLookupOutcome,
    WebTier, OPTION_ID_ENABLE_PERMANENT,
};
use teton_protocol::methods::{PermissionOutcome, PermissionRespondParams};
use teton_protocol::{Phase, RequestId};

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
    /// This session's web-lookup capability, as the event stream reports it
    /// (REQ-563 BR-7/BR-13).
    ///
    /// Folded here for the reason `loading` is: [`render_event`] is the one place
    /// every web event passes through, so the status field and the notice lines
    /// are two readings of one fold and cannot disagree about whether the session
    /// is restricted.
    pub web: WebState,
}

/// What the session's web capability currently is, for the status row.
///
/// Derived entirely from the event stream rather than from config, and that is
/// the point: the status row's job is to say what *this session* can do now, and
/// a config read at startup would keep saying `fetch` through a taint trip that
/// disabled it. Every field here is written by exactly one event kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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
}

impl WebState {
    /// The status-row field: `web: …`.
    ///
    /// A pure function of the three fields, so it is testable with no terminal —
    /// which matters because the row it belongs to is drawn only at a TTY.
    ///
    /// Order is precedence, not preference. The restricted and overridden states
    /// are *about* the tiers rather than alternatives to them, and a row can show
    /// one field: a session that is restricted has had a capability taken away,
    /// and saying `web: search` while search is refused would be the status row
    /// contradicting the notice that preceded it.
    #[must_use]
    pub fn status_field(self) -> &'static str {
        if self.overridden {
            return "web: overridden";
        }
        if self.restricted {
            return "web: restricted (taint)";
        }
        match self.granted {
            None | Some(WebTier::Off) => "web: off",
            Some(WebTier::FetchUserUrl | WebTier::FetchAnyUrl) => "web: fetch",
            Some(WebTier::Search) => "web: search",
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
    #[must_use]
    pub fn is_engaged(self) -> bool {
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
    }
}

/// The one-line verbose notice a `prefix_cache` event draws.
fn format_prefix_cache(cache: &PrefixCache) -> String {
    match &cache.outcome {
        PrefixCacheOutcome::Hit {
            cached_tokens,
            new_tokens,
        } => format!(
            "context: reused {cached_tokens} tokens, prefilled {new_tokens} ({})",
            cache.model
        ),
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
        ByteSpan, CostRecord, CostRecorded, FindingKind, ModelSelectionDecided, PlanEntry,
        PlanEntryStatus, SelectionSource, SessionUpdate, WebTaintOverridden,
    };
    use teton_protocol::{ProviderId, RequestId, SessionId};

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
}
