//! What the `agent` tool needs from the runtime to run one child turn
//! (REQ-623 ADR-2, ADR-8).
//!
//! A child is a real turn — the same loop, the session's tools minus `agent`,
//! the session's gate, root and boundaries — started with a fresh context that
//! holds only the session's system prompt, the parent's `context` string and
//! the `task` (BR-1), run under four bounds stamped before its first model call
//! (BR-7), and ended in exactly one of eight statuses (BR-10).
//!
//! # Layering
//!
//! The `agent` tool lives in `harness/tools/` and must not import
//! `runtime::*` (architecture "Service layer"). So the seam is a trait here —
//! [`ChildDispatcher`] — and the runtime implements it
//! (`runtime/child_turn.rs`). The tool builds one [`ChildSpec`] per task, hands
//! it to [`ChildDispatcher::run_child`], and gets a [`ChildOutcome`] back; it
//! never sees a router, a registry or an egress.
//!
//! # What lives here, and why here
//!
//! - [`PausableDeadline`] — BR-5's clock: a child's `deadline_secs` covers work,
//!   not waiting for a human, so the clock stops while the session's gate is
//!   parked on a consent prompt the child raised. A `tokio::time::timeout`
//!   cannot express that.
//! - [`ChildTaskScope`] and [`current_child`] — the task-local that tells code
//!   running *inside* a child's task which child it is. The gate's
//!   [`AskObserver`] hook fires synchronously on the waiting task, and one gate
//!   serves the parent and every sibling (ADR-5), so "which deadline do I
//!   pause" has to be answered by the task that is waiting, not by the session.
//!   [`ChildAskClock`] is that observer.
//! - [`bound_report`] — BR-11's loud cut.
//! - [`ChildTurn`] — the facts that make a turn a child's, carried on the
//!   turn's `TurnContext` so every stage the child shares with a prompt turn
//!   (route, assemble, attempt, duties) can tell which it is serving.
//!
//! # Four events a child does not publish
//!
//! `route_decided`, `context_compacted`, `turn_queued` and `prefill_progress`
//! carry no `child_id`/`parent_turn_id` (they are session-scoped payloads that
//! REQ-623 deliberately did not widen). A client reading the shared bus
//! attributes them to the parent turn — its activity row would flip to the
//! child's model, or to "compacting", while the parent is merely waiting. So a
//! child **suppresses** all four rather than widening the protocol:
//!
//! - `route_decided` — the child's route is already on
//!   `agent_child_started.route` and `ChildResult.route`, which name the child;
//!   the attempt loop and the child's duty routes publish none (see
//!   `run_attempts` and `resolve_duty`).
//! - `context_compacted` — the child's emitter publishes none
//!   (`SessionEvents::context_compacted`); its `context_pressure` lines, which
//!   *are* stamped, still say what a refit took.
//! - `turn_queued` — a child never takes the warming hold that publishes it: it
//!   runs under the parent's claim on a tier the parent is already using, and a
//!   child whose route has nowhere to run ends `failed` with the code.
//! - `prefill_progress` — the daemon publishes none today; listed so a future
//!   publisher on the turn path inherits the rule.
//!
//! [`PARENT_ONLY_EVENTS`] names them for the test that pins the rule.
//!
//! ASSUME-010: the test module stays last.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use teton_protocol::agent::{ChildId, ChildResult, ChildStatus};
use teton_protocol::events::{bytes_figure, thousands};
use teton_protocol::{RequestId, Tier, TurnId};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::budget::{RouteBudget, OVER_BUDGET_REASON};
use super::completion::{CompletionSource, SourceTurn};
use super::context::{Fit, PreparedPrompt};
use super::permissions::AskObserver;
use super::tools::ToolRegistry;
use super::turn_loop::{HarnessConfig, HarnessError};
use crate::cost::ChildSpend;
use crate::egress::Provenance;
use teton_inference::ChatFormat;

/// The wire names of the session-scoped events a child turn never publishes —
/// see the module docs for why each is suppressed rather than stamped.
pub const PARENT_ONLY_EVENTS: [&str; 4] = [
    "route_decided",
    "context_compacted",
    "turn_queued",
    "prefill_progress",
];

/// The token a truncated report's marker opens with (BR-11) — what a reader
/// greps for, and what [`bound_report`] writes.
pub const REPORT_TRUNCATED: &str = "report_truncated";

/// The token a `turns_exhausted` report's marker opens with (BR-10).
pub const TURNS_EXHAUSTED: &str = "turns_exhausted";

/// The refusal code of a child the permission gate stopped (BR-10's "refused
/// by a gate"): `gate_denied:<tool>` — see [`ChildToolCalls::gate_refusal`].
pub const GATE_DENIED: &str = "gate_denied";

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// What the `agent` tool needs from the runtime: run one child turn to a
/// terminal status (REQ-623 ADR-2).
///
/// **Never fails.** Every way a child can end — including a panic inside its
/// run — comes back as a [`ChildOutcome`] carrying one of the eight
/// [`ChildStatus`]es, because a child's failure must never fail the parent
/// turn (BR-10). The one ending this call cannot report is its own future being
/// dropped (the parent turn cancelled, `JoinSet::abort_all`): that lands in
/// [`ChildSpec::cancelled`], see [`ChildOutcomeSlot`].
#[async_trait]
pub trait ChildDispatcher: Send + Sync {
    /// Run `spec` as a child of the prompt turn this dispatcher was built for.
    async fn run_child(&self, spec: ChildSpec) -> ChildOutcome;
}

