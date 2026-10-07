//! REQ-623: the child turn runner — [`ChildDispatcher`] for the daemon.
//!
//! A child needs three of a prompt turn's eight stages — **route → assemble →
//! attempt** — and skips the other five: no claim (it runs under the parent's),
//! no naming duty, no skill settle (its task is not a skill), no seed from the
//! session's conversation (BR-1: a fresh context), and no commit (its context
//! is dropped; only its ledger rows, events and transcript records persist).
//! The three it runs are the prompt turn's own, reached through
//! `dispatch_route` (with the child's tier request), `assemble_child_harness`
//! and `run_child_attempts` in `turn.rs` — so a reroute, a privacy block, a
//! spend ceiling and a window refusal are handled by exactly the code that
//! handles them for the parent.
//!
//! What this module adds around them is what only a child has: the bounds
//! stamped before the first call and echoed (BR-7), the whole-or-refused check
//! on the task (BR-1), the work clock (BR-5), the eight-way status (BR-10), the
//! report bound (BR-11), the provenance union (BR-9, ADR-8), and the spend
//! share's release (BR-8).
//!
//! # The run is its own task
//!
//! [`ChildTurns::run_child`] spawns the child's work and races it against the
//! [`PausableDeadline`]. Racing in the same task would not do: a tool runs
//! synchronously inside the loop's poll (`block_in_place`, BUG-226's fix), and
//! a deadline polled by that same task cannot fire until the tool returns. In
//! its own task the work can be aborted the moment the clock runs out, and the
//! parent gets `timed_out` while a blocking tool is still in flight — the
//! abort then lands at the run's next await, exactly as a cancelled parent's
//! does (BR-10). The work task is aborted whenever the runner is dropped, so
//! `JoinSet::abort_all` on the parent's side reaches it too.
//!
//! ASSUME-010: the test module stays last.

use super::turn::{AssembledHarness, AttemptInputs, AttemptState, ParentTurn};
use super::*;

use std::sync::atomic::AtomicU32;
use std::sync::PoisonError;

use async_trait::async_trait;

use teton_protocol::agent::{ChildBounds, ChildResult, ChildRoute, ChildStatus};
use teton_protocol::events::AgentChildStarted;
use teton_protocol::methods::StopReason;
use teton_protocol::TurnId;

use crate::cost::ChildSpend;
use crate::harness::child::{
    bound_report, child_system_section, over_budget_refusal, turns_exhausted_report,
    ChildDispatcher, ChildOutcome, ChildOutcomeSlot, ChildRouteCell, ChildSpec, ChildTaskScope,
    ChildToolCalls, ChildTurn, PausableDeadline, CHILD_PANICKED,
};
use crate::harness::context::BlockRole;
use crate::harness::reply::prose_before_tool_call;

/// How long a deadline-aborted run is given to stop at its next await before
/// the runner reports `timed_out` without it.
///
/// An abort lands at the run's next await: a model stream being read stops at
/// once, and its response body is dropped — and billed — before the share is
/// released. A tool blocking its thread does not stop until it returns, and
/// the parent is not made to wait for it.
const ABORT_GRACE: Duration = Duration::from_secs(1);

/// Every child of one prompt turn runs under these facts — the per-turn half
/// of what a child needs, built once per prompt turn by
/// [`DaemonRuntime::child_turns`]; the per-child half arrives on each
/// [`ChildSpec`].
#[derive(Clone)]
pub(super) struct ChildTurns {
    runtime: Arc<DaemonRuntime>,
    events: Arc<EventBus>,
    /// Where the child re-reads the session root and the skills snapshot,
    /// under the parent's held claim (LESSON-539) — not a pre-claim copy.
    sessions: SessionRegistry,
    session_id: SessionId,
    /// The parent turn's one config snapshot: a child routes, assembles and
    /// spends against the config its parent's turn was built from.
    config: Arc<Config>,
    /// The session's gate — the parent's and every sibling's (ADR-5).
    gate: Arc<PermissionGate>,
    invoker: Option<ConnectionId>,
    parent_turn_id: TurnId,
    mode: SessionMode,
    phase: Option<ProtoPhase>,
}

impl DaemonRuntime {
    /// The dispatcher an `agent` call in this prompt turn runs its children
    /// through (REQ-623 ADR-2) — what `register_agent_tool` is handed in
    /// `build_tools`' [`super::turn::ToolSet::Prompt`] arm.
    pub(super) fn child_turns(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        parent: ParentTurn<'_>,
    ) -> Arc<dyn ChildDispatcher> {
        Arc::new(ChildTurns {
            runtime: Arc::clone(self),
            events: Arc::clone(tctx.core.events),
            sessions: parent.sessions.clone(),
            session_id: tctx.core.session_id.clone(),
            config: Arc::new(tctx.core.config.clone()),
            gate: Arc::clone(tctx.gate),
            invoker: tctx.invoker,
            parent_turn_id: parent.turn_id.clone(),
            mode: parent.mode,
            phase: parent.phase,
        })
    }
}

#[async_trait]
impl ChildDispatcher for ChildTurns {
    async fn run_child(&self, spec: ChildSpec) -> ChildOutcome {
        let deadline = PausableDeadline::start(spec.deadline);
        // BR-8 / AC-12: every remote call this child makes — its turn's and its
        // duties' — is billed under both ids at the choke point.
        let spend = spec.spend.clone().under_turn(self.parent_turn_id.clone());
        let progress = Arc::new(Mutex::new(Progress::default()));
        let model_calls = Arc::new(AtomicU32::new(0));
        let route = ChildRouteCell::default();

        // Declared before the work handle, so it drops after it: an aborted
        // runner stops the work first, then leaves its outcome.
        let mut cancelled = CancelGuard {
            armed: true,
            name: spec.name.clone(),
            slot: spec.cancelled.clone(),
            spend: spend.clone(),
            progress: Arc::clone(&progress),
            model_calls: Arc::clone(&model_calls),
            route: route.clone(),
            seed_provenance: spec.provenance.clone(),
        };
        let tool_calls = ChildToolCalls::default();
        let scope = ChildTaskScope {
            child_id: spec.child_id.clone(),
            name: spec.name.clone(),
            parent_turn_id: self.parent_turn_id.clone(),
            deadline: deadline.clone(),
            consent: Arc::clone(&spec.consent),
            tool_calls: tool_calls.clone(),
        };
        let work = Work {
            turns: self.clone(),
            spec: spec.clone(),
            spend: spend.clone(),
            progress: Arc::clone(&progress),
            model_calls: Arc::clone(&model_calls),
            route: route.clone(),
            tool_calls,
        };
        let mut handle = AbortOnDrop(tokio::spawn(scope.scope(work.run())));

        let ended = tokio::select! {
            // A run that finished at the same instant the clock ran out
            // finished: its answer is the truer one.
            biased;
            joined = &mut handle.0 => match joined {
                Ok(ended) => ended,
                // BR-10: a child's failure never fails the parent — not even a
                // panic inside its run.
                Err(err) if err.is_panic() => Ended::failed(CHILD_PANICKED.to_owned()),
                Err(_) => Ended::of(ChildStatus::Cancelled),
            },
            () = deadline.expired() => {
                handle.0.abort();
                let _ = tokio::time::timeout(ABORT_GRACE, &mut handle.0).await;
                Ended::of(ChildStatus::TimedOut)
            }
        };
        cancelled.armed = false;
        finish(Finish {
            name: spec.name.clone(),
            report_max_bytes: spec.report_max_bytes,
            spend: &spend,
            progress: &progress,
            model_calls: &model_calls,
            route: &route,
            seed_provenance: &spec.provenance,
            ended,
        })
    }
}

/// What the runner knows once the work has stamped it — read by the runner
/// after a `timed_out` and by the drop guard after a cancel, when the work
/// itself can no longer say.
///
/// The route is not here: it moves after stamping (a reroute), and the attempt
/// loop keeps it current in the [`ChildRouteCell`] it shares with the runner
/// (BR-6). The bounds never move (BR-7).
#[derive(Default)]
struct Progress {
    bounds: Option<ChildBounds>,
}

/// How the work ended, before the runner frames it.
struct Ended {
    status: ChildStatus,
    /// The report's whole text — `completed`'s final answer, or
    /// `turns_exhausted`'s marked last text. Empty for every other status.
    text: String,
    refusal: Option<String>,
    error: Option<String>,
    /// The child's context provenance when the work could read it; `None` for
    /// a run that never returned, whose report is empty (BR-9 — see
    /// [`ChildOutcome::provenance`]).
    provenance: Option<Provenance>,
}

impl Ended {
    fn of(status: ChildStatus) -> Self {
        Self {
            status,
            text: String::new(),
            refusal: None,
            error: None,
            provenance: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::of(ChildStatus::Failed)
        }
    }
}

