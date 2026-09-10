//! The turn's activity, as a pure projection: what it is doing, for how long,
//! and what it has cost so far (REQ-621 ADR-621-2).
//!
//! A turn is silent for most of its life. Reply text streams as it arrives and a
//! tool call prints its own two lines, but the stretch before the first byte
//! (routing, context assembly, a provider's queue, a local prefill, an adaptive
//! think), the stretch while a tool runs, and the stretch after a tool result
//! while the model composes its next step all render nothing at all. A user
//! watching that cursor cannot tell **working** from **hung**, and the
//! expensive resolution of that ambiguity is killing a turn the frontier call
//! has already been paid for. This module is the state that answers it: one
//! phase folded from events the daemon already publishes, plus the caller's
//! clock.
//!
//! Two properties are load-bearing and are why this is a module rather than a
//! few lines in the pump:
//!
//! 1. **No I/O, no terminal, and no clock of its own.** [`TurnActivity::frame`]
//!    is a pure function of the folded state, a caller-supplied `now`, a tick
//!    and a width; [`TurnActivity::observe`] takes `now` for the same reason.
//!    BR-6 makes the row emit nothing when stdout is not a terminal, so every
//!    piped test is structurally blind to it — computing the frame inside the
//!    render path would leave BR-1's core behaviour with **no verification
//!    route at all**, the TTY gate doubling as a test blindfold (BR-7,
//!    LESSON-481). Elapsed time is still *measured* rather than guessed: the
//!    measurement happens where the clock is, in the pump, and this module only
//!    formats a duration it was handed.
//! 2. **It cannot invent a phase, a figure, or a name.** Every sentence comes
//!    from an event that arrived: before `route_decided` the row says it is
//!    preparing rather than naming a model nobody has chosen yet (BR-2), the
//!    tool sentence is the title the daemon composed rather than a second
//!    reading of the tool's arguments (LESSON-456), and the only figure beyond
//!    time is the exact sum of this turn's `cost_recorded` rows, printed by
//!    [`crate::cost_ui::format_usd`] so the row and the cost meter cannot
//!    disagree. There is nothing here to derive an ETA *from* (BR-3).
//!
//! The row is **drawn** by exactly one owner — the pump that holds the clock and
//! can wake (ADR-621-3) — and this module never touches a [`crate::render::Surface`].
//! Phases that are their own liveness signal draw nothing: `streaming` (the
//! arriving text says it), `awaiting_permission` (the question owns the
//! terminal), and `idle` (there is no turn). The phase is still tracked through
//! all three, because BR-16's end-of-turn summary reads this same accumulator.
//!
//! # What breaks which test
//!
//! The mutation below was **applied and observed failing** (AC-4), not reasoned
//! about — a suite that stays green with the animation disabled has not tested
//! the animation (LESSON-441, LESSON-464):
//!
//! | Mutation | Fails |
//! |---|---|
//! | `frame` ignores its `tick` (`SPINNER[0]` for every frame) | `the_frame_advances_with_the_tick`, and with it `the_frame_table` and `a_stall_annotates_the_last_phase_and_a_running_tool_is_exempt` — 3 red of the 10 here, and **0 of `pty_e2e`'s 28** (re-run 2026-09-10 against the widened suite) |
//!
//! Two of those three are collateral and the third is the point: the table and
//! the stall test carry literal spinner glyphs, so they fail on a frozen
//! animation too, but `the_frame_advances_with_the_tick` is the one that fails
//! on the *property* — a cycle whose frames are all distinct — rather than on a
//! glyph that happened to be written down.
//!
//! The pty legs staying green is the finding, and it is **why this module's
//! unit tests exist**. A leg at a real terminal checks that the row's first
//! character is one of the spinner's glyphs and that its *clock clause*
//! changed — both of which a frozen spinner satisfies — so the animation
//! itself has no expression at that altitude. Nothing above this module can
//! tell a moving row from a still one, which is the shape LESSON-481 names: the
//! property that is invisible to the gated path is the one the pure function
//! owes a test. The count is rewritten here rather than appended to, by
//! REQ-621's last test-bearing task, so that one number speaks for the whole
//! suite (LESSON-652).

use std::time::{Duration, Instant};

use teton_protocol::events::{
    thousands, Event, EventEnvelope, PrefillProgress, RouteDecided, SessionUpdatePayload,
    TierWarming, ToolCallStatus, TurnQueued,
};
use teton_protocol::SessionId;

use crate::cost_ui::format_usd;
use crate::markdown::{char_display_width, display_width};
use crate::session_ui::other_session;

/// How long this turn may go without a daemon event before the row says so
/// (BR-11: the quiet bound is 15 seconds).
///
/// The bound exists because "wait for the next event" can wait forever
/// (LESSON-450), and because a wedged daemon must look different from a slow
/// model. Past it the row stops its spinner and states the silence — it does
/// **not** relabel the phase, which is ADR-621-5's correction to the first
/// draft: replacing the phase with `stalled` would have reported every
/// forty-second test suite as a stall at fifteen, which is the noise that makes
/// a real one easy to miss (LESSON-628).
pub const STALL_AFTER: Duration = Duration::from_secs(15);

/// The frames the spinner cycles through.
///
/// A spinner rather than [`crate::loading`]'s growing dots: that indicator
/// animates a forty-second model load in a row the user is typing over, where
/// jitter is a cost; this one has to say "alive" inside a phase that changes
/// every few seconds, and it is redrawn in a row of its own.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The glyph a stalled row shows in place of a spinner frame.
///
/// A full cell, so a stopped row reads as stopped rather than as a spinner that
/// happens to be between frames (ADR-621-5).
const STALLED_GLYPH: &str = "⠿";

