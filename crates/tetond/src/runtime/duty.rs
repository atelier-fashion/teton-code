//! REQ-599 step 3: duty routing.
//!
//! ADR-2's real seam. The five `*_route` resolvers, the `resolve_duty` /
//! `build_duty_route` pair they share, the detached `spawn_title_session`, and
//! the redaction gate that resolves the sixth category.
//!
//! This is the first slice that splits the **god-impl** rather than moving
//! top-level items: `impl DaemonRuntime` began at line 2641 of the pre-split
//! file and ran ~7,013 lines. ADR-3 is what makes that possible — Rust requires
//! an inherent `impl` to live in the defining *crate*, not the defining module,
//! so these methods keep their receiver and every call site still reads
//! `self.digest_route(dctx)`. No trait, no newtype, no call-site change.
//!
//! Cohesive by position as well as by topic, which is why it went first: the
//! nine duty functions were the one grouping the census found already clustered
//! (span 2,641 lines against 6,000–12,000 for everything else). REQ-598's
//! `DutyContext` is the other precondition — without it these arrive carrying
//! six loose arguments each.

use super::*;

// REQ-613 TASK-381: named here rather than added to `runtime`'s own harness
// import, so the twelfth duty costs this module one line and the module above it
// none.
use crate::harness::DRAFT_DUTY;

impl DaemonRuntime {
    /// Resolve the `digest` category for this turn (REQ-558 BR-1, BR-2, BR-7).
    ///
    /// Same two layers `dispatch_route` uses, in the same order, for the same
    /// reasons.
    ///
    /// 1. **Session taint** (BR-7). A session pinned to the local tier by boundary
    ///    exposure stays pinned for *every* model call it makes, and a duty is a
    ///    model call. `digest` is not exempt: the pin is a privacy guarantee and
    ///    the category table is a cost decision, and the two deliberately do not
    ///    compose (LESSON-432). Checked before a category is resolved, so nothing
    ///    here reads a binding on a tainted turn.
    /// 2. **The resolver** — one table, one precedence, the same one the turn
    ///    itself went through (BR-6).
    ///
    /// ## Why this function exists at all (REQ-561 ADR-3)
    ///
    /// Everything below the two lines that pick a [`Route`](crate::router::Route)
    /// is shared with every other duty and lives in [`Self::resolve_duty`]. What
    /// cannot be shared is the line naming the category, because
    /// [`crate::call_sites`]'s derived-marker test reads the daemon's own source
    /// looking for a routing call with a `Category::X` literal inside it. Fold
    /// that literal into a helper taking a category *variable* and the scan finds
    /// nothing — the `declared, no call site yet` marker would then keep claiming
    /// `digest` is unreached while it is fully wired, and the test would fail
    /// pointing at the marker rather than at the receiver. So the shared helper
    /// sits **behind** the literal, not in front of it.
    pub(super) fn digest_route(&self, dctx: DutyContext<'_>) -> DutyRoute {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `digest` duty"))
        } else {
            router.resolve(Category::Digest)
        };
        self.resolve_duty(DIGEST_DUTY, &route, dctx)
    }

    /// Resolve the `triage` category for this turn (REQ-561 TASK-060).
    ///
    /// The same two layers, in the same order, for the same reasons as
    /// [`Self::digest_route`] — session taint first, then the one resolver — and
    /// the same reason for existing separately at all: the line naming the
    /// category is what [`crate::call_sites`]'s derived-marker test reads out of
    /// the daemon's own source, so it cannot be folded into a helper taking a
    /// category *variable* without making that scan blind (ADR-3).
    ///
    /// `triage` is a `scan` duty, so it inherits whatever `scan` is bound to and
    /// sends **grep match text** — file content — there. That is the binding
    /// working as configured; what holds the line is BR-7's scoping at the
    /// egress choke point, by the provenance of the matched files rather than of
    /// the turn.
    pub(super) fn triage_route(&self, dctx: DutyContext<'_>) -> DutyRoute {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `triage` duty"))
        } else {
            router.resolve(Category::Triage)
        };
        self.resolve_duty(TRIAGE_DUTY, &route, dctx)
    }

    /// Resolve the `shell` category for this turn (REQ-561 TASK-061).
    ///
    /// The same two layers, in the same order, for the same reasons as
    /// [`Self::digest_route`] — session taint first, then the one resolver — and
    /// the same reason for existing separately at all: the line naming the
    /// category is what [`crate::call_sites`]'s derived-marker test reads out of
    /// the daemon's own source, so it cannot be folded into a helper taking a
    /// category *variable* without making that scan blind (ADR-3).
    ///
    /// `shell` is a `build` duty, so it inherits whatever `build` is bound to.
    /// What it sends is **command output**, whose files the daemon cannot know —
    /// so the choke point fail-closes on it wherever a boundary is configured,
    /// and a remotely bound `shell` duty simply degrades. That is BR-3 working;
    /// see [`crate::harness::shell_duty`].
    pub(super) fn shell_route(&self, dctx: DutyContext<'_>) -> DutyRoute {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `shell` duty"))
        } else {
            router.resolve(Category::Shell)
        };
        self.resolve_duty(SHELL_DUTY, &route, dctx)
    }

    /// Resolve the `title` category for this session (REQ-561 TASK-062).
    ///
    /// The same two layers, in the same order, for the same reasons as
    /// [`Self::digest_route`] — session taint first, then the one resolver — and
    /// the same reason for existing separately at all: the line naming the
    /// category is what [`crate::call_sites`]'s derived-marker test reads out of
    /// the daemon's own source, so it cannot be folded into a helper taking a
    /// category *variable* without making that scan blind (ADR-3).
    ///
    /// `title` is a `reflex` duty, and an unbound `reflex` tier inherits the
    /// **local** tier and never `default_provider` (REQ-558) — "sub-second, every
    /// turn, never leaves the machine". So a machine whose turns all go to a
    /// frontier provider still names its sessions on the local engine, and no
    /// branch here is what makes that true: it is the resolver's answer, reached
    /// through the same table every other category reads (LESSON-484). A user who
    /// binds `reflex` remotely on purpose gets what they asked for, scoped and
    /// metered by the shared seam like any other duty.
    pub(super) fn title_route(&self, dctx: DutyContext<'_>) -> DutyRoute {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `title` duty"))
        } else {
            router.resolve(Category::Title)
        };
        self.resolve_duty(TITLE_DUTY, &route, dctx)
    }

    /// Resolve the `compact` category for this turn (REQ-561 TASK-063).
    ///
    /// The same two layers, in the same order, for the same reasons as
    /// [`Self::digest_route`] — session taint first, then the one resolver — and
    /// the same reason for existing separately at all: the line naming the
    /// category is what [`crate::call_sites`]'s derived-marker test reads out of
    /// the daemon's own source, so it cannot be folded into a helper taking a
    /// category *variable* without making that scan blind (ADR-3).
    ///
    /// `compact` is a `scan` duty, so it inherits whatever `scan` is bound to and
    /// sends the **conversation itself** there — the widest content class of the
    /// five, and the one BR-11's disclosure exists for. What holds the line is
    /// BR-7's scoping at the egress choke point: the conversation's own merged
    /// provenance, so a session that read a `local-only` file compacts locally or
    /// not at all, while the turn proceeds either way.
    pub(super) fn compact_route(&self, dctx: DutyContext<'_>) -> DutyRoute {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `compact` duty"))
        } else {
            router.resolve(Category::Compact)
        };
        self.resolve_duty(COMPACT_DUTY, &route, dctx)
    }

    /// Resolve the `draft` category for this session (REQ-613 TASK-381, ADR-4).
    ///
    /// The same two layers, in the same order, for the same reasons as
    /// [`Self::digest_route`] — session taint first, then the one resolver — and
    /// the same reason for existing separately at all: the line naming the
    /// category is what [`crate::call_sites`]'s derived-marker test reads out of
    /// the daemon's own source, so it cannot be folded into a helper taking a
    /// category *variable* without making that scan blind (ADR-3).
    ///
    /// `draft` is the one `think` duty. Everything else the harness does on its
    /// own behalf is `reflex`, `scan` or `build`, because it happens on every
    /// turn and its cost multiplies; this one happens **once per repository**
    /// and its answer is read at the start of every session afterwards, which
    /// inverts the arithmetic (REQ-613 OQ-2). A user who disagrees writes
    /// `teton policy set-category draft local` and the resolver answers
    /// differently — no branch here, which is what keeps the policy row the only
    /// thing that decides.
    ///
    /// What it sends is **repository file content**: a listing, a README, a
    /// manifest. The taint arm above is the session-wide backstop; the
    /// per-evidence exclusion BR-4 requires happens where the evidence is
    /// gathered, before a prompt exists, and the remainder is judged at the
    /// egress choke point by the provenance of the files it came from — so a
    /// covered file cannot reach the call even on an untainted session.
    ///
    /// # Two answers from one resolution
    ///
    /// The pipeline needs the route *and* the tier — the tier is what the
    /// generated file's header names and what the event carries (BR-5) — and
    /// resolving twice would ask the router a second question whose answer can
    /// differ if a policy row or a provider's health moved in between. It is
    /// also the only shape that keeps the category named where AC-10 allows one
    /// to be named: in a [`Router::resolve`](crate::router::Router::resolve)
    /// call, in this module.
    ///
    /// Visibility: `pub(super)`, like its five siblings. The pipeline that
    /// spends this route lives in [`crate::repo_context`] (REQ-613 ADR-6), but
    /// it never resolves one: it takes a resolver closure, and the closure is
    /// built by `DaemonRuntime::generate_repo_context` — inside this tree,
    /// which is what keeps the seam that reads provider health where the router
    /// is (LESSON-596: established by demoting and building, not by grepping
    /// for the name).
    pub(super) fn draft_route(&self, dctx: DutyContext<'_>) -> DraftPlan {
        let (router, session_id) = (dctx.core.router, dctx.core.session_id);
        let route = if self.session_taint.is_tainted(session_id) {
            router.resolve_local_pin(taint_pin_reason("the `draft` duty"))
        } else {
            router.resolve(Category::Draft)
        };
        // The taint pin consults no resolution — it is a privacy guarantee
        // overriding every binding — so it has no tier of its own to report. The
        // tier a resolution *would* have carried is the category's compile-time
        // one, which `routing.rs`'s AC-14 case pins as `think`.
        let tier = route.resolution.as_ref().map_or(Tier::Think, |r| r.tier);
        DraftPlan {
            route: self.resolve_duty(DRAFT_DUTY, &route, dctx),
            tier: to_protocol_tier(tier),
        }
    }

    /// Name this session after `prompt`, at most once for its whole life
    /// (REQ-561 BR-9a, TASK-062).
    ///
    /// Unlike `triage` and `shell` this duty is not owned by a tool — a session is
    /// named because it *is* a session — so it hangs here, on the daemon's own
    /// prompt-turn entry point, which is the one place that knows a session both
    /// exists and has now been asked for something.
    ///
    /// ## Three gates, in the order that makes each one cheap
    ///
    /// 1. **Is there anything to name it after** ([`title::worth_titling`],
    ///    ADR-11). A session opened with `"hi"` declines *without* spending its
    ///    one attempt, so the turn that actually asks for something still gets a
    ///    name. This gate comes first because it costs a length comparison.
    /// 2. **Has this session already had its attempt**
    ///    ([`SessionRegistry::claim_title`]). The claim is taken **before** the
    ///    call, not after it succeeds — see that method for why a guard keyed on
    ///    `title.is_none()` alone turns a failing duty into a per-turn model call.
    /// 3. **Did the title land** ([`SessionRegistry::set_title`], BR-9). Only a
    ///    title that was actually written is announced, so `session_titled`
    ///    carries at most one naming per session (AC-15).
    ///
    /// ## Failure is silence on the wire, never a failed turn (BR-3)
    ///
    /// Every way this can fail — an unroutable `reflex` binding, no local engine,
    /// an engine error, an answer with no title in it — leaves the session with
    /// **no** title and the turn entirely unaffected. That is not a degraded mode
    /// to be repaired later: it is the state every session was in before this
    /// REQ. This function therefore returns nothing; there is no outcome a caller
    /// could act on that would be better than proceeding with the turn.
    ///
    /// ## The provenance is the **caller's**, and that is REQ-587's correction
    ///
    /// `provenance` is the egress provenance of `prompt` — the content this duty
    /// is about to send — and it is a parameter for the reason LESSON-432 gives:
    /// the call site is what knows where its content came from. This function
    /// used to hard-code [`Provenance::empty`], which was right while there was
    /// only one caller: a typed prompt is the user's own bytes and touched no
    /// file. REQ-585 added a second caller whose `prompt` is a **skill
    /// expansion** — a file's contents, plus whatever its dynamic commands
    /// printed — and REQ-587's verify found the hard-coded value still here.
    /// `Egress::send` short-circuits on an empty provenance before any boundary
    /// check, so `title` — which `title_route` resolves **remotely** unless the
    /// session is already tainted, and which fires on the session's first
    /// substantive prompt, before any taint exists — was putting up to
    /// `TITLE_REQUEST_MAX_BYTES` of that file on the wire.
    ///
    /// The typed-prompt call sites still pass [`Provenance::empty`], and that
    /// stays correct for them; what changed is that it is now something a caller
    /// states rather than something this function assumes on every caller's
    /// behalf.
    ///
    /// ## The naming is **detached**; the turn never waits on it (REQ-561 verify)
    ///
    /// Gates 1 and 2 are synchronous and stay on the caller's thread — they are a
    /// length comparison and one uncontended mutex — and so is building the
    /// route, which needs the `router` and `config` the caller is holding. The
    /// *model call* is spawned and this function returns immediately.
    ///
    /// Awaiting it here made the user wait for a complete local inference before
    /// their turn even started, on the first substantive prompt of every session.
    /// The position is still right, and for the reason it always was: the name is
    /// derived from the prompt, which is already in hand, so a client can label
    /// the session the moment the user hits enter rather than a whole answer
    /// later. That benefit never required *blocking* on it.
    ///
    /// Nothing on the turn path reads the result, so there is no ordering to
    /// preserve: [`SessionRegistry::claim_title`] is already exclusive under the
    /// registry lock, so the detached task cannot race a second turn into a
    /// second attempt, and [`SessionRegistry::set_title`] is idempotent-by-guard,
    /// so it cannot overwrite a name that arrived first.
    ///
    /// Returns the spawned task so a test can await it. **Production drops it**:
    /// a title that has not landed yet is a session with no title, which is BR-3's
    /// degraded state and costs the turn nothing.
    pub(super) fn spawn_title_session(
        &self,
        core: TurnCore<'_>,
        sessions: &SessionRegistry,
        prompt: &str,
        provenance: Provenance,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !crate::harness::title::worth_titling(prompt) {
            return None;
        }
        if !sessions.claim_title(core.session_id) {
            return None;
        }
        let local_engine = self.engine.get_with_format();
        // REQ-588: no accumulator — and so no ceiling — on the titling duty.
        // `spawn_title_session` outlives the prompt that triggered it: it is
        // detached onto its own task and may still be running after the turn
        // has ended. Binding it to that prompt's accumulator would let a
        // background job spend against a total nobody is watching any more, and
        // would race the next prompt's. Titling is cheap and outside the
        // ceiling, stated here rather than left to be inferred.
        //
        // Derived from a `TurnCore`, not a `TurnContext`: this duty holds no
        // gate, and there is none here to give it — which is why `DutyContext`
        // is gate-free and why this function takes the core rather than the
        // turn context (ADR-1).
        let route = self.title_route(core.duties(local_engine.as_ref(), None));

        let events = Arc::clone(core.events);
        let sessions = sessions.clone();
        let session_id = core.session_id.clone();
        let prompt = prompt.to_owned();
        Some(tokio::spawn(async move {
            let Ok(title) = crate::harness::title::name_session(&route, &prompt, &provenance).await
            else {
                return;
            };
            if sessions.set_title(&session_id, &title) {
                // ADR-6's amendment: `SessionTitled` carries no `session_id` of
                // its own — `Event` is internally tagged and flattened into the
                // envelope, which already has one. So the envelope MUST be scoped
                // here, or the event reaches the wire naming no session and
                // nobody can attribute it.
                events.publish(
                    Some(session_id),
                    Event::SessionTitled(SessionTitled { title }),
                );
            }
        }))
    }

    /// Turn a resolved [`Route`](crate::router::Route) into the [`DutyRoute`]
    /// that serves `duty` — the shared half of every duty resolver (REQ-561 BR-6).
    ///
    /// The per-duty resolvers differ only in which category they name; from the
    /// `Route` onward, locality, provider construction, egress wiring, the cost
    /// meter and every failure sentence are one implementation. Adding a duty
    /// adds a four-line resolver, not a copy of this.
    ///
    /// ## `route_decided` is *attached* here and *published* on use (BR-2)
    ///
    /// This is the one place that holds the `Route`, so this is where the event
    /// payload is projected off it — but publishing waits until
    /// [`DutyRoute::perform`] actually runs the duty. `digest_route` is built
    /// unconditionally once per turn attempt whether or not any tool result
    /// crosses the summarization threshold, so emitting here would announce a
    /// routed model call for every turn that never makes one — and would do it
    /// five times per turn once the remaining four duties are wired. BR-2 exists
    /// to make an egress path visible; a path that never fires produced no
    /// egress.
    ///
    /// [`Route::route_decided`](crate::router::Route::route_decided) self-guards
    /// on the other side: it yields nothing when no provider was selected, so an
    /// unroutable duty carries no announcement at all.
    ///
    /// ## Every unresolvable outcome carries a reason
    ///
    /// Never a bare `None`: the duty guards an invariant, so its caller must be
    /// able to say why it fell back to degraded means (LESSON-447). Where the
    /// sentence exists already — the resolver's — it is carried verbatim rather
    /// than re-authored (BR-6). Note what is *not* here: a credential that will
    /// not resolve fails the **turn** on the turn path (a config error the user
    /// must fix), but only the **duty** here — a duty is never fatal, and the
    /// failure is reported on the duty's own outcome instead.
    pub(super) fn resolve_duty(
        &self,
        duty: DutyKind,
        route: &crate::router::Route,
        dctx: DutyContext<'_>,
    ) -> DutyRoute {
        self.build_duty_route(duty, route, dctx).announcing(
            dctx.core.events,
            Some(dctx.core.session_id.clone()),
            route.route_decided(),
        )
    }

    /// Build the [`DutyRoute`] `route` calls for, without announcing anything.
    ///
    /// Split from [`Self::resolve_duty`] so the announcement has exactly **one**
    /// attachment site: this function has five returns, and an
    /// `.announcing(...)` on each is five chances for the sixth to forget.
    ///
    /// A remotely-bound duty builds its provider and transport eagerly, once per
    /// attempt, whether or not the duty ends up being called. That costs a
    /// keychain read and an HTTP client per turn against a turn whose floor is one
    /// model inference, so it is not worth the machinery to defer — but it is
    /// worth knowing that after REQ-557's migration (`default_provider` set to the
    /// first remote provider, no `[[tiers]]` rows) an unbound tier inherits that
    /// provider, so this is the *ordinary* upgraded config and not an exotic one.
    pub(super) fn build_duty_route(
        &self,
        duty: DutyKind,
        route: &crate::router::Route,
        dctx: DutyContext<'_>,
    ) -> DutyRoute {
        // Destructured rather than reached through `dctx.core.*` at each use:
        // the body below is REQ-557/REQ-561 routing logic that this REQ must
        // not touch, and rebinding the six names here keeps that body
        // byte-identical (BR-1).
        let DutyContext {
            core:
                TurnCore {
                    events,
                    session_id,
                    config,
                    router,
                },
            local_engine,
            prompt_spend,
        } = dctx;
        // The category's own name, read off the duty rather than spelled again:
        // two surfaces describing one routing state must not be able to drift.
        let name = duty.category().as_str();

        let Some(provider_id) = route.provider_id.as_ref().map(|p| p.0.clone()) else {
            return DutyRoute::unresolved(route.reason.clone());
        };

        // Locality is decided exactly as the turn path decides it, from the same
        // two facts: the provider's declared kind, or — for the local tier naming
        // itself with no `[[providers]]` entry (REQ-557 ADR-D) — the presence of
        // an engine.
        let provider_cfg = config.providers.iter().find(|p| p.id == provider_id);
        let is_local = match provider_cfg {
            Some(p) => matches!(p.kind, ProviderKind::Local),
            None => local_engine.is_some(),
        };

        if is_local {
            return match local_engine {
                Some((engine, _format)) => DutyRoute::local(duty, provider_id, Arc::clone(engine)),
                None => DutyRoute::unresolved(format!(
                    "The '{name}' category resolves to '{provider_id}', but no local engine is \
                     loaded to serve it yet."
                )),
            };
        }

        // Remote. Each way this can fail names what is missing rather than
        // returning a bare "unavailable" — an unresolvable duty is a
        // configuration fact the user can act on.
        let Some(provider_cfg) = provider_cfg else {
            return DutyRoute::unresolved(format!(
                "The '{name}' category resolves to '{provider_id}', which this daemon has no \
                 provider entry for, and no local engine is loaded to serve it instead."
            ));
        };
        // REQ-557 BR-1 / BUG-155: no model, no call. A provider id is not a model
        // name and must never stand in for one.
        let Some(model) = route.model.clone() else {
            return DutyRoute::unresolved(format!(
                "The '{name}' category resolves to '{provider_id}', which declares no model, so \
                 there is nothing to call."
            ));
        };
        let transport = match build_remote_transport(provider_cfg, &self.secret_resolver) {
            Ok(transport) => transport,
            Err(err) => {
                return DutyRoute::unresolved(format!(
                    "The '{name}' category resolves to '{provider_id}', whose transport could \
                     not be built: {err}"
                ))
            }
        };
        let caps = CapabilityProfile::from_core(provider_cfg.capabilities);
        // BR-1: the duty reaches the network only through the choke point, with
        // this daemon's boundaries and this session's cost meter — the same
        // construction the turn path uses, because a duty that egresses through a
        // second, laxer path is the hole BR-1 exists to close.
        //
        // The sink is the one thing that differs, and it differs because the
        // *outcome* does: a refused duty is degraded here and never surfaces as
        // a turn error, so nothing above would ever mark the session. Marking at
        // the choke point makes the backstop direct rather than dependent on the
        // refusing content still being in `ctx` when the turn ends.
        let sink = Arc::new(TaintingPrivacySink::for_turn_path(
            events.clone(),
            Arc::clone(&self.session_taint),
        ));
        let mut egress = Egress::new(transport, config.effective_boundaries(), sink)
            .with_cost_meter(Arc::new(self.ledger.clone()))
            // REQ-588 BR-1/ADR-6: the user's ceiling, when they set one. Absent
            // leaves the choke point exactly as it was — no check, no pricing
            // lookup, no branch.
            .with_optional_spend_ceiling(config.cost.ceiling_micro_cents())
            .with_prompt_spend(prompt_spend.cloned());
        // REQ-562 ADR-1: a remotely-bound duty's prompt is an outbound payload
        // like any other, so it crosses the same gate the turn path's does. It
        // is the same construction for the same reason the boundaries and the
        // meter are: a duty that egressed through a laxer choke point is the
        // hole the single choke point exists to close.
        if let Some(gate) = self.redaction_gate(router, config, events, session_id) {
            egress = egress.with_redaction_gate(gate);
        }
        DutyRoute::remote(
            duty,
            provider_id,
            build_provider(provider_cfg, caps),
            egress,
            model,
            session_id.clone(),
            // REQ-559: the duty's own route resolved its own effort, through the
            // same `Router::effort_for` the turn path uses. A duty bound to a
            // different provider than the turn therefore gets that provider's
            // clamp, not the turn's — which is the point of resolving per route.
            route.effective_effort(),
        )
    }
}