/// One child's work, owned, so it can run as its own task.
struct Work {
    turns: ChildTurns,
    spec: ChildSpec,
    spend: ChildSpend,
    progress: Arc<Mutex<Progress>>,
    model_calls: Arc<AtomicU32>,
    /// The child's current route — stamped here, kept current by the attempt
    /// loop (BR-6).
    route: ChildRouteCell,
    /// What the child's calls came to at the gate — the same log its scope
    /// carries, read when it ends (BR-10's "refused by a gate").
    tool_calls: ChildToolCalls,
}

impl Work {
    async fn run(self) -> Ended {
        let Work {
            turns,
            spec,
            spend,
            progress,
            model_calls,
            route: route_cell,
            tool_calls,
        } = self;
        let runtime = &turns.runtime;
        let session_id = &turns.session_id;

        // LESSON-539: the root is re-read from the registry now, under the
        // parent's held claim — no `/cd` can land while it is held — rather
        // than taken from any snapshot the parent was handed before its claim.
        let cwd = turns
            .sessions
            .get(session_id)
            .and_then(|summary| summary.cwd);
        let probed = runtime.session_root_for(cwd.as_deref());
        let skills = turns.sessions.skills(session_id);

        // Route: the session's pin first, then the child's tier request — the
        // order is `dispatch_route`'s shape (BR-6). No warming hold: a child
        // runs under a parent that is already being served, and a route with
        // nowhere to run ends the child `failed` with the code.
        let router = runtime.turn_router(&turns.config, session_id);
        let mut route = runtime
            .dispatch_route(
                &router,
                session_id,
                turns.mode,
                turns.phase.map(to_core_phase),
                &spec.task,
                spec.tier.map(to_core_tier),
            )
            .await;

        // BR-7: the turn cap — the config's, never past the parent's, never past
        // what the child's own route profile allows.
        let max_turns = spec
            .max_turns
            .min(spec.parent_max_turns)
            .min(route.harness.max_turns)
            .max(1);
        route.harness.max_turns = max_turns;
        let child = ChildTurn {
            child_id: spec.child_id.clone(),
            parent_turn_id: turns.parent_turn_id.clone(),
            max_turns,
            // BR-7: the budget the bounds below stamp, before the assemble
            // stage or any reroute can touch the route.
            budget: route.budget.clone(),
            spend: spend.clone(),
            model_calls,
            route: route_cell.clone(),
        };
        let tctx = TurnContext::new(
            &turns.events,
            session_id,
            &turns.config,
            &router,
            &turns.gate,
            turns.invoker,
        )
        .for_child(&child);

        let AssembledHarness {
            tools,
            tool_ctx,
            stream_events,
            system,
            repo_context,
        } = runtime
            .assemble_child_harness(tctx, &turns.sessions, &skills, &probed, &mut route)
            .await;
        // BR-1 / BR-11: the session's system prompt, then what this child is —
        // its report bound and the parent's context. Inserted *ahead of* the
        // repository notes, so a reroute that re-renders the notes at a new cap
        // (REQ-612) rebuilds the prompt with the child's section still in it.
        let section = child_system_section(spec.context.as_deref(), spec.report_max_bytes);
        let (system, repo_context) = match (repo_context, route.harness.repo_context.as_ref()) {
            (Some(mut carry), Some(block)) => {
                carry.base_system.push_str(&section);
                (append_repo_context(&carry.base_system, block), Some(carry))
            }
            (carry, _) => (format!("{system}{section}"), carry),
        };

        // BR-7: the four bounds, stamped before anything is sent and never
        // re-derived — `agent_child_started` publishes this value and the
        // result echoes it.
        let child_route = child_route_of(&route, runtime.engine.model());
        let bounds = child_route.as_ref().map(|_| ChildBounds {
            max_turns,
            context_budget_bytes: route.budget.budget_bytes as u64,
            // BR-8: the initial share, not the ceiling now — a sibling may
            // already have released into it (see `SharePool::stamped_share_of`).
            spend_ceiling_micro_cents: spend.stamped_share(),
            deadline_secs: spec.deadline.as_secs(),
        });
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bounds = bounds;
        if let Some(route) = &child_route {
            route_cell.set(route.clone());
        }

        // BR-1: the task and its context are admitted whole, or the child is
        // refused naming size, budget and bound. Measured with the estimator the
        // pressure gate itself runs on, over the very system prompt the child
        // would be sent — never digested, never elided.
        let fit = ContextManager::would_seed_fit(
            &system,
            &spec.task,
            route.budget.budget_tokens,
            route.budget.budget_bytes,
        );
        if !fit.fits {
            return Ended {
                refusal: Some(over_budget_refusal(&fit, &route.budget)),
                ..Ended::of(ChildStatus::Refused)
            };
        }

        // The first model call is next: say so, with the route and the bounds
        // it will run under (spec Events: `agent_child_started`).
        if let (Some(route), Some(bounds)) = (child_route, bounds) {
            turns.events.publish(
                Some(session_id.clone()),
                Event::AgentChildStarted(AgentChildStarted {
                    child_id: spec.child_id.clone(),
                    parent_turn_id: turns.parent_turn_id.clone(),
                    name: spec.name.clone(),
                    route,
                    bounds,
                }),
            );
        }

        // BR-1: a fresh context — the system prompt and the task, nothing
        // replayed, nothing committed. The task carries the provenance of the
        // parent context it was written from.
        let conversation = CarriedTurn::detached(
            &turns.sessions,
            session_id,
            system.clone(),
            &route.harness,
            spec.task.clone(),
            &spec.provenance,
            repo_context,
        );
        let mut st = AttemptState {
            attempts: 0,
            rerouted_local: false,
            withdrew_accepted_expansion: false,
            accepted: None,
            skill_refit: Vec::new(),
            conversation,
            route,
            finished: None,
        };
        let result = runtime
            .run_child_attempts(
                tctx,
                AttemptInputs {
                    turn_id: &turns.parent_turn_id,
                    phase: turns.phase,
                    tools: &tools,
                    tool_ctx: &tool_ctx,
                    stream_events: &stream_events,
                    refit_system: &system,
                    typed_refit: 0,
                    // The child's spend is its `ChildSpend`, on `tctx.child`;
                    // the prompt's accumulator is reached through it.
                    prompt_spend: None,
                    sessions: &turns.sessions,
                },
                &mut st,
            )
            .await;
        let provenance = context_provenance(st.conversation.ctx());
        let ended = match result {
            Ok(_) => match st.finished {
                Some(outcome) => match outcome.stop_reason {
                    // BR-10: the last text the child wrote, marked as such.
                    StopReason::MaxTurnRequests => Ended {
                        text: turns_exhausted_report(&last_prose(st.conversation.ctx()), max_turns),
                        ..Ended::of(ChildStatus::TurnsExhausted)
                    },
                    StopReason::Cancelled => Ended::of(ChildStatus::Cancelled),
                    _ => finished_on_its_own(outcome.final_text, &tool_calls),
                },
                // The success arm always leaves its outcome; an `Ok` without one
                // is a broken invariant, reported rather than unwrapped.
                None => Ended::failed(format!(
                    "{}: the child's attempt ended without an outcome",
                    error_code::INTERNAL_ERROR
                )),
            },
            Err(err) => ended_by(&err),
        };
        Ended {
            provenance: Some(provenance),
            ..ended
        }
    }
}

/// A child whose loop ended with its final text: `completed` — or `refused`,
/// when the text is empty and the gate is why (BR-10's "refused by a gate").
///
/// The rule (TASK-428): a child that has **nothing to report** and whose every
/// attempted tool call the gate denied, none having run, ends `refused` with
/// `gate_denied:<tool>` — an unattended deny of a tool the task needed (BR-5),
/// or a level that refuses it, named so the parent can tell "the child found
/// nothing" from "the child was not allowed to look". Any other denial is a
/// typed tool failure the child read and answered on its own (BR-5): a report
/// saying why, or another tool that ran, makes the ending its own.
fn finished_on_its_own(text: String, tool_calls: &ChildToolCalls) -> Ended {
    match tool_calls.gate_refusal() {
        Some(refusal) if text.trim().is_empty() => Ended {
            refusal: Some(refusal),
            ..Ended::of(ChildStatus::Refused)
        },
        _ => Ended {
            text,
            ..Ended::of(ChildStatus::Completed)
        },
    }
}

/// The status an attempt loop's typed error ends a child in (BR-10).
///
/// Read off the error's **code**, which the loop's arms assign one per typed
/// outcome — the same discrimination a client makes of a prompt turn's error.
fn ended_by(err: &RpcError) -> Ended {
    match err.code {
        // REQ-588's typed outcome, raised by the child's own choke point
        // against its share (ADR-4).
        error_code::SPEND_CEILING_REACHED => Ended::of(ChildStatus::SpendExhausted),
        // The context could not be fitted mid-run: a window refusal, anchors
        // that no longer fit after a reroute, or a model-invoked skill that a
        // reroute's refit could not keep whole.
        error_code::CONTEXT_LENGTH_EXCEEDED
        | error_code::TURN_ANCHORS_EXCEED_BUDGET
        | error_code::SKILL_EXPANSION_TOO_LARGE => Ended::of(ChildStatus::BudgetExhausted),
        // A provider or engine error after the loop's own retry and reroute
        // path, a privacy block with nowhere local to go, a credential that
        // will not resolve, no tier to run on: `failed`, with the code.
        code => Ended::failed(format!("{code}: {}", err.message)),
    }
}

