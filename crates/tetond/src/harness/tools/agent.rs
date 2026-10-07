//! The `agent` tool (REQ-623): the model hands one or more tasks to **child
//! turns** and gets each child's report back as one typed result.
//!
//! A call is validated whole before anything starts (BR-3): too many tasks,
//! too many children for this prompt turn, a name past its bound, a repeated
//! name or an empty task refuses the **whole call** with a typed
//! [`AgentRefusal`], publishes `agent_call_refused`, and starts no child. An
//! accepted call builds the call's [`SharePool`] from the prompt's remaining
//! spend headroom (ADR-4), one consent mutex (ADR-5), one [`ChildSpec`] per
//! task, and spawns every child into a [`JoinSet`] on the runtime (ADR-2). The
//! parent waits for **all** of them — there is no partial return (BR-4) — and
//! gets a JSON array of [`ChildResult`]s, framed as untrusted data, whose block
//! carries the union of every child's provenance (ADR-8, BR-9).
//!
//! # Why `run` refuses
//!
//! [`Tool::run`] is synchronous and the loop runs it off the async worker
//! (`block_in_place`, BUG-226's fix). A call that lasts minutes of awaiting
//! children does not belong there: blocking a worker on `block_on` is BUG-226's
//! shape at N times the duration. So the loop reaches this tool through
//! [`Tool::as_agent`] and **awaits** [`AgentTool::dispatch`] on its own async
//! path (ADR-1); `run` answers a typed `agent_requires_async_dispatch` refusal,
//! so a caller that dispatches by name gets an honest answer instead of a hang.
//!
//! # Layering
//!
//! This module depends on [`ChildDispatcher`], never on `runtime::*`: the
//! runtime implements the trait, exactly as `SkillTool` takes a registry and a
//! gate rather than a runtime. That is what keeps the tool testable with a
//! stub dispatcher and no daemon.
//!
//! # The per-turn cap is a counter on the tool
//!
//! `agent.max_children_per_turn` counts every child one prompt turn starts,
//! across all its `agent` calls. The registry is rebuilt per prompt turn
//! (`build_tools`), so the tool is too, and a counter on it is per turn by
//! construction — the next prompt's tool starts at zero without anything having
//! to remember to reset it (the repeat ledger's shape, REQ-617 BR-6).
//!
//! ASSUME-010: the test module stays last, after `impl Tool for AgentTool`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use teton_core::config::AgentConfig;
use teton_core::cost_ceiling::PromptSpend;
use teton_protocol::agent::{AgentRefusal, ChildId, ChildResult, ChildStatus, ChildTask};
use teton_protocol::events::{
    AgentCallFinished, AgentCallRefused, AgentCallStarted, AgentChildFinished,
    AgentChildShareReleased, Event, FinishedChild, PlannedChild, ShareRecipient,
};
use teton_protocol::{Tier, TurnId};
use tokio::runtime::Handle;
use tokio::task::{JoinError, JoinSet};

use super::{ResultDisposition, Tool, ToolContext, ToolError, ToolOutcome, ToolRegistry};
use crate::cost::share::headroom;
use crate::cost::{ChildSpend, SharePool};
use crate::egress::Provenance;
use crate::harness::child::{
    ChildDispatcher, ChildOutcome, ChildOutcomeSlot, ChildSpec, CHILD_PANICKED,
};
use crate::harness::context::{ToolProvenance, UnknownReach};
use crate::harness::turn_loop::SessionEvents;

/// The name the model calls the tool by (REQ-623 OQ-5).
pub const AGENT_TOOL_NAME: &str = "agent";

/// Why `agent` is exempt from the degraded-profile tool cap (ADR-7) — its row
/// in [`CAP_EXEMPT_TOOLS`](super::CAP_EXEMPT_TOOLS).
pub const AGENT_CAP_EXEMPT_REASON: &str =
    "a fan-out the user's skills depend on must not vanish on a degraded profile — a skill \
     that dispatches on one route and runs inline on another is a skill that lies";

/// The refusal [`Tool::run`] answers with: the tool is awaited by the loop, not
/// run (ADR-1).
pub const ASYNC_DISPATCH_REFUSAL: &str = "agent_requires_async_dispatch";

/// The bound on a task's `name`, in characters (the spec's entity table).
pub const NAME_MAX_CHARS: u32 = 40;

/// What the parent prompt turn lends its `agent` tool — the per-turn half of
/// what a call needs; the per-call half arrives as an [`AgentCall`].
#[derive(Clone)]
pub struct AgentParent {
    /// The prompt turn the children run under — every `parent_turn_id` they
    /// carry, and the prefix of every `call_id` this tool mints.
    pub turn_id: TurnId,
    /// The parent's emitter, which publishes the call's `agent_*` events and
    /// stamps nothing (ADR-3).
    pub events: SessionEvents,
    /// The prompt's spend ceiling in micro-cents, or `None` when the user set
    /// none — in which case children get no ceiling and nothing is released
    /// (BR-8).
    pub spend_ceiling: Option<u64>,
    /// The prompt's spend accumulator — the same `Arc` every egress of this
    /// prompt adds into, so a call's headroom is read live and every child's
    /// spend is the parent's too (ADR-4). `None` exactly when
    /// [`Self::spend_ceiling`] is.
    pub prompt_spend: Option<Arc<PromptSpend>>,
}

/// One `agent` call as the loop hands it over.
pub struct AgentCall<'a> {
    /// The loop's id for this tool call. The call id is minted from it and the
    /// parent turn's id, because the loop's ids restart with every prompt turn
    /// and a call id must be unique within the session (the spec's entity
    /// table).
    pub tool_call_id: &'a str,
    /// The call's arguments, as the model wrote them.
    pub arguments: &'a Value,
    /// The parent loop's own `max_turns` — what no child's cap may exceed
    /// (BR-7).
    pub parent_max_turns: u32,
    /// The parent's context provenance at the moment of the call: the tasks
    /// were written by a model that may have read boundary content, and text
    /// carried into a child must not shed that taint (LESSON-501).
    pub provenance: Provenance,
}

/// The `agent` tool — see the module docs.
pub struct AgentTool {
    dispatcher: Arc<dyn ChildDispatcher>,
    config: AgentConfig,
    parent: AgentParent,
    runtime: Handle,
    /// Children this prompt turn has started, across every call — the per-turn
    /// cap's counter (module docs).
    started: AtomicU32,
    /// The model-facing description, stating this session's two caps.
    description: String,
}

/// The parsed arguments: `{ tasks: [{task, name?, tier?, context?}] }`.
///
/// [`ChildTask`] ignores unknown keys (the vendored skills phrase dispatch in
/// another harness's vocabulary), so a stray `subagent_type` is not a refusal.
#[derive(Deserialize)]
struct AgentArgs {
    tasks: Vec<ChildTask>,
}

/// One task that passed validation, with the name it will run under.
struct Planned {
    name: String,
    task: ChildTask,
}

impl AgentTool {
    /// The tool for one prompt turn, dispatching through `dispatcher`.
    #[must_use]
    pub fn new(
        dispatcher: Arc<dyn ChildDispatcher>,
        config: AgentConfig,
        parent: AgentParent,
        runtime: Handle,
    ) -> Self {
        let description = describe(&config);
        Self {
            dispatcher,
            config,
            parent,
            runtime,
            started: AtomicU32::new(0),
            description,
        }
    }

    /// Run one call: validate it whole, start every child, wait for all of
    /// them, and hand back their results (ADR-1, ADR-2).
    ///
    /// Never fails the parent turn. A malformed call is an argument error, a
    /// call the caps or the names refuse is a typed refusal, and a child that
    /// ends badly is one entry with that status — the parent always gets a
    /// result to reason about (BR-10).
    ///
    /// # Cancellation
    ///
    /// The parent turn is cancelled by dropping its future, and this one with
    /// it. The children are not left running: the call's in-flight set aborts
    /// them all on drop, each child's runner leaves its `cancelled` outcome
    /// behind, and the call's `agent_child_finished` / `agent_call_finished`
    /// are still published for whoever else is watching the session (BR-10).
    pub async fn dispatch(&self, call: AgentCall<'_>) -> ToolOutcome {
        let tasks = match parse(call.arguments) {
            Ok(tasks) => tasks,
            Err(err) => return err.into(),
        };
        let call_id = format!("{}:{}", self.parent.turn_id, call.tool_call_id);
        // The no-`/` invariant `ChildId::name` splits on: both halves are
        // daemon-minted — `turn-<n>`, and the loop's own `call-<n>` (never a
        // provider's id) — so neither carries the separator.
        debug_assert!(
            !call_id.contains('/'),
            "a call id carries no `/`, or ChildId::name would split it: {call_id}"
        );
        let planned = match self.admit(tasks) {
            Ok(planned) => planned,
            Err(refusal) => return self.refuse(call_id, refusal),
        };
        self.run_children(call_id, planned, call).await
    }

