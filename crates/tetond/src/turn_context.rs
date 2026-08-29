//! REQ-598: the per-turn parameter cluster, named.
//!
//! # Why this module exists
//!
//! Twenty-five `#[allow(clippy::too_many_arguments)]` attributes were not
//! twenty-five independent design choices. Nearly all of them carried the same
//! recurring set of facts — the event bus, the session, the config, the router,
//! the permission gate — passed by hand through every layer of the turn path. A
//! suppression is a lint told to stop reporting a fact, and the fact was still
//! true: this was an unnamed concept.
//!
//! Removing suppressions is the visible effect. The actual value is that adding
//! a new per-turn fact stops requiring an edit to a dozen signatures and their
//! call sites — which is what makes REQ-599's decomposition of `runtime.rs`
//! tractable, since every extraction from that file currently produces another
//! ten-argument function.
//!
//! # Three types, not one — ADR-1
//!
//! The sites want **different bundles**, and the requirement anticipated this:
//! "If a subset of the sites turns out to want a *different* bundle, the answer
//! is two small structs, not one wide one."
//!
//! - [`TurnCore`] — the four facts every per-turn function needs.
//! - [`TurnContext`] — the core plus the gate that authorizes this turn's tools.
//! - [`DutyContext`] — the core plus the two facts that travel with every duty
//!   resolution. Deliberately **gate-free**: a duty route authorizes nothing,
//!   and `spawn_title_session` — which resolves the `title` duty on a detached
//!   task — has no gate to give it.
//!
//! The decisive evidence that duty is a real second bundle rather than a subset
//! that happens to be shorter: in `run_one_attempt`, the four calls to
//! `digest_route`, `triage_route`, `shell_route` and `compact_route` passed the
//! *identical six arguments in the identical order*.
//!
//! The four core fields are declared **once**, in [`TurnCore`], and reached
//! through it by both wrappers. Declaring them twice would be two surfaces
//! describing one state, free to drift (LESSON-586).
//!
//! # What these types deliberately do not do
//!
//! **No `route` field (ADR-3, answering OQ-1).** `route` is reassigned on every
//! fallback reroute inside `run_one_attempt`'s `'turn:` loop. A context owning
//! it would go stale or need rebuilding each iteration, and an `Option<Route>`
//! adds a state that cannot occur. Keeping `route` an explicit parameter also
//! keeps the reroute *visible* in the signature, which is what BR-7 asks of
//! ordering-dependent logic.
//!
//! **No id minting (BR-3).** None of these types holds a counter, sequence, or
//! allocator. Request-id minting for daemon-wide resources stays centralized in
//! `PendingPermissions`; a per-session counter handing out ids in a daemon-wide
//! namespace is what cross-authorized tool calls between sessions in BUG-161.
//!
//! **No I/O (BR-4).** Every field is an already-resolved borrow, so
//! construction performs no filesystem access and no blocking call — there is
//! nothing here for the `block_in_place_if_multithread` seam to wrap. This is a
//! rule, not an observation: synchronous skill discovery on the connection's
//! reader loop stalled RPCs behind a TCC dialog in BUG-184, and a constructor
//! that grew an I/O call would reintroduce that class. A change that adds one
//! has to confront this paragraph.
//!
//! **No behavior.** These are parameter bundles. A context that starts
//! answering questions becomes a second place for turn logic to live, which is
//! exactly what REQ-599 has to untangle.
//!
//! # Construction point — ADR-4 / BR-2.1
//!
//! Construction happens **after the last rebinding of every field it
//! captures**, which is a stronger rule than BR-2's "after the turn is claimed"
//! and is not implied by it. See [`TurnContext`] for the trace.

use std::sync::{Arc, Mutex};

use teton_core::config::Config;
use teton_core::cost_ceiling::PromptSpend;
use teton_inference::{ChatFormat, Engine};
use teton_protocol::SessionId;

use crate::broadcast::EventBus;
use crate::grants::ConnectionId;
use crate::harness::permissions::PermissionGate;
use crate::router::Router;

/// The local engine slot as the duty routes read it: a handle and the chat
/// format resolved beside it at install time.
///
/// Read once per attempt and passed as a pair, so the format is never fetched
/// through a lock on the async path — the engine mutex is held for the whole of
/// any in-flight completion, and a metadata lock there would park a tokio
/// worker behind another session's inference (LESSON-448).
pub type LocalEngineSlot = (Arc<Mutex<dyn Engine>>, ChatFormat);