// ---------------------------------------------------------------------------
// The redaction gate (REQ-562 TASK-070)
// ---------------------------------------------------------------------------

/// The daemon's [`RedactionGate`]: resolve the `redact` duty, run it over the
/// payload, hand the verdict back to the choke point (REQ-562 ADR-1).
///
/// ## What is *not* in this struct is the guarantee
///
/// No transport, no provider, no secret resolver, no ledger — the same absence
/// that makes `LocalDuty` unable to reach a network, one layer up. The redactor
/// is what inspects a payload *before* it may leave; a redactor that egressed
/// would re-enter [`Egress::send`], be handed its own prompt to scan, and do it
/// again. The fields here are the reason that cannot happen, rather than a
/// depth counter or a re-entrancy flag that says it must not.
///
/// ## Cheap to build, because it is built per turn attempt
///
/// A [`Router`] clone (a table and a provider map), two `Arc` handles and a
/// session id. It is constructed at each [`Egress`] construction site and only
/// when the user opted in, so a machine with the feature off pays for none of
/// it (ADR-2).
pub(super) struct RedactionGateImpl {
    /// This turn's router — the same one the turn and its five sibling duties
    /// resolved through, so the scan cannot be routed by a different table than
    /// the payload it is scanning.
    pub(super) router: Router,
    /// Where `route_decided` goes when a scan actually runs (BR-2).
    pub(super) events: Arc<EventBus>,
    /// The session the scanned payload belongs to.
    pub(super) session_id: SessionId,
    /// The engine slot, read **per scan** rather than captured once: a real
    /// engine arrives mid-run when a consent install completes, and a gate that
    /// snapshotted an empty slot at construction would keep failing closed on a
    /// machine whose local tier came up thirty seconds ago.
    pub(super) engine: Arc<EngineSlot>,
}

impl RedactionGateImpl {
    /// Resolve the `redact` category for this scan (REQ-562 ADR-3).
    ///
    /// The sixth resolver, and it names its category literally for the same
    /// reason [`DaemonRuntime::digest_route`] and its four siblings do: the
    /// [`crate::call_sites`] derived-marker test reads the daemon's own source
    /// looking for a routing call with a `Category::` literal inside it, and a
    /// helper taking a category *variable* would make that scan blind.
    ///
    /// ## No session-taint arm, deliberately (ADR-3)
    ///
    /// The five REQ-561 resolvers check taint *first*, because for them taint
    /// changes the answer: a configurable category bound to a remote provider
    /// resolves local instead. It cannot change this one. `redact` is pinned
    /// local by construction — REQ-558 ADR-B gives it no configurable
    /// counterpart, so the resolver reaches it through the pinned-local branch,
    /// which consults no binding and yields the engine-backed local tier or
    /// nothing. A taint arm here would be a guard predicated on a distinction
    /// that cannot occur (LESSON-443): dead code wearing a safety costume.
    ///
    /// AC-12's property — a tainted session produces zero scanner calls — holds
    /// one layer up and more strongly: a tainted turn is pinned local, never
    /// reaches remote egress, and so never reaches the gate at all. This
    /// asymmetry with the sibling resolvers is intentional and this comment is
    /// the written reason, so it does not get "fixed" into uniformity.
    ///
    /// ## No remote arm, also deliberately (ADR-1)
    ///
    /// The siblings share [`DaemonRuntime::build_duty_route`], which can build
    /// a remote route. This one cannot, and that is the anti-recursion
    /// guarantee: the only route it constructs is
    /// [`DutyRoute::local`](crate::harness::DutyRoute::local), which holds an
    /// engine handle and no transport. It is not a locality *check* — there is
    /// no id comparison here and BR-2 forbids one — it is simply the one
    /// construction available. The resolver's answer for this category can only
    /// ever be the engine-backed local tier anyway: `local_tier_id` yields
    /// `None` when a non-local provider has taken the canonical id
    /// (BUG-156/TASK-057), so the pin has nothing remote to name.
    ///
    /// A route that resolved to nothing, or a tier with no engine loaded,
    /// returns [`DutyRoute::unresolved`] — which the scan turns into
    /// `Unavailable`, which blocks (ADR-6). Fail closed, with the resolver's own
    /// sentence carried verbatim (BR-6).
    pub(super) fn redact_route(&self) -> DutyRoute {
        let route = self.router.resolve(Category::Redact);
        let Some(provider_id) = route.provider_id.as_ref().map(|p| p.0.clone()) else {
            return DutyRoute::unresolved(route.reason.clone());
        };
        let Some((engine, _format)) = self.engine.get_with_format() else {
            return DutyRoute::unresolved(format!(
                "The 'redact' category resolves to '{provider_id}', but no local engine is \
                 loaded to serve it yet."
            ));
        };
        DutyRoute::local(REDACT_DUTY, provider_id, engine).announcing(
            &self.events,
            Some(self.session_id.clone()),
            route.route_decided(),
        )
    }
}

#[async_trait::async_trait]
impl RedactionGate for RedactionGateImpl {
    async fn scan(&self, payload: &str) -> RedactionVerdict {
        // The route is resolved per scan, not per turn: it is two map lookups
        // and an `Arc` clone, and resolving it here means a scan reflects the
        // engine slot as it is *now*. Nothing is held across the await below —
        // `get_with_format` takes the slot lock, clones the handle, and drops
        // it before this line returns.
        let route = self.redact_route();
        crate::harness::redact::scan(&route, payload).await
    }
}

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

/// Load the config from `path`.
///
/// A *genuinely absent* config file defaults — a fresh install has none, and
/// defaulting there is correct. But a config that **exists** and fails to parse
/// or validate must NOT be silently replaced by [`Config::default`] (H-1): the
/// default carries `boundaries: vec![]`, so failing open would drop every
/// declared privacy boundary, provider, routing rule and MCP server on the floor
/// and bring the daemon up with a security posture the user never chose — a typo
/// in one field silently disabling every `local-only` boundary. A present-but-
/// invalid config is refused instead, with a diagnostic naming the failure, so
/// the operator fixes it rather than unknowingly running wide open.
///
/// # Errors
/// Returns an error when a config file is present but cannot be read, parsed, or
/// validated. The message names the validation failure but no filesystem path
/// (BR-11).
pub(super) fn load_config(path: Option<&Path>) -> anyhow::Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(path) {
        // Present and readable: it MUST parse and validate. Refusing here is the
        // whole point — a fail-open default would drop the user's boundaries.
        Ok(text) => Config::load(&text).map_err(|e| {
            anyhow::anyhow!(
                "the daemon configuration is present but invalid, so it was NOT loaded. \
                 Refusing to start rather than fall back to an empty config that would \
                 silently drop your privacy boundaries, providers, routing, and MCP servers. \
                 Fix the config and restart. Cause: {e}"
            )
        }),
        // Genuinely absent (a fresh install): defaulting is correct.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        // Present but unreadable (permissions, I/O): surface it rather than
        // defaulting — the operator has a config they meant to apply.
        Err(err) => Err(anyhow::anyhow!(
            "the daemon configuration file exists but could not be read ({}); \
             refusing to start rather than silently ignore it.",
            err.kind()
        )),
    }
}