    /// Validate a call whole, and reserve its children against the per-turn
    /// cap only once every check has passed — a refused call starts nothing
    /// and spends nothing of the cap (BR-3).
    ///
    /// The order: the two caps (the numbers the model is most likely to need
    /// to change), then the names — bound first, so a refusal never echoes a
    /// runaway name — then the task texts.
    fn admit(&self, tasks: Vec<ChildTask>) -> Result<Vec<Planned>, AgentRefusal> {
        let requested = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
        let per_call = self.config.max_children_per_call;
        if requested > per_call {
            return Err(AgentRefusal::TooManyChildren {
                requested,
                cap: per_call,
            });
        }
        let per_turn = self.config.max_children_per_turn;
        let already = self.started.load(Ordering::SeqCst);
        if already.saturating_add(requested) > per_turn {
            return Err(AgentRefusal::ChildCapReached {
                started: already,
                requested,
                cap: per_turn,
            });
        }

        let planned: Vec<Planned> = tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| Planned {
                name: task
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| format!("child-{}", index + 1)),
                task,
            })
            .collect();
        if let Some(long) = planned
            .iter()
            .find(|p| p.name.chars().count() > NAME_MAX_CHARS as usize)
        {
            return Err(AgentRefusal::NameTooLong {
                name: echo_bounded(&long.name),
                max: NAME_MAX_CHARS,
            });
        }
        for (index, p) in planned.iter().enumerate() {
            if planned[..index]
                .iter()
                .any(|earlier| earlier.name == p.name)
            {
                return Err(AgentRefusal::DuplicateName {
                    name: p.name.clone(),
                });
            }
        }
        if let Some(index) = planned.iter().position(|p| p.task.task.trim().is_empty()) {
            return Err(AgentRefusal::EmptyTask {
                index: u32::try_from(index).unwrap_or(u32::MAX),
            });
        }

        // The reservation, checked again as one step: the load above was a
        // read, and this is what makes the cap a cap.
        self.started
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |started| {
                let after = started.saturating_add(requested);
                (after <= per_turn).then_some(after)
            })
            .map_err(|started| AgentRefusal::ChildCapReached {
                started,
                requested,
                cap: per_turn,
            })?;
        Ok(planned)
    }

    /// Refuse a call whole: say so on the bus, and tell the model which number
    /// to change (BR-3).
    fn refuse(&self, call_id: String, refusal: AgentRefusal) -> ToolOutcome {
        let message = refusal_message(&refusal);
        self.parent
            .events
            .agent_event(Event::AgentCallRefused(AgentCallRefused {
                call_id,
                refusal,
            }));
        ToolOutcome::error(message)
    }

    /// Start every child of an accepted call and wait for all of them (BR-4).
    async fn run_children(
        &self,
        call_id: String,
        planned: Vec<Planned>,
        call: AgentCall<'_>,
    ) -> ToolOutcome {
        let accepted_at = Instant::now();
        let ids: Vec<ChildId> = planned
            .iter()
            .map(|p| ChildId::new(&call_id, &p.name))
            .collect();
        // ADR-4: the prompt's headroom **now**, split equally among this
        // call's children — read off the live accumulator, never a snapshot.
        let spent = self.parent.prompt_spend.as_ref().map_or(0, |s| s.spent());
        let pool = SharePool::new(headroom(self.parent.spend_ceiling, spent), &ids);
        // ADR-5: one consent queue per call.
        let consent = Arc::new(tokio::sync::Mutex::new(()));

        self.parent
            .events
            .agent_event(Event::AgentCallStarted(AgentCallStarted {
                call_id: call_id.clone(),
                parent_turn_id: self.parent.turn_id.clone(),
                children: planned
                    .iter()
                    .map(|p| PlannedChild {
                        name: p.name.clone(),
                        requested_tier: p.task.tier,
                    })
                    .collect(),
            }));

        let mut flight = Flight::new(
            call_id,
            self.parent.events.clone(),
            self.runtime.clone(),
            accepted_at,
        );
        for (p, child_id) in planned.into_iter().zip(ids) {
            let spend = ChildSpend::new(
                child_id.clone(),
                Arc::clone(&pool),
                self.parent.prompt_spend.clone(),
            );
            let spec = self.spec_for(p, child_id, spend, &consent, &call);
            let dispatcher = Arc::clone(&self.dispatcher);
            flight.launch(
                spec,
                move |spec| async move { dispatcher.run_child(spec).await },
            );
        }
        flight.land().await;
        let outcomes = flight.close();
        result_of(&outcomes)
    }

    /// One child's spec — the per-child half of what the dispatcher needs.
    fn spec_for(
        &self,
        planned: Planned,
        child_id: ChildId,
        spend: ChildSpend,
        consent: &Arc<tokio::sync::Mutex<()>>,
        call: &AgentCall<'_>,
    ) -> ChildSpec {
        let Planned { name, task } = planned;
        ChildSpec {
            child_id,
            name,
            task: task.task,
            context: task.context,
            tier: task.tier,
            provenance: call.provenance.clone(),
            max_turns: self.config.child_max_turns,
            parent_max_turns: call.parent_max_turns,
            deadline: Duration::from_secs(self.config.child_deadline_secs),
            report_max_bytes: self.config.report_max_bytes,
            spend,
            consent: Arc::clone(consent),
            cancelled: ChildOutcomeSlot::default(),
        }
    }
}

/// Parse `{ tasks: [...] }`; a call with no tasks is malformed, not refused —
/// the schema says `minItems: 1`, and there is no cap a zero could exceed.
fn parse(arguments: &Value) -> Result<Vec<ChildTask>, ToolError> {
    let args: AgentArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
        ToolError::args(format!(
            "`agent` takes {{\"tasks\": [{{\"task\": \"…\", \"name\"?, \"tier\"?, \"context\"?}}]}}: {err}"
        ))
    })?;
    if args.tasks.is_empty() {
        return Err(ToolError::args(
            "`tasks` is empty: give at least one task to dispatch",
        ));
    }
    Ok(args.tasks)
}

/// A name past the bound, cut to the bound for echoing — a refusal names the
/// offending name, and a runaway one must not ride the bus whole.
fn echo_bounded(name: &str) -> String {
    let mut cut: String = name.chars().take(NAME_MAX_CHARS as usize).collect();
    cut.push('…');
    cut
}

/// The sentence the model reads when its call was refused whole (BR-3): the
/// code first, so the model and a reader can key on it, then the numbers and
/// the config key that set the cap.
///
/// Composed here from integers and the model's own names, and returned
/// unframed — it asks the model to change the call, which the untrusted
/// envelope's closing sentence would contradict (the repeat refusal's
/// posture, REQ-617 BR-5).
#[must_use]
pub fn refusal_message(refusal: &AgentRefusal) -> String {
    let why = match refusal {
        AgentRefusal::TooManyChildren { requested, cap } => format!(
            "{requested} tasks in one call, and at most {cap} are allowed per call \
             (agent.max_children_per_call). Split the work across calls."
        ),
        AgentRefusal::ChildCapReached {
            started,
            requested,
            cap,
        } => format!(
            "this turn has already started {started} children and this call asks for \
             {requested} more; at most {cap} are allowed per turn \
             (agent.max_children_per_turn). Do the remaining work yourself, or finish."
        ),
        AgentRefusal::DuplicateName { name } => {
            format!("two tasks are named `{name}`; names must be unique within a call.")
        }
        AgentRefusal::EmptyTask { index } => {
            format!("the task at index {index} has no text; every task needs a `task`.")
        }
        AgentRefusal::NameTooLong { name, max } => {
            format!("the task name `{name}` is longer than {max} characters.")
        }
    };
    format!("{}: {why} No child was started.", refusal.code())
}

/// The tool result an accepted call returns: every child's [`ChildResult`], in
/// task order, as JSON framed as untrusted data — a report is model output
/// about repository content, never instructions — with the union of every
/// child's provenance on the block (ADR-8, BR-9).
fn result_of(outcomes: &[ChildOutcome]) -> ToolOutcome {
    let results: Vec<&ChildResult> = outcomes.iter().map(|o| &o.result).collect();
    let content = match serde_json::to_string_pretty(&results) {
        Ok(content) => content,
        Err(err) => {
            return ToolOutcome::error(format!("the children's results did not serialize: {err}"))
        }
    };
    let mut union = Provenance::empty();
    for outcome in outcomes {
        union.merge(&outcome.provenance);
    }
    ToolOutcome::ok(content)
        .with_provenance(block_provenance(&union))
        // BR-11: framed as data, and folded whole — never through the
        // `digest` duty, which would hand the parent a summary of the array
        // instead of the typed results it keys on.
        .with_disposition(ResultDisposition::UntrustedWhole)
}

/// The children's provenance union as the result block's [`ToolProvenance`] —
/// through the one precedence [`ToolProvenance::from_bits`] defines, so a
/// child's boundary touch pins its parent permanently, a child's unknown pins
/// it liftably, and the ids ride along either way.
fn block_provenance(union: &Provenance) -> ToolProvenance {
    ToolProvenance::from_bits(
        union.ids().cloned().collect(),
        UnknownReach::from_parts(union.is_unknown(), union.unknown_reason()),
        union.is_boundary_touch(),
    )
}

// ---------------------------------------------------------------------------
// The call in flight
// ---------------------------------------------------------------------------

/// One child as the call tracks it: what it is called, where an aborted run
/// leaves its outcome, and its spend wiring.
struct Child {
    id: ChildId,
    name: String,
    slot: ChildOutcomeSlot,
    spend: ChildSpend,
}