/// The child's route as the parent reads it (BR-6): what the router chose,
/// after the pin — `route_decided`'s projection, never a re-derivation. A
/// local-tier route that names no model takes the loaded engine's.
///
/// `pub(super)` for the attempt loop, which re-projects it at the top of every
/// attempt so a reroute is reported (see [`ChildTurn::route`]). That is the
/// whole of "the route it ended on": the cell holds the route of the last
/// attempt that began. A reroute arm that replaces the route and then ends the
/// run without another attempt (a refit refusal) leaves the route the child was
/// last served by, which is where it ran.
pub(super) fn child_route_of(
    route: &crate::router::Route,
    engine_model: Option<String>,
) -> Option<ChildRoute> {
    let decided = route.route_decided()?;
    Some(ChildRoute {
        tier: decided.tier,
        provider_id: decided.provider_id,
        model: decided.model.or(engine_model)?,
    })
}

/// The last text the child wrote, without the tool call it ended on — what a
/// `turns_exhausted` child had produced when the cap stopped it.
fn last_prose(ctx: &ContextManager) -> String {
    ctx.blocks()
        .iter()
        .rev()
        .find(|block| block.role == BlockRole::Assistant)
        .map(|block| {
            prose_before_tool_call(&block.text)
                .unwrap_or(&block.text)
                .trim()
                .to_owned()
        })
        .unwrap_or_default()
}

/// What [`finish`] frames an outcome from.
struct Finish<'a> {
    name: String,
    report_max_bytes: u64,
    spend: &'a ChildSpend,
    progress: &'a Mutex<Progress>,
    model_calls: &'a AtomicU32,
    route: &'a ChildRouteCell,
    seed_provenance: &'a Provenance,
    ended: Ended,
}

/// Frame a terminal outcome: bound the report, release the share, echo the
/// stamped bounds, and carry the provenance up (BR-7 to BR-11).
fn finish(finish: Finish<'_>) -> ChildOutcome {
    let Finish {
        name,
        report_max_bytes,
        spend,
        progress,
        model_calls,
        route,
        seed_provenance,
        ended,
    } = finish;
    let report = bound_report(&ended.text, report_max_bytes);
    let bounds = progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .bounds;
    // BR-8: released once, after its last response body has been dropped and
    // billed; the amount is the one the pool divided, under its lock.
    let share_released = spend.release();
    ChildOutcome {
        result: ChildResult {
            name,
            status: ended.status,
            report: report.text,
            refusal: ended.refusal,
            error: ended.error,
            turns_used: model_calls.load(std::sync::atomic::Ordering::Relaxed),
            // BR-6: where the child ended up — after any pin or fallback — not
            // where it started.
            route: route.get(),
            // BR-7: the stamped value, echoed — not re-derived from a route a
            // reroute may since have replaced.
            bounds,
            cost_micro_cents: spend.spent(),
            spend_ceiling_final_micro_cents: spend.ceiling(),
        },
        provenance: ended.provenance.unwrap_or_else(|| seed_provenance.clone()),
        report_bytes: report.whole_bytes,
        truncated: report.truncated,
        share_released,
    }
}