/// One child to run — everything per child that the dispatcher cannot know
/// from the parent turn it was built for.
///
/// The per-turn facts (session, parent turn id, mode, phase, the turn's config
/// snapshot, the session's gate, the invoker) live on the dispatcher, which the
/// runtime builds once per prompt turn; this is what the `agent` tool fills per
/// task.
#[derive(Clone)]
pub struct ChildSpec {
    /// `"<call_id>/<name>"` — [`ChildId::new`].
    pub child_id: ChildId,
    /// The task's name, or the `child-<n>` default the tool gave it.
    pub name: String,
    /// The child's user-role prompt, verbatim — its only user message (BR-1).
    pub task: String,
    /// Extra text the parent passed. Rendered into the child's system prompt
    /// beside the report bound, so the task stays the only user message (AC-2);
    /// admitted whole with `task` or the child is refused `over_budget`.
    pub context: Option<String>,
    /// The tier the task requested — a hint to the router, never a binding
    /// (BR-6).
    pub tier: Option<Tier>,
    /// The **parent's** context provenance at the moment of the call
    /// (`context_provenance` of the parent's manager).
    ///
    /// `task` and `context` are written by the parent model, which may have
    /// read boundary content; text carried into a child must not shed that
    /// taint (LESSON-501). The child's user block is seeded with it, so the
    /// child's own egress check — and its route pin — judge the task text as
    /// they would judge the parent's next call.
    pub provenance: Provenance,
    /// `agent.child_max_turns`. Clamped to [`Self::parent_max_turns`] and to the
    /// child's route profile before it is stamped.
    pub max_turns: u32,
    /// The parent loop's own `max_turns` — what a child's cap may never exceed
    /// (BR-7, entity table).
    pub parent_max_turns: u32,
    /// `agent.child_deadline_secs`: the child's work clock (BR-5).
    pub deadline: Duration,
    /// `agent.report_max_bytes`: where the report is cut (BR-11), stated in the
    /// child's system prompt so a well-behaved child never reaches it.
    pub report_max_bytes: u64,
    /// The child's spend share and accumulators (ADR-4). The runner stamps
    /// [`ChildSpend::ceiling`] into the bounds before the first call, builds
    /// every egress the child uses with it, and releases it when the child
    /// ends.
    pub spend: ChildSpend,
    /// The call's consent mutex (ADR-5), exposed to code inside the child's
    /// task through [`ChildTaskScope::consent`] — TASK-428 serialises asks on
    /// it and pauses the deadline while a child queues for it.
    pub consent: Arc<tokio::sync::Mutex<()>>,
    /// Where an aborted run leaves its `cancelled` outcome — see
    /// [`ChildOutcomeSlot`].
    pub cancelled: ChildOutcomeSlot,
}

/// What a child hands back (BR-10, ADR-8).
#[derive(Debug, Clone)]
pub struct ChildOutcome {
    /// The wire result the parent model reads: status, the bounded report,
    /// the typed refusal or error, turns, route, the bounds echoed, cost and
    /// the final ceiling.
    pub result: ChildResult,
    /// Every provenance the child's context touched — its system prompt's
    /// sources, every read, every skill expansion, the task's own seed, and
    /// `unknown` when any of them was (BR-9). The `agent` tool puts this on the
    /// result block, so the parent's next call is judged by it.
    ///
    /// For a child that ended `timed_out` or `cancelled` its context is still
    /// owned by the aborted run; this is then the provenance the child was
    /// seeded with. Those statuses carry an empty report, so nothing the child
    /// read reaches the parent's context.
    pub provenance: Provenance,
    /// The size of the child's whole final text before any cut —
    /// `agent_child_finished.report_bytes`.
    pub report_bytes: u64,
    /// Whether [`ChildResult::report`] was cut at the bound —
    /// `agent_child_finished.truncated` (BR-11).
    pub truncated: bool,
    /// The child's unspent share, released to its running siblings when it
    /// ended — `agent_child_share_released`'s payload. `None` when nothing
    /// moved.
    pub share_released: Option<ShareRelease>,
}

impl ChildOutcome {
    /// How the child ended.
    #[must_use]
    pub fn status(&self) -> ChildStatus {
        self.result.status
    }

    /// A child whose run never started — aborted before its future was first
    /// polled, so nothing was routed, stamped or spent (BR-10).
    ///
    /// For the `agent` tool's `JoinError::is_cancelled` arm when
    /// [`ChildOutcomeSlot::take`] comes back empty.
    #[must_use]
    pub fn cancelled_before_start(name: impl Into<String>) -> Self {
        Self {
            result: ChildResult {
                name: name.into(),
                status: ChildStatus::Cancelled,
                report: String::new(),
                refusal: None,
                error: None,
                turns_used: 0,
                route: None,
                bounds: None,
                cost_micro_cents: 0,
                spend_ceiling_final_micro_cents: None,
            },
            provenance: Provenance::empty(),
            report_bytes: 0,
            truncated: false,
            share_released: None,
        }
    }
}

