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
    DaemonClientAttach, Event, EventEnvelope, FailureClass, ModelLifecycle, ModelSelectionProposed,
    PermissionOption, PermissionOptionKind, PermissionRequest, PhaseTransition, PrivacyAction,
    PrivacyBlock, ProviderDegraded, RouteDecided, SessionUpdatePayload, ToolCallStatus,
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

    loop {
        let answer = prompter.ask(&format!(
            "  allow {tool}? [y]es / [n]o / [a]llow-always / [d]eny-always: "
        ));
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
            "d" | "deny" => {
                grants.reject_always(tool);
                return respond(req, deny_outcome(&req.options));
            }
            "" => return respond(req, PermissionOutcome::Cancelled),
            _ => surface.line(
                LineKind::Prompt,
                "  please answer y, n, a (allow-always), or d (deny-always)",
            ),
        }
    }
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

fn format_privacy(pb: &PrivacyBlock) -> String {
    let action = match pb.action {
        PrivacyAction::Stripped => "stripped from the outbound payload",
        PrivacyAction::ReroutedToLocal => "call re-routed to the local tier",
    };
    format!(
        "privacy: {} would have reached {} — {action}",
        pb.path, pb.provider_id
    )
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
        CostRecord, CostRecorded, ModelSelectionDecided, PlanEntry, PlanEntryStatus,
        SelectionSource, SessionUpdate,
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
                    reason: "session previously touched local-only content; pinned to the \
                             local tier (BR-1 backstop)"
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
}