/// Aborts the work task when the runner is dropped — the parent turn
/// cancelled, its `JoinSet` aborted — so the cancel reaches the child's run
/// the way it reaches the parent's (BR-10).
struct AbortOnDrop(tokio::task::JoinHandle<Ended>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Leaves a `cancelled` outcome in the spec's slot when the runner is dropped
/// before it can return one (BR-10, AC-14) — an abort reports, it never
/// panics.
struct CancelGuard {
    armed: bool,
    name: String,
    slot: ChildOutcomeSlot,
    spend: ChildSpend,
    progress: Arc<Mutex<Progress>>,
    model_calls: Arc<AtomicU32>,
    route: ChildRouteCell,
    seed_provenance: Provenance,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let outcome = finish(Finish {
            name: std::mem::take(&mut self.name),
            report_max_bytes: 0,
            spend: &self.spend,
            progress: &self.progress,
            model_calls: &self.model_calls,
            route: &self.route,
            seed_provenance: &self.seed_provenance,
            ended: Ended::of(ChildStatus::Cancelled),
        });
        self.slot.put(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::scratch_root;
    use super::*;

    use std::collections::VecDeque;
    use std::sync::mpsc;

    use teton_inference::{Completion, EngineError, GenParams};
    use teton_protocol::agent::ChildId;
    use teton_protocol::events::SessionUpdate;

    use crate::cost::SharePool;
    use crate::harness::child::PARENT_ONLY_EVENTS;
    use crate::harness::{ChildAskClock, PermissionConfig};

    // ---- the fixture ------------------------------------------------------

    /// What the scripted engine answers an agent-turn call with.
    enum Reply {
        /// Stream this text and end.
        Say(String),
        /// Fail as the backend, with no window involved.
        Fail,
        /// Refuse the prompt as too big for the window — the typed local
        /// refusal the loop reports as `CONTEXT_LENGTH_EXCEEDED`.
        OverWindow,
        /// Hold the call until the sender is dropped or sends — a model call in
        /// flight for as long as the test needs one.
        Park(mpsc::Receiver<()>),
    }

    /// A local engine that answers **agent turns** from a script, records every
    /// agent-turn prompt it is sent, and speaks ChatML so a recorded prompt can
    /// be read back as the role-typed request it was.
    ///
    /// Duties (the session title, mostly) reach [`Engine::complete`], never
    /// [`Engine::complete_cached`], so they answer a fixed word and never
    /// consume the script — a title duty racing a child cannot steal its reply.
    #[derive(Clone, Default)]
    struct ScriptEngine {
        script: Arc<Mutex<VecDeque<Reply>>>,
        prompts: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptEngine {
        fn then(&self, reply: Reply) {
            self.script.lock().unwrap().push_back(reply);
        }

        fn say(&self, text: &str) {
            self.then(Reply::Say(text.to_owned()));
        }

        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    impl Engine for ScriptEngine {
        fn model_id(&self) -> &str {
            "script"
        }

        fn complete(
            &self,
            _prompt: &str,
            _params: &GenParams,
            _on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            Ok(Completion::cold("edit".to_owned(), 1, 1))
        }

        fn complete_cached(
            &mut self,
            _session: &str,
            prompt: &str,
            _params: &GenParams,
            on_token: &mut dyn FnMut(&str) -> bool,
        ) -> Result<Completion, EngineError> {
            self.prompts.lock().unwrap().push(prompt.to_owned());
            let reply = self.script.lock().unwrap().pop_front();
            match reply.unwrap_or_else(|| Reply::Say("done".to_owned())) {
                Reply::Say(text) => {
                    on_token(&text);
                    Ok(Completion::cold(text, 10, 5))
                }
                Reply::Fail => Err(EngineError::Backend("scripted failure".to_owned())),
                Reply::OverWindow => Err(EngineError::ContextWindowExceeded {
                    prompt_tokens: 40_000,
                    budget_tokens: 30_000,
                    n_ctx: 32_768,
                    max_tokens: 2_768,
                }),
                Reply::Park(release) => {
                    let _ = release.recv_timeout(Duration::from_secs(30));
                    Ok(Completion::cold("late".to_owned(), 1, 1))
                }
            }
        }

        fn chat_format(&self) -> ChatFormat {
            ChatFormat::ChatMl
        }
    }

    /// A tool call in the reply grammar the loop parses.
    fn call(tool: &str, arguments: serde_json::Value) -> String {
        serde_json::json!({ "tool": tool, "arguments": arguments }).to_string()
    }

    /// A ChatML prompt as `(role, body)` pairs, the trailing generation cue
    /// left out.
    fn chatml(prompt: &str) -> Vec<(String, String)> {
        prompt
            .split("<|im_start|>")
            .filter_map(|chunk| {
                let (role, rest) = chunk.split_once('\n')?;
                let body = rest.strip_suffix("<|im_end|>\n")?;
                Some((role.to_owned(), body.to_owned()))
            })
            .collect()
    }

    /// One daemon, one structured session rooted in a scratch directory, a
    /// scripted local tier, and a session gate that allows every tool — these
    /// fixtures are about the child's turn, not its consent (BR-5's clock has
    /// its own test in `harness::child`).
    struct Rig {
        runtime: Arc<DaemonRuntime>,
        engine: ScriptEngine,
        events: Arc<EventBus>,
        sessions: SessionRegistry,
        session_id: SessionId,
        root: PathBuf,
        config: Config,
    }

    impl Rig {
        fn new(tag: &str, project: bool) -> Self {
            Self::with_config(tag, project, Config::default())
        }

        fn with_config(tag: &str, project: bool, config: Config) -> Self {
            let engine = ScriptEngine::default();
            let slot = EngineSlot::empty();
            slot.install(
                "script".to_owned(),
                Arc::new(Mutex::new(engine.clone())) as Arc<Mutex<dyn Engine>>,
            );
            let runtime = Arc::new(DaemonRuntime {
                engine: slot,
                local_available: AtomicBool::new(true),
                config: Mutex::new(config.clone()),
                ..DaemonRuntime::minimal()
            });
            let root = scratch_root(tag, project)
                .canonicalize()
                .expect("the scratch root resolves");
            let sessions = SessionRegistry::new();
            let session_id = sessions
                .create(
                    SessionMode::Structured,
                    Some(ProtoPhase::Implement),
                    Some(root.clone()),
                )
                .expect("a structured session")
                .session_id;
            let events = Arc::new(EventBus::new());
            runtime
                .session_gates
                .lock()
                .expect("session gate mutex")
                .insert(
                    session_id.clone(),
                    Arc::new(
                        PermissionGate::new(
                            session_id.clone(),
                            PermissionConfig::permissive(),
                            Arc::clone(&events),
                            Arc::clone(&runtime.pending),
                        )
                        .with_ask_observer(Arc::new(ChildAskClock::default())),
                    ),
                );
            Self {
                runtime,
                engine,
                events,
                sessions,
                session_id,
                root,
                config,
            }
        }

        /// The dispatcher a prompt turn's `agent` tool would be handed — built
        /// by the same [`DaemonRuntime::child_turns`] `build_tools` will call.
        fn dispatcher(&self) -> Arc<dyn ChildDispatcher> {
            let router = self.runtime.turn_router(&self.config, &self.session_id);
            let gate =
                self.runtime
                    .permission_gate_for(&self.session_id, &self.events, &self.config);
            self.runtime.child_turns(
                TurnContext::new(
                    &self.events,
                    &self.session_id,
                    &self.config,
                    &router,
                    &gate,
                    None,
                ),
                ParentTurn {
                    turn_id: &TurnId::from("turn-parent"),
                    sessions: &self.sessions,
                    mode: SessionMode::Structured,
                    phase: Some(ProtoPhase::Implement),
                    typed: true,
                    prompt_spend: None,
                },
            )
        }

        /// Replace the session's gate with one that allows every tool but
        /// `tool`, which the level denies — what an unattended session with
        /// no decision for that gate answers (BR-5).
        fn deny(&self, tool: &str) {
            let mut config = PermissionConfig::permissive();
            config.set(tool, crate::harness::PermissionPolicy::Deny);
            self.runtime
                .session_gates
                .lock()
                .expect("session gate mutex")
                .insert(
                    self.session_id.clone(),
                    Arc::new(PermissionGate::new(
                        self.session_id.clone(),
                        config,
                        Arc::clone(&self.events),
                        Arc::clone(&self.runtime.pending),
                    )),
                );
        }

        /// A child spec with the `[agent]` defaults and no spend ceiling.
        fn spec(&self, name: &str, task: &str) -> ChildSpec {
            let child_id = ChildId::new("call-1", name);
            ChildSpec {
                child_id: child_id.clone(),
                name: name.to_owned(),
                task: task.to_owned(),
                context: None,
                tier: None,
                provenance: Provenance::empty(),
                max_turns: 12,
                parent_max_turns: 40,
                deadline: Duration::from_secs(600),
                report_max_bytes: 32_768,
                spend: ChildSpend::new(
                    child_id.clone(),
                    SharePool::new(None, std::slice::from_ref(&child_id)),
                    None,
                ),
                consent: Arc::new(tokio::sync::Mutex::new(())),
                cancelled: ChildOutcomeSlot::default(),
            }
        }

        async fn parent_turn(&self, prompt: &str) -> Result<PromptTurnResult, RpcError> {
            self.runtime
                .run_prompt_turn(
                    &self.events,
                    &self.sessions,
                    self.session_id.clone(),
                    SessionMode::Structured,
                    Some(ProtoPhase::Implement),
                    Some(self.root.clone()),
                    prompt.to_owned(),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await
        }
    }

    /// Everything the bus has delivered to `sub` so far.
    fn drained(sub: &mut crate::broadcast::Subscription) -> Vec<Event> {
        std::iter::from_fn(|| sub.try_recv())
            .map(|envelope| envelope.event)
            .collect()
    }

    // ---- BR-1 / AC-2 ------------------------------------------------------

    /// **BR-1 / AC-2: a child's first request is the session's system prompt,
    /// the parent's `context`, and the task as the only user message — and
    /// nothing of the parent's conversation.** Read off the captured request,
    /// not inferred from a size (LESSON-519).
    ///
    /// The parent turn runs first and is committed, so the session really
    /// holds a conversation the child could have been seeded from — asserted
    /// before the absence is (non-vacuity). The oracle for "the session's
    /// system prompt" is the parent's own captured system segment **less
    /// exactly `agent`'s roster entry** — its line and its `arguments:` line,
    /// found by text, asserted present exactly once, never rebuilt from the
    /// tool (TASK-428): the child's is that, byte for byte, followed by the
    /// child's section. So the comparison also says the one roster difference
    /// is `agent` (BR-2), and a child prompt missing any other entry still
    /// reddens it.
    ///
    /// Benign path: the parent's own first request carries its ask as its only
    /// user message, so the parser reads a real request.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted — 1 red apiece, this test)
    ///
    /// - **Replay the session into the child** (`CarriedTurn::detached` calling
    ///   `ctx.replay(sessions.conversation_snapshot(..))`): reddens at the role
    ///   sequence — the child's request opens on the parent's exchange.
    /// - **Drop the context** (`child_system_section(None, ..)`): reddens at the
    ///   system-prompt equality.
    /// - **Register `agent` for children too** (TASK-428, the registration moved
    ///   out of the `ToolSet::Prompt` arm): reddens at the system-prompt
    ///   equality — the child's roster gains the entry the oracle stripped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_context_is_system_context_task_only() {
        const ASK: &str = "PARENT-ASK-MARKER please look around";
        const ANSWER: &str = "PARENT-ANSWER-MARKER looked";
        const TASK: &str = "CHILD-TASK-MARKER: summarise the layout";
        const CONTEXT: &str = "CHILD-CONTEXT-MARKER: the crate is tetond";
        let rig = Rig::new("child-context", true);
        rig.engine.say(ANSWER);
        rig.engine.say("child done");

        rig.parent_turn(ASK).await.expect("the parent turn runs");
        let carried = rig.sessions.conversation_snapshot(&rig.session_id);
        assert!(
            carried
                .blocks()
                .iter()
                .any(|b| b.text.contains("PARENT-ANSWER-MARKER")),
            "non-vacuity: the session holds the parent's exchange"
        );

        let outcome = rig
            .dispatcher()
            .run_child(ChildSpec {
                context: Some(CONTEXT.to_owned()),
                ..rig.spec("layout", TASK)
            })
            .await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");
        assert_eq!(outcome.result.report, "child done");

        let prompts = rig.engine.prompts();
        assert_eq!(prompts.len(), 2, "one parent call, one child call");
        let parent = chatml(&prompts[0]);
        let child = chatml(&prompts[1]);
        assert_eq!(
            parent.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
            ["system", "user"],
            "benign: the parser reads the parent's request as system + ask"
        );
        assert_eq!(parent[1].1, ASK);

        assert_eq!(
            child.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
            ["system", "user"],
            "the child's request is a system prompt and one user message"
        );
        assert_eq!(
            child[1].1, TASK,
            "the only user message is the task, verbatim"
        );
        assert_eq!(
            child[0].1,
            format!(
                "{}{}",
                without_agent_entry(&parent[0].1),
                child_system_section(Some(CONTEXT), 32_768)
            ),
            "the session's system prompt less `agent`, then the child's section with the context"
        );
        for marker in ["PARENT-ASK-MARKER", "PARENT-ANSWER-MARKER"] {
            assert!(
                !prompts[1].contains(marker),
                "the parent's conversation reached the child: {marker}"
            );
        }
    }

    /// `system` with exactly `agent`'s roster entry — the `- agent: ` line and
    /// the `  arguments: ` line under it — taken out, and nothing else.
    ///
    /// Found by text rather than rebuilt from the tool's own description, so
    /// the oracle is not computed by the subject; asserted present exactly
    /// once, so a roster that stopped listing `agent` cannot make the strip a
    /// silent no-op.
    fn without_agent_entry(system: &str) -> String {
        let lines: Vec<&str> = system.split_inclusive('\n').collect();
        let entries: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("- agent: "))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "non-vacuity: the parent's roster lists `agent` exactly once"
        );
        let at = entries[0];
        assert!(
            lines
                .get(at + 1)
                .is_some_and(|line| line.starts_with("  arguments: ")),
            "the entry's second line is its schema"
        );
        lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != at && *index != at + 1)
            .map(|(_, line)| *line)
            .collect()
    }

    // ---- BR-7 / AC-11 -----------------------------------------------------

    /// **BR-7 / AC-11: the four bounds are stamped before the child's first
    /// model call, published on `agent_child_started`, and echoed in the
    /// result unchanged.**
    ///
    /// `max_turns` is the config's 7 clamped to the parent's 5; the budget is
    /// the router's own figure for the session's category, derived
    /// independently here; the deadline is the spec's. "Before the first call"
    /// is read off the bus: `agent_child_started` precedes the first
    /// `session_update` the child's model call streamed.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted — 1 red apiece, this test)
    ///
    /// - **Drop the clamp** (`.min(spec.parent_max_turns)` removed): reddens at
    ///   `max_turns` — 7 against 5. No other test notices.
    /// - **Stamp after the attempts** (publish `agent_child_started` after
    ///   `run_child_attempts`): reddens at the ordering assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounds_stamped_before_first_call_and_echoed() {
        let rig = Rig::new("child-bounds", true);
        rig.engine.say("bounded");
        let mut sub = rig.events.subscribe(4096);
        let spec = ChildSpec {
            max_turns: 7,
            parent_max_turns: 5,
            deadline: Duration::from_secs(90),
            ..rig.spec("bounded", "stay inside the lines")
        };
        let child_id = spec.child_id.clone();

        let outcome = rig.dispatcher().run_child(spec).await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");

        let expected_budget = rig
            .runtime
            .turn_router(&rig.config, &rig.session_id)
            .resolve(category_for_phase(CorePhase::Implement))
            .budget
            .budget_bytes as u64;
        let bounds = outcome
            .result
            .bounds
            .expect("a routed child echoes its bounds");
        assert_eq!(
            bounds,
            ChildBounds {
                max_turns: 5,
                context_budget_bytes: expected_budget,
                spend_ceiling_micro_cents: None,
                deadline_secs: 90,
            }
        );

        let events = drained(&mut sub);
        let started_at = events
            .iter()
            .position(|e| matches!(e, Event::AgentChildStarted(_)))
            .expect("agent_child_started is published");
        let Event::AgentChildStarted(started) = &events[started_at] else {
            unreachable!()
        };
        assert_eq!(started.child_id, child_id);
        assert_eq!(started.parent_turn_id, TurnId::from("turn-parent"));
        assert_eq!(
            serde_json::to_value(started.bounds).unwrap(),
            serde_json::to_value(bounds).unwrap(),
            "the result echoes the published bounds byte for byte"
        );
        assert_eq!(Some(&started.route), outcome.result.route.as_ref());
        let first_streamed = events
            .iter()
            .position(|e| {
                matches!(e, Event::SessionUpdate(SessionUpdate { child_id: Some(id), .. }) if *id == child_id)
            })
            .expect("the child's model call streamed");
        assert!(
            started_at < first_streamed,
            "the bounds were published before the first call answered"
        );
    }

    // ---- BR-6 --------------------------------------------------------------

    /// A daemon whose default provider is a remote one at a loopback port
    /// nothing answers on, with `secret.md` under a `local-only` boundary — so
    /// a child seeded from that file is routed remote, refused at its first
    /// call's egress inspection, and pinned to the local tier mid-run.
    ///
    /// The endpoint is loopback so that a broken inspection fails fast against
    /// a closed port rather than reaching a real vendor.
    fn pinned_mid_run_rig(tag: &str) -> (Rig, Provenance) {
        let mut config = super::super::testsupport::config_with_remote("deepseek");
        config.default_provider = Some("deepseek".to_owned());
        for provider in &mut config.providers {
            if provider.id == "deepseek" {
                provider.endpoint = Some("http://127.0.0.1:9/v1/chat/completions".to_owned());
            }
        }
        config.boundaries.push(PrivacyBoundary {
            path_glob: "secret.md".to_owned(),
            mode: BoundaryMode::LocalOnly,
            origin: Default::default(),
        });
        let rig = Rig::with_config(tag, true, config);
        std::fs::write(rig.root.join("secret.md"), "local only\n").unwrap();
        let secret = ProvenanceId::from_resolved(&rig.root, &rig.root.join("secret.md")).unwrap();
        (rig, Provenance::tainted_by(secret))
    }

    /// **BR-6: a child pinned local mid-run reports the local route** — the
    /// route it ended on, not the one `agent_child_started` announced.
    ///
    /// Two legs over the same pin: one that completes on the local tier after
    /// the reroute, and one whose local call is still in flight when its
    /// deadline passes, so the run never returns and the result is framed from
    /// what the attempt loop last recorded.
    ///
    /// Non-vacuity: the started event names the remote provider, so the child
    /// really did begin remote and really was moved.
    ///
    /// # Mutation (run 2026-10-07, reverted)
    ///
    /// - **Stamp the route once** (the attempt loop's `child.route.set(..)`
    ///   removed from `run_attempts`): 1 red of the 67 tests matching `child`,
    ///   this one, at the completed leg's provider — `deepseek`, the route the
    ///   child started on. With that leg's two route assertions also removed,
    ///   the timed-out leg reddens on its own, at its provider.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_pinned_mid_run_reports_the_local_route() {
        let (rig, seed) = pinned_mid_run_rig("child-pinned");
        let mut sub = rig.events.subscribe(4096);
        rig.engine.say("answered locally");
        let outcome = rig
            .dispatcher()
            .run_child(ChildSpec {
                provenance: seed.clone(),
                ..rig.spec("pinned", "summarise the secret")
            })
            .await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");
        assert_eq!(outcome.result.report, "answered locally");

        let started = drained(&mut sub)
            .into_iter()
            .find_map(|e| match e {
                Event::AgentChildStarted(started) => Some(started),
                _ => None,
            })
            .expect("agent_child_started is published");
        assert_eq!(
            started.route.provider_id.0, "deepseek",
            "non-vacuity: the child began on the remote route"
        );
        assert!(
            rig.runtime.session_taint.is_tainted(&rig.session_id),
            "non-vacuity: the egress inspection pinned the session"
        );
        let route = outcome.result.route.as_ref().expect("a routed child");
        assert_ne!(
            route.provider_id.0, "deepseek",
            "the result names the route the child started on, not the one it ended on"
        );
        assert_eq!(route.model, "script", "the local engine's model: {route:?}");

        // Timed out on the local tier, after the pin: the run never returns,
        // and the result still names where it was.
        let (rig, seed) = pinned_mid_run_rig("child-pinned-timeout");
        let (release, parked) = mpsc::channel();
        rig.engine.then(Reply::Park(parked));
        let timed = rig
            .dispatcher()
            .run_child(ChildSpec {
                provenance: seed,
                deadline: Duration::from_secs(1),
                ..rig.spec("pinned-slow", "summarise the secret")
            })
            .await;
        let _ = release.send(());
        assert_eq!(timed.status(), ChildStatus::TimedOut, "{timed:?}");
        let route = timed.result.route.as_ref().expect("a routed child");
        assert_ne!(
            route.provider_id.0, "deepseek",
            "a timed-out child reports the route it was pinned to"
        );
    }

    // ---- BR-7 across a reroute ---------------------------------------------

    /// A loopback OpenAI-compatible vendor answering each request with the
    /// next scripted response, whole — a streamed turn or a bare status — and
    /// `503` once the script runs out. It keeps every request body it read.
    struct Vendor {
        url: String,
        bodies: Arc<Mutex<Vec<String>>>,
    }

    impl Vendor {
        fn serve(responses: Vec<String>) -> Self {
            use std::io::Write as _;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a mock vendor");
            let url = format!(
                "http://{}/v1/chat/completions",
                listener.local_addr().expect("the vendor's address")
            );
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&bodies);
            let mut script = VecDeque::from(responses);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let body = read_whole_request(&mut stream);
                    seen.lock().unwrap().push(body);
                    let response = script.pop_front().unwrap_or_else(|| vendor_status(503));
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self { url, bodies }
        }

        /// How many of the child's own turn requests the vendor read: those
        /// carrying its task beside a system prompt. A duty the child's tools
        /// raise (one fired here after the reroute) may quote the task too,
        /// but it is sent as a single user message.
        fn turn_requests_carrying(&self, task: &str) -> usize {
            self.bodies
                .lock()
                .unwrap()
                .iter()
                .filter(|body| body.contains(task) && body.contains(r#""role":"system""#))
                .count()
        }
    }

    /// Read one request — head, then exactly `Content-Length` body bytes — so
    /// the vendor never answers (and closes) before the client has finished
    /// sending, which a client reports as a transport failure. Returns the
    /// body as text.
    fn read_whole_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let Ok(n) = stream.read(&mut chunk) else {
                return String::new();
            };
            if n == 0 {
                return String::new();
            }
            seen.extend_from_slice(&chunk[..n]);
            let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while seen.len() - (end + 4) < length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&chunk[..n]),
                }
            }
            return String::from_utf8_lossy(&seen[end + 4..]).into_owned();
        }
    }

    /// A whole HTTP response carrying one streamed turn that calls `read`.
    fn vendor_read_call(id: &str, path: &str) -> String {
        let call = serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{
                "index": 0,
                "id": id,
                "function": {
                    "name": "read",
                    "arguments": serde_json::json!({ "path": path }).to_string()
                }
            }]}}]
        });
        let finish =
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        let usage = serde_json::json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 5 } });
        let body = format!("data: {call}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A whole HTTP response with `status` and a small JSON error body.
    fn vendor_status(status: u16) -> String {
        let body = r#"{"error":{"message":"scripted"}}"#;
        format!(
            "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A remote provider declaring a small window at `vendor`, bound to every
    /// tier with the local tier as its fallback.
    fn falling_back_config(vendor: &Vendor) -> Config {
        Config {
            providers: vec![
                ModelProvider {
                    id: "small".to_owned(),
                    kind: ProviderKind::OpenaiCompatible,
                    endpoint: Some(vendor.url.clone()),
                    model: Some("small-1".to_owned()),
                    auth_ref: None,
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities {
                        max_context: 8_000,
                        ..ProviderCapabilities::default()
                    },
                },
                ModelProvider {
                    id: "local".to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    model: None,
                    auth_ref: None,
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities::default(),
                },
            ],
            tiers: Tier::ALL
                .iter()
                .map(|tier| TierBinding {
                    tier: *tier,
                    provider_id: "small".to_owned(),
                    fallback_id: Some("local".to_owned()),
                })
                .collect(),
            ..Config::default()
        }
    }

    /// The vendor's script: two `read` calls, then `422` — a client error the
    /// router falls back on.
    fn two_calls_then_a_fallback() -> Vendor {
        Vendor::serve(vec![
            vendor_read_call("call-a", "notes.txt"),
            vendor_read_call("call-b", "notes.txt"),
            vendor_status(422),
        ])
    }

    /// **BR-7 across a reroute: a child moved to a new route mid-run keeps
    /// the bounds it was stamped with** — no more model calls than the
    /// stamped `max_turns` in total, and a context budget never wider than the
    /// stamped one.
    ///
    /// The child is routed to a remote provider declaring a small window
    /// (floored to 50,000 bytes — the stamp), which serves two `read` calls
    /// and then answers `422`. The fallback is the local tier, whose own
    /// budget is wider (asserted). The local engine opens with ~52 KB of prose
    /// — over the stamp, inside the local budget — and keeps calling tools.
    ///
    /// - **Turns**: the stamp is 4 and the remote served 2, so the local
    ///   attempt has 2 left. `turns_used` is 4 and the child ends
    ///   `turns_exhausted`.
    /// - **Budget**: every `context_pressure` the child publishes is fitted to
    ///   at most the stamp, and one is — the local attempt's gate cutting the
    ///   52 KB reply to it, which a local-wide budget would have let stand.
    ///
    /// "None left" — a reroute after the stamp is spent — is not reachable
    /// here: a reroute follows a failed call, a failed call is not counted, and
    /// the loop stops at the stamp before making one. `hold_to_bounds`'s own
    /// test pins that arithmetic.
    ///
    /// # Mutations (run 2026-10-07, each reverted)
    ///
    /// - **Restart the turn count per attempt** (`hold_to_bounds` taking
    ///   `made` as 0): 2 reds of the 69 tests matching `child` — this one at
    ///   `turns_used` (6 against a stamp of 4), and `hold_to_bounds`' own
    ///   test.
    /// - **Let the budget follow the route** (the `held_to` clamp skipped):
    ///   2 reds of 69 — this one at the budget assertion (the reroute's refit
    ///   announces the local tier's 63,488 bytes, past the 50,000 stamped),
    ///   and `hold_to_bounds`' own test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rerouted_child_keeps_its_stamped_bounds() {
        const TASK: &str = "REROUTE-TASK-MARKER: read the notes, repeatedly";
        let vendor = two_calls_then_a_fallback();
        let rig = Rig::with_config("child-reroute-bounds", true, falling_back_config(&vendor));
        std::fs::write(rig.root.join("notes.txt"), "notes\n").unwrap();
        let read = call("read", serde_json::json!({ "path": "notes.txt" }));
        rig.engine
            .say(&format!("{} {read}", "lorem ".repeat(52_000 / 6)));
        for _ in 0..6 {
            rig.engine.say(&read);
        }
        let mut sub = rig.events.subscribe(8192);
        let spec = ChildSpec {
            max_turns: 4,
            ..rig.spec("rerouted", TASK)
        };
        let child_id = spec.child_id.clone();

        let outcome = rig.dispatcher().run_child(spec).await;

        let bounds = outcome
            .result
            .bounds
            .expect("a routed child echoes its bounds");
        let router = rig.runtime.turn_router(&rig.config, &rig.session_id);
        let local = router.budget_for(Some("local"));
        assert_eq!(bounds.max_turns, 4, "{outcome:?}");
        assert!(
            local.budget_bytes as u64 > bounds.context_budget_bytes,
            "non-vacuity: the fallback's own budget ({}) is wider than the stamp ({})",
            local.budget_bytes,
            bounds.context_budget_bytes
        );
        assert_eq!(
            vendor.turn_requests_carrying("REROUTE-TASK-MARKER"),
            3,
            "non-vacuity: the remote served two of the child's calls and failed the third"
        );
        assert_eq!(
            outcome
                .result
                .route
                .as_ref()
                .map(|r| r.provider_id.0.as_str()),
            Some("local"),
            "non-vacuity: the child was moved to the fallback"
        );

        assert_eq!(
            outcome.status(),
            ChildStatus::TurnsExhausted,
            "{:?}",
            outcome.result
        );
        assert!(
            outcome.result.turns_used <= bounds.max_turns,
            "a reroute let the child make {} calls against a stamp of {}",
            outcome.result.turns_used,
            bounds.max_turns
        );
        assert_eq!(outcome.result.turns_used, 4);
        assert_eq!(
            rig.engine.prompts().len(),
            2,
            "the local attempt had the two turns the remote left it"
        );

        let pressured: Vec<teton_protocol::events::ContextPressure> = drained(&mut sub)
            .into_iter()
            .filter_map(|e| match e {
                Event::ContextPressure(cp) if cp.child_id.as_ref() == Some(&child_id) => Some(cp),
                _ => None,
            })
            .collect();
        assert!(
            pressured
                .iter()
                .all(|cp| cp.budget_bytes <= bounds.context_budget_bytes),
            "the child's context was fitted past its stamped budget: {pressured:#?}"
        );
        assert!(
            pressured.iter().any(|cp| cp.budget_bytes == bounds.context_budget_bytes
                && cp.dropped_blocks + cp.elided_bytes > 0),
            "non-vacuity: the local attempt's gate cut the 52 KB reply to the stamp: {pressured:#?}"
        );
    }

    // ---- BR-9 / ADR-8 -----------------------------------------------------

    /// **BR-9 / ADR-8: the outcome carries the union of everything the child's
    /// context touched** — the file it read, the provenance its task was
    /// written under, and `unknown` when that was unknown.
    ///
    /// The task's seed provenance names a file the child never opens
    /// (`secret.md`) and is unknown; the child reads `notes.txt`. Every id is
    /// minted here from the fixture's own paths.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted — 1 red apiece, this test)
    ///
    /// - **Seed provenance only** (the work's context provenance discarded, so
    ///   the outcome falls back to the seed): reddens at `notes.txt`.
    /// - **Unseeded task** (`CarriedTurn::detached` pushing the task with an
    ///   empty set): reddens at `secret.md`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outcome_carries_provenance_union() {
        let rig = Rig::new("child-provenance", true);
        std::fs::write(rig.root.join("notes.txt"), "the notes say hello\n").unwrap();
        rig.engine
            .say(&call("read", serde_json::json!({ "path": "notes.txt" })));
        rig.engine.say("read it");
        let secret = ProvenanceId::from_resolved(&rig.root, &rig.root.join("secret.md")).unwrap();
        let notes = ProvenanceId::from_resolved(&rig.root, &rig.root.join("notes.txt")).unwrap();
        let mut seed = Provenance::tainted_by(secret.clone());
        seed.mark_unknown();

        let outcome = rig
            .dispatcher()
            .run_child(ChildSpec {
                provenance: seed,
                ..rig.spec("reader", "read the notes")
            })
            .await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");

        let ids: Vec<&ProvenanceId> = outcome.provenance.ids().collect();
        assert!(ids.contains(&&notes), "the read: {ids:?}");
        assert!(ids.contains(&&secret), "the task's own seed: {ids:?}");
        assert!(outcome.provenance.is_unknown(), "unknown is carried up");
    }

    // ---- BR-10 / AC-14 ----------------------------------------------------

    /// **BR-10 / AC-14: every way a child can end is one of the eight
    /// statuses, typed — and never an error out of `run_child`.**
    ///
    /// All eight are produced here against the daemon's real route → assemble
    /// → attempt path, with the parent's side stood in for by the dispatcher:
    ///
    /// | status | how |
    /// |---|---|
    /// | `completed` | the model answers |
    /// | `refused` | a task larger than the child's budget — `over_budget`, nothing sent |
    /// | `turns_exhausted` | a one-turn cap and a model that keeps calling tools |
    /// | `budget_exhausted` | the local tier refusing the prompt at its window |
    /// | `failed` | the local tier failing outright, with the code |
    /// | `timed_out` | a model call in flight past the deadline, and a `shell` call in flight past it |
    /// | `spend_exhausted` | a remote route and a zero share — refused at the child's choke point |
    /// | `cancelled` | the runner aborted mid-call; the outcome is left in the slot |
    ///
    /// **What TASK-430 must still cover end to end** (AC-14 asks for each
    /// through the `agent` tool with the parent continuing): all eight, and the
    /// `refused` flavour this task cannot produce — a child refused by a
    /// project-skill gate. A gate refusal reaches a child as a typed tool
    /// failure (BR-5) and the child then ends on its own; which of those
    /// endings counts as `refused` is a rule TASK-430 has to pin.
    ///
    /// Benign path: the `completed` leg's report is the model's text,
    /// untouched and unmarked.
    ///
    /// # Mutations (run 2026-10-05, each reverted)
    ///
    /// - **Drop whole-or-refused** (the `!fit.fits` return made dead): 1 red
    ///   of the 60 tests matching `child`, this one, at the `refused` leg — the
    ///   oversized task is seeded and the loop refuses its anchors instead, so
    ///   the child ends `budget_exhausted`.
    /// - **Drop the deadline race** (`deadline.expired()` arm made pending):
    ///   the first `timed_out` leg waits out the park's own 30 s and ends
    ///   `completed`.
    /// - **Map every error to `failed`** (`ended_by` collapsed): reddens at the
    ///   `budget_exhausted` leg; with only the spend arm collapsed, at the
    ///   `spend_exhausted` leg.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_status_matrix() {
        let rig = Rig::new("child-statuses", true);
        let dispatcher = rig.dispatcher();
        std::fs::write(rig.root.join("notes.txt"), "notes\n").unwrap();

        // completed — and the benign report.
        rig.engine.say("finished cleanly");
        let completed = dispatcher.run_child(rig.spec("completes", "finish")).await;
        assert_eq!(completed.status(), ChildStatus::Completed);
        assert_eq!(completed.result.report, "finished cleanly");
        assert_eq!(completed.result.turns_used, 1);
        assert!(!completed.truncated);

        // refused: over_budget, naming size, budget and bound; nothing sent.
        let before = rig.engine.prompts().len();
        let refused = dispatcher
            .run_child(rig.spec("too-big", &"word ".repeat(120_000)))
            .await;
        assert_eq!(
            refused.status(),
            ChildStatus::Refused,
            "{:?}",
            refused.result
        );
        let refusal = refused.result.refusal.as_deref().expect("a typed refusal");
        assert!(refusal.starts_with("over_budget: "), "{refusal}");
        assert!(refusal.contains("context budget is") && refusal.contains("(bound: "));
        assert!(refused.result.report.is_empty());
        assert_eq!(refused.result.turns_used, 0);
        assert!(
            refused.result.bounds.is_some(),
            "the bounds it was refused under"
        );
        assert_eq!(rig.engine.prompts().len(), before, "no model saw the task");

        // turns_exhausted: the last text, marked.
        rig.engine.say(&format!(
            "Reading the notes first. {}",
            call("read", serde_json::json!({ "path": "notes.txt" }))
        ));
        let exhausted = dispatcher
            .run_child(ChildSpec {
                max_turns: 1,
                ..rig.spec("one-turn", "read and report")
            })
            .await;
        assert_eq!(exhausted.status(), ChildStatus::TurnsExhausted);
        assert_eq!(exhausted.result.turns_used, 1);
        assert!(
            exhausted.result.report.starts_with(
                "[turns_exhausted: this child used all 1 of its turns before it finished"
            ),
            "{}",
            exhausted.result.report
        );
        assert!(exhausted
            .result
            .report
            .ends_with("Reading the notes first."));

        // budget_exhausted: the window refused the prompt.
        rig.engine.then(Reply::OverWindow);
        let over = dispatcher.run_child(rig.spec("over-window", "go")).await;
        assert_eq!(
            over.status(),
            ChildStatus::BudgetExhausted,
            "{:?}",
            over.result
        );
        assert!(over.result.report.is_empty());

        // failed: with the code.
        rig.engine.then(Reply::Fail);
        let failed = dispatcher.run_child(rig.spec("fails", "go")).await;
        assert_eq!(failed.status(), ChildStatus::Failed);
        let error = failed.result.error.as_deref().expect("the code");
        assert!(
            error.starts_with(&format!("{}: ", error_code::INTERNAL_ERROR)),
            "{error}"
        );
        assert!(error.contains("scripted failure"), "{error}");

        // timed_out, a model call in flight.
        let (release, parked) = mpsc::channel();
        rig.engine.then(Reply::Park(parked));
        let timed = dispatcher
            .run_child(ChildSpec {
                deadline: Duration::from_secs(1),
                ..rig.spec("slow-model", "go")
            })
            .await;
        assert_eq!(timed.status(), ChildStatus::TimedOut);
        assert!(timed.result.report.is_empty());
        assert_eq!(timed.result.bounds.map(|b| b.deadline_secs), Some(1));
        let _ = release.send(());

        // timed_out, a `shell` call in flight — and the result arrives while it
        // still is: the flag the command writes after its sleep is not there yet.
        let flag = rig.root.join("done.flag");
        rig.engine.say(&call(
            "shell",
            serde_json::json!({ "command": "sleep 5 && touch done.flag" }),
        ));
        let timed = dispatcher
            .run_child(ChildSpec {
                deadline: Duration::from_secs(1),
                ..rig.spec("slow-tool", "go")
            })
            .await;
        assert_eq!(timed.status(), ChildStatus::TimedOut);
        assert!(
            !flag.exists(),
            "timed_out came back while the tool was still in flight"
        );
        // Let the command finish before the runtime is torn down under it —
        // and, non-vacuity, prove it ran: an absent flag above means nothing
        // if the command never started.
        for _ in 0..100 {
            if flag.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(flag.exists(), "non-vacuity: the in-flight command did run");

        // cancelled: the runner dropped mid-call leaves its outcome in the slot.
        let (release, parked) = mpsc::channel();
        rig.engine.then(Reply::Park(parked));
        let slot = ChildOutcomeSlot::default();
        let calls_before = rig.engine.prompts().len();
        let running = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let spec = ChildSpec {
                cancelled: slot.clone(),
                ..rig.spec("abandoned", "go")
            };
            async move { dispatcher.run_child(spec).await }
        });
        while rig.engine.prompts().len() == calls_before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        let cancelled = slot.take().expect("an aborted run leaves its outcome");
        assert_eq!(cancelled.status(), ChildStatus::Cancelled);
        assert!(cancelled.result.route.is_some() && cancelled.result.bounds.is_some());
        let _ = release.send(());

        // spend_exhausted: a remote route, and a share of nothing.
        let mut config = super::super::testsupport::config_with_remote("deepseek");
        config.default_provider = Some("deepseek".to_owned());
        let remote = Rig::with_config("child-spend", true, config);
        let spec = remote.spec("broke", "go");
        let child_id = spec.child_id.clone();
        let broke = remote
            .dispatcher()
            .run_child(ChildSpec {
                spend: ChildSpend::new(
                    child_id.clone(),
                    SharePool::new(Some(0), std::slice::from_ref(&child_id)),
                    None,
                ),
                ..spec
            })
            .await;
        assert_eq!(
            broke
                .result
                .route
                .as_ref()
                .map(|r| r.provider_id.0.as_str()),
            Some("deepseek"),
            "non-vacuity: the child was routed remote"
        );
        assert_eq!(
            broke.status(),
            ChildStatus::SpendExhausted,
            "{:?}",
            broke.result
        );
        assert_eq!(
            broke
                .result
                .bounds
                .and_then(|b| b.spend_ceiling_micro_cents),
            Some(0)
        );
        assert!(broke.result.report.is_empty());
    }

    /// **BR-10's "refused by a gate" (TASK-428's rule): a child with nothing to
    /// report whose every attempted call the gate denied ends `refused` with
    /// `gate_denied:<tool>` — and only that child.**
    ///
    /// The session gate denies `shell` (an unattended session with no decision
    /// for it answers the same). Three children, each meeting that denial:
    ///
    /// | child | after the denial | ends |
    /// |---|---|---|
    /// | `stopped` | an empty final answer | `refused`, `gate_denied:shell` |
    /// | `explains` | a report saying why | `completed`, that report |
    /// | `reads` | a `read` that ran, then an empty answer | `completed`, empty |
    ///
    /// The second and third are the benign half: a denial is a typed tool
    /// failure the child read (BR-5), and what it did next is its own ending.
    ///
    /// # Mutations (run 2026-10-07, each reverted — 1 red apiece, this test,
    /// over the lib and four integration binaries)
    ///
    /// - **Never refuse** (`finished_on_its_own` always `completed`): reddens
    ///   at the `stopped` leg.
    /// - **Refuse on any denial** (drop the `text.trim().is_empty()` guard):
    ///   reddens at the `explains` leg.
    /// - **Don't count a call that ran** (`note_ran` removed from the loop):
    ///   reddens at the `reads` leg.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gate_denial_refuses_only_a_child_that_had_nothing_else() {
        let rig = Rig::new("child-gate-denied", true);
        std::fs::write(rig.root.join("notes.txt"), "notes\n").unwrap();
        rig.deny("shell");
        let dispatcher = rig.dispatcher();
        let shell = call("shell", serde_json::json!({ "command": "ls" }));

        rig.engine.say(&shell);
        rig.engine.say("");
        let stopped = dispatcher
            .run_child(rig.spec("stopped", "list the tree"))
            .await;
        assert_eq!(stopped.status(), ChildStatus::Refused, "{stopped:?}");
        assert_eq!(stopped.result.refusal.as_deref(), Some("gate_denied:shell"));
        assert!(stopped.result.report.is_empty());

        rig.engine.say(&shell);
        rig.engine.say("I was not allowed to run `ls`.");
        let explains = dispatcher
            .run_child(rig.spec("explains", "list the tree"))
            .await;
        assert_eq!(explains.status(), ChildStatus::Completed, "{explains:?}");
        assert_eq!(explains.result.report, "I was not allowed to run `ls`.");
        assert_eq!(explains.result.refusal, None);

        rig.engine.say(&shell);
        rig.engine
            .say(&call("read", serde_json::json!({ "path": "notes.txt" })));
        rig.engine.say("");
        let reads = dispatcher
            .run_child(rig.spec("reads", "list the tree"))
            .await;
        assert_eq!(reads.status(), ChildStatus::Completed, "{reads:?}");
        assert_eq!(reads.result.refusal, None);
    }

    // ---- AC-19 ------------------------------------------------------------

    /// **AC-19: a child's `shell` starts in the session root, with the same
    /// cwd behaviour as the parent's.** The oracle is the parent's own result
    /// for the same command at the same root: the child's tool result is that,
    /// byte for byte, and it names the root.
    ///
    /// A non-project root, which is where REQ-615's cwd note applies.
    ///
    /// # Mutation (run 2026-10-05 over the 60 tests matching `child`, reverted)
    ///
    /// - **Root from the daemon, not the registry** (`session_root_for(None)` in
    ///   the work): 4 reds — this test (the child's `pwd` names the daemon's
    ///   fallback root), and the three others whose children read the session
    ///   root (`child_context_is_system_context_task_only`'s environment block,
    ///   `outcome_carries_provenance_union`'s read, and
    ///   `terminal_status_matrix`'s in-flight `shell`, whose flag lands in the
    ///   wrong directory). One leak, four witnesses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_shell_starts_in_session_root() {
        let rig = Rig::new("child-shell", false);
        let pwd = call("shell", serde_json::json!({ "command": "pwd" }));
        rig.engine.say(&pwd);
        rig.engine.say("parent saw it");
        rig.engine.say(&pwd);
        rig.engine.say("child saw it");

        rig.parent_turn("where are we?")
            .await
            .expect("the parent turn runs");
        let outcome = rig
            .dispatcher()
            .run_child(rig.spec("where", "where are we?"))
            .await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");

        let prompts = rig.engine.prompts();
        assert_eq!(prompts.len(), 4);
        let result_of = |prompt: &str| {
            chatml(prompt)
                .pop()
                .map(|(_, body)| body)
                .expect("the tool result is the request's last message")
        };
        let parent = result_of(&prompts[1]);
        let child = result_of(&prompts[3]);
        assert!(
            parent.contains(&*rig.root.to_string_lossy()),
            "non-vacuity: the parent's `pwd` names the root: {parent}"
        );
        assert_eq!(
            child, parent,
            "the child's shell ran where the parent's did"
        );
    }

    // ---- the four unstamped events ----------------------------------------

    /// **A child publishes none of `route_decided`, `context_compacted`,
    /// `turn_queued` or `prefill_progress`** — the session-scoped payloads that
    /// carry no child id and would read on the bus as the parent's
    /// (`harness::child` module docs).
    ///
    /// Three legs: a child's whole run (with a tool call, so its tool events
    /// flow) publishes none of the four, and every `session_update` it does
    /// publish carries its id; a child's emitter drops a compaction record the
    /// parent's publishes; and a child's duty route carries no announcement
    /// where the parent's does.
    ///
    /// Benign path: a prompt turn on the same daemon still publishes
    /// `route_decided`.
    ///
    /// # Mutations (run 2026-10-05 over the 60 tests matching `child`, each
    /// reverted — 1 red apiece, this test)
    ///
    /// - **Announce in the attempt loop** (`emit_route_decided` called in the
    ///   child arm too): reddens at the run leg.
    /// - **Publish a child's compaction** (the early return removed from
    ///   `SessionEvents::context_compacted`): reddens at the emitter leg.
    /// - **Announce a child's duty** (`resolve_duty` ignoring `dctx.child`):
    ///   reddens at the duty leg.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_publishes_none_of_the_unstamped_kinds() {
        // The run leg.
        let rig = Rig::new("child-quiet", true);
        std::fs::write(rig.root.join("notes.txt"), "notes\n").unwrap();
        rig.engine
            .say(&call("read", serde_json::json!({ "path": "notes.txt" })));
        rig.engine.say("quiet");
        let mut sub = rig.events.subscribe(4096);
        let spec = rig.spec("quiet", "read quietly");
        let child_id = spec.child_id.clone();
        let outcome = rig.dispatcher().run_child(spec).await;
        assert_eq!(outcome.status(), ChildStatus::Completed, "{outcome:?}");
        let events = drained(&mut sub);
        let names: Vec<&str> = events.iter().map(Event::name).collect();
        for parent_only in PARENT_ONLY_EVENTS {
            assert!(
                !names.contains(&parent_only),
                "a child published `{parent_only}`: {names:?}"
            );
        }
        let updates: Vec<&SessionUpdate> = events
            .iter()
            .filter_map(|e| match e {
                Event::SessionUpdate(update) => Some(update),
                _ => None,
            })
            .collect();
        assert!(
            updates.len() >= 2,
            "non-vacuity: the child's tool events flowed"
        );
        assert!(
            updates
                .iter()
                .all(|u| u.child_id.as_ref() == Some(&child_id)),
            "every session_update a child publishes names it"
        );

        // Benign: a prompt turn still announces its route.
        let parent = Rig::new("parent-loud", true);
        let mut sub = parent.events.subscribe(4096);
        parent.parent_turn("hello").await.expect("a prompt turn");
        assert!(
            drained(&mut sub)
                .iter()
                .any(|e| matches!(e, Event::RouteDecided(_))),
            "a prompt turn publishes route_decided"
        );

        // The emitter leg.
        let record = crate::harness::CompactionRecord {
            kept_bytes: 10,
            dropped_bytes: 10,
            summarized_bytes: 0,
            anchor_bytes: 5,
            dropped_blocks: Vec::new(),
            fallback: true,
        };
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let emitter = SessionEvents::new(Arc::clone(&bus), SessionId::from("s"));
        emitter
            .for_child(child_id.clone(), TurnId::from("turn-parent"))
            .context_compacted(&record, None);
        assert!(
            drained(&mut sub).is_empty(),
            "a child's compaction stays off the bus"
        );
        emitter.context_compacted(&record, None);
        assert!(
            matches!(drained(&mut sub).as_slice(), [Event::ContextCompacted(_)]),
            "benign: the parent's compaction is published"
        );

        // The duty leg.
        let router = rig.runtime.turn_router(&rig.config, &rig.session_id);
        let gate = rig
            .runtime
            .permission_gate_for(&rig.session_id, &rig.events, &rig.config);
        let engine = rig.runtime.engine.get_with_format();
        let tctx = TurnContext::new(
            &rig.events,
            &rig.session_id,
            &rig.config,
            &router,
            &gate,
            None,
        );
        let announces = |route: &DutyRoute| {
            matches!(
                route,
                DutyRoute::Serves {
                    announce: Some(_),
                    ..
                }
            )
        };
        assert!(
            announces(&rig.runtime.digest_route(tctx.duties(engine.as_ref(), None))),
            "benign: a prompt turn's duty announces its route"
        );
        let child = ChildTurn {
            child_id,
            parent_turn_id: TurnId::from("turn-parent"),
            max_turns: 12,
            budget: crate::harness::HarnessConfig::default().budget,
            spend: rig.spec("x", "x").spend,
            model_calls: Arc::new(AtomicU32::new(0)),
            route: ChildRouteCell::default(),
        };
        assert!(
            !announces(
                &rig.runtime
                    .digest_route(tctx.for_child(&child).duties(engine.as_ref(), None))
            ),
            "a child's duty announces nothing"
        );
    }
}