/// What the daemon has last reported this turn to be doing.
///
/// The vocabulary is fixed and every value is *reported*: no phase is entered
/// by elapsed time alone, and only the client's own prompt opens a turn
/// ([`TurnActivity::begin`]) — the spec's Events table has that first row as
/// client-local because it is the one transition the daemon does not announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// No turn in flight. Draws nothing, folds nothing.
    #[default]
    Idle,
    /// The prompt is on the wire and the daemon has not yet said what it chose.
    Preparing,
    /// A model call is out: routing is decided, no reply byte has arrived, or a
    /// tool result is being composed into the next step.
    AwaitingModel,
    /// Reply text is arriving. The text is the liveness signal, so the row is
    /// withdrawn — until the bytes stop for longer than [`STALL_AFTER`] (BR-11,
    /// OQ-2).
    Streaming,
    /// A tool is running. Exempt from the stall annotation: the daemon
    /// publishes nothing while a tool runs, so silence here is the expected
    /// state and the tool's own elapsed counter is the honest signal.
    ToolRunning,
    /// The daemon is waiting on the user. The permission prompt owns the
    /// terminal and its own question is the indication (BR-1), so the row is
    /// withdrawn; the phase is still tracked, and
    /// [`TurnActivity::permission_answered`] restores what it interrupted.
    AwaitingPermission,
    /// The turn is held for a warming local tier (REQ-580's `turn_queued`).
    Held,
    /// The daemon compacted context mid-turn and has said nothing since.
    Compacting,
}

/// What a finished turn spent, for BR-16's one durable line.
///
/// The figures are the accumulator's own — the same one the frames read — so a
/// piped verbose session recovers exactly what the row would have shown rather
/// than a second tally taken somewhere else (AC-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnSummary {
    /// Wall time from the prompt going on the wire to the turn's end.
    pub total: Duration,
    /// Of that, time spent waiting on or receiving from a model.
    pub model: Duration,
    /// Of that, time spent with a tool running.
    pub tools: Duration,
    /// The exact sum of this turn's `cost_recorded` rows, in micro-USD.
    pub cost_micros: i64,
}

/// The turn's activity. Cheap to clone, holds no handles, reads no clock.
#[derive(Debug, Clone, Default)]
pub struct TurnActivity {
    /// What the daemon last reported. [`Phase::Idle`] ⇔ no turn in flight.
    phase: Phase,
    /// The route clause for this turn — `anthropic claude-opus-5 (think)` — as
    /// `route_decided` reported it.
    ///
    /// Held apart from [`Self::detail`] because it is a fact about the **turn**
    /// rather than about the current phase: a tool call in the middle of a turn
    /// must not erase the model the turn is waiting on, since the phase after
    /// the tool result is `awaiting_model` again and would otherwise have lost
    /// the only name it is allowed to print.
    route: Option<String>,
    /// The current phase's own detail: the title the daemon composed for a
    /// running tool, or the held-turn clause.
    ///
    /// Deliberately **not** cleared on a phase change. Only the phase that owns
    /// a detail reads it, and keeping it is what lets a permission round-trip in
    /// the middle of a tool call come back to the sentence it interrupted.
    detail: Option<String>,
    /// The daemon's prefill fraction, while one is in flight (BR-3: a fraction
    /// appears only where the daemon supplied one).
    prefill: Option<String>,
    /// When the prompt went on the wire. `None` ⇔ idle.
    turn_started: Option<Instant>,
    /// When the current phase began.
    phase_since: Option<Instant>,
    /// When this turn last heard from the daemon. Drives the stall annotation.
    last_event: Option<Instant>,
    /// The exact sum of this turn's `cost_recorded` rows (OQ-1). Never an
    /// estimate, and never a token count.
    cost_micros: i64,
    /// Accrued time in the model phases, charged on each exit from them.
    model_time: Duration,
    /// Accrued time with a tool running, charged on exit from `tool_running`.
    tool_time: Duration,
    /// The phase a permission request interrupted, restored when it is answered.
    resume_after_permission: Option<Phase>,
}

impl TurnActivity {
    /// Open a turn: the prompt is going on the wire *now*.
    ///
    /// The one transition the daemon does not announce, and the only thing that
    /// starts a turn — an event can move a phase but never open one, so a
    /// sibling client prompting in the same session cannot raise a row over our
    /// idle prompt.
    ///
    /// Resets every field rather than clearing selectively, for the reason
    /// [`crate::session_ui::SessionState::begin_turn`] clears the turn record
    /// here rather than at turn end: a turn that ended by a path nobody
    /// anticipated must not lend its cost, its phase, or its clock to the next
    /// one.
    pub fn begin(&mut self, now: Instant) {
        *self = Self {
            phase: Phase::Preparing,
            turn_started: Some(now),
            phase_since: Some(now),
            last_event: Some(now),
            ..Self::default()
        };
    }