/// A call's children in flight: the [`JoinSet`] they run in, and what the
/// call publishes as each one lands (ADR-2).
///
/// **Dropped mid-flight** — the parent turn cancelled — it aborts every child
/// and hands the set to a reaper task that waits for the aborts to land and
/// publishes each child's `cancelled` finish and the call's end, so a client
/// watching the session sees the children stop rather than run forever
/// (BR-10). The reaper is itself a `Flight`, marked so its own drop never
/// spawns another.
struct Flight {
    call_id: String,
    events: SessionEvents,
    runtime: Handle,
    accepted_at: Instant,
    children: Vec<Child>,
    landed: Vec<Option<ChildOutcome>>,
    set: JoinSet<ChildOutcome>,
    index_of: HashMap<tokio::task::Id, usize>,
    reaper: bool,
}

impl Flight {
    fn new(call_id: String, events: SessionEvents, runtime: Handle, accepted_at: Instant) -> Self {
        Self {
            call_id,
            events,
            runtime,
            accepted_at,
            children: Vec::new(),
            landed: Vec::new(),
            set: JoinSet::new(),
            index_of: HashMap::new(),
            reaper: false,
        }
    }

    /// Spawn one child — `run(spec)` — on the runtime, into the set.
    fn launch<F, Fut>(&mut self, spec: ChildSpec, run: F)
    where
        F: FnOnce(ChildSpec) -> Fut,
        Fut: std::future::Future<Output = ChildOutcome> + Send + 'static,
    {
        self.children.push(Child {
            id: spec.child_id.clone(),
            name: spec.name.clone(),
            slot: spec.cancelled.clone(),
            spend: spec.spend.clone(),
        });
        self.landed.push(None);
        let handle = self.set.spawn_on(run(spec), &self.runtime);
        self.index_of.insert(handle.id(), self.children.len() - 1);
    }

    /// Wait for every child, publishing each one's finish as it lands.
    async fn land(&mut self) {
        while let Some(joined) = self.set.join_next_with_id().await {
            let (index, outcome) = match joined {
                Ok((id, outcome)) => (self.index_of[&id], outcome),
                Err(err) => {
                    let index = self.index_of[&err.id()];
                    (index, lost(&err, &self.children[index]))
                }
            };
            publish_finished(&self.events, &self.children[index].id, &outcome);
            self.landed[index] = Some(outcome);
        }
    }

    /// Every child has landed: publish the call's end and hand back the
    /// outcomes in task order.
    fn close(mut self) -> Vec<ChildOutcome> {
        let outcomes: Vec<ChildOutcome> = std::mem::take(&mut self.landed)
            .into_iter()
            .zip(&self.children)
            .map(|(landed, child)| {
                landed.unwrap_or_else(|| ChildOutcome::cancelled_before_start(child.name.clone()))
            })
            .collect();
        self.events
            .agent_event(Event::AgentCallFinished(AgentCallFinished {
                call_id: self.call_id.clone(),
                children: outcomes
                    .iter()
                    .map(|o| FinishedChild {
                        name: o.result.name.clone(),
                        status: o.status(),
                    })
                    .collect(),
                total_cost_micro_cents: outcomes.iter().map(|o| o.result.cost_micro_cents).sum(),
                elapsed_ms: u64::try_from(self.accepted_at.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            }));
        outcomes
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if self.set.is_empty() {
            return;
        }
        // BR-10: cancelling the parent turn cancels every running child — the
        // abort reaches each runner, which aborts its own work task.
        self.set.abort_all();
        if self.reaper {
            return;
        }
        let rest = Flight {
            call_id: std::mem::take(&mut self.call_id),
            events: self.events.clone(),
            runtime: self.runtime.clone(),
            accepted_at: self.accepted_at,
            children: std::mem::take(&mut self.children),
            landed: std::mem::take(&mut self.landed),
            set: std::mem::take(&mut self.set),
            index_of: std::mem::take(&mut self.index_of),
            reaper: true,
        };
        self.runtime.spawn(async move {
            let mut rest = rest;
            rest.land().await;
            let _ = rest.close();
        });
    }
}

/// The outcome of a child whose task did not return one (BR-10): aborted —
/// its runner's drop guard left the `cancelled` outcome in the slot, or it was
/// aborted before it was first polled — or panicked outside the runner's own
/// guard, which is reported `failed` with its share released.
///
/// The slot is read first either way. A panic that unwinds through the
/// runner drops its armed guard, which frames an outcome — **releasing the
/// share** — and leaves it there; releasing again here would find the child
/// already ended and move nothing, and the `agent_child_share_released` the
/// guard's release earned would never be published. So a guard's outcome
/// lends the panic its release (and the route, bounds and turns it had
/// stamped); only a panic that left nothing — a dispatcher with no guard —
/// is released here.
fn lost(err: &JoinError, child: &Child) -> ChildOutcome {
    let left = child.slot.take();
    if err.is_cancelled() {
        return left.unwrap_or_else(|| ChildOutcome::cancelled_before_start(child.name.clone()));
    }
    let (share_released, stamped) = match left {
        Some(left) => (left.share_released, Some(left.result)),
        None => (child.spend.release(), None),
    };
    ChildOutcome {
        result: ChildResult {
            name: child.name.clone(),
            status: ChildStatus::Failed,
            report: String::new(),
            refusal: None,
            error: Some(CHILD_PANICKED.to_owned()),
            turns_used: stamped.as_ref().map_or(0, |r| r.turns_used),
            route: stamped.as_ref().and_then(|r| r.route.clone()),
            bounds: stamped.as_ref().and_then(|r| r.bounds),
            cost_micro_cents: child.spend.spent(),
            spend_ceiling_final_micro_cents: child.spend.ceiling(),
        },
        provenance: Provenance::empty(),
        report_bytes: 0,
        truncated: false,
        share_released,
    }
}

/// `agent_child_finished` for one landed child, then — when its end moved
/// unspent share to running siblings — `agent_child_share_released` (BR-8).
/// The runner released the share; this says so.
fn publish_finished(events: &SessionEvents, child_id: &ChildId, outcome: &ChildOutcome) {
    events.agent_event(Event::AgentChildFinished(AgentChildFinished {
        child_id: child_id.clone(),
        status: outcome.status(),
        turns_used: outcome.result.turns_used,
        cost_micro_cents: outcome.result.cost_micro_cents,
        report_bytes: outcome.report_bytes,
        truncated: outcome.truncated,
    }));
    if let Some(release) = &outcome.share_released {
        events.agent_event(Event::AgentChildShareReleased(AgentChildShareReleased {
            child_id: child_id.clone(),
            released_micro_cents: release.released_micro_cents,
            recipients: release
                .recipients
                .iter()
                .map(|(child_id, ceiling)| ShareRecipient {
                    child_id: child_id.clone(),
                    new_ceiling_micro_cents: *ceiling,
                })
                .collect(),
        }));
    }
}

/// The model-facing description, stating `config`'s two caps.
///
/// A function rather than a `format!` inside [`AgentTool::new`] because two
/// things render it: the shipped tool, and the doc-only stand-in the two
/// prompt-margin sweeps register (`turn_loop::AgentToolDocs`), which cannot
/// build a real `AgentTool` — it holds a runtime [`Handle`] and a dispatcher,
/// and one of the sweeps is a sync `#[test]`. One renderer is what keeps the
/// bytes the sweeps measure the bytes the model reads; a second spelling beside
/// it would drift while the budget's tests stayed green (LESSON-481). The two
/// caps are rendered as decimal numbers, so the description grows with their
/// digit count — the reason the sweeps measure it at the largest caps the
/// config admits.
pub(crate) fn describe(config: &AgentConfig) -> String {
    format!(
        "Hand tasks to child turns that run concurrently and return their reports. Each \
         child starts with a fresh context — this session's system prompt and tools (minus \
         `agent`) and nothing of this conversation — so write everything it needs into \
         `task` (and optional `context`). The result is a JSON array, one entry per task, \
         with its `name`, `status` (completed, refused, cancelled, turns_exhausted, \
         budget_exhausted, spend_exhausted, timed_out, failed) and final `report`. At most \
         {} tasks per call and {} per turn; `tier` (reflex, scan, build, think) is a request \
         the router may not honour.",
        config.max_children_per_call, config.max_children_per_turn
    )
}

/// The schema the roster advertises (AC-1): `tasks: [{task, name?, tier?,
/// context?}]`, at most `per_call` of them.
///
/// `pub(crate)` for the same reason as [`describe`]: the sweeps' stand-in
/// renders its schema from here, and `maxItems` carries `per_call`'s digits.
pub(crate) fn schema(per_call: u32) -> Value {
    let tiers: Vec<&str> = [Tier::Reflex, Tier::Scan, Tier::Build, Tier::Think]
        .iter()
        .map(|tier| tier.as_str())
        .collect();
    json!({
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "minItems": 1,
                "maxItems": per_call,
                "description": "One entry per child turn; they run concurrently.",
                "items": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The child's only instruction, verbatim. It sees nothing else of this conversation."
                        },
                        "name": {
                            "type": "string",
                            "maxLength": NAME_MAX_CHARS,
                            "description": "A short label, unique within the call; defaults to child-<n>."
                        },
                        "tier": {
                            "type": "string",
                            "enum": tiers,
                            "description": "The model tier to request; the router decides."
                        },
                        "context": {
                            "type": "string",
                            "description": "Extra text for the child, beside the task."
                        }
                    },
                    "required": ["task"]
                }
            }
        },
        "required": ["tasks"]
    })
}