/// A child's unspent share as it was released to its running siblings
/// (BR-8, `agent_child_share_released`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRelease {
    /// What the child left unspent of its final ceiling.
    pub released_micro_cents: u64,
    /// Every sibling that received a part, with its new ceiling.
    pub recipients: Vec<(ChildId, u64)>,
}

/// Where a child's run leaves its `cancelled` outcome when the run's future is
/// dropped before it can return one (BR-10).
///
/// An aborted task never returns: `JoinSet::abort_all` drops the future, and
/// the join reports `JoinError::Cancelled`. So the runner holds a drop guard
/// that, if the run is dropped unfinished, writes a `cancelled` outcome here —
/// with the route and bounds it had already stamped, the spend it had already
/// made, and its share released — and the tool reads it back with
/// [`Self::take`]. A run that finishes writes nothing here.
#[derive(Debug, Clone, Default)]
pub struct ChildOutcomeSlot(Arc<Mutex<Option<ChildOutcome>>>);

impl ChildOutcomeSlot {
    /// The outcome an aborted run left, if it left one.
    #[must_use]
    pub fn take(&self) -> Option<ChildOutcome> {
        lock(&self.0).take()
    }

    /// Leave `outcome` for [`Self::take`] — the runner's drop guard.
    pub fn put(&self, outcome: ChildOutcome) {
        *lock(&self.0) = Some(outcome);
    }
}

/// The facts that make a turn a child's (REQ-623), carried on the turn's
/// `TurnContext` (`TurnContext::for_child`) and on its duty context.
///
/// Every stage a child shares with a prompt turn reads it to decide the few
/// things that differ: no `agent` in the registry, the emitter stamped with the
/// child's ids, no `route_decided`, the turn cap clamped to the stamped bound
/// on every attempt, and every egress built with the child's spend share.
#[derive(Debug, Clone)]
pub struct ChildTurn {
    /// The child.
    pub child_id: ChildId,
    /// The prompt turn it runs under.
    pub parent_turn_id: TurnId,
    /// The stamped `max_turns` bound — re-applied to every attempt's route,
    /// because a reroute swaps in the new provider's profile and must never
    /// raise the bound (BR-7).
    pub max_turns: u32,
    /// The child's spend wiring (ADR-4) — every choke point the child's calls
    /// go through is built with it.
    pub spend: ChildSpend,
    /// The model calls the child has made, across every attempt — its
    /// `turns_used` (BR-10). Shared with the runner rather than read off the
    /// loop's return, because a child that times out or is cancelled never
    /// returns one, and a reroute starts the loop's own count again.
    pub model_calls: Arc<AtomicU32>,
}

/// A [`CompletionSource`] that counts the model calls it serves into a shared
/// counter, when it is given one — a child's `turns_used` (BR-10).
///
/// Wraps whichever source an attempt runs on, so the count is taken where the
/// call is made rather than reconstructed from what a context still holds after
/// compaction. With no counter it is a passthrough.
pub struct CountedSource<'a> {
    inner: &'a mut dyn CompletionSource,
    calls: Option<&'a AtomicU32>,
}

impl<'a> CountedSource<'a> {
    /// Serve `inner`'s calls, counting each one that completes into `calls`.
    pub fn new(inner: &'a mut dyn CompletionSource, calls: Option<&'a AtomicU32>) -> Self {
        Self { inner, calls }
    }
}

#[async_trait]
impl CompletionSource for CountedSource<'_> {
    async fn produce_turn(
        &mut self,
        prompt: &PreparedPrompt,
        provenance: &crate::egress::Provenance,
        config: &HarnessConfig,
        tools: &ToolRegistry,
        exposed: &[&str],
        on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
    ) -> Result<SourceTurn, HarnessError> {
        let produced = self
            .inner
            .produce_turn(prompt, provenance, config, tools, exposed, on_token)
            .await;
        // Counted as the loop counts its own `turns`: a call that produced a
        // turn. A call refused before it was sent — a spend ceiling, a privacy
        // block — was not made.
        if let (Ok(_), Some(calls)) = (&produced, self.calls) {
            calls.fetch_add(1, Ordering::Relaxed);
        }
        produced
    }

    fn chat_format(&self) -> ChatFormat {
        self.inner.chat_format()
    }
}

// ---------------------------------------------------------------------------
// The work clock
// ---------------------------------------------------------------------------

/// A deadline over **work time**: a clock that stops while it is paused
/// (BR-5, ADR-2).
///
/// Started running. [`Self::pause`] stops it and hands back a guard; the clock
/// runs again when the last outstanding guard is dropped. Pauses nest — the
/// gate's consent wait and the call's consent-mutex queue (TASK-428) may
/// overlap, and the clock stays stopped until both have ended.
/// [`Self::expired`] resolves once the work time reaches the budget.
///
/// Measured on `tokio::time::Instant`, so a paused-clock test drives it
/// exactly.
#[derive(Debug, Clone)]
pub struct PausableDeadline {
    inner: Arc<DeadlineInner>,
}