    /// Fold one event into the turn's activity.
    ///
    /// Takes the whole envelope so that `session_id` and `event` are read
    /// together, and asks [`other_session`] rather than taking a second reading
    /// of whose event this is: another session's event changes nothing (BR-15),
    /// and an event naming no session counts as ours — unknown is no evidence
    /// of elsewhere, which is the reading the reply accumulator already takes.
    ///
    /// Every arm is one of the spec's consumed events. Everything else — the
    /// notices, the lifecycle stages, another client attaching — leaves the
    /// phase **and the stall clock** alone: no phase is invented (AC-8), and
    /// "no word from the daemon" is a claim about this turn, which a
    /// daemon-scoped heartbeat published while the turn is wedged does not
    /// refute.
    pub fn observe(&mut self, env: &EventEnvelope, own_session: Option<&SessionId>, now: Instant) {
        if other_session(own_session, env.session_id.as_ref()).is_some() {
            return;
        }
        if self.phase == Phase::Idle {
            return;
        }
        match &env.event {
            Event::RouteDecided(route) => {
                self.route = Some(route_clause(route));
                self.enter(Phase::AwaitingModel, now);
            }
            Event::SessionUpdate(update) => match &update.update {
                // `enter` on every chunk, not only the first: the phase clock is
                // refreshed by each one, so a mid-stream stall measures from the
                // last byte the user actually saw (BR-11's streaming clause).
                SessionUpdatePayload::AgentMessageChunk { .. } => {
                    self.enter(Phase::Streaming, now);
                }
                SessionUpdatePayload::ToolCall { title, .. } => {
                    self.enter(Phase::ToolRunning, now);
                    self.detail = Some(title.clone());
                }
                // A finished tool means the model is composing the next step —
                // there is no event for "model request issued", and by
                // construction the next thing the daemon can send is a chunk, a
                // tool call, or the result (BR-14 needed no new event).
                SessionUpdatePayload::ToolCallUpdate { status, .. } => match status {
                    ToolCallStatus::Completed | ToolCallStatus::Failed => {
                        self.enter(Phase::AwaitingModel, now);
                    }
                    ToolCallStatus::Pending | ToolCallStatus::InProgress => self.touch(now),
                },
                SessionUpdatePayload::Diff { .. } | SessionUpdatePayload::Plan { .. } => {
                    self.touch(now);
                }
            },
            Event::PermissionRequest(_) => {
                let interrupted = self.phase;
                self.enter(Phase::AwaitingPermission, now);
                self.resume_after_permission = Some(interrupted);
            }
            Event::TurnQueued(queued) => {
                self.enter(Phase::Held, now);
                self.detail = Some(held_clause(queued));
            }
            // Stays `awaiting_model`: a prefill is that phase making progress,
            // and the fraction is the daemon's own figure (BR-3).
            Event::PrefillProgress(progress) => {
                self.prefill = Some(prefill_clause(progress));
                self.touch(now);
            }
            // Exact, and this turn's only (OQ-1). The phase is unchanged: being
            // billed for a call is not a thing the turn is *doing*.
            Event::CostRecorded(recorded) => {
                self.cost_micros = self.cost_micros.saturating_add(recorded.record.usd_micros);
                self.touch(now);
            }
            Event::ContextCompacted(_) => self.enter(Phase::Compacting, now),
            _ => {}
        }
    }

    /// The permission the row stepped aside for has been answered.
    ///
    /// Restores the phase the request interrupted, so the row comes back to the
    /// sentence it left rather than to a phase re-derived from whatever arrives
    /// next. Idempotent by [`Option::take`]: the restore is the pump's to make,
    /// and a second caller finds nothing to restore rather than resetting a
    /// phase clock that has since moved on.
    pub fn permission_answered(&mut self, now: Instant) {
        if let Some(interrupted) = self.resume_after_permission.take() {
            self.enter(interrupted, now);
        }
    }

    /// What the daemon last reported this turn to be doing.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The row to draw at `tick`, or `None` when nothing should be drawn.
    ///
    /// Pure: the same state, `now`, `tick` and `width` always yield the same
    /// string, and there is no clock, no filesystem and no terminal anywhere in
    /// it (BR-7).
    ///
    /// `tick` may only select an animation frame; it is deliberately not
    /// convertible to elapsed time here, which is `now`'s job. `width` is the
    /// surface's, and a row wider than the terminal is truncated on a character
    /// boundary by the display-width measurement `markdown.rs` owns — a row
    /// measured in bytes or `char`s would exceed the width for CJK content and
    /// be hard-wrapped by the terminal into two rows, which is residue the
    /// withdraw cannot clear.
    ///
    /// No ellipsis is appended to a truncated row: the marker would cost a
    /// column of the sentence to state something the missing text already says.
    #[must_use]
    pub fn frame(&self, now: Instant, tick: u64, width: usize) -> Option<String> {
        let (Some(turn_started), Some(phase_since), Some(last_event)) =
            (self.turn_started, self.phase_since, self.last_event)
        else {
            return None;
        };
        let quiet = now.saturating_duration_since(last_event);
        let stalled = quiet >= STALL_AFTER && self.phase != Phase::ToolRunning;
        let sentence = match self.phase {
            // Nothing to draw, at any tick: there is no turn, or the phase is
            // its own liveness signal (BR-1).
            Phase::Idle | Phase::AwaitingPermission => return None,
            Phase::Streaming if !stalled => return None,
            Phase::Preparing => "preparing turn".to_owned(),
            Phase::AwaitingModel => self.waiting_sentence(),
            Phase::Streaming => "receiving the reply".to_owned(),
            Phase::ToolRunning => format!(
                "running {}",
                self.detail
                    .as_deref()
                    .filter(|title| !title.is_empty())
                    .unwrap_or("a tool")
            ),
            Phase::Held => self
                .detail
                .clone()
                .unwrap_or_else(|| "holding the turn".to_owned()),
            Phase::Compacting => "compacting context".to_owned(),
        };
        let spinner = if stalled {
            STALLED_GLYPH
        } else {
            SPINNER[(tick % SPINNER.len() as u64) as usize]
        };
        let mut row = format!(
            "{spinner} {sentence} · {}s · turn {}s",
            now.saturating_duration_since(phase_since).as_secs(),
            now.saturating_duration_since(turn_started).as_secs(),
        );
        // BR-3: shown once it is non-zero, in the cost meter's own formatting. A
        // single-call turn never shows it, because its only cost row arrives as
        // the turn ends.
        if self.cost_micros != 0 {
            row.push_str(&format!(" · {}", format_usd(self.cost_micros)));
        }
        // Last, so the phase, its clock and the turn's clock read in the same
        // places they do on a healthy row — a reader comparing two frames is
        // looking for what changed, and moving the counters would hide it.
        if stalled {
            row.push_str(&format!(
                " · no word from the daemon for {}s",
                quiet.as_secs()
            ));
        }
        let fitted = fit(&row, width);
        // A row with no columns is not a row: painting an empty line would leave
        // exactly the blank residue BR-5 forbids.
        (!fitted.is_empty()).then_some(fitted)
    }