impl Tool for AgentTool {
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        schema(self.config.max_children_per_call)
    }

    /// Never the path a call takes: the loop awaits [`AgentTool::dispatch`]
    /// through [`Tool::as_agent`] (ADR-1). A caller that dispatches by name
    /// gets this typed refusal rather than a worker blocked for minutes.
    fn run(&self, _ctx: &ToolContext, _args: &Value) -> ToolOutcome {
        ToolOutcome::error(format!(
            "{ASYNC_DISPATCH_REFUSAL}: the `agent` tool runs child turns, which the turn loop \
             awaits on its async path; it cannot be run synchronously, and nothing was started"
        ))
    }

    /// The one implementation that answers `Some` (ADR-1).
    fn as_agent(&self) -> Option<&AgentTool> {
        Some(self)
    }
}

/// Register the `agent` tool into `reg` for a **prompt** turn — and only when
/// `agent.enabled` (BR-14). Returns whether it was registered.
///
/// The condition lives here, once, on the [`register_skill_tool`](super::register_skill_tool)
/// precedent: `enabled = false` is the tool's absence, not a refusal, so the
/// model meets the registry's ordinary unknown-tool answer — which
/// [`ToolRegistry::note_absent`] makes name the key. `dispatcher` is built only
/// when the tool is.
///
/// Registered **cap-exempt** with its own stated reason
/// ([`AGENT_CAP_EXEMPT_REASON`], ADR-7). A child's registry never calls this
/// (BR-2: depth is one).
pub fn register_agent_tool(
    reg: &mut ToolRegistry,
    config: &AgentConfig,
    parent: AgentParent,
    dispatcher: impl FnOnce() -> Arc<dyn ChildDispatcher>,
    runtime: Handle,
) -> bool {
    if !config.enabled {
        reg.note_absent(AGENT_TOOL_NAME, AGENT_DISABLED_NOTE);
        return false;
    }
    reg.register_cap_exempt(Arc::new(AgentTool::new(
        dispatcher(),
        *config,
        parent,
        runtime,
    )));
    true
}