// ---------------------------------------------------------------------------
// The turn's category dispatch (REQ-558 TASK-053): `dispatch_route`
//
// These drive the exact function `run_prompt_turn` calls, so "a structured
// turn issues no classifier call" is a property of the daemon's dispatch
// rather than of a test that simply declined to call the classifier.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod dispatch {
    use super::*;
    use crate::classify::test_support::CountingEngine;
    use crate::fixture_id;
    use crate::runtime::testsupport::scratch_dir;
    use teton_core::category::JudgmentCategory;
    use teton_protocol::events::{Event as ProtoEvent, RouteDecided};
    use teton_protocol::Category as ProtoCategory;

    /// Every `route_decided` this subscription saw, oldest first.
    ///
    /// Drained with `try_recv` rather than awaited under a timeout:
    /// `EventBus::publish` is synchronous, so once the call under test has
    /// returned, everything it published is already queued (LESSON-450 — a
    /// wall-clock poll is the assertion shape that goes flaky first).
    ///
    /// One helper for all five duties, returning the **whole** event rather
    /// than a projection of it. A per-duty helper that extracted only the
    /// category was what let `compact` claim AC-2 while asserting a quarter
    /// of it: AC-2 asks for the category, the tier, the provider *and* a
    /// reason, and a helper that cannot see three of the four cannot be
    /// asked about them.
    fn announced(sub: &mut crate::broadcast::Subscription) -> Vec<RouteDecided> {
        std::iter::from_fn(|| sub.try_recv())
            .filter_map(|env| match env.event {
                ProtoEvent::RouteDecided(rd) => Some(rd),
                _ => None,
            })
            .collect()
    }

    /// Assert `decided` is the one `route_decided` a performed duty
    /// announces, and that it names all four things AC-2 asks for.
    ///
    /// Shared so that every duty is held to the same four, rather than to
    /// whichever subset its own test happened to spell out.
    fn assert_announced_route(
        decided: &[RouteDecided],
        category: ProtoCategory,
        tier: ProtoTier,
        provider_id: &str,
    ) {
        assert_eq!(
            decided.len(),
            1,
            "one performed duty announces exactly one route: {decided:?}"
        );
        let rd = &decided[0];
        assert_eq!(rd.category, Some(category), "{rd:?}");
        assert_eq!(rd.tier, Some(tier), "{rd:?}");
        assert_eq!(rd.provider_id.0, provider_id, "{rd:?}");
        assert!(
            !rd.reason.is_empty(),
            "a routing decision with no reason explains nothing: {rd:?}"
        );
    }

    fn remote(id: &str, model: &str) -> ModelProvider {
        ModelProvider {
            id: id.to_owned(),
            kind: ProviderKind::OpenaiCompatible,
            endpoint: Some("https://api.example.com/v1/chat/completions".to_owned()),
            model: Some(model.to_owned()),
            auth_ref: None,
            allow_cleartext: false,
            capabilities: ProviderCapabilities::default(),
        }
    }

    /// `think` on a frontier provider, `build` on a cheap one — AC-1's shape.
    /// The local tier names itself (no `[[providers]]` entry), which is the
    /// ordinary case.
    fn config() -> Config {
        Config {
            providers: vec![
                remote("frontier", "claude-opus-4"),
                remote("cheap", "deepseek-chat"),
            ],
            tiers: vec![
                TierBinding {
                    tier: Tier::Think,
                    provider_id: "frontier".to_owned(),
                    fallback_id: None,
                },
                TierBinding {
                    tier: Tier::Build,
                    provider_id: "cheap".to_owned(),
                    fallback_id: None,
                },
            ],
            ..Config::default()
        }
    }

    /// A runtime with `config`, `engine` in the serving slot, and the local
    /// tier's BR-8 latency duty set to `local_available`.
    fn runtime(config: Config, engine: &CountingEngine, local_available: bool) -> DaemonRuntime {
        let runtime = DaemonRuntime::minimal();
        *runtime.config.lock().expect("config mutex") = config;
        runtime
            .engine
            .install("counting".to_owned(), engine.handle());
        runtime
            .local_available
            .store(local_available, Ordering::SeqCst);
        runtime
    }

    /// The router the turn path builds, from the same runtime state.
    fn router_for(runtime: &DaemonRuntime) -> Router {
        let config = runtime.config.lock().expect("config mutex").clone();
        build_router(&config, runtime.local_tier_available(), &BTreeMap::new())
    }

    // -- a refused DUTY marks the session, directly (REQ-544 C-2) --------

    /// **The choke point marks the session it refused, not just the turn.**
    ///
    /// A refused duty never becomes a turn error, so
    /// [`DaemonRuntime::run_prompt_turn`]'s own `is_privacy_blocked` arm
    /// cannot see it: the seam turns the refusal into a sentence, the call
    /// site degrades by its own means, and the turn completes. Today the
    /// session is tainted anyway — but only *incidentally*, because the
    /// content that got the duty refused is still in `ctx` when the turn
    /// ends and `context_is_sensitive` reads it there. That cover depends on
    /// truncation and compaction not having dropped it, which both are
    /// entitled to do. This makes it direct.
    ///
    /// No byte leaves in either leg: the refusal happens before the
    /// transport is reached, which is the whole point of the choke point.
    #[tokio::test]
    async fn a_duty_refused_at_the_choke_point_taints_its_session() {
        let engine = CountingEngine::answering("Retry the download client");
        let mut config = config();
        // `title` is `reflex` and never inherits a tier, so an explicit
        // category override is the one way a user binds it off the machine —
        // and it is what makes this route remote enough to be refusable.
        config.categories.push(CategoryOverride {
            name: ConfigurableCategory::Title,
            provider_id: "frontier".to_owned(),
            fallback_id: None,
        });
        config.boundaries = vec![PrivacyBoundary {
            path_glob: "secrets/**".to_owned(),
            mode: BoundaryMode::LocalOnly,
            origin: Default::default(),
        }];
        let runtime = runtime(config.clone(), &engine, true);
        let router = router_for(&runtime);
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);

        let blocked = SessionId::from("blocked");
        let bystander = SessionId::from("bystander");
        let slot = runtime.engine.get_with_format();
        let route = runtime.title_route(DutyContext::detached(
            &bus,
            &blocked,
            &config,
            &router,
            slot.as_ref(),
            None,
        ));

        // Non-vacuity, both halves: the route really is remote — so there
        // really was a transport a byte could have left through — and the
        // session really was clean before the duty ran.
        assert_eq!(
            route.provider(),
            Some("frontier"),
            "a local route has no choke point to be refused at"
        );
        assert!(!runtime.session_taint.is_tainted(&blocked));

        let err = route
            .perform(
                "name this",
                &crate::egress::Provenance::tainted_by(fixture_id("secrets/prod.env")),
            )
            .await
            .expect_err("boundary content must not be titled remotely");
        assert!(err.contains("privacy boundary"), "{err}");

        assert!(
            runtime.session_taint.is_tainted(&blocked),
            "a duty refused at the choke point left its session unpinned, so the \
             next turn is free to reroute remotely"
        );
        assert!(
            !runtime.session_taint.is_tainted(&bystander),
            "and it taints only the session it happened in"
        );
        // The event is still published — marking is in addition to
        // announcing, never instead of it.
        assert!(
            std::iter::from_fn(|| sub.try_recv())
                .any(|env| matches!(env.event, Event::PrivacyBlock(_))),
            "the authoritative `privacy_block` stopped being emitted"
        );
    }

    /// The other half of the same rule, stated at the sink because it is the
    /// one case the wiring test above cannot reach: a block the choke point
    /// could not attribute to a session pins nothing, rather than pinning
    /// something arbitrary.
    #[test]
    fn an_unattributable_privacy_block_pins_no_session() {
        let taint = Arc::new(SessionTaint::new());
        let sink =
            TaintingPrivacySink::for_turn_path(Arc::new(EventBus::new()), Arc::clone(&taint));
        let block = teton_protocol::events::PrivacyBlock {
            path: "secrets/prod.env".to_owned(),
            provider_id: ProviderId::from("frontier"),
            action: teton_protocol::events::PrivacyAction::ReroutedToLocal,
            cause: teton_protocol::events::BlockCause::Boundary,
        };

        crate::egress::PrivacyEventSink::privacy_block(&sink, None, block.clone());
        crate::egress::PrivacyEventSink::privacy_block(&sink, Some(SessionId::from("s")), block);

        assert!(
            taint.is_tainted(&SessionId::from("s")),
            "non-vacuity: a scoped block really does pin"
        );
        assert!(!taint.is_tainted(&SessionId::from("somebody-else")));
    }

    // -- which causes pin, and which do not (REQ-562) --------------------

    /// **A scan that could not run establishes nothing, so it pins
    /// nothing** (REQ-544 C-2 × REQ-562 BR-3).
    ///
    /// The taint is a *durable, session-wide* consequence: every remaining
    /// turn goes to the local tier and the user has no way to undo it short
    /// of a new session. C-2's justification for that is that content
    /// **crossed a boundary** and the model may restate it later. A
    /// `ScanUnavailable` block carries no such fact — no local tier, an
    /// over-cap payload, an engine error, a 120-second deadline — nothing
    /// looked at the payload, so nothing is known about it. Pinning on it
    /// lets one transient stall silently downgrade the rest of a session.
    ///
    /// The payload is still refused either way; that is BR-3's fail-closed
    /// posture and it is per-payload.
    ///
    /// All three causes are driven through the same sink, in the same test,
    /// so the two that pin are the discrimination for the one that does not
    /// (LESSON-485).
    #[test]
    fn a_scan_unavailable_block_refuses_the_payload_without_pinning_the_session() {
        fn pinned_by(cause: BlockCause) -> bool {
            let taint = Arc::new(SessionTaint::new());
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sink = TaintingPrivacySink::for_turn_path(bus, Arc::clone(&taint));
            let session = SessionId::from("s");
            crate::egress::PrivacyEventSink::privacy_block(
                &sink,
                Some(session.clone()),
                teton_protocol::events::PrivacyBlock {
                    path: "the outbound payload".to_owned(),
                    provider_id: ProviderId::from("frontier"),
                    action: teton_protocol::events::PrivacyAction::ReroutedToLocal,
                    cause,
                },
            );
            // Whatever the cause, the block is still announced — the pin is
            // a *consequence* of a report, never a replacement for one.
            assert!(
                std::iter::from_fn(|| sub.try_recv())
                    .any(|env| matches!(env.event, Event::PrivacyBlock(_))),
                "{cause:?}: the authoritative privacy_block must still fire"
            );
            taint.is_tainted(&session)
        }

        assert!(
            pinned_by(BlockCause::Boundary),
            "content came from a local-only source: C-2's original case"
        );
        assert!(
            pinned_by(BlockCause::Redaction {
                kind: teton_protocol::events::FindingKind::Credential,
                span: teton_protocol::events::ByteSpan { start: 10, end: 30 },
            }),
            "the scan FOUND something, and the model can restate it next turn"
        );
        assert!(
            !pinned_by(BlockCause::ScanUnavailable),
            "a scan that never ran established nothing about the payload, so it \
             must not pin the whole session to the local tier"
        );
    }

    /// The turn path's taint gate and the sink's are the same rule written
    /// in two type systems, and they must agree cause for cause.
    ///
    /// The turn path never sees a [`BlockCause`] — the cause reaches it as a
    /// [`BlockDetail`] across the `teton-providers` seam, which declares no
    /// protocol dependency by design. So the rule exists twice, and this is
    /// what stops the two copies drifting into a session that a *duty*
    /// pinned and a *turn* did not.
    ///
    /// **The MCP gate is not one of these two copies** and is deliberately
    /// absent from the rows below: it is a different rule for a different
    /// choke point, not a third spelling of this one. The test directly
    /// after this asserts where it diverges, so "these two agree" and "that
    /// third one does not" are both pinned rather than one of them being
    /// inferred from the other's silence.
    #[test]
    fn the_two_taint_gates_agree_cause_for_cause() {
        let rows = [
            (BlockCause::Boundary, BlockDetail::Boundary),
            (
                BlockCause::Redaction {
                    kind: teton_protocol::events::FindingKind::Pii,
                    span: teton_protocol::events::ByteSpan { start: 0, end: 4 },
                },
                BlockDetail::Redaction,
            ),
            (BlockCause::ScanUnavailable, BlockDetail::ScanUnavailable),
        ];
        for (cause, detail) in rows {
            assert_eq!(
                cause_taints_the_session(&cause),
                taints_the_session(detail),
                "the duty path and the turn path disagree about {cause:?}"
            );
        }
        // Non-vacuity: the rule is not constant, so agreeing about
        // everything is not agreeing about nothing.
        assert!(taints_the_session(BlockDetail::Boundary));
        assert!(!taints_the_session(BlockDetail::ScanUnavailable));
    }

    /// **The MCP gate is a third rule, and the divergence is the point**
    /// (REQ-562; user decision, 2026-08-08).
    ///
    /// Stated as a difference rather than left to be discovered, because
    /// the failure mode is a later tidy-up: two functions that agree on two
    /// of three causes look like duplication, and folding them into one
    /// would silently re-decide REQ-544's MCP boundary posture — the
    /// decision that says an MCP boundary refusal is an in-context tool
    /// error and nothing more. Asserting the disagreement makes that fold
    /// turn red.
    ///
    /// The redaction row is where the two rules *must* agree: the model
    /// authored the tool arguments the scan refused, so it holds a secret
    /// it can restate through an ordinary turn, and the surface it was
    /// caught on does not change that.
    #[test]
    fn the_mcp_gate_pins_redaction_and_diverges_from_the_turn_path_on_boundary() {
        let redaction = BlockCause::Redaction {
            kind: teton_protocol::events::FindingKind::Credential,
            span: teton_protocol::events::ByteSpan { start: 10, end: 30 },
        };

        // Where they agree, and why.
        assert!(
            mcp_cause_taints_the_session(&redaction),
            "the model wrote those arguments and can restate the finding next turn"
        );
        assert!(
            cause_taints_the_session(&redaction),
            "non-vacuity: the turn path's answer for the same cause"
        );
        assert!(!mcp_cause_taints_the_session(&BlockCause::ScanUnavailable));
        assert!(!cause_taints_the_session(&BlockCause::ScanUnavailable));

        // Where they differ, and the direction of the difference.
        assert!(
            !mcp_cause_taints_the_session(&BlockCause::Boundary),
            "REQ-544's fold-without-pinning posture for the MCP surface is kept"
        );
        assert!(
            cause_taints_the_session(&BlockCause::Boundary),
            "and the turn path still pins on it — this row is the divergence"
        );
    }

    /// **AC-1, the direct regression, end to end through the daemon's own
    /// dispatch.**
    ///
    /// A freeform session, `think` bound to a frontier provider, and the
    /// prompt *"explain the tradeoffs between these two architectures"*. The
    /// deleted `AUXILIARY_SIGNALS` list sent this to the 3B local model for
    /// containing the word `explain` and never read the table at all. Now the
    /// local tier is asked what the prompt *is*, answers `design`, and
    /// `design` inherits `think`.
    #[tokio::test]
    async fn a_freeform_design_prompt_reaches_the_think_binding_not_the_local_tier() {
        let engine = CountingEngine::answering("design");
        let runtime = runtime(config(), &engine, true);
        let router = router_for(&runtime);

        let route = runtime
            .dispatch_route(
                &router,
                &SessionId::from("sess"),
                SessionMode::Freeform,
                None,
                "explain the tradeoffs between these two architectures",
            )
            .await;

        assert_eq!(engine.calls(), 1, "exactly one classification");
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("frontier"),
            "a design turn goes to the think binding: {}",
            route.reason
        );
        assert_eq!(
            route.resolution.as_ref().map(|r| r.category),
            Some(Category::Design)
        );
        assert!(route.phase.is_none(), "a freeform turn attributes no phase");

        // BR-3: the decision names the category, the tier, the provider, and
        // the signal that fired.
        let decided = route.route_decided().expect("a provider was selected");
        assert_eq!(decided.category, Some(ProtoCategory::Design));
        assert_eq!(decided.tier, Some(teton_protocol::Tier::Think));
        assert!(decided.reason.contains("classifier"), "{}", decided.reason);
        assert!(decided.reason.contains("'design'"), "{}", decided.reason);
    }

    /// **ADR-C, by call count.** A structured turn already knows what it is
    /// doing, so it derives its category from its phase with no model call —
    /// with a perfectly good classifier engine sitting in the slot.
    #[tokio::test]
    async fn a_structured_turn_issues_zero_classifier_calls() {
        let engine = CountingEngine::answering("design");
        let runtime = runtime(config(), &engine, true);
        let router = router_for(&runtime);

        for (phase, provider, category) in [
            (CorePhase::Implement, "cheap", Category::Edit),
            (CorePhase::Architect, "frontier", Category::Design),
            (CorePhase::Review, "frontier", Category::Review),
        ] {
            let route = runtime
                .dispatch_route(
                    &router,
                    &SessionId::from("sess"),
                    SessionMode::Structured,
                    Some(phase),
                    "explain the tradeoffs between these two architectures",
                )
                .await;

            assert_eq!(
                engine.calls(),
                0,
                "a structured turn classifies nothing (ADR-C)"
            );
            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                Some(provider)
            );
            assert_eq!(
                route.resolution.as_ref().map(|r| r.category),
                Some(category)
            );
            // BR-11: the phase is attribution, stamped on after the decision.
            assert_eq!(route.phase, Some(to_protocol_phase(phase)));
        }
    }

    /// **AC-5 / BR-5, by call count.** The local tier cannot meet its latency
    /// duty, so `route` resolves to nothing, classification is skipped
    /// entirely, and the turn takes the BR-9 declared default *through the
    /// same resolver chain*. The engine is present and reachable — it is the
    /// counter, not its absence, that proves no call was issued.
    #[tokio::test]
    async fn an_unavailable_local_tier_bypasses_classification_with_no_call() {
        let engine = CountingEngine::answering("design");
        let runtime = runtime(config(), &engine, false);
        let router = router_for(&runtime);

        let route = runtime
            .dispatch_route(
                &router,
                &SessionId::from("sess"),
                SessionMode::Freeform,
                None,
                "explain the tradeoffs between these two architectures",
            )
            .await;

        assert_eq!(engine.calls(), 0, "the bypass issues no call at all (BR-5)");
        // The declared default is `edit`, which inherits `build`.
        assert_eq!(
            route.resolution.as_ref().map(|r| r.category),
            Some(Category::Edit)
        );
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("cheap"),
            "the degraded means is still a category resolved through the table"
        );
        let decided = route.route_decided().expect("a provider was selected");
        assert!(decided.reason.contains("bypassed"), "{}", decided.reason);
        assert!(
            decided
                .reason
                .contains("no classification call was issued, locally or remotely"),
            "{}",
            decided.reason
        );
    }

    /// The bypass takes the **configured** default, not a constant: change
    /// `judgment_default` and the bypassed turn lands somewhere else entirely
    /// (BR-9, AC-12). `review` inherits `think`, so this one goes frontier.
    #[tokio::test]
    async fn the_bypassed_default_is_the_configured_one() {
        let engine = CountingEngine::answering("design");
        let runtime = runtime(
            Config {
                judgment_default: JudgmentCategory::Review,
                ..config()
            },
            &engine,
            false,
        );
        let router = router_for(&runtime);

        let route = runtime
            .dispatch_route(
                &router,
                &SessionId::from("sess"),
                SessionMode::Freeform,
                None,
                "anything at all",
            )
            .await;

        assert_eq!(engine.calls(), 0);
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("frontier")
        );
        assert_eq!(
            route.resolution.as_ref().map(|r| r.category),
            Some(Category::Review)
        );
    }

    /// **The unserved-turn guard, driven through `dispatch_route` itself.**
    ///
    /// A tier bound to a declared local provider that is above the floor and
    /// decided, but whose weights are still loading — BUG-152's own state.
    /// `dispatch_route` genuinely **selects** it (the binding is perfect),
    /// and the harness then returns `NoTierAvailable` because the slot is
    /// empty. What the user must read is the tier's state, not the
    /// resolver's success sentence read out as an error.
    ///
    /// The sibling of `a_selected_route_keeps_the_classifiers_sentence_
    /// unchanged`, at the layer the daemon actually calls: that one pins the
    /// composition, this one proves the daemon's own dispatch reaches the
    /// arm at all.
    #[tokio::test]
    async fn a_selected_but_unloaded_local_tier_reports_a_tier_state_not_a_routing_failure() {
        let mut config = config();
        config.providers.push(ModelProvider {
            id: "on-device".to_owned(),
            kind: ProviderKind::Local,
            endpoint: None,
            model: None,
            auth_ref: None,
            allow_cleartext: false,
            capabilities: ProviderCapabilities::default(),
        });
        // `edit` inherits `build`; bind it to the local tier explicitly.
        config.tiers.retain(|t| t.tier != Tier::Build);
        config.tiers.push(TierBinding {
            tier: Tier::Build,
            provider_id: "on-device".to_owned(),
            fallback_id: None,
        });

        let engine = CountingEngine::answering("edit");
        // `local_available` is true — the tier is above the floor and
        // decided — which is what lets the resolver select it.
        let runtime = runtime(config.clone(), &engine, true);
        let router = router_for(&runtime);

        let route = runtime
            .dispatch_route(
                &router,
                &SessionId::from("sess"),
                SessionMode::Freeform,
                None,
                "add a retry to the upload helper",
            )
            .await;
        assert!(
            route.selected(),
            "the premise: a binding that resolves cleanly — {}",
            route.reason
        );
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some("on-device")
        );

        // What `run_prompt_turn` does with `HarnessError::NoTierAvailable`.
        let category = route.resolution.as_ref().map(|r| r.category);
        let classified = runtime.unserved_turn_error(&config, category);
        let shown = unserved_turn_sentence(&route, classified.clone());

        assert_eq!(
            shown.message, classified.message,
            "the binding worked; only the tier's state failed, and only the \
             classifier can describe that"
        );
        assert!(
            !shown.message.contains("Routing the"),
            "the resolver's SUCCESS sentence must not be prefixed onto an \
             error — it contradicts it and blames the wrong subsystem: {}",
            shown.message
        );
    }

    /// **BR-7 / LESSON-432.** Taint is the outermost check, so a pinned
    /// session does not even reach the classifier. Category routing is a cost
    /// decision and the boundary is a privacy guarantee; a classification call
    /// on a tainted turn would be the two starting to compose.
    #[tokio::test]
    async fn a_tainted_session_is_pinned_local_and_classifies_nothing() {
        let engine = CountingEngine::answering("design");
        let runtime = runtime(config(), &engine, true);
        let router = router_for(&runtime);
        let session = SessionId::from("tainted");
        runtime.session_taint.mark(&session);

        let route = runtime
            .dispatch_route(
                &router,
                &session,
                SessionMode::Freeform,
                None,
                "explain the tradeoffs between these two architectures",
            )
            .await;

        assert_eq!(engine.calls(), 0, "a tainted turn classifies nothing");
        assert_eq!(
            route.provider_id.as_ref().map(|p| p.0.as_str()),
            Some(LOCAL_PROVIDER_ID)
        );
        // And — the load-bearing half — that id is one this daemon serves
        // **on the machine**. Asserting the name alone is what let a config
        // with a remote provider registered under `local` keep this test
        // green while dispatching the pinned turn over HTTP.
        assert_engine_backed(&config(), &route);
        assert!(
            route.resolution.is_none(),
            "the taint pin resolves no category at all (BR-7)"
        );
    }

    /// The pin **asserts locality**: whatever provider it names, the daemon
    /// must serve it on this machine, and where it can name none it must
    /// name none rather than reach for a lookalike.
    ///
    /// Swept across the three shapes `local_tier_id` distinguishes, because
    /// the whole defect was that the third one was indistinguishable from
    /// the first by name.
    #[tokio::test]
    async fn the_taint_pin_never_names_a_provider_the_daemon_would_dial() {
        /// A `[[providers]]` entry that is genuinely the on-device tier.
        fn local(id: &str) -> ModelProvider {
            ModelProvider {
                id: id.to_owned(),
                kind: ProviderKind::Local,
                endpoint: None,
                model: None,
                auth_ref: None,
                allow_cleartext: false,
                capabilities: ProviderCapabilities::default(),
            }
        }

        // 1. The canonical case: no `[[providers]]` entry, the engine-backed
        //    tier names itself.
        let mut declared = config();
        // 2. A declared `kind = "local"` entry under any id at all.
        let mut named = config();
        named.providers.push(local("on-device"));
        // 3. The hazard: a REMOTE provider holding the canonical id, and no
        //    `kind = "local"` entry anywhere. `local` here is a vendor API
        //    that merely shares a name with the tier.
        let mut squatted = config();
        squatted
            .providers
            .push(remote(LOCAL_PROVIDER_ID, "some-hosted-model"));

        for (label, config, expected) in [
            ("canonical", &mut declared, Some(LOCAL_PROVIDER_ID)),
            ("declared", &mut named, Some("on-device")),
            ("squatted", &mut squatted, None),
        ] {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(config.clone(), &engine, true);
            let router = router_for(&runtime);
            let session = SessionId::from("tainted");
            runtime.session_taint.mark(&session);

            let route = runtime
                .dispatch_route(&router, &session, SessionMode::Freeform, None, "anything")
                .await;

            assert_eq!(
                route.provider_id.as_ref().map(|p| p.0.as_str()),
                expected,
                "{label}: the pin named the wrong provider — {}",
                route.reason
            );
            assert_engine_backed(config, &route);
        }
    }

    /// The provider a route names must be one the daemon serves **without a
    /// network call**, read from the same two facts `run_one_attempt` and
    /// `digest_route` read: a `[[providers]]` entry declaring
    /// `kind = "local"`, or no entry at all — in which case there is nothing
    /// to dial and only the engine can serve it.
    ///
    /// Naming no provider passes: the turn stops rather than going out.
    fn assert_engine_backed(config: &Config, route: &crate::router::Route) {
        let Some(id) = route.provider_id.as_ref().map(|p| p.0.as_str()) else {
            return;
        };
        // No entry at all is fine: `run_one_attempt` finds no
        // `provider_cfg`, so it either runs on the engine or fails closed
        // with `NoTierAvailable`. Neither reaches a transport.
        if let Some(p) = config.providers.iter().find(|p| p.id == id) {
            assert!(
                matches!(p.kind, ProviderKind::Local),
                "a route pinned local named `{id}`, which this config registers as a \
                 `{:?}` provider — dispatch reads that kind and sends the turn over \
                 HTTP. The pin must assert locality, not a name.",
                p.kind
            );
        }
    }

    // -------------------------------------------------------------------
    // The `digest` duty's own dispatch (REQ-558 TASK-054): `digest_route`.
    //
    // `digest` is the one harness-known category with a real call site, and
    // before this it was hardcoded to the local engine — a configuration
    // surface the runtime never read, which is BR-1's defect in miniature.
    // These drive the exact function `run_one_attempt` calls.
    // -------------------------------------------------------------------
    mod digest {
        use super::*;
        use teton_core::category::{CategoryOverride, ConfigurableCategory};

        /// `config()` plus a `scan` binding — the tier `digest` inherits.
        fn scan_bound_to(provider_id: &str) -> Config {
            let mut config = config();
            config.tiers.push(TierBinding {
                tier: Tier::Scan,
                provider_id: provider_id.to_owned(),
                fallback_id: None,
            });
            config
        }

        /// The `digest` route the turn path builds, from the same runtime
        /// state and through the same router.
        fn digest_for(runtime: &DaemonRuntime, session: &SessionId) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.digest_route(DutyContext::detached(
                &Arc::new(EventBus::new()),
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// **BR-1 for a harness-known category.** `digest` is a `scan` duty,
        /// so binding `scan` sends the summarizer there — the configured
        /// table is read for this call as for any other. Before TASK-054
        /// this binding was inert and the duty ran on the local engine no
        /// matter what the config said.
        #[test]
        fn digest_inherits_the_scan_tier_binding() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("cheap"), &engine, true);
            assert_eq!(
                digest_for(&runtime, &SessionId::from("sess")).provider(),
                Some("cheap")
            );
        }

        /// A per-category override beats the tier, here as everywhere —
        /// override → tier → error is one precedence, not one per call site.
        #[test]
        fn a_digest_override_beats_the_scan_binding() {
            let engine = CountingEngine::answering("design");
            let mut config = scan_bound_to("cheap");
            config.categories.push(CategoryOverride {
                name: ConfigurableCategory::Digest,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            assert_eq!(
                digest_for(&runtime, &SessionId::from("sess")).provider(),
                Some("frontier")
            );
        }

        /// With nothing bound to `scan`, `digest` inherits the local tier —
        /// the pre-REQ behaviour, preserved for every user who configures
        /// nothing.
        #[test]
        fn an_unbound_scan_tier_digests_locally() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(config(), &engine, true);
            assert_eq!(
                digest_for(&runtime, &SessionId::from("sess")).provider(),
                Some(LOCAL_PROVIDER_ID)
            );
        }

        /// **BR-7 / LESSON-432.** Session taint overrides the category
        /// binding for a *duty* as for a turn. A tainted session with `scan`
        /// bound to a remote provider still digests locally — otherwise the
        /// boundary backstop would hold for the conversation and leak
        /// through the summarizer, which reads the same files.
        ///
        /// This is the mutation-sensitive one: deleting the taint check in
        /// `digest_route` turns this red on its own, at its own layer.
        #[test]
        fn a_tainted_session_digests_on_the_local_tier() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("frontier"), &engine, true);
            let session = SessionId::from("tainted");

            // Non-vacuity: the same config, untainted, genuinely goes remote.
            assert_eq!(
                digest_for(&runtime, &SessionId::from("clean")).provider(),
                Some("frontier")
            );

            runtime.session_taint.mark(&session);
            assert_eq!(
                digest_for(&runtime, &session).provider(),
                Some(LOCAL_PROVIDER_ID),
                "a tainted session must digest locally (BR-7)"
            );
        }

        /// An unresolvable binding is a *reason*, not a silent `None`: the
        /// resolver's own sentence rides onto the route so the caller can
        /// say why it fell back to mechanical truncation (BR-6, BR-8,
        /// LESSON-447). `ghost` is bound but registered nowhere, so nothing
        /// can serve `digest` and no id is synthesized to pretend otherwise.
        #[test]
        fn an_unroutable_scan_binding_leaves_digest_unresolved_with_a_reason() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("ghost"), &engine, true);

            let route = digest_for(&runtime, &SessionId::from("sess"));

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("an unroutable binding must not resolve to a provider");
            };
            assert!(reason.contains("digest"), "{reason}");
            assert!(reason.contains("ghost"), "{reason}");
        }

        /// A remote-only machine with nothing bound to `scan`: `digest`
        /// inherits the local tier, which cannot serve. Unresolved — and the
        /// sentence is the **resolver's**, carried verbatim rather than
        /// re-authored here (BR-6, AC-11). The old code's answer to this
        /// state was to fold the oversized result raw.
        #[test]
        fn a_machine_with_no_engine_and_no_scan_binding_cannot_digest() {
            let runtime = DaemonRuntime::minimal();
            *runtime.config.lock().expect("config mutex") = config();

            let route = digest_for(&runtime, &SessionId::from("sess"));

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("there is nothing to serve the duty");
            };
            // Byte-for-byte the resolver's own sentence for this state.
            let config = runtime.config.lock().expect("config mutex").clone();
            let resolved = build_router(&config, runtime.local_tier_available(), &BTreeMap::new())
                .resolve(Category::Digest);
            assert_eq!(reason, resolved.reason);
            assert!(reason.contains("'digest' cannot be routed"), "{reason}");
        }

        // ---------------------------------------------------------------
        // REQ-561 BR-2: `route_decided` for the duty, and *when* it fires.
        //
        // These two are a pair (LESSON-485). The positive alone would pass
        // against an emitter that announced at resolution time; the negative
        // alone would pass against an emitter that never announced at all.
        // Only together do they pin "announced iff the duty actually ran".
        // ---------------------------------------------------------------

        /// The `digest` route the turn path builds, on a bus the test can
        /// watch. `config()` leaves `scan` unbound, so `digest` inherits the
        /// **local** tier — which is what makes performing it in-process
        /// possible without a network call.
        fn watched_digest(
            runtime: &DaemonRuntime,
            bus: &Arc<EventBus>,
            session: &SessionId,
        ) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.digest_route(DutyContext::detached(
                bus,
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// **REQ-561 BR-2, the positive half.** A `digest` that actually runs
        /// announces where it went, on the same `route_decided` surface a
        /// turn uses: the category, the tier it resolved through, the
        /// provider serving it, and a non-empty reason.
        ///
        /// REQ-558 routed the duty and told nobody. That is the one category
        /// whose whole premise is that it resolves *independently of the
        /// turn* — so a user watching only the turn's `route_decided` saw a
        /// frontier `think` provider while their file bodies went to whatever
        /// `scan` was bound to, with no event saying so.
        ///
        /// Deliberately asserted off the **bus**, not off the returned route:
        /// "the user can see it" is a claim about a published event, and a
        /// duty that ran correctly while announcing nothing is exactly the
        /// state this test exists to fail on.
        #[tokio::test]
        async fn a_performed_digest_announces_its_route() {
            let engine = CountingEngine::answering("CONDENSED");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);

            let route = watched_digest(&runtime, &bus, &SessionId::from("sess"));
            assert_eq!(
                route.provider(),
                Some(LOCAL_PROVIDER_ID),
                "the duty must resolve, or this test proves nothing"
            );

            let out = route
                .perform("Summarize this.", &crate::egress::Provenance::empty())
                .await;
            assert_eq!(out.as_deref(), Ok("CONDENSED"), "the duty really ran");

            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Digest,
                ProtoTier::Scan,
                LOCAL_PROVIDER_ID,
            );
        }

        /// **REQ-561 BR-2, the negative half — and the whole point of it.**
        ///
        /// `digest_route` is built unconditionally once per turn attempt,
        /// whether or not any tool result crosses the summarization
        /// threshold. Announcing at *resolution* would therefore put a
        /// `route_decided` on the wire for a routed model call that never
        /// happened, on every turn — and five of them per turn once the
        /// remaining four duties are wired. BR-2 exists to make an egress
        /// path visible, and a path that never fires produced no egress.
        ///
        /// This is the assertion that fails if emission moves back to the
        /// resolver. Its non-vacuity is the test above, which shows this same
        /// route *does* announce the moment it is performed.
        #[test]
        fn a_digest_that_never_runs_announces_nothing() {
            let engine = CountingEngine::answering("CONDENSED");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);

            let route = watched_digest(&runtime, &bus, &SessionId::from("sess"));

            // The discriminating state is reachable: this route resolved to a
            // provider and carries an announcement it is holding back. A
            // fixture that could not resolve would pass this vacuously.
            assert_eq!(route.provider(), Some(LOCAL_PROVIDER_ID));
            assert_eq!(
                engine.calls(),
                0,
                "resolving a duty must not call the model"
            );

            let decided = announced(&mut sub);
            assert!(
                decided.is_empty(),
                "resolving a duty is not performing one; announcing here would \
                 report a routed model call that never happened: {decided:?}"
            );
        }
    }

    // -------------------------------------------------------------------
    // The `triage` duty's own dispatch (REQ-561 TASK-060): `triage_route`.
    //
    // Same two layers as `digest`, asserted separately because they are two
    // decisions: a session may well digest locally and triage remotely, and
    // a shared resolver that quietly collapsed them would pass `digest`'s
    // tests while breaking this one.
    // -------------------------------------------------------------------
    mod triage {
        use super::*;
        use teton_core::category::{CategoryOverride, ConfigurableCategory};

        /// `config()` plus a `scan` binding — the tier `triage` inherits.
        fn scan_bound_to(provider_id: &str) -> Config {
            let mut config = config();
            config.tiers.push(TierBinding {
                tier: Tier::Scan,
                provider_id: provider_id.to_owned(),
                fallback_id: None,
            });
            config
        }

        /// The `triage` route the turn path builds, from the same runtime
        /// state and through the same router.
        fn triage_for(runtime: &DaemonRuntime, session: &SessionId) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.triage_route(DutyContext::detached(
                &Arc::new(EventBus::new()),
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// **BR-1.** `triage` is a `scan` duty, so binding `scan` sends the
        /// ranking there — grep match text, which is file content, goes to
        /// whatever that tier names. The configured table is read for this
        /// call as for any other.
        #[test]
        fn triage_inherits_the_scan_tier_binding() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("cheap"), &engine, true);
            assert_eq!(
                triage_for(&runtime, &SessionId::from("sess")).provider(),
                Some("cheap")
            );
        }

        /// A per-category override beats the tier here as everywhere.
        #[test]
        fn a_triage_override_beats_the_scan_binding() {
            let engine = CountingEngine::answering("design");
            let mut config = scan_bound_to("cheap");
            config.categories.push(CategoryOverride {
                name: ConfigurableCategory::Triage,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            assert_eq!(
                triage_for(&runtime, &SessionId::from("sess")).provider(),
                Some("frontier")
            );
        }

        /// With nothing bound to `scan`, `triage` inherits the local tier —
        /// so a user who configures nothing gets ranking without egress.
        #[test]
        fn an_unbound_scan_tier_triages_locally() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(config(), &engine, true);
            assert_eq!(
                triage_for(&runtime, &SessionId::from("sess")).provider(),
                Some(LOCAL_PROVIDER_ID)
            );
        }

        /// **BR-5 / LESSON-432.** Session taint overrides the category
        /// binding. A tainted session with `scan` bound remotely still ranks
        /// locally — and `triage` is the duty where that matters most
        /// concretely, because the content it sends is *lines out of the
        /// files that tainted the session in the first place*.
        ///
        /// The mutation-sensitive one: deleting the taint check in
        /// `triage_route` turns this red on its own, at its own layer.
        #[test]
        fn a_tainted_session_triages_on_the_local_tier() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("frontier"), &engine, true);
            let session = SessionId::from("tainted");

            // Non-vacuity: the same config, untainted, genuinely goes remote.
            assert_eq!(
                triage_for(&runtime, &SessionId::from("clean")).provider(),
                Some("frontier")
            );

            runtime.session_taint.mark(&session);
            assert_eq!(
                triage_for(&runtime, &session).provider(),
                Some(LOCAL_PROVIDER_ID),
                "a tainted session must rank locally (BR-5)"
            );
        }

        /// An unroutable binding is a *reason*, not a silent `None`: the
        /// caller has to be able to say why the matches came back unranked
        /// (BR-3, LESSON-447).
        #[test]
        fn an_unroutable_scan_binding_leaves_triage_unresolved_with_a_reason() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(scan_bound_to("ghost"), &engine, true);

            let route = triage_for(&runtime, &SessionId::from("sess"));

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("an unroutable binding must not resolve to a provider");
            };
            assert!(reason.contains("triage"), "{reason}");
            assert!(reason.contains("ghost"), "{reason}");
        }
    }

    // -------------------------------------------------------------------
    // The `shell` duty's own dispatch (REQ-561 TASK-061): `shell_route`.
    //
    // `shell` is a **build** duty where `triage` is a `scan` one, so these
    // are not `triage`'s tests with a word changed: a config that sends
    // ranking to a cheap model and interpretation to a stronger one is the
    // ordinary case, and a resolver that quietly collapsed the two would
    // pass `triage`'s tests while breaking these.
    // -------------------------------------------------------------------
    mod shell {
        use super::*;
        use teton_core::category::{CategoryOverride, ConfigurableCategory};

        /// The `shell` route the turn path builds, from the same runtime
        /// state and through the same router.
        fn shell_for(runtime: &DaemonRuntime, session: &SessionId) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.shell_route(DutyContext::detached(
                &Arc::new(EventBus::new()),
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// **BR-1.** `shell` is a `build` duty, so it follows the `build`
        /// binding — `config()`'s "cheap" — and not `scan`'s. Asserted
        /// against the tier `triage` uses, so a resolver that named the
        /// wrong category would be caught rather than look plausible.
        #[test]
        fn shell_inherits_the_build_tier_binding() {
            let engine = CountingEngine::answering("design");
            let mut config = config();
            config.tiers.push(TierBinding {
                tier: Tier::Scan,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            assert_eq!(
                shell_for(&runtime, &SessionId::from("sess")).provider(),
                Some("cheap"),
                "`shell` must follow `build`, not the `scan` tier beside it"
            );
        }

        /// A per-category override beats the tier here as everywhere.
        #[test]
        fn a_shell_override_beats_the_build_binding() {
            let engine = CountingEngine::answering("design");
            let mut config = config();
            config.categories.push(CategoryOverride {
                name: ConfigurableCategory::Shell,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            assert_eq!(
                shell_for(&runtime, &SessionId::from("sess")).provider(),
                Some("frontier")
            );
        }

        /// **BR-5 / LESSON-432.** Session taint overrides the category
        /// binding. A tainted session with `build` bound remotely still
        /// interprets locally.
        ///
        /// The mutation-sensitive one: deleting the taint check in
        /// `shell_route` turns this red on its own, at its own layer. Note
        /// that the egress choke point would *also* refuse this content —
        /// `shell` output is unattributable — but a guarantee that only holds
        /// because a second mechanism happens to catch it is not a guarantee
        /// stated where the decision is made (LESSON-484).
        #[test]
        fn a_tainted_session_interprets_on_the_local_tier() {
            let engine = CountingEngine::answering("design");
            let runtime = runtime(config(), &engine, true);
            let session = SessionId::from("tainted");

            // Non-vacuity: the same config, untainted, genuinely goes remote.
            assert_eq!(
                shell_for(&runtime, &SessionId::from("clean")).provider(),
                Some("cheap")
            );

            runtime.session_taint.mark(&session);
            assert_eq!(
                shell_for(&runtime, &session).provider(),
                Some(LOCAL_PROVIDER_ID),
                "a tainted session must interpret locally (BR-5)"
            );
        }

        /// An unroutable binding is a *reason*, not a silent `None`: the
        /// caller has to be able to say why the output came back
        /// uninterpreted (BR-3, LESSON-447).
        #[test]
        fn an_unroutable_build_binding_leaves_shell_unresolved_with_a_reason() {
            let engine = CountingEngine::answering("design");
            let mut config = config();
            config.tiers.retain(|t| t.tier != Tier::Build);
            config.tiers.push(TierBinding {
                tier: Tier::Build,
                provider_id: "ghost".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);

            let route = shell_for(&runtime, &SessionId::from("sess"));

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("an unroutable binding must not resolve to a provider");
            };
            assert!(reason.contains("shell"), "{reason}");
            assert!(reason.contains("ghost"), "{reason}");
        }

        // ---------------------------------------------------------------
        // REQ-561 AC-2 / BR-2: `route_decided` for the duty, and *when* it
        // fires.
        //
        // Missing until now. The seam's publish arm was mutated away and
        // the whole workspace was run: five tests went red, and not one of
        // them was `shell`'s — the category's routing was pinned only by
        // `.provider()` on the resolved route, which says where the duty
        // *would* go and nothing about what reached the wire. The five
        // `*_route` resolvers differ by one `Category::` literal, so that
        // gap is one copy-paste away from a `shell` duty announcing itself
        // as something else with every test still green.
        // ---------------------------------------------------------------

        /// The `shell` route the turn path builds, on a bus the test can
        /// watch. The `build` binding is dropped by the caller so `shell`
        /// inherits the **local** tier, which is what makes performing it
        /// in-process possible without a network call.
        fn watched_shell(
            runtime: &DaemonRuntime,
            bus: &Arc<EventBus>,
            session: &SessionId,
        ) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.shell_route(DutyContext::detached(
                bus,
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// `ShellTool::run` then `ShellTool::refine` over `route`, in `root`.
        ///
        /// Driven through the real tool rather than a hand-built outcome
        /// because the negative half's whole claim is that *the call site*
        /// declined — `worth_interpreting` reading a status and a length off
        /// a result `run` produced. A fixture that hand-wrote that result
        /// would be asserting against its author's belief about the trigger.
        async fn run_and_refine(
            root: &std::path::Path,
            command: &str,
            route: &DutyRoute,
        ) -> crate::harness::RefinedOutcome {
            use crate::harness::tools::{ShellTool, Tool, ToolContext};
            use crate::harness::ToolDuties;

            let args = serde_json::json!({ "command": command });
            let raw = ShellTool::default().run(&ToolContext::new(root), &args);
            ShellTool::default()
                .refine(
                    &args,
                    "make the tests pass",
                    &ToolDuties {
                        // `shell` never reaches it.
                        triage: &DutyRoute::unresolved("no triage route in this test"),
                        shell: route,
                    },
                    raw,
                )
                .await
        }

        /// **AC-2 / BR-2 for `shell`, both halves against one route**
        /// (LESSON-485).
        ///
        /// The two calls differ in exactly one respect and the positive half
        /// reaches the duty while the negative half does not. Split apart, the
        /// negative half would be satisfied by an emitter that never emits and
        /// the positive by one that emits at resolution; only the pair pins
        /// "announced iff performed".
        ///
        /// **REQ-617 BR-7 changed which difference that is.** The pair used to
        /// be the same command succeeding and failing, because a failure was the
        /// duty's primary trigger. It no longer is: a failed command is never
        /// interpreted, whatever its size. So the discriminator is now **size** —
        /// a short successful command against one whose output ran past the
        /// tool's cap — which is the only trigger left.
        ///
        /// A third call is added below, and it is the one that would have been
        /// missed: the *failing* command that used to be the positive half must
        /// now announce **nothing**. Without it, reverting BR-7 would leave this
        /// test green.
        ///
        /// The route comes from `shell_route` rather than from a
        /// hand-assembled announcement, so the four fields asserted below
        /// are the **resolver's** answers. That is what makes a category
        /// swap inside `shell_route` fail here rather than only showing up
        /// as a different provider in a tier-binding test.
        #[tokio::test]
        async fn a_shell_duty_announces_its_route_only_when_the_output_needs_reading() {
            let engine = CountingEngine::answering("The check failed: the file is missing.");
            // `config()` binds `build` remotely and `shell` is a `build`
            // duty; dropping the binding leaves it on the local tier, which
            // is what lets the duty actually run here. Where it routes is
            // `shell_inherits_the_build_tier_binding`'s claim, not this
            // one's.
            let mut config = config();
            config.tiers.retain(|t| t.tier != Tier::Build);
            let runtime = runtime(config, &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);

            let route = watched_shell(&runtime, &bus, &SessionId::from("sess"));
            assert_eq!(
                route.provider(),
                Some(LOCAL_PROVIDER_ID),
                "the duty must resolve, or this test proves nothing"
            );

            let root = scratch_dir("shell-announce");

            // Declined: exit 0, output nowhere near the cap. No duty, and so
            // no routed model call to announce.
            let refined = run_and_refine(&root, "echo hi", &route).await;
            assert_eq!(refined.duty_error, None);
            assert_eq!(
                engine.calls(),
                0,
                "a short successful command must buy no model call"
            );
            assert!(
                announced(&mut sub).is_empty(),
                "a duty that never ran announces a routed model call that never happened"
            );

            // Performed: the command succeeded and its output ran past the
            // tool's cap, so what entered context is a fragment of a thing and
            // reading it unaided is the hard part. The one trigger left.
            let refined =
                run_and_refine(&root, "head -c 20000 /dev/zero | tr '\\0' 'x'", &route).await;
            assert_eq!(refined.duty_error, None, "the fixture must reach the duty");
            assert_eq!(engine.calls(), 1);
            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Shell,
                ProtoTier::Build,
                LOCAL_PROVIDER_ID,
            );

            // REQ-617 BR-7: and the command that used to be the positive half
            // announces nothing now. Placed after the positive call on purpose —
            // the engine's counter is cumulative, so "still 1" is a stronger
            // statement here than "still 0" would have been before it.
            let refined = run_and_refine(&root, "echo hi; exit 3", &route).await;
            assert_eq!(refined.duty_error, None);
            assert_eq!(
                refined.duty_skipped,
                Some("failed_exit"),
                "a failed command must be skipped, and must say that is why"
            );
            assert_eq!(
                engine.calls(),
                1,
                "a failed command must buy no model call (BR-7) — this is the \
                 assertion that goes red if the failure trigger comes back"
            );
            assert!(
                announced(&mut sub).is_empty(),
                "nor announce a routed model call it did not make"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // -------------------------------------------------------------------
    // The `title` duty's own dispatch and lifecycle (REQ-561 TASK-062).
    //
    // `title` is the odd one of the five: it belongs to no tool, it runs on
    // the `reflex` tier — which never inherits `default_provider` — and it
    // is the only duty whose "when" is a fact about the *session* rather
    // than about a tool result. So these cover both halves: where it routes,
    // and how many times it is allowed to run.
    //
    // They drive `title_session`, which is the exact function
    // `run_prompt_turn` calls, so "once per session" is a property of the
    // daemon's own path rather than of a test that only called it once.
    // -------------------------------------------------------------------
    mod title {
        use super::*;
        use crate::harness::title::{TITLE_MIN_REQUEST_BYTES, TITLE_OUTPUT_CONTRACT};
        use crate::sessions::SessionRegistry;
        use teton_core::category::{CategoryOverride, ConfigurableCategory};
        use teton_protocol::events::Event;

        /// A first prompt long enough to be worth naming a session after.
        const REQUEST: &str = "Add retry-with-backoff to the download client.";

        /// The `title` route the turn path builds, from the same runtime
        /// state and through the same router.
        fn title_for(runtime: &DaemonRuntime, session: &SessionId) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.title_route(DutyContext::detached(
                &Arc::new(EventBus::new()),
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// A registry holding one freeform session, and its id.
        fn one_session(reg: &SessionRegistry) -> SessionId {
            reg.create(SessionMode::Freeform, None, None)
                .expect("a freeform session")
                .session_id
        }

        /// Run the daemon's own titling step for `session`, on `bus`, **to
        /// completion**.
        ///
        /// The step itself is detached (REQ-561 verify M1), so the handle is
        /// awaited here rather than dropped: these tests are about what the
        /// naming eventually does, and a test that raced the task it started
        /// would assert on whichever half won. The one test that is about the
        /// detachment does not use this helper.
        async fn run_title(
            runtime: &DaemonRuntime,
            bus: &Arc<EventBus>,
            sessions: &SessionRegistry,
            session: &SessionId,
            prompt: &str,
        ) {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            if let Some(handle) = runtime.spawn_title_session(
                TurnCore {
                    events: bus,
                    session_id: session,
                    config: &config,
                    router: &router,
                },
                sessions,
                prompt,
                // These fixtures name a session after a **typed** prompt,
                // which is the call site the empty value belongs to.
                Provenance::empty(),
            ) {
                handle.await.expect("the titling task must not panic");
            }
        }

        /// Every `session_titled` title this subscription saw, with the
        /// session the envelope scoped it to.
        ///
        /// Drained with `try_recv` rather than awaited under a timeout:
        /// `EventBus::publish` is synchronous, so once the call under test
        /// has returned, everything it published is already queued
        /// (LESSON-450).
        fn titles(sub: &mut crate::broadcast::Subscription) -> Vec<(Option<SessionId>, String)> {
            std::iter::from_fn(|| sub.try_recv())
                .filter_map(|env| match env.event {
                    Event::SessionTitled(t) => Some((env.session_id, t.title)),
                    _ => None,
                })
                .collect()
        }

        // -- where it routes --------------------------------------------

        /// **BR-5, the `reflex` guarantee.** A machine whose turns all go to
        /// a remote provider still names its sessions **locally**:
        /// `default_provider` is the ordinary post-REQ-557 upgrade shape, and
        /// `reflex` is the one tier that does not inherit it.
        ///
        /// Non-vacuity is the second half: the very same
        /// `default_provider` genuinely carries a `build` category remotely,
        /// so this is the reflex rule holding rather than a config that
        /// could not reach a provider.
        #[test]
        fn title_stays_local_even_when_a_remote_default_provider_is_set() {
            let engine = CountingEngine::answering("Retry the download client");
            let mut config = config();
            config.default_provider = Some("frontier".to_owned());
            let runtime = runtime(config, &engine, true);

            assert_eq!(
                title_for(&runtime, &SessionId::from("sess")).provider(),
                Some(LOCAL_PROVIDER_ID),
                "`reflex` never inherits `default_provider`, so `title` never leaves \
                 the machine"
            );
            // Bound to locals rather than built inline as arguments, and
            // that is load-bearing, not style. `config.lock()`'s temporary
            // guard lives to the end of the *enclosing statement*, so as an
            // argument it is still held when a later argument evaluates
            // `router_for`, which locks the same mutex — and
            // `std::sync::Mutex` is not reentrant, so the thread deadlocks
            // against itself.
            //
            // The old signature ordered `router` before `config`, so the
            // two never overlapped; `DutyContext`'s field order is the
            // reverse, and reordering the arguments was enough to introduce
            // the hang (REQ-598 — an ordering the refactor could relocate
            // without any test naming it). Every other duty fixture in this
            // module already binds first; this one now matches them.
            let cfg = runtime.config.lock().expect("config mutex").clone();
            let router = router_for(&runtime);
            let slot = runtime.engine.get_with_format();
            assert_eq!(
                runtime
                    .shell_route(DutyContext::detached(
                        &Arc::new(EventBus::new()),
                        &SessionId::from("sess"),
                        &cfg,
                        &router,
                        slot.as_ref(),
                        None,
                    ))
                    .provider(),
                Some("cheap"),
                "non-vacuity: this config really does route other duties off the machine"
            );
        }

        /// **BR-5 / LESSON-432.** Session taint overrides the category
        /// binding for `title` as for every other duty. The mutation-sensitive
        /// one: deleting the taint check in `title_route` turns this red on
        /// its own, at its own layer.
        ///
        /// Its non-vacuity pair is a per-category override that genuinely
        /// sends `title` remotely — the one way a user can bind this category
        /// off the machine — so the pin is doing work here rather than
        /// agreeing with a route that was local anyway.
        #[test]
        fn a_tainted_session_titles_on_the_local_tier() {
            let engine = CountingEngine::answering("Retry the download client");
            let mut config = config();
            config.categories.push(CategoryOverride {
                name: ConfigurableCategory::Title,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            let session = SessionId::from("tainted");

            // Non-vacuity: the same config, untainted, genuinely goes remote.
            assert_eq!(
                title_for(&runtime, &SessionId::from("clean")).provider(),
                Some("frontier")
            );

            runtime.session_taint.mark(&session);
            let route = title_for(&runtime, &session);
            assert_eq!(
                route.provider(),
                Some(LOCAL_PROVIDER_ID),
                "a tainted session must name itself locally (BR-5)"
            );
        }

        /// A remote-only machine cannot name its sessions, and says why: the
        /// resolver's own sentence rides onto the route so nothing has to
        /// invent one (BR-6, LESSON-447).
        #[test]
        fn a_machine_with_no_engine_cannot_title_and_says_so() {
            let runtime = DaemonRuntime::minimal();
            *runtime.config.lock().expect("config mutex") = config();

            let route = title_for(&runtime, &SessionId::from("sess"));

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("there is nothing to serve the duty");
            };
            assert!(reason.contains("title"), "{reason}");
        }

        // -- how often it runs ------------------------------------------

        /// **AC-6, by call count.** Five turns of one session, one model
        /// call. Asserted on the counter rather than on the stored title,
        /// because "it was requested once" and "it ended up with one title"
        /// are different claims and only the first one is about cost.
        ///
        /// **AC-15, on captured events.** Exactly one `session_titled`
        /// reaches the wire, it carries a non-empty title, and — ADR-6's
        /// amendment — the envelope names the session, because the payload
        /// no longer does.
        #[tokio::test]
        async fn a_multi_turn_session_is_titled_once_and_announced_once() {
            let engine = CountingEngine::answering("Retry the download client");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sessions = SessionRegistry::new();
            let session = one_session(&sessions);

            for turn in 1..=5 {
                run_title(
                    &runtime,
                    &bus,
                    &sessions,
                    &session,
                    &format!("{REQUEST} (turn {turn})"),
                )
                .await;
            }

            assert_eq!(
                engine.calls(),
                1,
                "a session is named once, however many turns it runs"
            );
            let announced = titles(&mut sub);
            assert_eq!(
                announced.len(),
                1,
                "exactly one `session_titled` per session: {announced:?}"
            );
            let (scoped_to, title) = &announced[0];
            assert!(!title.is_empty(), "a titled session gets a real name");
            assert_eq!(title, "Retry the download client");
            assert_eq!(
                scoped_to.as_ref(),
                Some(&session),
                "the payload carries no session_id (ADR-6 amendment), so the envelope \
                 MUST — an unscoped event is one nobody can attribute"
            );
            assert_eq!(
                sessions
                    .get(&session)
                    .expect("the session")
                    .title
                    .as_deref(),
                Some("Retry the download client"),
                "the existing `SessionSummary.title` is the field that gets populated"
            );
        }

        /// **AC-6 / AC-15, the zero case.** A session that already carries a
        /// title requests nothing and announces nothing — the guard is keyed
        /// on the title being absent (BR-9), so a re-derivation cannot happen
        /// even when the duty is invoked again.
        #[tokio::test]
        async fn a_session_that_already_has_a_title_requests_and_announces_nothing() {
            let engine = CountingEngine::answering("A completely different name");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sessions = SessionRegistry::new();
            let session = one_session(&sessions);
            assert!(sessions.set_title(&session, "The name it already answers to"));

            run_title(&runtime, &bus, &sessions, &session, REQUEST).await;

            assert_eq!(engine.calls(), 0, "a named session buys no call");
            assert!(titles(&mut sub).is_empty(), "and announces nothing");
            assert_eq!(
                sessions
                    .get(&session)
                    .expect("the session")
                    .title
                    .as_deref(),
                Some("The name it already answers to"),
                "BR-9: an existing title is never overwritten"
            );
        }

        /// **The cost trap, end to end.** A duty that *fails* must still spend
        /// the session's one attempt. Two turns, a duty that answers with
        /// nothing usable, and exactly **one** call — a guard keyed only on
        /// `title.is_none()` would make this two, and would keep making it one
        /// more on every turn for the life of the session.
        ///
        /// Non-vacuity is built in: the failure is asserted (no title stored,
        /// nothing announced), so this cannot pass by the duty having
        /// quietly succeeded.
        #[tokio::test]
        async fn a_failed_title_does_not_retry_on_the_next_turn() {
            // An answer with no title in it: the duty ran, and produced
            // nothing that could name a session.
            let engine = CountingEngine::answering("   ");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sessions = SessionRegistry::new();
            let session = one_session(&sessions);

            run_title(&runtime, &bus, &sessions, &session, REQUEST).await;
            run_title(&runtime, &bus, &sessions, &session, REQUEST).await;

            assert_eq!(
                engine.calls(),
                1,
                "a failed title must not become a per-turn model call"
            );
            assert_eq!(
                sessions.get(&session).expect("the session").title,
                None,
                "the failure path leaves the session with no title (BR-3)"
            );
            assert!(
                titles(&mut sub).is_empty(),
                "and puts no `session_titled` on the wire"
            );
        }

        /// **ADR-11's zero-call case.** An opener with nothing in it to name a
        /// session by costs nothing — and, crucially, does **not** spend the
        /// session's one attempt, so the turn that actually asks for something
        /// still gets a name.
        ///
        /// The second half is what makes the threshold a deferral rather than
        /// a denial, and it is the part a `return` in the wrong place would
        /// break silently.
        #[tokio::test]
        async fn a_request_too_short_to_name_a_session_by_defers_rather_than_declines() {
            let engine = CountingEngine::answering("Retry the download client");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sessions = SessionRegistry::new();
            let session = one_session(&sessions);

            for opener in ["hi", "ok", "  ", "go on"] {
                assert!(opener.trim().len() < TITLE_MIN_REQUEST_BYTES);
                run_title(&runtime, &bus, &sessions, &session, opener).await;
            }
            assert_eq!(engine.calls(), 0, "a bare opener buys no model call");
            assert!(titles(&mut sub).is_empty());

            // The attempt was deferred, not spent.
            run_title(&runtime, &bus, &sessions, &session, REQUEST).await;
            assert_eq!(engine.calls(), 1, "the first real request still names it");
            assert_eq!(titles(&mut sub).len(), 1);
        }

        // -- what reaches the wire (AC-2, ADR-8) -------------------------
        //
        // Missing until now, and pinned only by accident: mutating the
        // seam's publish arm away turned `cli_e2e`'s
        // `an_escaped_line_and_a_plain_line_both_reach_the_model` red — but
        // only because that test counts `route [title/reflex]` lines while
        // proving something else entirely. A `title` announcement is a BR-2
        // guarantee and deserves a test that says so.

        /// **AC-2 / BR-2 for `title`, both halves against one bus**
        /// (LESSON-485).
        ///
        /// The length of the opener is the only difference between the two
        /// sessions. The real request reaches the duty, so the route
        /// announces; the bare opener is refused by ADR-11's threshold
        /// before the route is even built, so it announces nothing.
        ///
        /// **What this pair does not show, stated rather than implied.**
        /// `title_session` builds its route and performs it on the next
        /// line — there is no state where a `title` route exists and is not
        /// about to run — so for this duty an emit-at-resolution design and
        /// ADR-8's emit-on-perform are indistinguishable. The negative half
        /// here pins "no spurious announcement", not "not at resolution".
        /// The duties whose routes are built unconditionally per turn
        /// (`digest`, `shell`, `compact`) are where that distinction is
        /// discriminated, and their negatives do it.
        ///
        /// Driven through `title_session` — the exact function
        /// `run_prompt_turn` calls — so the four fields asserted below are
        /// the **resolver's** answers on the daemon's own path, and a
        /// category swap inside `title_route` fails here.
        #[tokio::test]
        async fn a_title_announces_its_route_only_when_it_names_a_session() {
            let engine = CountingEngine::answering("Retry the download client");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let sessions = SessionRegistry::new();

            // Declined: an opener with nothing in it to name a session by.
            let bare = one_session(&sessions);
            run_title(&runtime, &bus, &sessions, &bare, "hi").await;
            assert_eq!(engine.calls(), 0, "a bare opener buys no model call");
            assert!(
                announced(&mut sub).is_empty(),
                "a duty that never ran announces a routed model call that never happened"
            );

            // Performed. `reflex` is unbound in `config()` and never
            // inherits `default_provider`, so this resolves locally — which
            // is both the guarantee and what lets the duty run in-process.
            let named = one_session(&sessions);
            run_title(&runtime, &bus, &sessions, &named, REQUEST).await;
            assert_eq!(engine.calls(), 1, "the real request names the session");
            assert_eq!(
                sessions.get(&named).expect("the session").title.as_deref(),
                Some("Retry the download client"),
                "non-vacuity: the duty really produced a name"
            );
            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Title,
                ProtoTier::Reflex,
                LOCAL_PROVIDER_ID,
            );
        }

        /// **BR-10 / AC-12.** The `title` duty is answered by the scripted
        /// stand-in **off-script**, so it consumes no reply block and every
        /// fixture's turn sequence means what its author wrote.
        ///
        /// `title` is the one that would bite hardest: it fires on the first
        /// turn of every session, so a missing arm would shift the whole
        /// suite by one rather than one fixture at a time. Asserted by
        /// running a duty prompt through the engine and then checking the
        /// script is still on block one.
        #[test]
        fn a_title_duty_consumes_no_scripted_block() {
            let engine = ScriptedFileEngine::from_script("m", "first reply\n---\nsecond reply");
            let params = GenParams::default();

            let duty = engine
                .complete(
                    &crate::harness::title::title_prompt(REQUEST),
                    &params,
                    &mut |_| true,
                )
                .expect("the stand-in answers the duty");
            assert_eq!(duty.text.trim(), SCRIPTED_TITLE);
            assert!(!duty.text.trim().is_empty(), "and with a usable name");

            // The script has not moved: the next *turn* still gets block one.
            let turn = engine
                .complete("an ordinary turn", &params, &mut |_| true)
                .expect("a turn");
            assert_eq!(turn.text.trim(), "first reply");
        }

        /// **A conversation that quotes a duty's output contract is still a
        /// turn** (REQ-561 verify).
        ///
        /// Recognition used to be `contains` over the whole rendered prompt,
        /// so a block echoing a contract sentence — a prior compaction
        /// summary that carried one, a repository file, a `grep` hit on this
        /// crate — was answered off-script as a duty. That diverts the turn
        /// *and* leaves the script where it was, so every later reply in the
        /// fixture is one behind: the failure mode `ScriptedFileEngine`'s own
        /// docs record having shipped twice, arriving by a different route.
        ///
        /// Both contract positions are quoted, because the two anchors are
        /// different: the five harness duties are recognized in the prompt's
        /// instruction prefix, and the classifier — which states its contract
        /// last on purpose — by the contract terminating the prompt.
        #[test]
        fn a_quoted_duty_contract_in_a_conversation_does_not_divert_a_turn() {
            let engine = ScriptedFileEngine::from_script("m", "first reply\n---\nsecond reply");
            let params = GenParams::default();

            let quoting_turn = format!(
                "{filler}\n\n<tool-result tool=\"read\">\n{triage}\n{classifier}\n\
                 </tool-result>\nAssistant:",
                filler = "You are a coding agent. Available tools: ".repeat(40),
                triage = crate::harness::triage::TRIAGE_OUTPUT_CONTRACT,
                classifier = crate::classify::CLASSIFIER_OUTPUT_CONTRACT,
            );
            // The fixture must quote the contracts *outside* the window a
            // duty's own instruction occupies, or it tests nothing.
            assert!(
                quoting_turn
                    .find(crate::harness::triage::TRIAGE_OUTPUT_CONTRACT)
                    .is_some_and(|at| at > DUTY_CONTRACT_PREFIX_BYTES),
                "the quoted contract must fall past the instruction window"
            );

            let first = engine
                .complete(&quoting_turn, &params, &mut |_| true)
                .expect("a turn");
            assert_eq!(
                first.text.trim(),
                "first reply",
                "a conversation quoting a duty contract was answered as a duty"
            );
            let second = engine
                .complete(&quoting_turn, &params, &mut |_| true)
                .expect("a turn");
            assert_eq!(
                second.text.trim(),
                "second reply",
                "...and it must consume a script block, like every other turn"
            );
        }

        /// **Non-vacuity for the anchor**: every duty prompt the harness
        /// really builds is still recognized, and still consumes no block.
        ///
        /// The four with a `pub` prompt builder. `digest`'s is assembled
        /// inline inside `summarize_if_large` and the classifier's is private
        /// to [`crate::classify`]; both are covered by their own modules'
        /// tests and by the dispatch tests above, which would not route at
        /// all if the classifier arm stopped firing.
        #[test]
        fn every_harness_duty_prompt_is_still_answered_off_script() {
            let engine = ScriptedFileEngine::from_script("m", "first reply");
            let params = GenParams::default();
            let matches = ["src/a.rs:1: fn parse()", "src/b.rs:2: fn parse2()"];

            for (label, prompt) in [
                (
                    "triage",
                    crate::harness::triage::triage_prompt("find it", "grep `parse`", &matches),
                ),
                (
                    "shell",
                    crate::harness::shell_duty::shell_prompt("cargo test", "(exit 101)"),
                ),
                ("title", crate::harness::title::title_prompt(REQUEST)),
                (
                    "compact",
                    crate::harness::compact::compact_prompt(
                        &[crate::harness::context::ContextBlock {
                            role: crate::harness::context::BlockRole::User,
                            anchor: crate::harness::context::Anchor::None,
                            text: "do the thing".to_owned(),
                            provenance: crate::harness::context::Provenance::user(),
                        }],
                        crate::harness::compact::COMPACT_PROMPT_BUDGET_BYTES,
                    ),
                ),
            ] {
                let out = engine
                    .complete(&prompt, &params, &mut |_| true)
                    .expect("the stand-in answers the duty");
                assert!(
                    !out.text.trim().is_empty(),
                    "{label}: the duty must be answered"
                );
                assert_ne!(
                    out.text.trim(),
                    "first reply",
                    "{label}: the duty ate a scripted turn block"
                );
            }
        }

        // -- the turn does not wait for it (REQ-561 verify M1) ------------

        /// An [`Engine`] that will not answer until it is released.
        struct GatedEngine {
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            reply: String,
        }

        impl Engine for GatedEngine {
            fn model_id(&self) -> &str {
                "gated"
            }
            fn complete(
                &self,
                _prompt: &str,
                _params: &GenParams,
                _on_token: &mut dyn FnMut(&str) -> bool,
            ) -> Result<Completion, EngineError> {
                let _ = self.release.lock().expect("gate poisoned").recv();
                Ok(Completion::cold(self.reply.clone(), 0, 1))
            }
        }

        /// **The turn does not wait for the session to be named**
        /// (REQ-561 verify M1).
        ///
        /// `title` is `reflex`-tier and therefore local, and `LocalDuty` runs
        /// a complete inference on the blocking pool. Awaiting it here put
        /// that whole inference *ahead of the turn* on the first substantive
        /// prompt of every session — the user watching nothing happen while a
        /// model chose a name for the thing they had not seen an answer to
        /// yet.
        ///
        /// The engine below never answers until it is released, and a watcher
        /// releases it after a beat and records that it did. The call under
        /// test must return with that flag still false: an implementation
        /// that awaits the naming cannot, because the only thing that can
        /// unblock it is the very watcher whose firing the flag reports.
        ///
        /// The tail is the non-vacuity, and it does two jobs: the duty really
        /// was in flight rather than skipped, and the *detached* task still
        /// writes the name back.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_turn_path_does_not_wait_for_the_session_to_be_named() {
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let engine: Arc<Mutex<dyn Engine>> = Arc::new(Mutex::new(GatedEngine {
                release: Mutex::new(gate),
                reply: "Retry the download client".to_owned(),
            }));
            let runtime = DaemonRuntime::minimal();
            *runtime.config.lock().expect("config mutex") = config();
            runtime.engine.install("gated".to_owned(), engine);
            runtime.local_available.store(true, Ordering::SeqCst);

            let bus = Arc::new(EventBus::new());
            let sessions = SessionRegistry::new();
            let session = one_session(&sessions);
            let cfg = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&cfg, runtime.local_tier_available(), &BTreeMap::new());

            let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn({
                let released = Arc::clone(&released);
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    released.store(true, Ordering::SeqCst);
                    let _ = release.send(());
                }
            });

            let handle = runtime
                .spawn_title_session(
                    TurnCore {
                        events: &bus,
                        session_id: &session,
                        config: &cfg,
                        router: &router,
                    },
                    &sessions,
                    REQUEST,
                    Provenance::empty(),
                )
                .expect("the fixture must claim the title");

            assert!(
                !released.load(Ordering::SeqCst),
                "the turn path did not return until the naming had answered: every \
                 session's first substantive prompt waits for a whole local \
                 inference before its turn begins"
            );
            assert!(
                sessions.get(&session).expect("the session").title.is_none(),
                "non-vacuity: the naming really is still in flight, not skipped"
            );

            // It finishes on its own task, and still writes the name back.
            tokio::time::timeout(std::time::Duration::from_secs(30), handle)
                .await
                .expect("the detached naming must finish once the engine answers")
                .expect("the titling task must not panic");
            assert_eq!(
                sessions
                    .get(&session)
                    .expect("the session")
                    .title
                    .as_deref(),
                Some("Retry the download client"),
                "a detached naming must still land"
            );
        }

        /// The recognition arm keys on the contract the prompt actually
        /// carries — one constant, both sides, so the stand-in cannot drift
        /// away from the duty it is meant to answer.
        #[test]
        fn the_stand_in_recognizes_the_contract_the_prompt_carries() {
            assert!(crate::harness::title::title_prompt(REQUEST).contains(TITLE_OUTPUT_CONTRACT));
        }
    }

    // -------------------------------------------------------------------
    // The `compact` duty's dispatch (REQ-561 TASK-063).
    //
    // The duty itself — what it decides and what it refuses — is tested
    // against `ContextManager` in `harness::context`, because that is where
    // it hangs. What is tested here is the half only the daemon owns: where
    // the category routes, and what reaches the wire when it performs.
    // -------------------------------------------------------------------
    mod compact {
        use super::*;
        use crate::harness::compact::COMPACT_OUTPUT_CONTRACT;
        use crate::harness::ContextManager;
        use teton_core::category::{CategoryOverride, ConfigurableCategory};

        /// The `compact` route the turn path builds, from the same runtime
        /// state and through the same router, announcing on `bus`.
        fn compact_for(
            runtime: &DaemonRuntime,
            bus: &Arc<EventBus>,
            session: &SessionId,
        ) -> DutyRoute {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            runtime.compact_route(DutyContext::detached(
                bus,
                session,
                &config,
                &router,
                slot.as_ref(),
                None,
            ))
        }

        /// A conversation over its byte budget, with a decision in it.
        fn pressured() -> ContextManager {
            let mut ctx = ContextManager::new("sys", 1_000_000).with_budget_bytes(4_000);
            for i in 0..5 {
                ctx.push_user(format!("block {i} {}", "x".repeat(1_000)));
            }
            assert!(ctx.under_compaction_pressure());
            ctx
        }

        // -- where it routes --------------------------------------------

        /// **BR-5 / LESSON-432.** Session taint overrides the category
        /// binding for `compact` as for every other duty — and it matters
        /// most here, because what this duty sends is the *conversation*.
        ///
        /// The mutation-sensitive one: deleting the taint check in
        /// `compact_route` turns this red on its own, at its own layer. Its
        /// non-vacuity pair is the same config untainted, which genuinely
        /// sends the conversation off the machine.
        #[test]
        fn a_tainted_session_compacts_on_the_local_tier() {
            let engine = CountingEngine::answering("FORGET: 1\nSUMMARY: x");
            let mut config = config();
            config.categories.push(CategoryOverride {
                name: ConfigurableCategory::Compact,
                provider_id: "frontier".to_owned(),
                fallback_id: None,
            });
            let runtime = runtime(config, &engine, true);
            let bus = Arc::new(EventBus::new());
            let session = SessionId::from("tainted");

            // Non-vacuity: the same config, untainted, genuinely goes remote.
            assert_eq!(
                compact_for(&runtime, &bus, &SessionId::from("clean")).provider(),
                Some("frontier")
            );

            runtime.session_taint.mark(&session);
            assert_eq!(
                compact_for(&runtime, &bus, &session).provider(),
                LOCAL_PROVIDER_ID.into(),
                "a tainted session compacts on the machine (BR-5)"
            );
        }

        /// A machine with no engine cannot compact, and says why: the
        /// resolver's own sentence rides onto the route so nothing has to
        /// invent one (BR-6, LESSON-447). The context is still bounded —
        /// that is `truncate_to_budget`'s job, not this route's.
        #[test]
        fn a_machine_with_no_engine_cannot_compact_and_says_so() {
            let runtime = DaemonRuntime::minimal();
            *runtime.config.lock().expect("config mutex") = config();

            let route = compact_for(
                &runtime,
                &Arc::new(EventBus::new()),
                &SessionId::from("sess"),
            );

            assert_eq!(route.provider(), None);
            let DutyRoute::Unresolved { reason } = route else {
                panic!("there is nothing to serve the duty");
            };
            assert!(reason.contains("compact"), "{reason}");
        }

        // -- what reaches the wire (AC-2, ADR-8) -------------------------

        /// **AC-2 and its ADR-8 pairing.** A compaction that *performs*
        /// announces its route naming `compact`; a resolved route whose
        /// context is never pressured announces nothing.
        ///
        /// The negative half is what distinguishes emit-on-perform from the
        /// design it replaced: `compact_route` is built once per turn
        /// attempt whether or not any conversation ever crosses the
        /// threshold, so a resolution-time event would fire on every turn in
        /// the daemon.
        #[tokio::test]
        async fn a_performed_compaction_announces_its_route_and_a_declined_one_does_not() {
            let engine =
                CountingEngine::answering("FORGET: 1 2 3\nSUMMARY: the agent looked around.");
            let runtime = runtime(config(), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let session = SessionId::from("sess");

            // Declined: resolved, never pressured, never performed.
            let mut roomy = ContextManager::new("sys", 1_000_000).with_budget_bytes(4_000);
            roomy.push_user("a");
            roomy.push_user("b");
            roomy.push_user("c");
            let out = roomy
                .compact_if_pressured(&compact_for(&runtime, &bus, &session))
                .await;
            assert_eq!(out.dropped_blocks, 0);
            assert_eq!(engine.calls(), 0);
            assert!(
                announced(&mut sub).is_empty(),
                "a duty that never ran announces no routing decision"
            );

            // Performed.
            let out = pressured()
                .compact_if_pressured(&compact_for(&runtime, &bus, &session))
                .await;
            assert_eq!(out.dropped_blocks, 3);
            // All four of AC-2's fields, not just the category: `compact`
            // is the duty that sends the *conversation*, so "where did it
            // go, through which tier, and why" is the whole of what a user
            // watching this event needs.
            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Compact,
                ProtoTier::Scan,
                LOCAL_PROVIDER_ID,
            );
        }

        // -- the stand-in engine (BR-10, AC-12) --------------------------

        /// **BR-10 / AC-12.** The `compact` duty is answered by the scripted
        /// stand-in **off-script**, so it consumes no reply block and every
        /// fixture's turn sequence means what its author wrote.
        #[test]
        fn a_compact_duty_consumes_no_scripted_block() {
            let engine = ScriptedFileEngine::from_script("m", "first reply\n---\nsecond reply");
            let params = GenParams::default();
            let blocks = pressured().blocks().to_vec();

            let duty = engine
                .complete(
                    &crate::harness::compact::compact_prompt(
                        &blocks,
                        crate::harness::compact::COMPACT_PROMPT_BUDGET_BYTES,
                    ),
                    &params,
                    &mut |_| true,
                )
                .expect("the stand-in answers the duty");
            // And with an answer the parser accepts, rather than one that
            // would make every pressured fixture report a duty failure.
            let read = crate::harness::compact::read_compaction(
                &duty.text,
                blocks.len() - 1,
                &std::collections::BTreeSet::new(),
            )
            .expect("the stand-in's answer is a usable compaction");
            assert_eq!(read.forget(), [0], "the oldest block, as the gate would");

            // The script has not moved: the next *turn* still gets block one.
            let turn = engine
                .complete("an ordinary turn", &params, &mut |_| true)
                .expect("a turn");
            assert_eq!(turn.text.trim(), "first reply");
        }

        /// The recognition arm keys on the contract the prompt actually
        /// carries — one constant, both sides, so the stand-in cannot drift
        /// away from the duty it is meant to answer.
        #[test]
        fn the_stand_in_recognizes_the_contract_the_prompt_carries() {
            assert!(crate::harness::compact::compact_prompt(
                pressured().blocks(),
                crate::harness::compact::COMPACT_PROMPT_BUDGET_BYTES,
            )
            .contains(COMPACT_OUTPUT_CONTRACT));
        }
    }

    // -------------------------------------------------------------------
    // The `redact` duty's own dispatch (REQ-562 TASK-070).
    //
    // The sixth caller of the seam, and the only one whose resolver does
    // not live on the runtime: it lives on the gate the choke point holds,
    // because that is where the scan happens (ADR-1). Two things it has
    // that the five do not — and both are asserted here rather than only
    // commented:
    //
    // - **no taint arm** (ADR-3): taint cannot change a pinned-local
    //   resolution, so a taint check would be a guard on a distinction that
    //   cannot occur. The test below proves the *resolution* is identical
    //   tainted and clean, against a sibling on the same runtime that
    //   genuinely changes.
    // - **no remote arm** (ADR-1): the pin can only name an engine-backed
    //   tier, so a squatted local id leaves the scan unavailable rather
    //   than routing it through the squatter — which is what makes the
    //   scan structurally unable to re-enter the choke point.
    // -------------------------------------------------------------------
    mod redact {
        use super::*;
        use crate::egress::redact::{decide, EgressDecision, Outcome};
        use teton_core::config::PrivacyConfig;

        /// `config` with the `[privacy]` opt-in switched on.
        fn opted_in(mut config: Config) -> Config {
            config.privacy = PrivacyConfig {
                redact: true,
                ..Default::default()
            };
            config
        }

        /// `config` with every remote endpoint pointed at a closed local
        /// port.
        ///
        /// The two turn-path tests below drive a real turn through a real
        /// transport, and the point of the first is that the gate refuses
        /// the payload **before** the transport is ever used. A fixture
        /// reaching the public internet would make that claim depend on
        /// what answered, and would put a DNS lookup inside a unit test.
        /// Port 1 on the loopback refuses instantly and resolves nothing.
        fn offline_endpoints(mut config: Config) -> Config {
            for provider in &mut config.providers {
                provider.endpoint = Some("http://127.0.0.1:1/v1/chat/completions".to_owned());
            }
            config
        }

        /// `config()` plus a remote `reflex` binding — the tier `redact`
        /// declares, and the one a resolver that consulted the table would
        /// inherit.
        fn reflex_bound_to(provider_id: &str) -> Config {
            let mut config = config();
            config.tiers.push(TierBinding {
                tier: Tier::Reflex,
                provider_id: provider_id.to_owned(),
                fallback_id: None,
            });
            config
        }

        /// The gate the turn path installs, from the same runtime state and
        /// through the same router, announcing on `bus`.
        fn gate_on(
            runtime: &DaemonRuntime,
            bus: &Arc<EventBus>,
            session: &SessionId,
        ) -> Option<Arc<dyn RedactionGate>> {
            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            runtime.redaction_gate(&router, &config, bus, session)
        }

        fn gate_for(runtime: &DaemonRuntime, session: &SessionId) -> Arc<dyn RedactionGate> {
            gate_on(runtime, &Arc::new(EventBus::new()), session).expect("the privacy switch is on")
        }

        /// A runtime with `config` and **no** engine in the slot, whose
        /// local tier the router still registers.
        ///
        /// The discriminating state for fail-closed: the pin resolves to a
        /// tier that exists and nothing is loaded to serve it.
        fn runtime_without_an_engine(config: Config) -> DaemonRuntime {
            let runtime = DaemonRuntime::minimal();
            *runtime.config.lock().expect("config mutex") = config;
            runtime.local_available.store(true, Ordering::SeqCst);
            runtime
        }

        // -- the switch (ADR-2, AC-13) -----------------------------------

        /// **AC-13, both legs.** With no `[privacy]` table there is no gate
        /// at all — not a gate that permits — so a turn makes zero scanner
        /// calls and nothing exists to claim one ran. Flipping the switch on
        /// the same runtime produces exactly one call per scan.
        #[tokio::test]
        async fn off_means_no_gate_and_on_means_a_gate_that_reaches_the_engine() {
            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(config(), &engine, true);
            let session = SessionId::from("sess");

            assert!(
                gate_on(&runtime, &Arc::new(EventBus::new()), &session).is_none(),
                "absence of the [privacy] table is the off state (ADR-2)"
            );
            assert_eq!(
                engine.calls(),
                0,
                "an un-opted-in machine makes zero scanner calls"
            );

            *runtime.config.lock().expect("config mutex") = opted_in(config());
            let verdict = gate_for(&runtime, &session)
                .scan("an ordinary prompt")
                .await;
            assert_eq!(verdict.outcome(), Outcome::Clean);
            assert!(verdict.scanned(), "and it really did scan");
            assert_eq!(engine.calls(), 1, "exactly one scan for one payload");
        }

        // -- where it routes (ADR-3, BR-2) -------------------------------

        /// **The pin ignores the table.** `redact` declares the `reflex`
        /// tier, so a remotely bound `reflex` is the configuration that
        /// would send the scan off the machine if anything consulted the
        /// binding. Nothing does: the scan runs on the local engine.
        ///
        /// The non-vacuity is `title`, the other `reflex` duty, on the same
        /// runtime and the same router — it genuinely goes remote, so the
        /// binding under test is real rather than inert.
        #[tokio::test]
        async fn the_redact_pin_ignores_a_remote_reflex_binding_that_title_obeys() {
            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(opted_in(reflex_bound_to("frontier")), &engine, true);
            let session = SessionId::from("sess");

            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            assert_eq!(
                runtime
                    .title_route(DutyContext::detached(
                        &Arc::new(EventBus::new()),
                        &session,
                        &config,
                        &router,
                        slot.as_ref(),
                        None,
                    ))
                    .provider(),
                Some("frontier"),
                "the reflex binding is live for the duty that reads it"
            );

            let verdict = gate_for(&runtime, &session).scan("ordinary prose").await;
            assert_eq!(verdict.outcome(), Outcome::Clean);
            assert_eq!(
                engine.calls(),
                1,
                "the scan ran on the local engine regardless of the binding"
            );
        }

        /// **ADR-3's asymmetry, asserted.** Taint changes a sibling's
        /// resolution and cannot change this one, because this one was never
        /// anything but local. Without the sibling leg this test would pass
        /// against a resolver that ignored everything.
        #[tokio::test]
        async fn a_tainted_session_resolves_redact_exactly_as_a_clean_one_does() {
            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(opted_in(reflex_bound_to("frontier")), &engine, true);
            let clean = SessionId::from("clean");
            let tainted = SessionId::from("tainted");
            runtime.session_taint.mark(&tainted);

            let config = runtime.config.lock().expect("config mutex").clone();
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let slot = runtime.engine.get_with_format();
            let bus = Arc::new(EventBus::new());
            // The sibling: taint moves it from the frontier to the tier.
            assert_eq!(
                runtime
                    .title_route(DutyContext::detached(
                        &bus,
                        &clean,
                        &config,
                        &router,
                        slot.as_ref(),
                        None,
                    ))
                    .provider(),
                Some("frontier")
            );
            assert_eq!(
                runtime
                    .title_route(DutyContext::detached(
                        &bus,
                        &tainted,
                        &config,
                        &router,
                        slot.as_ref(),
                        None,
                    ))
                    .provider(),
                Some(LOCAL_PROVIDER_ID)
            );

            // And `redact` answers the same on both sessions.
            for session in [&clean, &tainted] {
                let verdict = gate_for(&runtime, session).scan("ordinary prose").await;
                assert_eq!(
                    verdict.outcome(),
                    Outcome::Clean,
                    "the pin resolves identically on a tainted session"
                );
            }
            assert_eq!(engine.calls(), 2, "both scans really ran");
        }

        /// **BR-2.** A scan that runs announces its route, with the four
        /// things every other duty announces — and the provider is the local
        /// tier, which is the visible half of the pin.
        #[tokio::test]
        async fn a_scan_that_runs_announces_its_route_like_every_other_duty() {
            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(opted_in(config()), &engine, true);
            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);

            let gate = gate_on(&runtime, &bus, &SessionId::from("sess"))
                .expect("the privacy switch is on");
            let verdict = gate.scan("ordinary prose").await;
            assert_eq!(verdict.outcome(), Outcome::Clean);

            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Redact,
                ProtoTier::Reflex,
                LOCAL_PROVIDER_ID,
            );
        }

        // -- fail closed (ADR-6, BR-3) -----------------------------------

        /// **Fail closed at the resolver.** The switch is on, the tier
        /// exists, and nothing is loaded to serve it: the route is
        /// unresolved, the verdict is `Unavailable`, and it blocks.
        ///
        /// The payload is deliberately **clean** — the one case that would
        /// forward if an unresolved route were treated permissively, so a
        /// permissive failure turns this red rather than leaving it green.
        #[tokio::test]
        async fn a_machine_with_no_engine_loaded_blocks_rather_than_passing_the_scan() {
            let runtime = runtime_without_an_engine(opted_in(config()));
            let verdict = gate_for(&runtime, &SessionId::from("sess"))
                .scan("entirely ordinary prose")
                .await;
            assert_eq!(verdict.outcome(), Outcome::Unavailable);
            assert!(
                !verdict.scanned(),
                "a scan that could not run must not claim it did"
            );
            assert_eq!(decide(&verdict), EgressDecision::Block);
        }

        /// **The anti-recursion foundation (ADR-1), stated as behaviour.**
        ///
        /// A non-local provider that has taken the canonical local-tier id
        /// does not become the local tier (BUG-156/TASK-057): `local_tier_id`
        /// yields nothing, so the pin has nothing to name and the scan is
        /// unavailable. It does **not** fall through to the squatter — which
        /// is the case that would put a `RemoteDuty` behind the gate and let
        /// a scan re-enter the choke point with its own prompt.
        ///
        /// Non-vacuity: the same runtime, the same engine, without the
        /// squatter, scans normally.
        #[tokio::test]
        async fn a_squatted_local_tier_id_leaves_the_scan_unavailable_never_remote() {
            let engine = CountingEngine::answering("NONE");
            let mut squatted = opted_in(config());
            squatted
                .providers
                .push(remote(LOCAL_PROVIDER_ID, "squatter-model"));
            let runtime = runtime(squatted, &engine, true);
            let session = SessionId::from("sess");

            let verdict = gate_for(&runtime, &session).scan("ordinary prose").await;
            assert_eq!(verdict.outcome(), Outcome::Unavailable);
            assert_eq!(decide(&verdict), EgressDecision::Block);
            assert_eq!(
                engine.calls(),
                0,
                "nothing was asked to scan, locally or otherwise"
            );

            *runtime.config.lock().expect("config mutex") = opted_in(config());
            let verdict = gate_for(&runtime, &session).scan("ordinary prose").await;
            assert_eq!(verdict.outcome(), Outcome::Clean);
            assert_eq!(engine.calls(), 1);
        }

        /// **AC-4's "no locality guard was added" leg, at the daemon's own
        /// resolver** (BR-2, LESSON-484, LESSON-443).
        ///
        /// The squat test above is the same coin's other face. There, a
        /// *remote* provider holding the canonical id `local` leaves the pin
        /// with nothing to name. Here, a genuinely engine-backed tier holds
        /// an id that is **not** `local` — `[[providers]] id = "on-device",
        /// kind = "local"` is an ordinary thing for a user to write — and
        /// the pin must resolve to it and serve.
        ///
        /// An id comparison anywhere on this path
        /// (`if provider_id != LOCAL_PROVIDER_ID { … }`) would fail this
        /// machine's scan closed, and with the gate on the synchronous send
        /// path, every one of its remote turns with it. So this test's
        /// *success* is the discriminating evidence that no such guard
        /// exists (LESSON-485), asserted behaviourally rather than by
        /// grepping the source for a comparison (LESSON-489/BUG-159).
        ///
        /// ## Why it exists: a mutation that came back green
        ///
        /// TASK-071's AC-8 run applied exactly that guard to
        /// `RedactionGateImpl`'s resolver and **nothing turned red** — every
        /// fixture in this module built its router from a config whose local
        /// tier carried the canonical id, so the guard could never fire. The
        /// integration suite covered the property one layer down
        /// (`tests/redact_egress.rs::an_engine_backed_local_tier_under_another_id_still_serves_the_scan`,
        /// over a real `Router` and the real `scan`) but could not reach this
        /// crate-private resolver. This is the fixture that closes it; the
        /// green observation is kept in `harness::duty`'s mutation table
        /// because it is the reason the fixture is here.
        #[tokio::test]
        async fn an_engine_backed_local_tier_under_another_id_still_serves_the_scan() {
            /// A `[[providers]]` entry that is genuinely the on-device tier.
            fn declared_local(id: &str) -> ModelProvider {
                ModelProvider {
                    id: id.to_owned(),
                    kind: ProviderKind::Local,
                    endpoint: None,
                    model: None,
                    auth_ref: None,
                    allow_cleartext: false,
                    capabilities: ProviderCapabilities::default(),
                }
            }

            const NON_CANONICAL: &str = "on-device";
            assert_ne!(
                NON_CANONICAL, LOCAL_PROVIDER_ID,
                "the fixture's whole point is a local tier under some other name"
            );

            let engine = CountingEngine::answering("NONE");
            let mut config = opted_in(config());
            config.providers.push(declared_local(NON_CANONICAL));
            let runtime = runtime(config, &engine, true);
            let session = SessionId::from("sess");

            // The premise: `local_tier_id` names the declared tier, so the
            // pin resolves to an id that is not the canonical one.
            assert_eq!(
                router_for(&runtime)
                    .resolve(Category::Redact)
                    .provider_id
                    .as_ref()
                    .map(|p| p.0.as_str()),
                Some(NON_CANONICAL),
                "the pin must name the declared tier, or this fixture is not \
                 the one the AC asks for"
            );

            let bus = Arc::new(EventBus::new());
            let mut sub = bus.subscribe(16);
            let verdict = gate_on(&runtime, &bus, &session)
                .expect("the privacy switch is on")
                .scan("ordinary prose")
                .await;

            // The claim: it served. Not `Unavailable`, which is what an id
            // comparison would have produced.
            assert_eq!(
                verdict.outcome(),
                Outcome::Clean,
                "a local tier under a non-canonical id must still serve the scan"
            );
            assert!(verdict.scanned(), "and it really did scan");
            assert_eq!(decide(&verdict), EgressDecision::Forward);
            assert_eq!(
                engine.calls(),
                1,
                "on this machine's own engine, exactly once"
            );

            // And it announced the route under that tier's own name, so the
            // provider the scan ran on is observable rather than inferred.
            assert_announced_route(
                &announced(&mut sub),
                ProtoCategory::Redact,
                ProtoTier::Reflex,
                NON_CANONICAL,
            );
        }

        // -- what it finds -----------------------------------------------

        /// The gate end to end: a planted credential blocks, and the *same*
        /// gate lets clean prose through. The pattern pass is what catches
        /// this one — the stand-in engine answers "found nothing" — which is
        /// exactly the division of labour ADR-4 describes.
        #[tokio::test]
        async fn a_planted_credential_blocks_and_the_same_gate_forwards_clean_prose() {
            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(opted_in(config()), &engine, true);
            let gate = gate_for(&runtime, &SessionId::from("sess"));

            let dirty = gate
                .scan("please summarize sk-ABCDEFGHIJKLMNOPQRSTUVWX for me")
                .await;
            assert_eq!(dirty.outcome(), Outcome::Findings);
            assert_eq!(decide(&dirty), EgressDecision::Block);

            let clean = gate.scan("please summarize src/main.rs for me").await;
            assert_eq!(clean.outcome(), Outcome::Clean);
            assert_eq!(decide(&clean), EgressDecision::Forward);

            assert_eq!(engine.calls(), 2, "both payloads were really scanned");
        }

        // -- the MCP path (ADR-003 × ADR-1) ------------------------------

        /// A `Transport` that answers JSON-RPC by method and records every
        /// body it was handed — the wire, for a remote MCP server.
        #[derive(Default, Clone)]
        struct McpWire {
            sent: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        }

        impl McpWire {
            fn bodies(&self) -> Vec<String> {
                self.sent
                    .lock()
                    .expect("wire poisoned")
                    .iter()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect()
            }
        }

        #[async_trait::async_trait]
        impl Transport for McpWire {
            async fn execute(
                &self,
                request: teton_providers::transport::TransportRequest,
            ) -> Result<
                teton_providers::transport::TransportResponse,
                teton_providers::transport::TransportError,
            > {
                let method = serde_json::from_slice::<serde_json::Value>(&request.body)
                    .ok()
                    .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
                    .unwrap_or_default();
                self.sent
                    .lock()
                    .expect("wire poisoned")
                    .push(request.body.clone());
                let result = match method.as_str() {
                    "initialize" => {
                        serde_json::json!({"serverInfo":{"name":"kb","version":"1"}})
                    }
                    "tools/list" => serde_json::json!({"tools":[{
                        "name": "lookup",
                        "description": "look something up",
                        "inputSchema": {"type":"object"}
                    }]}),
                    _ => serde_json::json!({
                        "content":[{"type":"text","text":"ok"}],
                        "isError": false
                    }),
                };
                let body = serde_json::to_vec(
                    &serde_json::json!({"jsonrpc":"2.0","id":1,"result":result}),
                )
                .expect("serialize");
                Ok(teton_providers::transport::TransportResponse {
                    location: None,
                    status: 200,
                    body: Box::pin(futures::stream::once(async move { Ok(body) })),
                })
            }
        }

        /// A remote MCP server, the only kind that egresses.
        fn http_server(id: &str) -> McpServerConfig {
            McpServerConfig {
                id: id.to_owned(),
                transport: crate::mcp::McpTransport::Http {
                    endpoint: "https://mcp.example.com/rpc".to_owned(),
                },
                trusted: false,
            }
        }

        /// One `tools/call` through the construction `build_tools` uses:
        /// [`DaemonRuntime::mcp_egress`] over `wire`, the real
        /// [`McpRegistry`] over that, the real [`crate::mcp::HttpConnection`],
        /// the real handshake.
        ///
        /// Shared by every test below rather than rebuilt per test, because
        /// what they are all asserting *about* is this wiring: a second
        /// hand-rolled copy could keep passing after the production one had
        /// changed underneath it.
        ///
        /// `session` is a parameter because attribution is the subject of
        /// half these tests — a block the choke point cannot attribute pins
        /// nothing, so the session has to be a thing the caller controls and
        /// can then ask the taint about.
        async fn mcp_lookup(
            runtime: &DaemonRuntime,
            wire: &McpWire,
            session: &SessionId,
            arguments: serde_json::Value,
        ) -> Result<crate::mcp::McpToolResult, crate::mcp::McpError> {
            let config = runtime.config.lock().expect("config mutex").clone();
            let events = Arc::new(EventBus::new());
            let router = build_router(&config, runtime.local_tier_available(), &BTreeMap::new());
            let egress =
                Arc::new(runtime.mcp_egress(wire.clone(), &router, &config, &events, session));
            let registry = McpRegistry::with_egress(
                egress as Arc<dyn crate::mcp::EgressGate>,
                Some(session.clone()),
                vec![http_server("kb")],
            );
            registry.call_tool("mcp__kb__lookup", arguments).await
        }

        /// **The MCP choke point carries the gate** (ADR-003, ADR-1).
        ///
        /// ## Why this test exists
        ///
        /// `build_tools` attached the gate to the MCP egress and **nothing
        /// covered it**: deleting `.with_redaction_gate(…)` from that
        /// function left the entire suite green, because the only path to it
        /// ran through `HttpTransport::new()` and a real socket. An MCP tool
        /// argument is exactly the payload the feature is for — a credential
        /// pasted into a `query` field is off the machine the moment the
        /// call goes out, and provenance cannot see it because the argument
        /// came from the model, not from a file.
        ///
        /// ## What is real here, and what is not
        ///
        /// Real: the runtime, its config switch, its router, its engine slot,
        /// [`DaemonRuntime::mcp_egress`] (the exact construction
        /// `build_tools` calls), the real [`McpRegistry`] over it, the real
        /// [`crate::mcp::HttpConnection`], the real handshake, the real
        /// `tools/call`, the real two-pass scan, and the wire captured.
        ///
        /// Not real, and the whole of what remains uncovered: the
        /// `HttpTransport::new()` line that supplies the transport in
        /// production, and `register_mcp_tools`, which turns the registry
        /// into `ToolRegistry` entries and is orthogonal to egress. Both are
        /// above the gate, not between it and the wire.
        ///
        /// ## The discrimination
        ///
        /// The same runtime, the same server, the same tool arguments, twice
        /// — and the only difference is the `[privacy]` switch. On: the call
        /// is refused, the error names **redaction** (not a boundary), and
        /// the credential is absent from every captured body. Off: the same
        /// call succeeds, the engine is never asked, and the credential is on
        /// the wire — which is what proves the on-leg's absence is the gate
        /// and not the fixture (LESSON-485).
        #[tokio::test]
        async fn an_mcp_tool_call_crosses_the_gate_when_redact_is_on() {
            /// Pattern-shaped, so the deterministic pass alone blocks it and
            /// the stand-in engine's "NONE" cannot rescue the payload.
            const CREDENTIAL: &str = "AKIAMCPWIRESENTINEL0";

            async fn call_lookup(
                runtime: &DaemonRuntime,
                wire: &McpWire,
            ) -> Result<crate::mcp::McpToolResult, crate::mcp::McpError> {
                mcp_lookup(
                    runtime,
                    wire,
                    &SessionId::from("sess-mcp"),
                    serde_json::json!({ "q": format!("what does {CREDENTIAL} unlock?") }),
                )
                .await
            }

            // -- on ------------------------------------------------------
            let engine = CountingEngine::answering("NONE");
            let on_runtime = runtime(opted_in(config()), &engine, true);
            let wire = McpWire::default();
            let blocked = call_lookup(&on_runtime, &wire).await;

            match blocked {
                Err(crate::mcp::McpError::PrivacyBlocked { detail, .. }) => assert_eq!(
                    detail,
                    BlockDetail::Redaction,
                    "an MCP block must name which inspection refused it"
                ),
                other => panic!("expected a redaction block, got {other:?}"),
            }
            assert!(
                engine.calls() > 0,
                "the scan must actually have run on the MCP path"
            );
            for body in wire.bodies() {
                assert!(
                    !body.contains(CREDENTIAL) && !body.contains("MCPWIRESENTINEL"),
                    "the credential reached a remote MCP server: {body}"
                );
            }

            // -- off -----------------------------------------------------
            let off_engine = CountingEngine::answering("NONE");
            let off_runtime = runtime(config(), &off_engine, true);
            let off_wire = McpWire::default();
            let allowed = call_lookup(&off_runtime, &off_wire).await;
            assert!(
                allowed.is_ok(),
                "with the switch off the same call must go through: {allowed:?}"
            );
            assert_eq!(
                off_engine.calls(),
                0,
                "off means no gate at all — zero scanner calls (AC-13)"
            );
            assert!(
                off_wire.bodies().iter().any(|b| b.contains(CREDENTIAL)),
                "non-vacuity: with no gate the credential really does reach \
                 the wire, so the on-leg's absence is the gate"
            );
        }

        // -- which MCP blocks pin the session (user decision, 2026-08-08) --

        /// **An MCP block pins its session iff the redaction scan found
        /// something** — all three causes, through the production wiring.
        ///
        /// One config and one helper serve all three legs, so what varies
        /// between them is the cause and nothing else: the tool argument
        /// picks boundary vs redaction, and the presence of an engine picks
        /// whether the scan could run at all. A fixture that had simply
        /// stopped attributing blocks to a session would fail the redaction
        /// leg rather than pass all three, which is what makes the two
        /// `false`s evidence of the gate rather than of a broken fixture
        /// (LESSON-485).
        ///
        /// Why they differ is on [`mcp_cause_taints_the_session`]; this is
        /// the behavioural half, driven through
        /// [`DaemonRuntime::mcp_egress`] rather than by calling the
        /// predicate — the sink has to actually be wired to it.
        #[tokio::test]
        async fn an_mcp_block_pins_its_session_for_redaction_and_for_no_other_cause() {
            /// Pattern-shaped, so the deterministic pass alone blocks it.
            const CREDENTIAL: &str = "AKIAMCPTAINTSENTINEL";

            /// The opt-in **and** a `local-only` boundary, so one config
            /// can produce all three causes.
            fn guarded() -> Config {
                let mut config = opted_in(config());
                config.boundaries = vec![PrivacyBoundary {
                    path_glob: "secrets/**".to_owned(),
                    mode: BoundaryMode::LocalOnly,
                    origin: Default::default(),
                }];
                config
            }

            /// The refusal `arguments` produces, and whether it pinned the
            /// session it happened in.
            async fn block_from(
                runtime: &DaemonRuntime,
                arguments: serde_json::Value,
            ) -> (BlockDetail, bool) {
                let session = SessionId::from("sess-mcp-taint");
                assert!(
                    !runtime.session_taint.is_tainted(&session),
                    "the fixture must start clean or it proves nothing"
                );
                let err = mcp_lookup(runtime, &McpWire::default(), &session, arguments)
                    .await
                    .expect_err("the call must be refused");
                let crate::mcp::McpError::PrivacyBlocked { detail, .. } = err else {
                    panic!("expected a privacy block, got {err:?}");
                };
                (detail, runtime.session_taint.is_tainted(&session))
            }

            // The model wrote these arguments, so it is holding what the
            // scan found and can restate it next turn.
            let engine = CountingEngine::answering("NONE");
            let (detail, pinned) = block_from(
                &runtime(guarded(), &engine, true),
                serde_json::json!({ "q": format!("what does {CREDENTIAL} unlock?") }),
            )
            .await;
            assert_eq!(detail, BlockDetail::Redaction);
            assert!(
                pinned,
                "a redaction block through MCP must pin the session, exactly as one \
                 through a turn does"
            );

            // REQ-544's posture for this surface, kept: a boundary refusal
            // folds back into the loop as an in-context tool error.
            let engine = CountingEngine::answering("NONE");
            let (detail, pinned) = block_from(
                &runtime(guarded(), &engine, true),
                serde_json::json!({ "path": "secrets/prod.env" }),
            )
            .await;
            assert_eq!(
                detail,
                BlockDetail::Boundary,
                "the boundary leg must be refused by provenance, not by the scan"
            );
            assert!(
                !pinned,
                "REQ-544 folds an MCP boundary block back into the loop without \
                 pinning, and this REQ does not re-decide that"
            );

            // Nothing looked at the payload, so nothing was established —
            // the one answer both paths share.
            let (detail, pinned) =
                block_from(&runtime_without_an_engine(guarded()), serde_json::json!({})).await;
            assert_eq!(detail, BlockDetail::ScanUnavailable);
            assert!(
                !pinned,
                "a scan that never ran must not pin a whole session to the local tier"
            );
        }

        /// **What the pin is *for*, on the MCP path**: the next turn in that
        /// session resolves local.
        ///
        /// `is_tainted` is a flag, and a pin that never reached
        /// [`DaemonRuntime::dispatch_route`] would satisfy the flag while
        /// changing nothing a user could observe. This asserts the
        /// consequence instead, with a second untouched session on the same
        /// runtime as the non-vacuity — it still routes remotely, so the
        /// pinned leg is the taint and not the fixture's routing.
        ///
        /// Structured mode on both, so no classification runs and the
        /// engine in the slot is only ever the scanner.
        #[tokio::test]
        async fn an_mcp_redaction_block_pins_the_session_so_the_next_turn_resolves_local() {
            const CREDENTIAL: &str = "AKIAMCPNEXTTURNSENT0";

            let engine = CountingEngine::answering("NONE");
            let runtime = runtime(opted_in(config()), &engine, true);
            let blocked = SessionId::from("blocked");
            let bystander = SessionId::from("bystander");

            let err = mcp_lookup(
                &runtime,
                &McpWire::default(),
                &blocked,
                serde_json::json!({ "q": format!("what does {CREDENTIAL} unlock?") }),
            )
            .await
            .expect_err("the credential must be refused");
            assert!(
                matches!(
                    err,
                    crate::mcp::McpError::PrivacyBlocked {
                        detail: BlockDetail::Redaction,
                        ..
                    }
                ),
                "expected a redaction block, got {err:?}"
            );

            let router = router_for(&runtime);
            let next = runtime
                .dispatch_route(
                    &router,
                    &blocked,
                    SessionMode::Structured,
                    Some(CorePhase::Implement),
                    "carry on",
                )
                .await;
            assert_eq!(
                next.provider_id.as_ref().map(|p| p.0.as_str()),
                Some(LOCAL_PROVIDER_ID),
                "the turn after an MCP redaction block must be pinned local — {}",
                next.reason
            );
            assert!(
                next.resolution.is_none(),
                "the taint pin resolves no category at all (BR-7)"
            );
            assert_engine_backed(&opted_in(config()), &next);

            let untouched = runtime
                .dispatch_route(
                    &router,
                    &bystander,
                    SessionMode::Structured,
                    Some(CorePhase::Implement),
                    "carry on",
                )
                .await;
            assert_eq!(
                untouched.provider_id.as_ref().map(|p| p.0.as_str()),
                Some("cheap"),
                "non-vacuity: the identical next turn in an untouched session still \
                 goes remote, and the pin reaches only the session it happened in"
            );
        }

        // -- the turn-failure sentence (BR-3) ----------------------------

        /// **BR-3 on the primary user surface.** The three causes produce
        /// three different sentences in each of the three situations the
        /// turn path can report a block from, and the scan-unavailable
        /// wording never reads as a finding.
        ///
        /// Exhaustive over cause × situation rather than three examples,
        /// because the failure this replaced was not a wrong sentence — it
        /// was **one** sentence used for every cause, which is what a table
        /// with a missing row silently reproduces. The `unique.len()`
        /// assertion per situation is what makes a collapsed clause fail.
        #[test]
        fn the_three_block_causes_produce_three_distinct_turn_failure_sentences() {
            use std::collections::BTreeSet;

            let details = [
                BlockDetail::Boundary,
                BlockDetail::Redaction,
                BlockDetail::ScanUnavailable,
            ];
            /// One of the three places the turn path reports a block.
            type Situation = (&'static str, fn(BlockDetail) -> String);

            let situations: [Situation; 3] = [
                ("no local tier", unrerouteable_block_sentence),
                ("reroute failed", failed_reroute_block_sentence),
                ("rerouted", reroute_after_block_reason),
            ];

            for (situation, compose) in situations {
                let rendered: Vec<String> = details.into_iter().map(compose).collect();
                let unique: BTreeSet<&String> = rendered.iter().collect();
                assert_eq!(
                    unique.len(),
                    3,
                    "{situation}: the three causes must not share a sentence: {rendered:?}"
                );

                // REQ-544's sentence, unchanged: it is what is already in
                // every log and what a user has seen before.
                assert!(
                    rendered[0].contains("local-only privacy boundary"),
                    "{situation}: {}",
                    rendered[0]
                );
                // A finding says something was found...
                assert!(
                    rendered[1].contains("found sensitive content"),
                    "{situation}: {}",
                    rendered[1]
                );
                // ...and the scan that could not run says exactly that, and
                // never the other thing. This is the assertion BR-3 is
                // about: told the wrong one, a user hunts for a secret that
                // is not there instead of for the tier that is not loaded.
                assert!(
                    rendered[2].contains("could not run"),
                    "{situation}: {}",
                    rendered[2]
                );
                assert!(
                    !rendered[2].contains("found"),
                    "{situation}: a scan that never ran cannot have found \
                     anything: {}",
                    rendered[2]
                );
                assert!(
                    !rendered[2].contains("local-only privacy boundary"),
                    "{situation}: a scan-unavailable block is not a boundary \
                     block: {}",
                    rendered[2]
                );
            }
        }

        /// **The bug this replaced, through the real turn path.**
        ///
        /// `[privacy] redact = true` on a machine with no local tier is not
        /// an exotic configuration — it is remote-only operation with the
        /// switch on — and in it *every* remote turn fails closed, because
        /// the scan has no engine to run on. That turn used to be reported
        /// as "this turn's content is under a local-only privacy boundary",
        /// which is false and sends the user looking for a glob that does
        /// not exist.
        ///
        /// This drives `run_prompt_turn` itself rather than the sentence
        /// helper: what is being pinned is that the cause survives the whole
        /// journey — choke point, `BlockCause`, the transport seam's
        /// `BlockDetail`, `ProviderError`, `HarnessError` — and reaches the
        /// RPC error a client renders. Any hop that collapses it turns this
        /// red at the end.
        ///
        /// No network is touched: the gate refuses the payload before
        /// `inner.execute`, which is the same reason the boundary check
        /// needs none.
        #[tokio::test]
        async fn a_scan_that_could_not_run_fails_the_turn_saying_so_not_blaming_a_boundary() {
            const SENTINEL: &str = "sk-ZZQUUXSENTINELCREDENTIAL0123";

            // Remote-only: a bound `build` tier, the switch on, no engine.
            let runtime = Arc::new(runtime_without_an_engine(offline_endpoints(opted_in(
                config(),
            ))));
            runtime.local_available.store(false, Ordering::SeqCst);
            let events = Arc::new(EventBus::new());
            let sessions = SessionRegistry::new();
            let session = sessions
                .create(SessionMode::Freeform, None, None)
                .expect("a freeform session needs no phase");

            let err = runtime
                .run_prompt_turn(
                    &events,
                    &sessions,
                    session.session_id.clone(),
                    session.mode,
                    None,
                    None,
                    format!("please summarize {SENTINEL} for me"),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await
                .expect_err("a scan that cannot run must fail the turn closed");

            assert_eq!(err.code, error_code::PRIVACY_BLOCKED);
            assert!(
                err.message.contains("the redaction scan could not run"),
                "the user must be told the scan could not run: {}",
                err.message
            );
            // The discriminating half: this is the sentence that used to be
            // emitted here, and emitting it again turns this red.
            assert!(
                !err.message.contains("local-only privacy boundary"),
                "a scan-unavailable block must not be reported as a boundary \
                 block: {}",
                err.message
            );
            assert!(
                !err.message.contains("found"),
                "nothing looked, so nothing was found: {}",
                err.message
            );
            // BR-6: the sentence names a cause, never the payload.
            assert!(
                !err.message.contains("QUUXSENTINEL") && !err.message.contains(SENTINEL),
                "no payload content may reach a turn-failure sentence: {}",
                err.message
            );
        }

        /// The non-vacuity twin: the **same** turn with the switch off
        /// reaches the provider instead of failing closed.
        ///
        /// Without it, the test above would pass just as well against a
        /// daemon that refused every remote turn for some unrelated reason
        /// and happened to word it this way. The turn here fails — there is
        /// no server at the fixture's endpoint — but it fails as a
        /// *provider* problem, with no privacy sentence anywhere in it.
        #[tokio::test]
        async fn the_same_turn_with_the_switch_off_is_not_blocked_at_all() {
            let runtime = Arc::new(runtime_without_an_engine(offline_endpoints(config())));
            runtime.local_available.store(false, Ordering::SeqCst);
            let events = Arc::new(EventBus::new());
            let sessions = SessionRegistry::new();
            let session = sessions
                .create(SessionMode::Freeform, None, None)
                .expect("a freeform session needs no phase");

            let err = runtime
                .run_prompt_turn(
                    &events,
                    &sessions,
                    session.session_id.clone(),
                    session.mode,
                    None,
                    None,
                    "please summarize src/main.rs for me".to_owned(),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await
                .expect_err("the fixture endpoint answers nothing");

            assert_ne!(
                err.code,
                error_code::PRIVACY_BLOCKED,
                "with the switch off nothing inspects the payload: {}",
                err.message
            );
            assert!(
                !err.message.contains("redaction scan"),
                "an un-opted-in machine must not mention a scan: {}",
                err.message
            );
        }

        /// **The Redaction cause, through the real turn path** — the mirror
        /// of the ScanUnavailable leg above.
        ///
        /// Same journey, and the point is the same: the cause has to survive
        /// choke point → `BlockCause` → the transport seam's `BlockDetail` →
        /// `ProviderError` → `HarnessError` and reach a surface a person
        /// reads. Any hop that collapses it turns this red.
        ///
        /// **Which surface, and why it is this one.** The two turn-*failure*
        /// sentences are unreachable for a Redaction block by construction:
        /// `unrerouteable_block_sentence` needs no engine loaded, and with
        /// no engine the scan cannot run, so the cause is `ScanUnavailable`
        /// rather than `Redaction`; `failed_reroute_block_sentence` needs
        /// the local reroute to be blocked too, and a local route has no
        /// choke point to block at. The reachable surface is the third one —
        /// `reroute_after_block_reason`, carried on the `route_decided` of
        /// the reroute — which is also the one a user actually meets, since
        /// the turn recovers and the failure sentences never fire.
        #[tokio::test]
        async fn a_redaction_block_reaches_the_reroute_sentence_naming_redaction() {
            const SENTINEL: &str = "sk-ZZQUUXSENTINELCREDENTIAL0123";

            let engine = CountingEngine::answering("NONE");
            let runtime = Arc::new(runtime(
                offline_endpoints(opted_in(config())),
                &engine,
                true,
            ));
            let events = Arc::new(EventBus::new());
            let mut sub = events.subscribe(64);
            let sessions = SessionRegistry::new();
            let session = sessions
                .create(SessionMode::Freeform, None, None)
                .expect("a freeform session needs no phase");

            let _ = runtime
                .run_prompt_turn(
                    &events,
                    &sessions,
                    session.session_id.clone(),
                    session.mode,
                    None,
                    None,
                    format!("please summarize {SENTINEL} for me"),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await;

            let reasons: Vec<String> = announced(&mut sub)
                .into_iter()
                .map(|rd| rd.reason)
                .collect();
            let reroute = reasons
                .iter()
                .find(|reason| reason.contains("remote egress refused"))
                .unwrap_or_else(|| {
                    panic!("no reroute was announced; route_decided reasons: {reasons:?}")
                });

            assert!(
                reroute.contains("found sensitive content"),
                "the reroute must name redaction as the cause: {reroute}"
            );
            // The discriminating half, in both directions.
            assert!(
                !reroute.contains("local-only privacy boundary"),
                "a redaction block is not a boundary block: {reroute}"
            );
            assert!(
                !reroute.contains("could not run"),
                "the scan DID run and DID find something: {reroute}"
            );
            // BR-6: the sentence names a cause, never the payload.
            assert!(
                !reroute.contains("QUUXSENTINEL") && !reroute.contains(SENTINEL),
                "no payload content may reach a routing sentence: {reroute}"
            );
        }

        // -- what a block does to the SESSION (REQ-544 C-2 × BR-3) -------

        /// **A scan that could not run refuses the payload and leaves the
        /// session alone.**
        ///
        /// This is the turn path's half of the rule the sink test states
        /// (`a_scan_unavailable_block_refuses_the_payload_without_pinning_the_session`),
        /// driven through `run_prompt_turn` so the gate really is the thing
        /// deciding.
        ///
        /// Why it matters here specifically: with `redact = true` and no
        /// local tier, **every** remote turn is `ScanUnavailable`. If that
        /// pinned the session, a machine in the configuration this daemon
        /// most expects — remote-only, switch on — would taint itself on its
        /// first turn and stay tainted, and a user whose engine finished
        /// downloading thirty seconds later would still be routed local for
        /// the rest of the session. The block is per-payload; the taint is
        /// forever.
        #[tokio::test]
        async fn a_scan_unavailable_turn_does_not_pin_the_session() {
            let runtime = Arc::new(runtime_without_an_engine(offline_endpoints(opted_in(
                config(),
            ))));
            runtime.local_available.store(false, Ordering::SeqCst);
            let events = Arc::new(EventBus::new());
            let sessions = SessionRegistry::new();
            let session = sessions
                .create(SessionMode::Freeform, None, None)
                .expect("a freeform session needs no phase");

            let err = runtime
                .run_prompt_turn(
                    &events,
                    &sessions,
                    session.session_id.clone(),
                    session.mode,
                    None,
                    None,
                    "please summarize src/main.rs for me".to_owned(),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await
                .expect_err("a scan that cannot run must fail the turn closed");

            // Non-vacuity: this really was the scan-unavailable block and
            // not some other failure that never reached the taint arm.
            assert_eq!(err.code, error_code::PRIVACY_BLOCKED);
            assert!(
                err.message.contains("the redaction scan could not run"),
                "{}",
                err.message
            );
            assert!(
                !runtime.session_taint.is_tainted(&session.session_id),
                "a transient scanner outage must not permanently pin the \
                 session to the local tier"
            );
        }

        /// The discriminating twin: a scan that **found** something does
        /// pin, because that is C-2's case one layer in — the payload
        /// carried a credential, and the model that wrote it can restate it
        /// next turn.
        ///
        /// Same runtime shape as the test above, same entry point; what
        /// changes is that an engine is loaded (so the scan runs) and the
        /// prompt carries a pattern-shaped credential (so it finds one).
        /// The config declares no boundaries, so `context_is_sensitive`
        /// cannot be what marked it.
        #[tokio::test]
        async fn a_redaction_block_does_pin_the_session() {
            let engine = CountingEngine::answering("NONE");
            let runtime = Arc::new(runtime(
                offline_endpoints(opted_in(config())),
                &engine,
                true,
            ));
            assert!(
                runtime
                    .config
                    .lock()
                    .expect("config mutex")
                    .boundaries
                    .is_empty(),
                "no boundaries, so `context_is_sensitive` cannot be what pins"
            );
            let events = Arc::new(EventBus::new());
            let sessions = SessionRegistry::new();
            let session = sessions
                .create(SessionMode::Freeform, None, None)
                .expect("a freeform session needs no phase");

            let _ = runtime
                .run_prompt_turn(
                    &events,
                    &sessions,
                    session.session_id.clone(),
                    session.mode,
                    None,
                    None,
                    "please summarize sk-ABCDEFGHIJKLMNOPQRSTUVWX for me".to_owned(),
                    None,
                    None,
                    ClientPresence::unwatched(),
                )
                .await;

            assert!(
                runtime.session_taint.is_tainted(&session.session_id),
                "a payload the scan found a credential in must pin the session, \
                 or the next turn is free to send the model's paraphrase of it \
                 to the same provider"
            );
        }
    }
}