#[derive(Debug)]
struct DeadlineInner {
    budget: Duration,
    clock: Mutex<WorkClock>,
    /// Woken on every pause and resume, so a waiting [`PausableDeadline::expired`]
    /// re-reads the clock instead of sleeping to a wake time a pause moved.
    changed: Notify,
}

#[derive(Debug)]
struct WorkClock {
    /// Work time banked before the current running stretch.
    worked: Duration,
    /// When the current running stretch began; `None` while paused.
    running_since: Option<Instant>,
    /// Outstanding [`DeadlinePause`] guards.
    pauses: usize,
}

impl PausableDeadline {
    /// A deadline of `budget` work time, running from now.
    #[must_use]
    pub fn start(budget: Duration) -> Self {
        Self {
            inner: Arc::new(DeadlineInner {
                budget,
                clock: Mutex::new(WorkClock {
                    worked: Duration::ZERO,
                    running_since: Some(Instant::now()),
                    pauses: 0,
                }),
                changed: Notify::new(),
            }),
        }
    }

    /// The work time this deadline allows.
    #[must_use]
    pub fn budget(&self) -> Duration {
        self.inner.budget
    }

    /// Work time so far — paused stretches excluded.
    #[must_use]
    pub fn worked(&self) -> Duration {
        let clock = lock(&self.inner.clock);
        clock.worked + clock.running_since.map_or(Duration::ZERO, |s| s.elapsed())
    }

    /// Whether the clock is stopped.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        lock(&self.inner.clock).running_since.is_none()
    }

    /// Stop the clock until the returned guard — and every other outstanding
    /// one — is dropped.
    #[must_use = "the clock resumes when the guard is dropped"]
    pub fn pause(&self) -> DeadlinePause {
        {
            let mut clock = lock(&self.inner.clock);
            if let Some(since) = clock.running_since.take() {
                clock.worked += since.elapsed();
            }
            clock.pauses += 1;
        }
        self.inner.changed.notify_waiters();
        DeadlinePause {
            deadline: self.clone(),
        }
    }

    fn resume_one(&self) {
        {
            let mut clock = lock(&self.inner.clock);
            clock.pauses = clock.pauses.saturating_sub(1);
            if clock.pauses == 0 && clock.running_since.is_none() {
                clock.running_since = Some(Instant::now());
            }
        }
        self.inner.changed.notify_waiters();
    }

    /// Resolves once the work time reaches the budget — never while paused.
    ///
    /// Cancel-safe: it holds no state of its own, so a `select!` that drops it
    /// and builds it again loses nothing.
    pub async fn expired(&self) {
        loop {
            // Registered before the clock is read, so a pause or resume that
            // lands between the read and the wait still wakes this one.
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake_at = {
                let clock = lock(&self.inner.clock);
                match clock.running_since {
                    None => None,
                    Some(since) => {
                        if clock.worked + since.elapsed() >= self.inner.budget {
                            return;
                        }
                        Some(since + (self.inner.budget - clock.worked))
                    }
                }
            };
            match wake_at {
                None => notified.await,
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => {}
                        () = &mut notified => {}
                    }
                }
            }
        }
    }
}

/// One outstanding pause of a [`PausableDeadline`]: dropping it resumes the
/// clock (once no other pause is outstanding).
#[derive(Debug)]
pub struct DeadlinePause {
    deadline: PausableDeadline,
}

impl DeadlinePause {
    /// End this pause now — the same as dropping the guard, spelled so a call
    /// site reads as what it does.
    pub fn resume(self) {}
}

impl Drop for DeadlinePause {
    fn drop(&mut self) {
        self.deadline.resume_one();
    }
}

// ---------------------------------------------------------------------------
// Which child is this task?
// ---------------------------------------------------------------------------

tokio::task_local! {
    static CURRENT_CHILD: ChildTaskScope;
}

/// What code running inside a child's task can learn about the child it is
/// running for — the task-local [`current_child`] reads.
///
/// Set once, around the child's whole run, by the runner. It reaches every
/// poll of that run: the loop's awaits, and the synchronous work a tool does
/// on the same thread (`block_in_place` and a nested `block_on` run inside the
/// poll that set it). It does **not** follow work onto a different task, which
/// is why the local engine's blocking completion — which asks nobody anything
/// — is outside it.
#[derive(Debug, Clone)]
pub struct ChildTaskScope {
    /// The child.
    pub child_id: ChildId,
    /// Its name — what a consent prompt is labelled with (BR-5).
    pub name: String,
    /// The prompt turn the child runs under.
    ///
    /// Here beside [`Self::child_id`] because the permission gate stamps both
    /// on a `permission_request` this child raises (ADR-5), and the gate holds
    /// no emitter — it is the session's, shared by the parent and every
    /// sibling — so the asking *task* is the one place that knows whose ask it
    /// is. Minted by the runner from the same facts the child's
    /// `SessionEvents::for_child` emitter was, so the two cannot disagree.
    pub parent_turn_id: TurnId,
    /// Its work clock, for anything that makes it wait on a human.
    pub deadline: PausableDeadline,
    /// The call's consent mutex (ADR-5): the gate takes it around a child's
    /// ask, so concurrent asks from one call's children reach the user one at
    /// a time, and the child's clock is paused while it queues for it.
    pub consent: Arc<tokio::sync::Mutex<()>>,
    /// What the child's tool calls came to at the gate — read when the child
    /// ends, to tell BR-10's "refused by a gate" from a child that ended on its
    /// own.
    pub tool_calls: ChildToolCalls,
}