    /// Close the turn and report what it spent (BR-16).
    ///
    /// Charges the phase in flight before reading the totals, so a turn that
    /// ended while a tool was still running still reports that tool's time —
    /// the accrual happens on phase *exit*, and this is the last exit.
    ///
    /// Leaves the activity idle, which is what makes every exit path safe to
    /// call it on (BR-12): a second call reports a spent turn's zeroes rather
    /// than the previous turn's figures a second time.
    pub fn finish(&mut self, now: Instant) -> TurnSummary {
        self.accrue(now);
        let summary = TurnSummary {
            total: self.turn_started.map_or(Duration::ZERO, |started| {
                now.saturating_duration_since(started)
            }),
            model: self.model_time,
            tools: self.tool_time,
            cost_micros: self.cost_micros,
        };
        *self = Self::default();
        summary
    }

    /// The `awaiting_model` sentence.
    ///
    /// With no `route_decided` yet the row says it is preparing — BR-2's rule
    /// that the client never names a model the daemon has not chosen, which is
    /// also why this reads [`Self::route`] rather than composing a name from
    /// anything else it holds.
    fn waiting_sentence(&self) -> String {
        let mut sentence = match &self.route {
            Some(route) => format!("waiting on {route}"),
            None => "preparing turn".to_owned(),
        };
        if let Some(prefill) = &self.prefill {
            sentence.push_str(" · ");
            sentence.push_str(prefill);
        }
        sentence
    }

    /// Enter `next` at `now`, charging the phase being left.
    fn enter(&mut self, next: Phase, now: Instant) {
        self.accrue(now);
        self.phase = next;
        self.phase_since = Some(now);
        self.last_event = Some(now);
        // The fraction belongs to the prefill that was in flight, and any phase
        // change means it is over.
        self.prefill = None;
    }

    /// The daemon spoke about this turn without changing what it is doing.
    ///
    /// Refreshes the stall clock only. The phase clock is untouched on purpose:
    /// a cost row arriving mid-tool must not reset the tool's elapsed counter.
    fn touch(&mut self, now: Instant) {
        self.last_event = Some(now);
    }

    /// Charge the time spent in the current phase to the right accumulator.
    ///
    /// Split model from tools because that is the split BR-16's line reports and
    /// the split a user can act on: a turn that spent four minutes in a test
    /// suite and eight seconds in a model is a different turn from the reverse,
    /// and the row itself only ever showed one phase at a time.
    fn accrue(&mut self, now: Instant) {
        let Some(since) = self.phase_since else {
            return;
        };
        let spent = now.saturating_duration_since(since);
        match self.phase {
            Phase::AwaitingModel | Phase::Streaming => {
                self.model_time = self.model_time.saturating_add(spent);
            }
            Phase::ToolRunning => self.tool_time = self.tool_time.saturating_add(spent),
            Phase::Idle
            | Phase::Preparing
            | Phase::AwaitingPermission
            | Phase::Held
            | Phase::Compacting => {}
        }
    }
}

/// The route clause a `route_decided` contributes: `anthropic claude-opus-5 (think)`.
///
/// The model and the tier are each printed only when the event carries one
/// (BR-2). A daemon that reached a decision by the pre-category path names no
/// tier, and a provider that has not resolved a concrete model names no model;
/// in both cases the provider alone is what was actually reported, and inventing
/// the rest would be the row claiming to know which model a turn is waiting on
/// (REQ-580 BR-7, LESSON-456).
fn route_clause(route: &RouteDecided) -> String {
    let mut clause = route.provider_id.to_string();
    if let Some(model) = &route.model {
        clause.push(' ');
        clause.push_str(model);
    }
    if let Some(tier) = route.tier {
        clause.push_str(&format!(" ({tier})"));
    }
    clause
}

/// The held-turn clause a `turn_queued` contributes.
///
/// Branched on the event's typed `waiting_on` rather than on prose, so this and
/// [`crate::session_ui`]'s durable notice are two presentations of **one**
/// classification and cannot come to disagree about which transient state the
/// tier is in (LESSON-456). No countdown: the load window publishes nothing to
/// derive one from (BR-3).
fn held_clause(queued: &TurnQueued) -> String {
    let doing = match queued.waiting_on {
        TierWarming::Installing => "installing",
        TierWarming::Loading => "loading",
    };
    format!("held until {} finishes {doing}", queued.model_id)
}

/// The prefill clause a `prefill_progress` contributes.
///
/// The daemon's two figures, in the words its own durable line uses ("reading
/// context") so the row is not a second vocabulary for the same news (BR-10).
/// It is a fraction the client did not compute, which is the only kind BR-3
/// permits.
fn prefill_clause(progress: &PrefillProgress) -> String {
    format!(
        "reading context {}/{}",
        thousands(u64::from(progress.tokens_done)),
        thousands(u64::from(progress.tokens_total)),
    )
}

/// `row` truncated to `width` display columns on a character boundary.
///
/// Measured with the display width `markdown.rs` owns (ASSUME-023): a CJK
/// character counted as one column but drawn as two makes the row exceed the
/// terminal, and the terminal then hard-wraps it into a second row the withdraw
/// does not clear.
fn fit(row: &str, width: usize) -> String {
    if display_width(row) <= width {
        return row.to_owned();
    }
    let mut fitted = String::new();
    let mut used = 0;
    for c in row.chars() {
        let cost = char_display_width(c);
        if used + cost > width {
            break;
        }
        used += cost;
        fitted.push(c);
    }
    fitted
}