/// The four facts every per-turn function needs.
///
/// All fields are shared borrows, so this is `Copy` — pass it by value rather
/// than threading a reference to it.
#[derive(Clone, Copy)]
pub struct TurnCore<'a> {
    /// The daemon-scoped bus this turn's news is published on.
    pub events: &'a Arc<EventBus>,
    /// The session this turn belongs to.
    pub session_id: &'a SessionId,
    /// **This turn's one config snapshot.** Handed on rather than re-read, so
    /// every consumer is reading the same config and a commit landing mid-turn
    /// moves the *next* turn instead of leaving this one's prompt disagreeing
    /// with its own tool set.
    pub config: &'a Config,
    /// The router this turn routes through.
    ///
    /// On a turn held for a warming local tier this is the router built
    /// **after** the hold, from the settled tier state — not the one built
    /// before it. See [`TurnContext`] for why that distinction is load-bearing.
    pub router: &'a Router,
}

/// The turn path's context: [`TurnCore`] plus the gate that authorizes this
/// turn's tools.
///
/// # Where this may be constructed — ADR-4 / BR-2.1
///
/// BR-2 requires construction after the turn is claimed, because session state
/// snapshotted before the claim is stale (LESSON-539). That is necessary and
/// **not sufficient**.
///
/// BR-2 names one instance of a class. The class is: *a context must not be
/// constructed before any point that rebinds a field it captures.* In
/// `run_prompt_turn` the ordering is:
///
/// 1. the session claim is taken — before any of the turn's work
/// 2. `session_cwd` is re-read from the registry (the BR-2 / LESSON-539 point)
/// 3. `config` is snapshotted
/// 4. the `gate` is fetched — not built, so session grants survive
/// 5. the skill expansion runs, which needs the gate
/// 6. `router` is bound
/// 7. **the REQ-580 warming hold may rebind `router` and re-dispatch `route`**
/// 8. ← a `TurnContext` may be constructed here, and no earlier
///
/// Step 7 is the trap. When the local tier is still coming up, the turn is
/// parked; on wake the router is rebuilt from the settled tier state and the
/// route is dispatched afresh. A context constructed at step 6 satisfies BR-2,
/// passes the whole existing suite, and hands every downstream consumer a
/// router describing a tier state that no longer exists — silently breaking
/// REQ-580's guarantee that a turn served after the wait is built from the
/// route it is served *by*.
///
/// The guard is mechanical rather than this comment: a test drives a
/// warming-tier turn and asserts the captured router is the post-hold one.
#[derive(Clone, Copy)]
pub struct TurnContext<'a> {
    /// The four universal facts.
    pub core: TurnCore<'a>,
    /// This session's permission gate.
    ///
    /// Carried as `&Arc<PermissionGate>` rather than `&PermissionGate` because
    /// the gate is *fetched* per turn (a rebuilt gate forgets every "allow for
    /// this session" answer it earned) and one consumer needs to clone the
    /// `Arc` while the others want the bare reference. The `Arc` form serves
    /// both; the narrower type would force a clone at the one site.
    pub gate: &'a Arc<PermissionGate>,
    /// The connection that submitted this turn, when one did.
    ///
    /// Carried here rather than passed alongside because it is a per-turn fact
    /// with the same lifetime as the rest: bound from `run_prompt_turn`'s own
    /// parameters and never rebound. It is the addressee of any consent a tool
    /// raises mid-turn (REQ-587 ADR-3), which is why it belongs to the *turn*
    /// context and not to [`TurnCore`] — a duty route asks nobody anything, and
    /// `spawn_title_session` has no connection to name.
    ///
    /// `ConnectionId` is `Copy`, so consumers still take their own.
    pub invoker: Option<ConnectionId>,
}