/// What one child's tool calls came to at the permission gate (BR-10).
///
/// The loop notes a call the gate **denied** (its `Denied` arm) and a call
/// that **ran** (dispatched). One tool holds a gate of its own that the loop
/// cannot see: `skill`'s project-skill acknowledgment, asked inside `run`, so a
/// refusal there reaches the loop as a dispatch. The tool notes that refusal
/// itself ([`Self::note_refused_inside`]) and it is taken back out of the
/// dispatches — BR-10 names "a project-skill gate" as a `refused` ending, and a
/// call whose own gate stopped it did not run. Read once, when the child ends,
/// by [`Self::gate_refusal`].
///
/// Shared through the task-local [`ChildTaskScope`] rather than returned by the
/// loop, because the loop is the prompt turn's, unchanged, and the scope is
/// already how code inside a child's task learns which child it is serving.
#[derive(Debug, Clone, Default)]
pub struct ChildToolCalls(Arc<Mutex<ToolCallTally>>);

#[derive(Debug, Default)]
struct ToolCallTally {
    /// Every tool the gate denied, in the order it denied them.
    denied: Vec<String>,
    /// Calls that were dispatched.
    ran: u32,
    /// Dispatched calls whose own consent gate refused them, so nothing ran —
    /// counted by the loop among `ran`, and subtracted from it here.
    refused_inside: u32,
}

impl ChildToolCalls {
    /// The gate denied a call to `tool`; it did not run.
    pub fn note_denied(&self, tool: &str) {
        lock(&self.0).denied.push(tool.to_owned());
    }

    /// A call was dispatched.
    pub fn note_ran(&self) {
        lock(&self.0).ran += 1;
    }

    /// A dispatched call to `tool` was refused by the consent gate the tool
    /// holds itself, so it did not run — `skill`'s project-skill
    /// acknowledgment, declined or unanswerable (BR-10's "a project-skill
    /// gate"). A denial like the loop's; the dispatch the loop counts for it
    /// is not a call that ran.
    pub fn note_refused_inside(&self, tool: &str) {
        let mut tally = lock(&self.0);
        tally.denied.push(tool.to_owned());
        tally.refused_inside += 1;
    }

    /// The refusal a child that ended with **nothing to report** carries when
    /// the gate is why: every call it attempted was denied and none ran —
    /// `gate_denied:<the first tool denied>`.
    ///
    /// `None` when any call ran, or when none was denied. A denial is otherwise
    /// a typed tool failure inside the child (BR-5): the child read it, and
    /// whatever it did next — a report saying why, another tool — is its own
    /// ending.
    #[must_use]
    pub fn gate_refusal(&self) -> Option<String> {
        let tally = lock(&self.0);
        match (
            tally.ran.saturating_sub(tally.refused_inside),
            tally.denied.first(),
        ) {
            (0, Some(tool)) => Some(format!("{GATE_DENIED}:{tool}")),
            _ => None,
        }
    }
}

impl ChildTaskScope {
    /// Run `future` as this child: [`current_child`] answers `self` for every
    /// poll of it.
    pub fn scope<F: Future>(
        self,
        future: F,
    ) -> tokio::task::futures::TaskLocalFuture<ChildTaskScope, F> {
        CURRENT_CHILD.scope(self, future)
    }
}

/// The child the current task is running for, or `None` outside any child —
/// the parent turn, a fixture, a detached duty.
#[must_use]
pub fn current_child() -> Option<ChildTaskScope> {
    CURRENT_CHILD.try_with(Clone::clone).ok()
}

/// The session gate's [`AskObserver`]: pauses the asking child's deadline for
/// exactly the time its ask waits on a human (BR-5).
///
/// Installed on every session's gate (`permission_gate_for`). An ask from the
/// parent turn — or from anything outside a child — finds no
/// [`current_child`] and pauses nothing. The pause is keyed by the request id,
/// because the gate guarantees one `ask_settled` per `ask_awaiting` for the
/// same id, wherever that settle runs.
#[derive(Debug, Default)]
pub struct ChildAskClock {
    open: Mutex<HashMap<RequestId, DeadlinePause>>,
}

impl AskObserver for ChildAskClock {
    fn ask_awaiting(&self, request_id: &RequestId, _tool_name: &str) {
        let Some(child) = current_child() else {
            return;
        };
        let pause = child.deadline.pause();
        lock(&self.open).insert(request_id.clone(), pause);
    }

    fn ask_settled(&self, request_id: &RequestId) {
        // Taken out under the lock, resumed outside it: the guard's drop takes
        // the deadline's own lock.
        let pause = lock(&self.open).remove(request_id);
        drop(pause);
    }
}

// ---------------------------------------------------------------------------
// The words a child's run is framed with
// ---------------------------------------------------------------------------