#[cfg(test)]
mod tests {
    use super::*;

    use teton_protocol::events::{
        ContextCompacted, CostRecord, CostRecorded, PermissionOption, PermissionOptionKind,
        PermissionRequest, SessionTitled, SessionUpdate,
    };
    use teton_protocol::{ProviderId, RequestId, Tier, TurnId};

    /// The session this client is in. Every fixture below is scoped to it, so a
    /// test that means "somebody else's event" has to say so.
    fn ours() -> SessionId {
        SessionId::from("ours")
    }

    fn env(event: Event) -> EventEnvelope {
        EventEnvelope::new(1, Some(ours()), event)
    }

    /// `secs` after the turn started. The tests' whole clock is this function —
    /// nothing here reads the wall clock except the base instant.
    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    fn route(model: Option<&str>, tier: Option<Tier>) -> Event {
        Event::RouteDecided(RouteDecided {
            category: None,
            tier,
            phase: None,
            provider_id: ProviderId::from("anthropic"),
            model: model.map(str::to_owned),
            reason: "fixture".to_owned(),
            effort: None,
            window_tokens: None,
            budget_tokens: None,
            budget_bytes: None,
            bound: None,
            spend_ceiling_micro_cents: None,
            bound_floored: None,
            repo_context_cap: None,
        })
    }

    /// The route every table row below uses: a provider, a model and a tier.
    fn full_route() -> Event {
        route(Some("claude-opus-5"), Some(Tier::Think))
    }