/// What the unknown-tool answer adds when the model names `agent` in a session
/// that turned it off (BR-14, AC-1).
pub const AGENT_DISABLED_NOTE: &str =
    "`agent` is turned off in this session's configuration (agent.enabled = false)";

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use futures::future::BoxFuture;
    use futures::FutureExt;
    use teton_core::ProvenanceId;
    use teton_protocol::methods::PermissionOutcome;
    use teton_protocol::SessionId;

    use crate::broadcast::{EventBus, Subscription};
    use crate::harness::child::{ChildAskClock, ChildTaskScope, ChildToolCalls, PausableDeadline};
    use crate::harness::permissions::{
        PendingPermissions, PermissionConfig, PermissionDecision, PermissionGate, PermissionPolicy,
    };

    const SESSION: &str = "agent-tool-tests";
    const TURN: &str = "turn-9";

    type Run = Arc<dyn Fn(ChildSpec) -> BoxFuture<'static, ChildOutcome> + Send + Sync>;

    /// A dispatcher that records every spec it is handed and runs `run` on it —
    /// the runtime's place, taken by a closure, so the tool is under test and
    /// nothing else is.
    struct Stub {
        run: Run,
        seen: Arc<StdMutex<Vec<ChildSpec>>>,
    }

    #[async_trait]
    impl ChildDispatcher for Stub {
        async fn run_child(&self, spec: ChildSpec) -> ChildOutcome {
            self.seen.lock().unwrap().push(spec.clone());
            (self.run)(spec).await
        }
    }

    fn run<F, Fut>(f: F) -> Run
    where
        F: Fn(ChildSpec) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildOutcome> + Send + 'static,
    {
        Arc::new(move |spec| f(spec).boxed())
    }

    /// A child that completed with `report`.
    fn completed(spec: &ChildSpec, report: &str) -> ChildOutcome {
        ChildOutcome {
            result: ChildResult {
                name: spec.name.clone(),
                status: ChildStatus::Completed,
                report: report.to_owned(),
                refusal: None,
                error: None,
                turns_used: 1,
                route: None,
                bounds: None,
                cost_micro_cents: 0,
                spend_ceiling_final_micro_cents: spec.spend.ceiling(),
            },
            provenance: Provenance::empty(),
            report_bytes: report.len() as u64,
            truncated: false,
            share_released: None,
        }
    }

    /// Every child completes at once, reporting its task back.
    fn echo() -> Run {
        run(|spec: ChildSpec| async move {
            let report = format!("did: {}", spec.task);
            completed(&spec, &report)
        })
    }

    struct Fixture {
        bus: Arc<EventBus>,
        tool: AgentTool,
        seen: Arc<StdMutex<Vec<ChildSpec>>>,
    }

    fn parent(bus: &Arc<EventBus>) -> AgentParent {
        AgentParent {
            turn_id: TurnId::from(TURN),
            events: SessionEvents::new(Arc::clone(bus), SessionId::from(SESSION)),
            spend_ceiling: None,
            prompt_spend: None,
        }
    }

    fn fixture_with(
        config: AgentConfig,
        parent_of: impl Fn(&Arc<EventBus>) -> AgentParent,
        run: Run,
    ) -> Fixture {
        let bus = Arc::new(EventBus::new());
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let tool = AgentTool::new(
            Arc::new(Stub {
                run,
                seen: Arc::clone(&seen),
            }),
            config,
            parent_of(&bus),
            Handle::current(),
        );
        Fixture { bus, tool, seen }
    }

    fn fixture(run: Run) -> Fixture {
        fixture_with(AgentConfig::default(), parent, run)
    }

    /// `{tasks: [...]}` with `n` unnamed tasks.
    fn tasks(n: usize) -> Value {
        json!({ "tasks": (1..=n).map(|i| json!({ "task": format!("task {i}") })).collect::<Vec<_>>() })
    }

    async fn call(tool: &AgentTool, id: &str, arguments: Value) -> ToolOutcome {
        tool.dispatch(AgentCall {
            tool_call_id: id,
            arguments: &arguments,
            parent_max_turns: 40,
            provenance: Provenance::empty(),
        })
        .await
    }

    fn drained(sub: &mut Subscription) -> Vec<Event> {
        std::iter::from_fn(|| sub.try_recv())
            .map(|envelope| envelope.event)
            .collect()
    }

    fn refusals(events: &[Event]) -> Vec<(String, AgentRefusal)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::AgentCallRefused(r) => Some((r.call_id.clone(), r.refusal.clone())),
                _ => None,
            })
            .collect()
    }

    fn started_calls(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, Event::AgentCallStarted(_)))
            .count()
    }

    fn results(outcome: &ToolOutcome) -> Vec<ChildResult> {
        assert!(!outcome.is_error, "an accepted call: {}", outcome.content);
        serde_json::from_str(&outcome.content).expect("the result is a JSON array of ChildResult")
    }

    /// **BR-3 / AC-4: both caps refuse the whole call, typed, and start no
    /// child** — and so do a repeated name, an empty task and an overlong name.
    ///
    /// `agent` defaults: 5 per call, 8 per turn. Six tasks are refused
    /// `too_many_children` naming 6 and 5. Five pass (benign: the bound is
    /// inclusive) and start five children named `child-1`..`child-5`. A second
    /// call of four would make nine: `child_cap_reached` naming 5, 4 and 8. A
    /// second call of three makes eight and passes (benign: under the per-turn
    /// cap); one more is refused at 8 + 1. Each refusal publishes
    /// `agent_call_refused` and no `agent_call_started`, and the dispatcher is
    /// never reached.
    ///
    /// The name checks run on a fresh turn, and after all three refusals five
    /// and three children still fit — a refusal reserves nothing of the cap.
    ///
    /// # Mutations (run 2026-10-06/07, each reverted; over the 2,360 tests of
    /// the lib and the `repeat_refusal`, `cost_attribution`,
    /// `provenance_egress` and `boundary_coverage` binaries)
    ///
    /// - **Drop the per-call cap check**: 1 red, this test, at the six-task
    ///   leg (the per-turn cap admits six of eight, and six children start).
    /// - **Reserve before the name checks** (move the `fetch_update` above
    ///   them): 1 red, this test, at "a refusal reserves nothing".
    #[tokio::test]
    async fn caps_refuse_whole_and_typed() {
        let f = fixture(echo());
        let mut sub = f.bus.subscribe(1024);

        let six = call(&f.tool, "call-1", tasks(6)).await;
        assert!(six.is_error);
        assert!(
            six.content
                .starts_with("too_many_children: 6 tasks in one call")
                && six.content.contains("at most 5")
                && six.content.contains("agent.max_children_per_call")
                && six.content.ends_with("No child was started."),
            "{}",
            six.content
        );
        let events = drained(&mut sub);
        assert_eq!(
            refusals(&events),
            [(
                "turn-9:call-1".to_owned(),
                AgentRefusal::TooManyChildren {
                    requested: 6,
                    cap: 5
                }
            )]
        );
        assert_eq!(started_calls(&events), 0, "a refused call is not started");
        assert!(f.seen.lock().unwrap().is_empty(), "no child was dispatched");

        let five = call(&f.tool, "call-2", tasks(5)).await;
        let five = results(&five);
        assert_eq!(five.len(), 5, "benign: five is the cap, inclusive");
        let names: Vec<&str> = five.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["child-1", "child-2", "child-3", "child-4", "child-5"]
        );
        assert!(five.iter().all(|r| r.status == ChildStatus::Completed));
        assert_eq!(five[2].report, "did: task 3", "task order is result order");
        assert_eq!(
            f.seen.lock().unwrap()[0].child_id,
            ChildId::new("turn-9:call-2", "child-1"),
            "the id is `<call_id>/<name>`, the call id `<turn>:<tool call id>`"
        );
        assert_eq!(started_calls(&drained(&mut sub)), 1);

        let four = call(&f.tool, "call-3", tasks(4)).await;
        assert!(four.is_error);
        assert!(
            four.content.starts_with("child_cap_reached:"),
            "{}",
            four.content
        );
        assert_eq!(
            refusals(&drained(&mut sub)),
            [(
                "turn-9:call-3".to_owned(),
                AgentRefusal::ChildCapReached {
                    started: 5,
                    requested: 4,
                    cap: 8
                }
            )]
        );
        assert_eq!(
            f.seen.lock().unwrap().len(),
            5,
            "the refused call started nothing"
        );

        let three = call(&f.tool, "call-4", tasks(3)).await;
        assert_eq!(
            results(&three).len(),
            3,
            "benign: 5 + 3 is the per-turn cap"
        );
        let one = call(&f.tool, "call-5", tasks(1)).await;
        assert!(one.is_error);
        assert_eq!(
            refusals(&drained(&mut sub)),
            [(
                "turn-9:call-5".to_owned(),
                AgentRefusal::ChildCapReached {
                    started: 8,
                    requested: 1,
                    cap: 8
                }
            )]
        );
        assert_eq!(f.seen.lock().unwrap().len(), 8);

        // The names and texts, on a fresh turn's tool.
        let g = fixture(echo());
        let mut sub = g.bus.subscribe(1024);
        let dup = call(
            &g.tool,
            "call-1",
            json!({ "tasks": [{ "task": "a", "name": "audit" }, { "task": "b", "name": "audit" }] }),
        )
        .await;
        assert!(
            dup.content.starts_with("duplicate_name:"),
            "{}",
            dup.content
        );
        // An unnamed task's default collides like any name.
        let dup_default = call(
            &g.tool,
            "call-2",
            json!({ "tasks": [{ "task": "a" }, { "task": "b", "name": "child-1" }] }),
        )
        .await;
        assert!(dup_default.content.starts_with("duplicate_name:"));
        let empty = call(
            &g.tool,
            "call-3",
            json!({ "tasks": [{ "task": "a" }, { "task": "  \n" }] }),
        )
        .await;
        assert!(
            empty.content.starts_with("empty_task:"),
            "{}",
            empty.content
        );
        let long_name = "n".repeat(41);
        let long = call(
            &g.tool,
            "call-4",
            json!({ "tasks": [{ "task": "a", "name": long_name }] }),
        )
        .await;
        assert!(
            long.content.starts_with("name_too_long:"),
            "{}",
            long.content
        );
        assert_eq!(
            refusals(&drained(&mut sub))
                .into_iter()
                .map(|(_, refusal)| refusal)
                .collect::<Vec<_>>(),
            [
                AgentRefusal::DuplicateName {
                    name: "audit".to_owned()
                },
                AgentRefusal::DuplicateName {
                    name: "child-1".to_owned()
                },
                AgentRefusal::EmptyTask { index: 1 },
                AgentRefusal::NameTooLong {
                    name: format!("{}…", "n".repeat(40)),
                    max: 40
                },
            ]
        );
        assert!(g.seen.lock().unwrap().is_empty());
        // A 40-character name is within the bound.
        let at_bound = call(
            &g.tool,
            "call-5",
            json!({ "tasks": [{ "task": "a", "name": "n".repeat(40) }] }),
        )
        .await;
        assert_eq!(
            results(&at_bound).len(),
            1,
            "benign: 40 characters is the bound"
        );
        // A refusal reserves nothing: 1 + 5 + 2 is still the per-turn cap.
        assert_eq!(results(&call(&g.tool, "call-6", tasks(5)).await).len(), 5);
        assert_eq!(
            results(&call(&g.tool, "call-7", tasks(2)).await).len(),
            2,
            "a refusal reserves nothing of the per-turn cap"
        );

        // A malformed call is an argument error, not a typed refusal.
        let malformed = call(&g.tool, "call-8", json!({ "tasks": [] })).await;
        assert!(malformed.is_error && malformed.content.starts_with("invalid arguments:"));
        assert!(refusals(&drained(&mut sub)).is_empty());
    }

    /// **BR-4: the children of one call run concurrently, and the parent
    /// waits for every one of them.**
    ///
    /// Three children park on one barrier sized three, so the call can only
    /// complete if all three are running at once — a sequential dispatch
    /// deadlocks on the barrier and the bound below fails it. Past the
    /// barrier they finish in the reverse of task order. The result is
    /// returned only once all three are terminal: nothing is still running
    /// when it arrives, every `agent_child_finished` precedes
    /// `agent_call_finished`, and both the result and the call's tally list
    /// the children in task order, not finish order.
    ///
    /// # Mutations (run 2026-10-06/07, each reverted; same 2,360-test scope as
    /// `caps_refuse_whole_and_typed`)
    ///
    /// - **Await each child before starting the next** (`land` inside the
    ///   `launch` loop): 3 red — this test (the barrier is never reached),
    ///   `asks_serialise_and_grants_are_shared` and
    ///   `the_parent_emitter_stays_live_while_children_run` — and
    ///   `parent_cancel_aborts_every_child_and_each_reports_cancelled` **hangs**
    ///   (its first child parks for ever, so the second never starts and the
    ///   test waits on it with no bound); the run was killed to end it.
    /// - **Return on the first child to land** (`land` breaking after one): 6
    ///   red — this test, `caps_refuse_whole_and_typed`,
    ///   `asks_serialise_and_grants_are_shared`,
    ///   `parent_cancel_aborts_every_child_and_each_reports_cancelled`,
    ///   `result_is_untrusted_json_with_the_provenance_union` and
    ///   `shares_split_the_prompts_live_headroom`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn children_overlap_and_parent_waits_for_all() {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let child_run = {
            let (barrier, running, peak) = (barrier.clone(), running.clone(), peak.clone());
            run(move |spec: ChildSpec| {
                let (barrier, running, peak) = (barrier.clone(), running.clone(), peak.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    barrier.wait().await;
                    // `child-1` finishes last, `child-3` first.
                    let order: u64 = spec.name.trim_start_matches("child-").parse().unwrap();
                    tokio::time::sleep(Duration::from_millis(60 - 20 * order)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    completed(&spec, "done")
                }
            })
        };
        let f = fixture(child_run);
        let mut sub = f.bus.subscribe(1024);

        let outcome = tokio::time::timeout(Duration::from_secs(10), call(&f.tool, "call-1", tasks(3)))
            .await
            .expect("all three children were running at once: a sequential dispatch never passes the barrier");
        assert_eq!(peak.load(Ordering::SeqCst), 3, "three children overlapped");
        assert_eq!(
            running.load(Ordering::SeqCst),
            0,
            "nothing is still running when the parent gets its result"
        );
        let names: Vec<String> = results(&outcome).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["child-1", "child-2", "child-3"], "task order");

        let events = drained(&mut sub);
        let finished: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Event::AgentChildFinished(_)))
            .map(|(at, _)| at)
            .collect();
        let call_finished = events
            .iter()
            .position(|e| matches!(e, Event::AgentCallFinished(_)))
            .expect("agent_call_finished");
        assert_eq!(finished.len(), 3);
        assert!(
            finished.iter().all(|at| *at < call_finished),
            "every child finished before the call did"
        );
        let landed: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                Event::AgentChildFinished(f) => Some(f.child_id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            landed,
            [
                "turn-9:call-1/child-3",
                "turn-9:call-1/child-2",
                "turn-9:call-1/child-1"
            ],
            "published as each landed — the reverse of task order here"
        );
        let Event::AgentCallFinished(tally) = &events[call_finished] else {
            unreachable!()
        };
        let tally: Vec<&str> = tally.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            tally,
            ["child-1", "child-2", "child-3"],
            "the tally is in task order"
        );
    }

    /// **AC-7's tool half / BR-4: the parent's emitter stays live while its
    /// children run** — a child's event reaches a subscriber while the call is
    /// still awaiting it, not in a burst when the call returns (BUG-226).
    ///
    /// Each stub child publishes what the real runner publishes from inside a
    /// child's task — `agent_child_started` — and then holds until released.
    /// Both events are received, and the call is still running when they are.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_parent_emitter_stays_live_while_children_run() {
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let bus = Arc::new(EventBus::new());
        let child_run = {
            let (release, bus) = (release.clone(), bus.clone());
            run(move |spec: ChildSpec| {
                let (release, bus) = (release.clone(), bus.clone());
                async move {
                    bus.publish(
                        Some(SessionId::from(SESSION)),
                        Event::AgentChildStarted(teton_protocol::events::AgentChildStarted {
                            child_id: spec.child_id.clone(),
                            parent_turn_id: TurnId::from(TURN),
                            name: spec.name.clone(),
                            route: teton_protocol::agent::ChildRoute {
                                tier: None,
                                provider_id: teton_protocol::ProviderId::from("stub"),
                                model: "stub-1".to_owned(),
                            },
                            bounds: teton_protocol::agent::ChildBounds {
                                max_turns: 12,
                                context_budget_bytes: 65_536,
                                spend_ceiling_micro_cents: None,
                                deadline_secs: 600,
                            },
                        }),
                    );
                    release.acquire().await.expect("open").forget();
                    completed(&spec, "done")
                }
            })
        };
        let tool = AgentTool::new(
            Arc::new(Stub {
                run: child_run,
                seen: Arc::default(),
            }),
            AgentConfig::default(),
            parent(&bus),
            Handle::current(),
        );
        let mut sub = bus.subscribe(1024);
        let dispatch = tokio::spawn(async move { call(&tool, "call-1", tasks(2)).await });

        let mut from_children = 0;
        while from_children < 2 {
            let envelope = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("a child's event arrives while the call is running")
                .expect("the bus is open");
            if let Event::AgentChildStarted(_) = envelope.event {
                from_children += 1;
            }
        }
        assert!(
            !dispatch.is_finished(),
            "the events arrived while the call was still awaiting its children"
        );
        release.add_permits(2);
        let outcome = dispatch.await.expect("the call joins");
        assert_eq!(results(&outcome).len(), 2);
    }

    /// **BR-5 / ADR-5: one call's asks reach the user one at a time, and a
    /// grant answered for one child answers its sibling.**
    ///
    /// The session's gate asks about `shell`. Two children ask at once: one
    /// question is out, carrying its child's id and labelled by an
    /// `agent_child_consent_requested`; while it is open no second question
    /// appears, and both children's clocks are stopped — the asker's by the
    /// gate's observer, the queued sibling's by its queue. The first is
    /// answered "allow for this session", and the sibling runs on that grant
    /// without a question of its own.
    ///
    /// Benign path: a child whose question a remembered grant (or the level)
    /// already answers never queues — answered at once while its call's
    /// consent queue is held by someone else.
    ///
    /// # Mutations (run 2026-10-06/07, each reverted; same 2,360-test scope as
    /// `caps_refuse_whole_and_typed`) — 1 red apiece, this test, and nothing
    /// else
    ///
    /// - **Remove the mutex** (`queue_for_consent` replaced by a fresh mutex
    ///   per ask in the gate's `settle`): at "one question at a time".
    /// - **A mutex per child** (the tool handing each spec its own): the same
    ///   red — the queue only serialises what shares it.
    /// - **No grant re-check after queueing**: a second question is raised and
    ///   the call does not finish on the one answer.
    /// - **No child id on the request** (`PermissionRequest.child_id: None`):
    ///   at "the question names the child that asked".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn asks_serialise_and_grants_are_shared() {
        let bus = Arc::new(EventBus::new());
        let pending = Arc::new(PendingPermissions::new());
        let mut table = PermissionConfig::with_default(PermissionPolicy::Allow);
        table.set("shell", PermissionPolicy::Ask);
        let gate = Arc::new(
            PermissionGate::new(
                SessionId::from(SESSION),
                table,
                Arc::clone(&bus),
                Arc::clone(&pending),
            )
            .with_ask_observer(Arc::new(ChildAskClock::default())),
        );
        let clocks: Arc<StdMutex<HashMap<String, PausableDeadline>>> = Arc::default();
        let child_run = {
            let (gate, clocks) = (gate.clone(), clocks.clone());
            run(move |spec: ChildSpec| {
                let (gate, clocks) = (gate.clone(), clocks.clone());
                async move {
                    let deadline = PausableDeadline::start(Duration::from_secs(600));
                    clocks
                        .lock()
                        .unwrap()
                        .insert(spec.name.clone(), deadline.clone());
                    let scope = ChildTaskScope {
                        child_id: spec.child_id.clone(),
                        name: spec.name.clone(),
                        parent_turn_id: TurnId::from(TURN),
                        deadline,
                        consent: Arc::clone(&spec.consent),
                        tool_calls: ChildToolCalls::default(),
                    };
                    let decision = scope.scope(gate.authorize("shell", None)).await;
                    let said = if decision == PermissionDecision::Allowed {
                        "allowed"
                    } else {
                        "denied"
                    };
                    completed(&spec, said)
                }
            })
        };
        let tool = AgentTool::new(
            Arc::new(Stub {
                run: child_run,
                seen: Arc::default(),
            }),
            AgentConfig::default(),
            parent(&bus),
            Handle::current(),
        );
        let mut sub = bus.subscribe(1024);
        let dispatch = tokio::spawn(async move {
            call(
                &tool,
                "call-1",
                json!({ "tasks": [{ "task": "build it", "name": "first" }, { "task": "test it", "name": "second" }] }),
            )
            .await
        });

        // The one question, and its label.
        let mut label = None;
        let request = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("a child asks")
                .expect("the bus is open");
            match envelope.event {
                Event::AgentChildConsentRequested(l) => label = Some(l),
                Event::PermissionRequest(request) => break request,
                _ => {}
            }
        };
        let label = label.expect("the ask is labelled with the child before it is asked");
        assert_eq!(label.tool, "shell");
        assert_eq!(
            request.child_id,
            Some(ChildId::new("turn-9:call-1", &label.name)),
            "the question names the child that asked"
        );
        assert_eq!(request.parent_turn_id, Some(TurnId::from(TURN)));

        // While it is open, the sibling is queued — not asking.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let meanwhile = drained(&mut sub);
        assert!(
            !meanwhile
                .iter()
                .any(|e| matches!(e, Event::PermissionRequest(_))),
            "one question at a time: a second reached the user while the first was open"
        );
        {
            let clocks = clocks.lock().unwrap();
            assert_eq!(clocks.len(), 2, "both children are running");
            for (name, clock) in clocks.iter() {
                assert!(
                    clock.is_paused(),
                    "`{name}`'s clock runs while it waits on the user"
                );
            }
        }

        assert!(pending.resolve(
            &request.request_id,
            PermissionOutcome::Selected {
                option_id: "allow_always".to_owned()
            }
        ));
        let outcome = tokio::time::timeout(Duration::from_secs(5), dispatch)
            .await
            .expect("both children finish once the one question is answered")
            .expect("the call joins");
        let said: Vec<String> = results(&outcome).into_iter().map(|r| r.report).collect();
        assert_eq!(said, ["allowed", "allowed"]);
        assert!(
            !drained(&mut sub)
                .iter()
                .any(|e| matches!(e, Event::PermissionRequest(_))),
            "the sibling ran on the grant: no second question"
        );

        // Benign: a question already answered never queues.
        let held = Arc::new(tokio::sync::Mutex::new(()));
        let _queue_is_busy = Arc::clone(&held).lock_owned().await;
        for tool in ["shell", "read"] {
            let scope = ChildTaskScope {
                child_id: ChildId::new("turn-9:call-2", "late"),
                name: "late".to_owned(),
                parent_turn_id: TurnId::from(TURN),
                deadline: PausableDeadline::start(Duration::from_secs(600)),
                consent: Arc::clone(&held),
                tool_calls: ChildToolCalls::default(),
            };
            let decision = tokio::time::timeout(
                Duration::from_secs(1),
                scope.scope(gate.authorize(tool, None)),
            )
            .await
            .unwrap_or_else(|_| panic!("`{tool}` queued although nothing needed asking"));
            assert_eq!(decision, PermissionDecision::Allowed);
        }
    }

    /// **BR-10: cancelling the parent turn cancels every running child, and
    /// each one still reports `cancelled`.**
    ///
    /// The parent is cancelled the way the daemon cancels a turn — its task is
    /// aborted, dropping the `agent` call's future mid-await. Both children
    /// were running; both are dropped (the abort reached them), and the
    /// session still hears `agent_child_finished { cancelled }` for each and
    /// the call's `agent_call_finished`. The outcomes published are the ones
    /// the children's runners left in their slots on the way down — `turns_used:
    /// 7` marks them — not a fabricated stand-in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parent_cancel_aborts_every_child_and_each_reports_cancelled() {
        struct LeaveCancelled {
            slot: ChildOutcomeSlot,
            name: String,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for LeaveCancelled {
            fn drop(&mut self) {
                let mut outcome = ChildOutcome::cancelled_before_start(self.name.clone());
                outcome.result.turns_used = 7;
                self.slot.put(outcome);
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let child_run = {
            let (dropped, entered) = (dropped.clone(), entered.clone());
            run(move |spec: ChildSpec| {
                let (dropped, entered) = (dropped.clone(), entered.clone());
                async move {
                    let _guard = LeaveCancelled {
                        slot: spec.cancelled.clone(),
                        name: spec.name.clone(),
                        dropped,
                    };
                    entered.add_permits(1);
                    std::future::pending::<ChildOutcome>().await
                }
            })
        };
        let f = fixture(child_run);
        let mut sub = f.bus.subscribe(1024);
        let tool = f.tool;
        let parent_turn = tokio::spawn(async move { call(&tool, "call-1", tasks(2)).await });
        let _ = entered.acquire_many(2).await.expect("both children run");

        parent_turn.abort();
        assert!(parent_turn.await.unwrap_err().is_cancelled());

        let mut finished = Vec::new();
        let tally = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("the call's end is still published")
                .expect("the bus is open");
            match envelope.event {
                Event::AgentChildFinished(f) => finished.push(f),
                Event::AgentCallFinished(tally) => break tally,
                _ => {}
            }
        };
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            2,
            "the abort reached both children"
        );
        assert_eq!(finished.len(), 2);
        for child in &finished {
            assert_eq!(child.status, ChildStatus::Cancelled);
            assert_eq!(
                child.turns_used, 7,
                "the runner's own outcome, from its slot"
            );
        }
        assert!(tally
            .children
            .iter()
            .all(|c| c.status == ChildStatus::Cancelled));
    }

    /// **ADR-8 / BR-9: the result is the children's results as JSON, framed
    /// as untrusted data, and its block carries the union of their
    /// provenance.**
    ///
    /// `a` read `notes/a.md`; `b`'s context was unknown. The block is
    /// `UnknownWith({notes/a.md})` — fail-closed, still naming what was proved
    /// — so the parent's next call is judged by both. The content is the
    /// `ChildResult` array, nothing else, in task order.
    ///
    /// # Mutation (run 2026-10-06/07, reverted; same 2,360-test scope as
    /// `caps_refuse_whole_and_typed`)
    ///
    /// - **Drop the union** (`result_of` leaving the block `none()`): 2 red —
    ///   this test at the provenance assertion, and
    ///   `provenance_egress::an_agent_childs_boundary_read_blocks_the_parents_next_remote_turn`,
    ///   where the secret then leaves on the parent's next request.
    #[tokio::test]
    async fn result_is_untrusted_json_with_the_provenance_union() {
        let read = ProvenanceId::from_resolved(Path::new("/repo"), Path::new("/repo/notes/a.md"))
            .expect("a canonical id");
        let child_run = {
            let read = read.clone();
            run(move |spec: ChildSpec| {
                let read = read.clone();
                async move {
                    let mut outcome = completed(&spec, "found it");
                    outcome.provenance = if spec.name == "a" {
                        Provenance::tainted_by(read)
                    } else {
                        Provenance::unknown()
                    };
                    outcome
                }
            })
        };
        let f = fixture(child_run);
        let outcome = call(
            &f.tool,
            "call-1",
            json!({ "tasks": [{ "task": "x", "name": "a" }, { "task": "y", "name": "b" }] }),
        )
        .await;
        assert_eq!(outcome.disposition, ResultDisposition::UntrustedWhole);
        assert_eq!(
            outcome.provenance,
            ToolProvenance::UnknownWith(std::iter::once(read).collect(), None)
        );
        let names: Vec<String> = results(&outcome).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// **ADR-1: the loop awaits `agent` on its async path; `run` is never the
    /// way in.**
    ///
    /// A turn whose model calls `agent` gets the children's results folded
    /// into its context — framed as untrusted data — and the stub's report is
    /// in it. `Tool::run`, reached by name through the registry, answers the
    /// typed `agent_requires_async_dispatch` refusal and starts nothing.
    ///
    /// # Mutation (run 2026-10-06/07, reverted; same 2,360-test scope as
    /// `caps_refuse_whole_and_typed`)
    ///
    /// - **Remove the `as_agent` arm** (every call through
    ///   `block_in_place_if_multithread(|| tools.dispatch(..))`, so `run`'s
    ///   refusal is what the model gets): 3 red — this test, and the two that
    ///   drive a real loop: `repeat_refusal::agent_is_write_capable_third_identical_refused`
    ///   and `provenance_egress::an_agent_childs_boundary_read_blocks_the_parents_next_remote_turn`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_loop_awaits_agent_and_never_runs_it() {
        use crate::harness::completion::{CompletionSource, SourceTurn, TurnDecision};
        use crate::harness::context::{ContextManager, NoopProvenanceHook, PreparedPrompt};
        use crate::harness::duty::DutyRoute;
        use crate::harness::turn_loop::{
            run_session_turn_with_source, HarnessConfig, HarnessError,
        };
        use crate::harness::ToolDuties;

        struct CallsAgentOnce(usize);

        #[async_trait]
        impl CompletionSource for CallsAgentOnce {
            fn chat_format(&self) -> teton_inference::ChatFormat {
                teton_inference::ChatFormat::Flat
            }

            async fn produce_turn(
                &mut self,
                _prompt: &PreparedPrompt,
                _provenance: &Provenance,
                _config: &HarnessConfig,
                _tools: &ToolRegistry,
                _exposed: &[&str],
                _on_token: &mut (dyn for<'s> FnMut(&'s str) + Send),
            ) -> Result<SourceTurn, HarnessError> {
                self.0 += 1;
                let decision = if self.0 == 1 {
                    TurnDecision::ToolCall {
                        name: AGENT_TOOL_NAME.to_owned(),
                        arguments: json!({ "tasks": [{ "task": "look around", "name": "scout" }] }),
                    }
                } else {
                    TurnDecision::EndTurn {
                        final_text: "Done.".to_owned(),
                    }
                };
                Ok(SourceTurn {
                    text: String::new(),
                    decision,
                    usage: teton_providers::TokenUsage::default(),
                    dropped_calls: 0,
                    cache: None,
                    call_in_text: false,
                })
            }
        }

        let bus = Arc::new(EventBus::new());
        let seen: Arc<StdMutex<Vec<ChildSpec>>> = Arc::default();
        let mut tools = ToolRegistry::with_builtins();
        tools.register_cap_exempt(Arc::new(AgentTool::new(
            Arc::new(Stub {
                run: run(|spec: ChildSpec| async move { completed(&spec, "SCOUT-REPORT") }),
                seen: Arc::clone(&seen),
            }),
            AgentConfig::default(),
            parent(&bus),
            Handle::current(),
        )));
        let ctx_root = ToolContext::new(std::env::temp_dir());

        // `run` by name: the typed refusal, and nothing started.
        let by_name = tools.dispatch(
            AGENT_TOOL_NAME,
            &ctx_root,
            &json!({ "tasks": [{ "task": "x" }] }),
        );
        assert!(by_name.is_error);
        assert!(
            by_name.content.starts_with(ASYNC_DISPATCH_REFUSAL),
            "{}",
            by_name.content
        );
        assert!(seen.lock().unwrap().is_empty());

        let gate = PermissionGate::new(
            SessionId::from(SESSION),
            crate::harness::permissions::table_for(
                teton_protocol::permissions::PermissionLevel::Guarded,
            ),
            Arc::clone(&bus),
            Arc::new(PendingPermissions::new()),
        );
        let events = SessionEvents::new(Arc::clone(&bus), SessionId::from(SESSION));
        let mut ctx = ContextManager::new("sys", 1_000_000);
        ctx.push_user("send a scout");
        run_session_turn_with_source(
            &mut CallsAgentOnce(0),
            &tools,
            &ctx_root,
            &gate,
            &events,
            &mut ctx,
            &HarnessConfig::default(),
            &mut NoopProvenanceHook,
            &DutyRoute::unresolved("no digest here"),
            &DutyRoute::unresolved("no compact here"),
            &ToolDuties {
                triage: &DutyRoute::unresolved("no triage here"),
                shell: &DutyRoute::unresolved("no shell duty here"),
            },
        )
        .await
        .expect("the turn completes");

        let folded = ctx
            .blocks()
            .iter()
            .rev()
            .find(|b| b.role == crate::harness::context::BlockRole::Tool)
            .map(|b| b.text.clone())
            .expect("the result was folded");
        assert!(
            folded.contains("SCOUT-REPORT"),
            "the child's report: {folded}"
        );
        assert!(!folded.contains(ASYNC_DISPATCH_REFUSAL), "{folded}");
        assert_eq!(seen.lock().unwrap().len(), 1, "one child ran");
        assert_eq!(
            seen.lock().unwrap()[0].parent_max_turns,
            HarnessConfig::default().max_turns,
            "the parent's own cap rides to the child (BR-7)"
        );
    }

    /// **BR-12 / BR-2: a child's registry has `skill` and not `agent`** —
    /// the registry the daemon builds, through the runtime's own fixture.
    ///
    /// The prompt turn's registry holds both; the child's holds `skill` and
    /// answers a call to `agent` with the ordinary unknown-tool refusal, which
    /// names no config key (the tool is not off — a child simply cannot
    /// dispatch).
    #[tokio::test]
    async fn child_registry_has_skill_not_agent() {
        let prompt = crate::runtime::testsupport::turn_registry(false, true).await;
        assert!(
            prompt.get(AGENT_TOOL_NAME).is_some(),
            "non-vacuity: the parent has `agent`"
        );
        assert!(prompt.get("skill").is_some());

        let child = crate::runtime::testsupport::turn_registry(true, true).await;
        assert!(
            child.get("skill").is_some(),
            "BR-12: a child may invoke skills"
        );
        assert!(child.get(AGENT_TOOL_NAME).is_none(), "BR-2: depth is one");
        let answer = child.dispatch(
            AGENT_TOOL_NAME,
            &ToolContext::new(std::env::temp_dir()),
            &json!({ "tasks": [{ "task": "x" }] }),
        );
        assert!(answer.is_error && answer.content.starts_with("unknown tool `agent`"));
        assert!(
            !answer.content.contains("agent.enabled"),
            "{}",
            answer.content
        );
    }

    /// **BR-8 / BR-10: a runner that panics past its own guard is reported
    /// `failed` with `child_panicked` — and the share its guard released while
    /// the panic unwound is announced, not lost.**
    ///
    /// The stub plays the daemon's runner: on the way down it does what
    /// `CancelGuard` does in `child_turn.rs` — frames an outcome, releasing the
    /// child's share, and leaves it in the slot — and then panics. Two
    /// children share a 1,000 pool (500 each); `doomed` spends nothing, so its
    /// 500 goes to `steady`, which is still running, and lifts it to 1,000.
    ///
    /// Benign: `steady` completes as usual, and its result reports the raised
    /// ceiling.
    ///
    /// Mutation (run 2026-10-07, reverted): `lost` releasing again instead of
    /// reusing the slot's release (the pre-fix shape) — 1 red of the 2,320 lib
    /// tests, this one, at the share event (the second release finds the
    /// child ended and moves nothing).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_runner_is_failed_and_its_guards_release_is_published() {
        let doomed_left = Arc::new(tokio::sync::Notify::new());
        let child_run = {
            let doomed_left = Arc::clone(&doomed_left);
            run(move |spec: ChildSpec| {
                let doomed_left = Arc::clone(&doomed_left);
                async move {
                    if spec.name == "doomed" {
                        let mut left = ChildOutcome::cancelled_before_start(spec.name.clone());
                        left.result.turns_used = 3;
                        left.share_released = spec.spend.release();
                        spec.cancelled.put(left);
                        doomed_left.notify_one();
                        panic!("the runner itself panicked");
                    }
                    // Still running when `doomed` releases, so it receives.
                    doomed_left.notified().await;
                    completed(&spec, "steady")
                }
            })
        };
        let f = fixture_with(
            AgentConfig::default(),
            |bus| AgentParent {
                spend_ceiling: Some(1_000),
                prompt_spend: Some(Arc::new(PromptSpend::new())),
                ..parent(bus)
            },
            child_run,
        );
        let mut sub = f.bus.subscribe(1024);
        let outcome = call(
            &f.tool,
            "call-1",
            json!({ "tasks": [{ "task": "x", "name": "doomed" }, { "task": "y", "name": "steady" }] }),
        )
        .await;

        let got = results(&outcome);
        assert_eq!(got[0].status, ChildStatus::Failed, "{got:?}");
        assert_eq!(got[0].error.as_deref(), Some(CHILD_PANICKED));
        assert_eq!(got[0].turns_used, 3, "what the guard had stamped");
        assert_eq!(got[1].status, ChildStatus::Completed);
        assert_eq!(
            got[1].spend_ceiling_final_micro_cents,
            Some(1_000),
            "benign: the sibling received the doomed child's 500"
        );

        let released: Vec<AgentChildShareReleased> = drained(&mut sub)
            .into_iter()
            .filter_map(|e| match e {
                Event::AgentChildShareReleased(released) => Some(released),
                _ => None,
            })
            .collect();
        assert_eq!(
            released.len(),
            1,
            "the release the guard made while the panic unwound is announced: {released:?}"
        );
        assert_eq!(
            released[0].child_id,
            ChildId::new("turn-9:call-1", "doomed")
        );
        assert_eq!(released[0].released_micro_cents, 500);
    }

    /// **ADR-4: a call splits the prompt's live headroom among its children.**
    ///
    /// A 900 micro-cent ceiling with 300 already spent leaves 600; three
    /// children get 200 each — read off the prompt's accumulator at the call,
    /// not a snapshot. That their spend is then also the parent's is the
    /// `ChildSpend` wiring's claim, pinned in `cost::share`.
    #[tokio::test]
    async fn shares_split_the_prompts_live_headroom() {
        let prompt_spend = Arc::new(PromptSpend::new());
        prompt_spend.add(300);
        let f = fixture_with(
            AgentConfig::default(),
            {
                let prompt_spend = Arc::clone(&prompt_spend);
                move |bus| AgentParent {
                    spend_ceiling: Some(900),
                    prompt_spend: Some(Arc::clone(&prompt_spend)),
                    ..parent(bus)
                }
            },
            echo(),
        );
        let outcome = call(&f.tool, "call-1", tasks(3)).await;
        let ceilings: Vec<Option<u64>> = results(&outcome)
            .into_iter()
            .map(|r| r.spend_ceiling_final_micro_cents)
            .collect();
        assert_eq!(ceilings, [Some(200), Some(200), Some(200)]);
        let seen = f.seen.lock().unwrap();
        assert!(seen.iter().all(|spec| spec.spend.ceiling() == Some(200)));
    }

    /// **The byte-identity pin for the sweeps' stand-in** — REQ-587 ADR-9's
    /// `the_doc_only_tool_and_the_real_one_render_one_set_of_prompt_bytes`,
    /// for `agent`.
    ///
    /// The two prompt-margin sweeps (`egress::redact`'s
    /// `the_total_cap_clears_the_harness_context_budget_with_margin` and
    /// `web`'s `the_web_tool_docs_clear_the_outbound_body_overhead`) cannot
    /// build a real [`AgentTool`] — one is a sync `#[test]` and the tool holds a
    /// [`Handle`] and a dispatcher — so they register
    /// `turn_loop::AgentToolDocs`. A hand-typed stand-in would drift from the
    /// renderer while the margin tests stayed green; this compares the two
    /// tools' prompt surfaces directly, at three `[agent]` tables:
    ///
    /// - the **defaults** (5 per call, 8 per turn), what every session that
    ///   never wrote `[agent]` renders;
    /// - a **departed** table (2 and 3), so a stand-in that ignored its config
    ///   and rendered the defaults could not pass on the first row alone;
    /// - **both caps at `u32::MAX`**, the largest the config admits
    ///   (`validate_agent` bounds them from below only) — the row the sweeps
    ///   register as `AgentToolDocs::worst_case()`.
    ///
    /// And the worst case is a ceiling: its docs are exactly **27 bytes**
    /// longer than the defaults' — nine more digits in each of the
    /// description's two caps and the schema's `maxItems`. The 27 is written
    /// out here rather than computed from either tool, so the expected value
    /// does not come from the subject.
    ///
    /// The growth assertion is the one that still stands if the `u32::MAX` row
    /// and `worst_case` were both walked back to a smaller table together.
    ///
    /// # Mutations (run 2026-10-07, each reverted by edit)
    ///
    /// - **`AgentToolDocs::input_schema` rendering `schema(5)`** (a stand-in
    ///   frozen at the default cap): red at the departed row's schema
    ///   assertion — the first row whose cap is not 5.
    /// - **`AgentToolDocs::worst_case` at the defaults**: red here at the
    ///   `u32::MAX` row's description assertion, and in both prompt-margin
    ///   sweeps at their BUG-193 pins (margins 235 / 282 against 208 / 255 —
    ///   the 27 bytes a sweep stops measuring).
    #[tokio::test]
    async fn the_doc_only_agent_tool_and_the_real_one_render_one_set_of_prompt_bytes() {
        use crate::harness::turn_loop::AgentToolDocs;

        let widest = AgentConfig {
            max_children_per_call: u32::MAX,
            max_children_per_turn: u32::MAX,
            ..AgentConfig::default()
        };
        let departed = AgentConfig {
            max_children_per_call: 2,
            max_children_per_turn: 3,
            ..AgentConfig::default()
        };
        for (label, config, docs) in [
            (
                "defaults",
                AgentConfig::default(),
                AgentToolDocs::new(&AgentConfig::default()),
            ),
            ("departed", departed, AgentToolDocs::new(&departed)),
            ("u32::MAX", widest, AgentToolDocs::worst_case()),
        ] {
            let real = fixture_with(config, parent, echo()).tool;
            assert_eq!(real.name(), docs.name(), "{label}");
            assert_eq!(
                real.description(),
                docs.description(),
                "{label}: the doc-only `agent` tool and the shipped one render different \
                 descriptions, so the two prompt-margin sweeps are measuring bytes the model \
                 never reads. Both must come from `agent::describe`."
            );
            assert_eq!(
                real.input_schema(),
                docs.input_schema(),
                "{label}: the doc-only `agent` tool and the shipped one render different input \
                 schemas. `ToolRegistry::docs` puts the schema in the resident prompt beside \
                 the description, so this is prompt bytes the sweeps would miss."
            );
        }

        // Non-vacuity: the description under comparison states the caps it was
        // built with, so two paths agreeing on it agree on the numbers too.
        assert!(
            describe(&departed).contains("At most 2 tasks per call and 3 per turn"),
            "{}",
            describe(&departed)
        );

        let rendered =
            |docs: &AgentToolDocs| docs.description().len() + docs.input_schema().to_string().len();
        let default_bytes = rendered(&AgentToolDocs::new(&AgentConfig::default()));
        let worst_bytes = rendered(&AgentToolDocs::worst_case());
        assert_eq!(
            worst_bytes.checked_sub(default_bytes),
            Some(27),
            "the worst case the sweeps register is not the defaults plus nine digits in each \
             of the three places a cap is rendered ({default_bytes} → {worst_bytes})"
        );
    }
}