/// The section a child's system prompt gains after the session's own (BR-1,
/// BR-11): what it is, the report bound, and the parent's `context` when one
/// was passed.
///
/// The session's system prompt is otherwise the prompt turn's, byte for byte —
/// same tools minus `agent`, same environment block, same repository notes and
/// their provenance. The context rides here rather than as a second user
/// message so the task is the child's only user message (AC-2).
#[must_use]
pub fn child_system_section(context: Option<&str>, report_max_bytes: u64) -> String {
    let mut section = format!(
        "\n\n# You are a child turn\n\n\
         Another turn dispatched you to do the task in the user message. Your context is \
         fresh: nothing from that turn's conversation is visible to you except what it wrote \
         into the task and the context below. When you finish, your final answer is returned \
         to that turn as your report. Keep the report under {report_max_bytes} bytes — a \
         longer one is cut at that bound."
    );
    if let Some(context) = context {
        section.push_str("\n\n## Context from the dispatching turn\n\n");
        section.push_str(context);
    }
    section
}

/// The refusal a child gets when its task and context do not fit its budget
/// (BR-1, AC-14): `over_budget`, then the size, the budget and the bound —
/// never a digest, never an elision.
///
/// The code leads, then a colon, so a reader can key on the token and the
/// model can read the numbers it needs to shorten the task.
#[must_use]
pub fn over_budget_refusal(fit: &Fit, budget: &RouteBudget) -> String {
    format!(
        "{OVER_BUDGET_REASON}: the task and its context, with the system prompt, come to about \
         {} words / {}, and this child's context budget is {} words / {} (bound: {}). Nothing \
         was sent: a child's task is admitted whole or refused, never shortened. Dispatch it \
         again with a shorter task or context.",
        thousands(fit.tokens as u64),
        bytes_figure(fit.bytes as u64),
        thousands(budget.budget_tokens as u64),
        bytes_figure(budget.budget_bytes as u64),
        budget.bound.words(),
    )
}

/// A `turns_exhausted` child's report (BR-10): the last text it wrote before
/// the cap, **marked** so the parent cannot read an unfinished answer as a
/// finished one.
#[must_use]
pub fn turns_exhausted_report(text_so_far: &str, max_turns: u32) -> String {
    let marker = format!(
        "[{TURNS_EXHAUSTED}: this child used all {max_turns} of its turns before it finished; \
         what follows is the last text it wrote, not a final answer]"
    );
    if text_so_far.trim().is_empty() {
        marker
    } else {
        format!("{marker}\n\n{text_so_far}")
    }
}

/// A report cut to its bound (BR-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedReport {
    /// What the parent receives: the whole text, or its first `kept_bytes`
    /// followed by the typed marker.
    pub text: String,
    /// Whether the text was cut.
    pub truncated: bool,
    /// The whole text's size, before any cut.
    pub whole_bytes: u64,
    /// How much of it was kept.
    pub kept_bytes: u64,
}

/// Cut `text` at `max_bytes` — on a character boundary at or below it — and
/// say so (BR-11).
///
/// Under the bound the text is returned untouched and nothing is marked. Over
/// it, the kept prefix is followed by a marker naming the kept and dropped byte
/// counts and the bound; the full text is in the session transcript, where the
/// child's streamed reply was recorded as it was written. This is the one
/// deliberate departure from REQ-589's whole-or-refused rule: a skill body is
/// a procedure that cannot survive elision, a report is a summary the parent
/// asked for, and a bounded summary beats a refused one.
#[must_use]
pub fn bound_report(text: &str, max_bytes: u64) -> BoundedReport {
    let whole = text.len();
    let bound = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if whole <= bound {
        return BoundedReport {
            text: text.to_owned(),
            truncated: false,
            whole_bytes: whole as u64,
            kept_bytes: whole as u64,
        };
    }
    let mut cut = bound;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = whole - cut;
    BoundedReport {
        text: format!(
            "{}\n\n[{REPORT_TRUNCATED}: kept {cut} bytes, dropped {dropped} bytes at \
             agent.report_max_bytes = {max_bytes}; the full report is in the session transcript]",
            &text[..cut]
        ),
        truncated: true,
        whole_bytes: whole as u64,
        kept_bytes: cut as u64,
    }
}