/// The duty-routing context: [`TurnCore`] plus the two facts that travel with
/// every duty resolution.
///
/// Gate-free by design. A duty route decides *where* a duty runs; it authorizes
/// nothing, and `spawn_title_session` resolves the `title` duty on a detached
/// task that has no gate at all. Requiring one here would make that call site
/// unrepresentable, which would be the type lying about what a duty route
/// needs.
#[derive(Clone, Copy)]
pub struct DutyContext<'a> {
    /// The four universal facts.
    pub core: TurnCore<'a>,
    /// The engine slot as read once for this attempt, or `None` when no local
    /// engine is installed.
    pub local_engine: Option<&'a LocalEngineSlot>,
    /// The per-prompt spend accumulator, or `None` for a duty that runs outside
    /// one.
    ///
    /// `spawn_title_session` passes `None` deliberately: it outlives the prompt
    /// that triggered it, and binding a detached background job to that
    /// prompt's accumulator would let it spend against a total nobody is
    /// watching any more (REQ-588).
    pub prompt_spend: Option<&'a Arc<PromptSpend>>,
}

impl<'a> TurnCore<'a> {
    /// The duty context these four facts support, given the two that travel
    /// with a duty resolution.
    ///
    /// Lives on [`TurnCore`] rather than only on [`TurnContext`] because the
    /// caller that needs it most has no gate to build a [`TurnContext`] from:
    /// `spawn_title_session` resolves the `title` duty on a detached task. It
    /// takes a `TurnCore` for exactly that reason, and reaches a `DutyContext`
    /// through here.
    #[must_use]
    pub fn duties(
        self,
        local_engine: Option<&'a LocalEngineSlot>,
        prompt_spend: Option<&'a Arc<PromptSpend>>,
    ) -> DutyContext<'a> {
        DutyContext {
            core: self,
            local_engine,
            prompt_spend,
        }
    }
}

impl<'a> TurnContext<'a> {
    /// The turn path's context, from its already-resolved parts.
    #[must_use]
    pub fn new(
        events: &'a Arc<EventBus>,
        session_id: &'a SessionId,
        config: &'a Config,
        router: &'a Router,
        gate: &'a Arc<PermissionGate>,
        invoker: Option<ConnectionId>,
    ) -> Self {
        Self {
            core: TurnCore {
                events,
                session_id,
                config,
                router,
            },
            gate,
            invoker,
        }
    }

    /// The duty context for this turn, given the attempt's one engine-slot read
    /// and its spend accumulator.
    ///
    /// The gate is dropped rather than carried: see [`DutyContext`].
    ///
    /// Delegates to [`TurnCore::duties`] rather than building a `DutyContext`
    /// of its own, so there is one place that knows how a duty context is
    /// assembled.
    #[must_use]
    pub fn duties(
        &self,
        local_engine: Option<&'a LocalEngineSlot>,
        prompt_spend: Option<&'a Arc<PromptSpend>>,
    ) -> DutyContext<'a> {
        self.core.duties(local_engine, prompt_spend)
    }
}