    fn chunk(text: &str) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::AgentMessageChunk {
                text: text.to_owned(),
            },
        })
    }

    fn tool_call(title: &str) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::ToolCall {
                tool_call_id: "c1".to_owned(),
                title: title.to_owned(),
                status: ToolCallStatus::InProgress,
            },
        })
    }

    fn tool_update(status: ToolCallStatus) -> Event {
        Event::SessionUpdate(SessionUpdate {
            update: SessionUpdatePayload::ToolCallUpdate {
                tool_call_id: "c1".to_owned(),
                status,
            },
        })
    }

    fn permission() -> Event {
        Event::PermissionRequest(PermissionRequest {
            request_id: RequestId::from("r1"),
            tool_name: "shell".to_owned(),
            description: Some("run `cargo test`".to_owned()),
            subject: None,
            options: vec![PermissionOption {
                option_id: "allow_once".to_owned(),
                label: "Allow once".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        })
    }

    fn queued(waiting_on: TierWarming) -> Event {
        Event::TurnQueued(TurnQueued {
            turn_id: TurnId::from("turn-3"),
            model_id: "qwen3-coder-30b-a3b".to_owned(),
            waiting_on,
        })
    }

    fn cost(usd_micros: i64) -> Event {
        Event::CostRecorded(CostRecorded {
            record: CostRecord {
                session_id: ours(),
                phase: None,
                category: None,
                provider_id: ProviderId::from("anthropic"),
                model: "claude-opus-5".to_owned(),
                input_tokens: 100,
                output_tokens: 50,
                usd_micros,
                cached_tokens: None,
                reasoning_tokens: None,
                probe: false,
            },
        })
    }

    fn prefill(tokens_done: u32, tokens_total: u32) -> Event {
        Event::PrefillProgress(PrefillProgress {
            tokens_done,
            tokens_total,
            tokens_per_second: 120.0,
        })
    }

    fn compacted() -> Event {
        Event::ContextCompacted(ContextCompacted {
            kept_bytes: 4_096,
            dropped_bytes: 512,
            summarized_bytes: 2_048,
            anchor_bytes: 128,
            dropped_blocks: Vec::new(),
            dropped_blocks_omitted: 0,
            provider_id: None,
            fallback: true,
        })
    }

    /// One row of [`the_frame_table`]: what the state is, the instant and tick it
    /// is rendered at, the surface width, and the exact string it must produce
    /// (`None` for a phase that draws nothing).
    ///
    /// Named rather than written inline because the table is the point of the
    /// test and a six-deep tuple in the middle of it reads as noise.
    type FrameCase = (
        &'static str,
        TurnActivity,
        Instant,
        u64,
        usize,
        Option<&'static str>,
    );

    /// A turn armed at `t0`, with `script`'s events folded at their offsets.
    fn folded(t0: Instant, script: &[(u64, Event)]) -> TurnActivity {
        let mut activity = TurnActivity::default();
        activity.begin(t0);
        for (secs, event) in script {
            activity.observe(&env(event.clone()), Some(&ours()), at(t0, *secs));
        }
        activity
    }

    /// BR-2: every word of detail in the row came from the event that carried
    /// it. The provider, model and tier are `route_decided`'s; the tool
    /// sentence is the title the daemon composed, not a second reading of the
    /// command; the held sentence is branched off the typed warming state.
    ///
    /// The route clause is what a tool call must **not** erase: the phase after
    /// a tool result is `awaiting_model` again, and re-deriving the model from
    /// anything else the client holds is the failure BR-2 exists to prevent.
    #[test]
    fn detail_comes_only_from_the_event_that_carried_it() {
        let t0 = Instant::now();

        // No route yet: no model is named, because none has been chosen.
        let preparing = folded(t0, &[]);
        assert_eq!(
            preparing.frame(at(t0, 1), 0, 120).as_deref(),
            Some("⠋ preparing turn · 1s · turn 1s")
        );

        // A route with no model names the provider and the tier only.
        let no_model = folded(t0, &[(1, route(None, Some(Tier::Build)))]);
        assert_eq!(
            no_model.frame(at(t0, 2), 0, 120).as_deref(),
            Some("⠋ waiting on anthropic (build) · 1s · turn 2s")
        );

        // A route with no tier names the provider and the model only.
        let no_tier = folded(t0, &[(1, route(Some("claude-opus-5"), None))]);
        assert_eq!(
            no_tier.frame(at(t0, 2), 0, 120).as_deref(),
            Some("⠋ waiting on anthropic claude-opus-5 · 1s · turn 2s")
        );

        // The daemon's own tool title, verbatim.
        let running = folded(
            t0,
            &[(1, full_route()), (3, tool_call("shell: cargo test"))],
        );
        assert_eq!(
            running.frame(at(t0, 5), 0, 120).as_deref(),
            Some("⠋ running shell: cargo test · 2s · turn 5s")
        );

        // ...and the route survives it, because the next phase is the model's.
        let after_tool = folded(
            t0,
            &[
                (1, full_route()),
                (3, tool_call("shell: cargo test")),
                (8, tool_update(ToolCallStatus::Completed)),
            ],
        );
        assert_eq!(
            after_tool.frame(at(t0, 9), 0, 120).as_deref(),
            Some("⠋ waiting on anthropic claude-opus-5 (think) · 1s · turn 9s")
        );

        // The held sentence names the model and which transient state it is in.
        let held = folded(t0, &[(1, queued(TierWarming::Loading))]);
        assert_eq!(
            held.frame(at(t0, 2), 0, 120).as_deref(),
            Some("⠋ held until qwen3-coder-30b-a3b finishes loading · 1s · turn 2s")
        );
        let installing = folded(t0, &[(1, queued(TierWarming::Installing))]);
        assert_eq!(
            installing.frame(at(t0, 2), 0, 120).as_deref(),
            Some("⠋ held until qwen3-coder-30b-a3b finishes installing · 1s · turn 2s")
        );
    }

    /// BR-3 / BR-7: the whole vocabulary of the row, as literal strings, with
    /// no terminal and no clock involved. Every expectation is written out
    /// rather than computed from the same inputs the frame reads — an oracle
    /// that called `frame` would pass against any implementation of it,
    /// including one that returned the empty string (LESSON-569).
    #[test]
    fn the_frame_table() {
        let t0 = Instant::now();
        let streaming = &[(1, full_route()), (2, chunk("he"))][..];
        let permissioned = &[(1, full_route()), (2, permission())][..];

        let cases: Vec<FrameCase> = vec![
            (
                "no turn in flight draws nothing",
                TurnActivity::default(),
                at(t0, 3),
                0,
                120,
                None,
            ),
            (
                "before route_decided",
                folded(t0, &[]),
                at(t0, 2),
                0,
                120,
                Some("⠋ preparing turn · 2s · turn 2s"),
            ),
            (
                "awaiting the model, one tick on",
                folded(t0, &[(1, full_route())]),
                at(t0, 4),
                1,
                120,
                Some("⠙ waiting on anthropic claude-opus-5 (think) · 3s · turn 4s"),
            ),
            (
                "streaming is its own liveness signal",
                folded(t0, streaming),
                at(t0, 3),
                0,
                120,
                None,
            ),
            (
                "a running tool, two ticks on",
                folded(
                    t0,
                    &[(1, full_route()), (3, tool_call("shell: cargo test"))],
                ),
                at(t0, 9),
                2,
                120,
                Some("⠹ running shell: cargo test · 6s · turn 9s"),
            ),
            (
                "the permission prompt owns the terminal",
                folded(t0, permissioned),
                at(t0, 8),
                0,
                120,
                None,
            ),
            (
                "a held turn",
                folded(t0, &[(1, queued(TierWarming::Loading))]),
                at(t0, 2),
                0,
                120,
                Some("⠋ held until qwen3-coder-30b-a3b finishes loading · 1s · turn 2s"),
            ),
            (
                "compacting mid-turn",
                folded(t0, &[(1, full_route()), (2, compacted())]),
                at(t0, 3),
                0,
                120,
                Some("⠋ compacting context · 1s · turn 3s"),
            ),
            (
                "cost so far, once a row has landed",
                folded(t0, &[(1, full_route()), (1, cost(12_345))]),
                at(t0, 2),
                0,
                120,
                Some("⠋ waiting on anthropic claude-opus-5 (think) · 1s · turn 2s · $0.012345"),
            ),
            (
                "two rows are summed exactly",
                folded(
                    t0,
                    &[(1, full_route()), (1, cost(12_345)), (1, cost(1_655))],
                ),
                at(t0, 2),
                0,
                120,
                Some("⠋ waiting on anthropic claude-opus-5 (think) · 1s · turn 2s · $0.014000"),
            ),
            (
                "a zero cost row adds no clause",
                folded(t0, &[(1, full_route()), (1, cost(0))]),
                at(t0, 2),
                0,
                120,
                Some("⠋ waiting on anthropic claude-opus-5 (think) · 1s · turn 2s"),
            ),
            (
                "the daemon's own prefill fraction",
                folded(t0, &[(1, full_route()), (1, prefill(1_200, 4_096))]),
                at(t0, 2),
                0,
                120,
                Some(
                    "⠋ waiting on anthropic claude-opus-5 (think) · reading context 1,200/4,096 · \
                     1s · turn 2s",
                ),
            ),
            (
                "a row wider than the terminal is cut on a character boundary",
                folded(t0, &[(1, full_route())]),
                at(t0, 2),
                0,
                20,
                Some("⠋ waiting on anthrop"),
            ),
            (
                "a row with no columns is not a row",
                folded(t0, &[(1, full_route())]),
                at(t0, 2),
                0,
                0,
                None,
            ),
        ];

        for (what, activity, now, tick, width, expected) in cases {
            assert_eq!(
                activity.frame(now, tick, width).as_deref(),
                expected,
                "{what}"
            );
        }
    }

    /// BR-11 / ADR-621-5: past the quiet bound the row keeps the phase the
    /// daemon last reported, stops its spinner and states the silence. A
    /// running tool is exempt — the daemon publishes nothing while a tool runs,
    /// so annotating it would relabel every long test suite as a stall and make
    /// a real one easy to miss (LESSON-628).
    #[test]
    fn a_stall_annotates_the_last_phase_and_a_running_tool_is_exempt() {
        let t0 = Instant::now();
        let waiting = folded(t0, &[(1, full_route())]);

        // One second inside the bound: an ordinary, moving row.
        assert_eq!(
            waiting.frame(at(t0, 15), 3, 120).as_deref(),
            Some("⠸ waiting on anthropic claude-opus-5 (think) · 14s · turn 15s")
        );

        // On the bound: the phase is unchanged, the silence is stated.
        assert_eq!(
            waiting.frame(at(t0, 16), 3, 120).as_deref(),
            Some(
                "⠿ waiting on anthropic claude-opus-5 (think) · 15s · turn 16s · no word from \
                 the daemon for 15s"
            )
        );

        // ...and the spinner has stopped: the tick no longer changes the row.
        assert_eq!(
            waiting.frame(at(t0, 16), 0, 120),
            waiting.frame(at(t0, 16), 7, 120)
        );

        // A mid-stream stall brings the row back beneath the partial reply
        // (OQ-2), naming streaming rather than a phase nobody reported.
        let mid_stream = folded(t0, &[(1, full_route()), (2, chunk("he"))]);
        assert_eq!(
            mid_stream.frame(at(t0, 16), 0, 120),
            None,
            "still inside the bound"
        );
        assert_eq!(
            mid_stream.frame(at(t0, 18), 0, 120).as_deref(),
            Some("⠿ receiving the reply · 16s · turn 18s · no word from the daemon for 16s")
        );

        // A further byte withdraws it again.
        let resumed = folded(
            t0,
            &[(1, full_route()), (2, chunk("he")), (18, chunk("llo"))],
        );
        assert_eq!(resumed.frame(at(t0, 19), 0, 120), None);

        // Ten minutes into a tool, with no annotation and a moving spinner.
        let tool = folded(
            t0,
            &[(1, full_route()), (3, tool_call("shell: cargo test"))],
        );
        assert_eq!(
            tool.frame(at(t0, 600), 4, 120).as_deref(),
            Some("⠼ running shell: cargo test · 597s · turn 600s")
        );
        assert_ne!(
            tool.frame(at(t0, 600), 4, 120),
            tool.frame(at(t0, 600), 5, 120),
            "a running tool's row keeps moving"
        );
    }

    /// BR-15 / AC-9: the bus is daemon-wide. An event carrying another
    /// session's id changes neither the phase, the counters, nor the cost — and
    /// an event carrying no session at all counts as ours, because unknown is
    /// no evidence of elsewhere (the reading the reply accumulator takes).
    #[test]
    fn another_sessions_event_changes_nothing() {
        let t0 = Instant::now();
        let mut ours_turn = folded(t0, &[(1, full_route())]);
        let before = ours_turn.frame(at(t0, 2), 0, 120);
        assert!(before.is_some(), "the fixture has a row to disturb");

        let theirs = |event: Event| EventEnvelope::new(7, Some(SessionId::from("theirs")), event);
        for event in [
            route(Some("qwen3-coder-30b-a3b"), Some(Tier::Reflex)),
            tool_call("shell: rm -rf /"),
            cost(999_999),
            chunk("not ours"),
            permission(),
        ] {
            ours_turn.observe(&theirs(event), Some(&ours()), at(t0, 2));
        }
        assert_eq!(
            ours_turn.frame(at(t0, 2), 0, 120),
            before,
            "another session's events left this row unchanged"
        );

        // An event with no session id is ours.
        ours_turn.observe(
            &EventEnvelope::new(8, None, tool_call("shell: cargo build")),
            Some(&ours()),
            at(t0, 3),
        );
        assert_eq!(
            ours_turn.frame(at(t0, 4), 0, 120).as_deref(),
            Some("⠋ running shell: cargo build · 1s · turn 4s")
        );
    }

    /// AC-4: the row must actually move. **Mutation (re-run 2026-09-10 over the
    /// widened suite):** `frame` ignoring its `tick` — `SPINNER[0]` in place of
    /// `SPINNER[(tick % SPINNER.len() as u64) as usize]` — reddens 3 of the 10
    /// tests in this module and **none** of `pty_e2e`'s 28. This test is the one
    /// that fails on the property, on its distinct-frames assertion;
    /// `the_frame_table` and
    /// `a_stall_annotates_the_last_phase_and_a_running_tool_is_exempt` fail as
    /// collateral, on literal glyphs they happen to have written down. No leg at
    /// a real terminal notices at all — see the module header — so this
    /// assertion is the animation's only proof. Reverted with the same edit.
    #[test]
    fn the_frame_advances_with_the_tick() {
        let t0 = Instant::now();
        let waiting = folded(t0, &[(1, full_route())]);
        let frames: Vec<String> = (0..SPINNER.len() as u64)
            .map(|tick| waiting.frame(at(t0, 2), tick, 120).expect("visible"))
            .collect();
        let distinct: std::collections::BTreeSet<&String> = frames.iter().collect();
        assert_eq!(
            distinct.len(),
            SPINNER.len(),
            "each tick in the cycle must render differently: {frames:?}"
        );
        // And it cycles rather than growing without bound.
        assert_eq!(
            waiting.frame(at(t0, 2), 0, 120),
            waiting.frame(at(t0, 2), SPINNER.len() as u64, 120)
        );
    }

    /// AC-8: nothing the daemon has not reported ever reaches the row — not a
    /// model name before a route, not a tool before a tool call, and not a
    /// phase invented by the passage of time. An event this projection does not
    /// consume leaves the row exactly as it was.
    #[test]
    fn no_phase_is_invented() {
        let t0 = Instant::now();

        // A turn that only ever heard cost and prefill news: no route, so no
        // model, and no tool call, so nothing is running — at any tick, at any
        // instant, including well past the stall bound.
        let unrouted = folded(t0, &[(1, cost(12_345)), (1, prefill(10, 4_096))]);
        for secs in [1_u64, 2, 14, 16, 90, 600] {
            for tick in 0..SPINNER.len() as u64 {
                let row = unrouted
                    .frame(at(t0, secs), tick, 200)
                    .expect("a preparing turn has a row");
                for invented in [
                    "anthropic",
                    "claude",
                    "waiting on",
                    "running",
                    "held",
                    "compacting",
                    "receiving",
                ] {
                    assert!(
                        !row.contains(invented),
                        "the daemon reported no {invented}: {row}"
                    );
                }
            }
        }

        // A stall states the silence and keeps the phase; it is not a phase.
        let stalled = folded(t0, &[(1, full_route())])
            .frame(at(t0, 30), 0, 200)
            .expect("visible");
        assert!(
            stalled.contains("waiting on anthropic claude-opus-5 (think)"),
            "{stalled}"
        );
        assert!(!stalled.contains("stalled"), "{stalled}");

        // An event the projection does not consume changes nothing at all.
        let mut activity = folded(t0, &[(1, full_route())]);
        let before = activity.frame(at(t0, 2), 0, 120);
        activity.observe(
            &env(Event::SessionTitled(SessionTitled {
                title: "why is the build slow".to_owned(),
            })),
            Some(&ours()),
            at(t0, 2),
        );
        assert_eq!(activity.frame(at(t0, 2), 0, 120), before);
        assert_eq!(activity.phase(), Phase::AwaitingModel);
    }

    /// BR-1: the row steps aside for a permission prompt and comes back to the
    /// sentence it left — not to a phase re-derived from whatever arrives next.
    /// The restore is idempotent, because the pump is not the only caller that
    /// could plausibly make it.
    #[test]
    fn a_permission_round_trip_restores_the_phase_it_interrupted() {
        let t0 = Instant::now();
        let mut activity = folded(
            t0,
            &[
                (1, full_route()),
                (3, tool_call("shell: cargo test")),
                (4, permission()),
            ],
        );
        assert_eq!(activity.phase(), Phase::AwaitingPermission);
        assert_eq!(activity.frame(at(t0, 5), 0, 120), None);

        activity.permission_answered(at(t0, 9));
        assert_eq!(
            activity.frame(at(t0, 10), 0, 120).as_deref(),
            Some("⠋ running shell: cargo test · 1s · turn 10s")
        );

        // A second answer has nothing to restore and must not reset the clock.
        activity.permission_answered(at(t0, 11));
        assert_eq!(
            activity.frame(at(t0, 12), 0, 120).as_deref(),
            Some("⠋ running shell: cargo test · 3s · turn 12s")
        );
    }

    /// BR-16: `finish` charges the phase still in flight, reports the turn's
    /// figures, and leaves the activity idle so every exit path is safe to call
    /// it on (BR-12).
    #[test]
    fn finish_accrues_the_last_phase_and_leaves_the_activity_idle() {
        let t0 = Instant::now();
        let mut activity = folded(
            t0,
            &[
                (1, full_route()),
                (3, tool_call("shell: cargo test")),
                (8, tool_update(ToolCallStatus::Completed)),
                (9, cost(12_345)),
            ],
        );

        let summary = activity.finish(at(t0, 10));
        assert_eq!(summary.total, Duration::from_secs(10));
        // 1s→3s awaiting the model, 8s→10s composing the next step.
        assert_eq!(summary.model, Duration::from_secs(4));
        assert_eq!(summary.tools, Duration::from_secs(5));
        assert_eq!(summary.cost_micros, 12_345);

        assert_eq!(activity.phase(), Phase::Idle);
        assert_eq!(activity.frame(at(t0, 11), 0, 120), None);
        assert_eq!(activity.finish(at(t0, 12)), TurnSummary::default());
    }

    /// A turn armed over a spent one inherits nothing — not its cost, not its
    /// phase, not its clock.
    #[test]
    fn a_new_turn_inherits_nothing_from_the_last_one() {
        let t0 = Instant::now();
        let mut activity = folded(t0, &[(1, full_route()), (2, cost(500_000))]);
        activity.begin(at(t0, 20));
        assert_eq!(
            activity.frame(at(t0, 22), 0, 120).as_deref(),
            Some("⠋ preparing turn · 2s · turn 2s")
        );
        assert_eq!(activity.finish(at(t0, 22)).cost_micros, 0);
    }

    /// BR-7's whole point, as an executable claim: none of the above
    /// constructed a `Surface`, opened a terminal, or let the projection read a
    /// clock. The frame is a pure function of `(state, now, tick, width)`.
    #[test]
    fn frames_are_computable_with_no_terminal_and_no_clock() {
        let t0 = Instant::now();
        let activity = folded(t0, &[(1, full_route())]);
        assert_eq!(
            activity.frame(at(t0, 7), 3, 80),
            activity.frame(at(t0, 7), 3, 80)
        );
    }
}