/// A lock recovered from poisoning: every update under these locks is plain
/// arithmetic or a map insert that cannot leave them inconsistent, and a
/// child's clock must not die because an unrelated thread panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::broadcast::EventBus;
    use crate::harness::permissions::{
        PendingPermissions, PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
    };
    use teton_protocol::events::Event;
    use teton_protocol::methods::PermissionOutcome;
    use teton_protocol::SessionId;

    fn scope_for(deadline: &PausableDeadline) -> ChildTaskScope {
        ChildTaskScope {
            child_id: ChildId::new("call-1", "audit"),
            name: "audit".to_owned(),
            parent_turn_id: TurnId::from("turn-1"),
            deadline: deadline.clone(),
            consent: Arc::new(tokio::sync::Mutex::new(())),
            tool_calls: ChildToolCalls::default(),
        }
    }

    /// A session gate the daemon's way — level `ask` on `shell` — with the
    /// child clock installed exactly as `permission_gate_for` installs it.
    fn asking_gate() -> (Arc<EventBus>, Arc<PendingPermissions>, Arc<PermissionGate>) {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let mut config = PermissionConfig::with_default(PermissionPolicy::Allow);
        config.set("shell", PermissionPolicy::Ask);
        let gate = PermissionGate::new(
            SessionId::from("s-child"),
            config,
            Arc::clone(&bus),
            Arc::clone(&pending),
        )
        .with_ask_observer(Arc::new(ChildAskClock::default()));
        (bus, pending, Arc::new(gate))
    }

    /// **BR-5 / ADR-2: the work clock stops while a child's ask waits on a
    /// human, and only then.**
    ///
    /// A ten-second deadline. The child asks for `shell` and nobody answers for
    /// ninety seconds: the deadline has not expired, and its work time is the
    /// two seconds the child worked before asking. After the grant, eight more
    /// seconds of work expire it — not one second sooner.
    ///
    /// Benign path: the same ten seconds with no consent wait expires at ten
    /// seconds of wall clock, so the deadline is a bound and not a clock that
    /// never runs; and an ask that the *level* answers (`read`, allowed) pauses
    /// nothing, because nobody was asked.
    ///
    /// The clock is paused, so every interval is exact.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted)
    ///
    /// - **Drop the pause** (`ChildAskClock::ask_awaiting` returns without
    ///   pausing): 1 red, this test — the deadline expires while the ask is
    ///   pending ("ninety unanswered seconds did not spend the deadline").
    /// - **Never resume** (`DeadlinePause::drop` empty): 2 reds, this test ("the
    ///   clock runs again after the grant") and
    ///   `pauses_nest_and_move_the_wake_time`.
    ///
    /// A pause *outside* a child has no deadline to stop, so it is not a
    /// mutation this module can express; the gate's own
    /// `ask_await_hook_brackets_the_pending_interval` pins that a decision
    /// which asked nobody fires no hook at all.
    #[tokio::test(start_paused = true)]
    async fn deadline_pauses_during_consent() {
        let (bus, pending, gate) = asking_gate();
        let mut prompts = bus.subscribe(16);
        let deadline = PausableDeadline::start(Duration::from_secs(10));

        let child = tokio::spawn(scope_for(&deadline).scope({
            let gate = Arc::clone(&gate);
            async move {
                // Work, then a decision the level makes (no human), then an ask.
                tokio::time::sleep(Duration::from_secs(2)).await;
                assert_eq!(
                    gate.authorize("read", None).await,
                    PermissionDecision::Allowed
                );
                gate.authorize("shell", None).await
            }
        }));

        // TASK-428: the gate labels a child's ask with
        // `agent_child_consent_requested` just before the question itself.
        let request_id = loop {
            match tokio::time::timeout(Duration::from_secs(5), prompts.recv())
                .await
                .expect("the child's ask is published")
                .expect("a prompt arrives")
                .event
            {
                Event::PermissionRequest(request) => break request.request_id,
                Event::AgentChildConsentRequested(_) => {}
                other => panic!("expected permission_request, got {other:?}"),
            }
        };
        assert!(
            deadline.is_paused(),
            "the ask is out: the clock has stopped"
        );
        assert_eq!(
            deadline.worked(),
            Duration::from_secs(2),
            "the two seconds the child worked before it asked are counted"
        );

        // Ninety seconds nobody answers.
        let expired = tokio::time::timeout(Duration::from_secs(90), deadline.expired()).await;
        assert!(
            expired.is_err(),
            "ninety unanswered seconds did not spend the deadline"
        );
        assert_eq!(deadline.worked(), Duration::from_secs(2));

        assert!(pending.resolve(
            &request_id,
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned()
            }
        ));
        assert_eq!(child.await.unwrap(), PermissionDecision::Allowed);
        assert!(
            !deadline.is_paused(),
            "the clock runs again after the grant"
        );

        let resumed_at = Instant::now();
        deadline.expired().await;
        assert_eq!(
            resumed_at.elapsed(),
            Duration::from_secs(8),
            "the remaining eight seconds of work expire it, and not one sooner"
        );
    }

    /// The benign twin: no consent wait, and the deadline is an ordinary
    /// deadline — it expires at its budget of wall clock.
    #[tokio::test(start_paused = true)]
    async fn with_no_consent_wait_the_clock_runs() {
        let started = Instant::now();
        let deadline = PausableDeadline::start(Duration::from_secs(10));
        deadline.expired().await;
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(deadline.worked(), Duration::from_secs(10));
    }

    /// Pauses nest: the gate's wait and the consent-mutex queue (TASK-428) can
    /// overlap, and the clock stays stopped until both end. A pause opened and
    /// closed while the deadline is being awaited moves its wake time out.
    #[tokio::test(start_paused = true)]
    async fn pauses_nest_and_move_the_wake_time() {
        let deadline = PausableDeadline::start(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(4)).await;
        let queued = deadline.pause();
        let asking = deadline.pause();
        tokio::time::sleep(Duration::from_secs(30)).await;
        queued.resume();
        assert!(deadline.is_paused(), "one pause still outstanding");
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(asking);
        assert!(!deadline.is_paused());

        let resumed_at = Instant::now();
        let waiter = tokio::spawn({
            let deadline = deadline.clone();
            async move { deadline.expired().await }
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        let held = deadline.pause();
        tokio::time::sleep(Duration::from_secs(100)).await;
        assert!(!waiter.is_finished(), "paused mid-wait: no expiry");
        drop(held);
        waiter.await.unwrap();
        assert_eq!(
            resumed_at.elapsed(),
            Duration::from_secs(106),
            "6 s of work remained; 100 s of it was paused"
        );
    }

    /// **BR-11 / AC-15: over the bound, the cut is loud.** A report of
    /// `report_max_bytes + 1` comes back as exactly the first `report_max_bytes`
    /// bytes followed by the typed marker naming what was kept and dropped, and
    /// the outcome says truncated.
    ///
    /// Benign path: a report at exactly the bound, and one under it, come back
    /// byte-identical with no marker.
    ///
    /// The bound and the expected counts are literals, never read back from the
    /// subject.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted — 1 red apiece, this test)
    ///
    /// - **Cut silently** (drop the marker from the `format!`): reddens at "the
    ///   marker names kept and dropped".
    /// - **Cut off a character boundary** (remove the `is_char_boundary` walk):
    ///   panics on the multi-byte leg — byte 5 is not a char boundary.
    /// - **Compare with `<`** (truncate at exactly the bound): reddens on the
    ///   benign "exactly the bound" leg.
    #[test]
    fn report_cut_is_loud() {
        const BOUND: u64 = 32_768;
        let over = "r".repeat(32_769);
        let cut = bound_report(&over, BOUND);
        assert!(cut.truncated);
        assert_eq!(cut.whole_bytes, 32_769);
        assert_eq!(cut.kept_bytes, 32_768);
        let (kept, marker) = cut.text.split_at(32_768);
        assert_eq!(
            kept,
            "r".repeat(32_768),
            "exactly report_max_bytes of the text"
        );
        assert_eq!(
            marker,
            "\n\n[report_truncated: kept 32768 bytes, dropped 1 bytes at \
             agent.report_max_bytes = 32768; the full report is in the session transcript]",
            "the marker names kept and dropped"
        );

        // A multi-byte character straddling the bound is kept whole or not at
        // all — never split.
        let straddle = format!("abcd{}", "é".repeat(4));
        let cut = bound_report(&straddle, 5);
        assert_eq!(cut.kept_bytes, 4, "`é` at bytes 4..6 does not fit under 5");
        assert!(cut
            .text
            .starts_with("abcd\n\n[report_truncated: kept 4 bytes, dropped 8 bytes"));

        // Benign: at and under the bound, untouched.
        for text in ["r".repeat(32_768), "a short report".to_owned()] {
            let kept = bound_report(&text, BOUND);
            assert!(!kept.truncated, "{} bytes is within the bound", text.len());
            assert_eq!(kept.text, text, "byte-identical, no marker");
            assert_eq!(kept.whole_bytes, kept.kept_bytes);
        }
    }

    /// The task-local answers inside a child's scope and nowhere else — the
    /// premise [`ChildAskClock`] stands on.
    #[tokio::test]
    async fn current_child_is_the_scope_and_nothing_outside_it() {
        assert!(current_child().is_none(), "outside any child");
        let deadline = PausableDeadline::start(Duration::from_secs(1));
        let seen = scope_for(&deadline)
            .scope(async { current_child().map(|c| c.child_id) })
            .await;
        assert_eq!(seen, Some(ChildId::new("call-1", "audit")));
        assert!(current_child().is_none(), "the scope ended with the run");
    }

    /// The child's system section states the report bound and carries the
    /// context verbatim; with no context it adds no context header.
    #[test]
    fn the_child_section_states_the_bound_and_carries_the_context() {
        let with = child_system_section(Some("CONTEXT-BYTES"), 4_096);
        assert!(with.contains("under 4096 bytes"), "{with}");
        assert!(with.ends_with("## Context from the dispatching turn\n\nCONTEXT-BYTES"));
        let without = child_system_section(None, 4_096);
        assert!(!without.contains("Context from the dispatching turn"));
    }

    /// **BR-10's "a project-skill gate": a call refused inside its own gate
    /// counts as denied, and its dispatch does not count as a call that ran.**
    ///
    /// The loop notes every dispatch, including `skill`'s, whose project-skill
    /// acknowledgment is asked inside `run`; the tool notes the refusal. A
    /// child whose only call was refused that way has nothing that ran — and a
    /// second call that genuinely ran makes the ending its own again.
    ///
    /// Mutation (run 2026-10-07, reverted): `gate_refusal` reading `ran`
    /// without subtracting `refused_inside` reddens this test at its first
    /// assertion and `agent_dispatch::statuses::refused` at its status.
    #[test]
    fn a_call_refused_inside_its_own_gate_is_denied_not_ran() {
        let calls = ChildToolCalls::default();
        calls.note_refused_inside("skill");
        calls.note_ran(); // the loop's count of the same dispatch
        assert_eq!(calls.gate_refusal().as_deref(), Some("gate_denied:skill"));
        calls.note_ran(); // a later call that did run
        assert_eq!(calls.gate_refusal(), None);
    }
}