impl<'a> DutyContext<'a> {
    /// A duty context with no turn context behind it — the `spawn_title_session`
    /// path, which resolves a duty on a detached task and holds no gate.
    #[must_use]
    pub fn detached(
        events: &'a Arc<EventBus>,
        session_id: &'a SessionId,
        config: &'a Config,
        router: &'a Router,
        local_engine: Option<&'a LocalEngineSlot>,
        prompt_spend: Option<&'a Arc<PromptSpend>>,
    ) -> Self {
        Self {
            core: TurnCore {
                events,
                session_id,
                config,
                router,
            },
            local_engine,
            prompt_spend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::permissions::{PendingPermissions, PermissionGate};
    use teton_core::category::CategoryTable;

    /// A gate built over a fresh bus, standing in for the per-session gate the
    /// runtime fetches. The same four-argument constructor the runtime's own
    /// tests use — not a bespoke double, so this test cannot pass against a
    /// gate shape the daemon no longer builds (LESSON-451).
    fn gate_for(events: &Arc<EventBus>, session_id: &SessionId) -> Arc<PermissionGate> {
        Arc::new(PermissionGate::new(
            session_id.clone(),
            crate::harness::table_for(teton_protocol::permissions::PermissionLevel::default()),
            Arc::clone(events),
            Arc::new(PendingPermissions::default()),
        ))
    }

    /// An empty router — enough to be borrowed, which is all these types do
    /// with it.
    fn router() -> Router {
        Router::new(CategoryTable::new(), None)
    }

    /// **BR-6: the seams survive.** The context types are constructible from
    /// test doubles and hand back what they were given.
    ///
    /// This is the criterion that keeps the refactor from quietly closing the
    /// injection points the presence and permission suites depend on — a
    /// context that could only be built from a live `DaemonRuntime` would make
    /// `AlwaysFailsVerifier`, the counting gates, and the
    /// `TETON_PRESENCE_ACCEPT=fail` seam unreachable (LESSON-519).
    #[test]
    fn every_context_is_constructible_from_doubles_and_returns_what_it_was_given() {
        let events = Arc::new(EventBus::new());
        let session_id = SessionId::from("session-req598");
        let config = Config::default();
        let router = router();
        let gate = gate_for(&events, &session_id);

        let tctx = TurnContext::new(&events, &session_id, &config, &router, &gate, None);
        assert_eq!(tctx.core.session_id, &session_id);
        assert!(Arc::ptr_eq(tctx.core.events, &events));
        assert!(Arc::ptr_eq(tctx.gate, &gate));

        // The duty view drops the gate and gains the two duty facts.
        let dctx = tctx.duties(None, None);
        assert_eq!(dctx.core.session_id, &session_id);
        assert!(dctx.local_engine.is_none());
        assert!(dctx.prompt_spend.is_none());

        // And the detached constructor reaches the same shape without a gate,
        // which is the `spawn_title_session` path.
        let detached = DutyContext::detached(&events, &session_id, &config, &router, None, None);
        assert_eq!(detached.core.session_id, &session_id);
        assert!(Arc::ptr_eq(detached.core.events, &events));
    }

    /// **The core is shared, not copied.** `TurnContext::duties` must carry the
    /// *same* four facts, not a second set assembled beside them — two surfaces
    /// describing one state are free to drift (LESSON-586).
    #[test]
    fn the_duty_view_carries_the_turns_own_core() {
        let events = Arc::new(EventBus::new());
        let session_id = SessionId::from("session-req598-core");
        let config = Config::default();
        let router = router();
        let gate = gate_for(&events, &session_id);

        let tctx = TurnContext::new(&events, &session_id, &config, &router, &gate, None);
        let dctx = tctx.duties(None, None);

        assert!(std::ptr::eq(tctx.core.config, dctx.core.config));
        assert!(std::ptr::eq(tctx.core.router, dctx.core.router));
        assert!(std::ptr::eq(tctx.core.session_id, dctx.core.session_id));
    }

    /// **REQ-598 AC-5 (c) — the context carries nothing derived from `cwd`.**
    ///
    /// AC-5's original (a) and (b) asked for a test on `TurnContext`'s view of
    /// the session root, and a mutation building it from the pre-claim
    /// `session_cwd` parameter. Neither is performable: the context has no
    /// cwd-derived field, by ADR-1's measurement and the requirement's own
    /// entity table. The one probed root feeds the jail, the prompt's
    /// environment block and REQ-585's skill pin — never this type.
    ///
    /// That makes BR-2's cwd clause vacuously true today, and a vacuous truth
    /// is exactly what stops being true without anyone noticing. This test is
    /// the premise's guard rather than the hazard's: the destructuring below is
    /// **exhaustive**, with no `..` rest pattern, so adding any field to either
    /// struct fails to compile *here*. Whoever adds one has to come to this
    /// comment and decide whether it can go stale between the claim and the
    /// construction point — which is the question AC-5 was really asking.
    ///
    /// It is a compile-time guard, so it has no runtime mutation to record; the
    /// mutation *is* "add a field", and the compiler is what goes red.
    #[test]
    fn the_context_holds_exactly_these_facts_and_none_derived_from_cwd() {
        let events = Arc::new(EventBus::new());
        let session_id = SessionId::from("session-req598-fields");
        let config = Config::default();
        let router = router();
        let gate = gate_for(&events, &session_id);
        let tctx = TurnContext::new(&events, &session_id, &config, &router, &gate, None);

        // Exhaustive on purpose — see the doc comment. Do not add `..`.
        let TurnContext {
            core:
                TurnCore {
                    events: _,
                    session_id: _,
                    config: _,
                    router: _,
                },
            gate: _,
            invoker: _,
        } = tctx;
    }
}
