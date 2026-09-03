//! The prompt turn — `run_prompt_turn` and the methods only it reaches.
//!
//! REQ-599 split `runtime.rs` into seven modules and left `impl DaemonRuntime`
//! at 6,543 production lines, because six of its seven steps moved *top-level*
//! items and only `duty.rs` took methods out of the impl. This module is the
//! step it deferred: the turn path itself.
//!
//! # What is here, and why exactly this set
//!
//! Sixteen methods. The set is not "everything large" — it is everything on the
//! turn path. `derive_provider_setup` (324 lines) and `provider_test_within`
//! (310), the second and third largest methods in the impl, stay behind
//! precisely because they are not on it; REQ-600's Out of Scope names them so
//! their exclusion is a decision rather than an oversight.
//!
//! The arithmetic that chose the set: moving `run_prompt_turn` alone leaves the
//! impl at 5,458, which misses AC-2's 4,500. Moving the cluster leaves 3,728.
//!
//! # An inherent impl, split across modules
//!
//! This is a second `impl DaemonRuntime` block in a different file of the same
//! crate. Rust permits that directly — no trait, no newtype, and not one call
//! site changes (REQ-599 ADR-3). What does change is visibility: a method the
//! turn path shares with `mod.rs` is now cross-module and says so with
//! `pub(super)`, established by demoting and building rather than by grepping
//! for the name (LESSON-596).
//!
//! # A move, not a restructure
//!
//! Bodies are byte-identical to what they were in `mod.rs`. Decomposing
//! `run_prompt_turn` into named stages is the *next* commit, deliberately
//! separate: landing them together would bury a control-flow change inside a
//! diff dominated by relocation, which is why REQ-599 deferred this work rather
//! than finishing it.
//!
//! **Each method came with the comment run above it, not just its `///` block.**
//! The first attempt at this move took only doc comments and left a 58-line
//! plain-`//` rationale run behind — detaching `AC-7`, `ADR-3`, `ADR-7`, `BR-8`,
//! `REQ-580` and `REQ-585` from `run_prompt_turn`, the function they describe.
//! `traceability_sweep.rs`'s re-attachment arm caught it, which is the arm's
//! entire purpose (LESSON-594).

use super::*;

/// Where this daemon writes session transcripts, for the config it is handed —
/// REQ-611 ADR-4, the one place that pairing is spelled.
///
/// Two halves, and neither belongs to the other's crate. `TranscriptConfig::
/// effective_dir` is pure and answers *"the user's `dir`, or `transcripts`
/// under whatever data directory you give me"*; `socket_path::data_dir` reads
/// the environment and answers *"which data directory this machine has"*.
/// Composing them anywhere but one function invites a second caller to compose
/// them differently — and the pair that matters here is that the directory the
/// tools are told to refuse (`assemble_harness`, below) is byte-for-byte the
/// one the sink will write to. A denial aimed at a directory nothing writes to
/// is a denial of nothing at all.
///
/// **Not `DaemonRuntime::data_dir`.** That field is the *base* directory — the
/// socket's, which on Linux is `$XDG_RUNTIME_DIR`, a tmpfs cleared at logout.
/// A thirty-day retention policy under a directory that does not survive a
/// logout is a promise the daemon cannot keep, which is the whole reason ADR-4
/// added a second resolver instead of reusing the first. Relocating `cost.db`
/// to match is a filed follow-up (TASK-367), not a side effect of this.
///
/// `pub(super)`, which is what `runtime_visibility.rs` requires of everything
/// under `runtime/` that no other module needs — and it reaches `runtime/mod.rs`,
/// where the sink is constructed. A consumer outside `runtime/` promotes it to
/// `pub(crate)` **and** registers it in that suite's `CRATE_WIDE` with its
/// consumer named; the ratchet is the argument, not a grep (LESSON-596).
pub(super) fn effective_transcript_dir(
    transcript: &teton_core::config::TranscriptConfig,
) -> PathBuf {
    transcript.effective_dir(&teton_protocol::socket_path::data_dir())
}

/// The turn's prompt as the blocks its `prompt_submitted` record carries
/// (REQ-611).
///
/// **One text block holding the flattened prompt, and that is a stated
/// approximation.** The wire form is a `Vec<PromptBlock>` and `session/prompt`
/// flattens it (`server::flatten_prompt`) before the turn is spawned: a
/// resource link becomes `[resource: name (uri)]` in the one string every layer
/// below reads. `run_prompt_turn` receives that string and never the blocks, so
/// this is the prompt *the turn actually ran on* rather than a re-derivation of
/// something the daemon threw away. Threading the original vector down would
/// widen the turn's entry signature — already at the argument ceiling — to
/// carry a second spelling of a value the turn does not otherwise have.
///
/// An empty prompt yields no blocks rather than one empty block: a skill turn's
/// `prompt` is `""` by ADR-3 (the invocation travels as a name, and rides on
/// the record's `skill` field), and a text block holding nothing would tell a
/// reader the user typed an empty line.
fn transcript_prompt_blocks(prompt: &str) -> Vec<teton_protocol::methods::PromptBlock> {
    if prompt.is_empty() {
        Vec::new()
    } else {
        vec![teton_protocol::methods::PromptBlock::Text {
            text: prompt.to_owned(),
        }]
    }
}

/// What [`DaemonRuntime::claim_the_turn`] establishes before anything else runs.
///
/// A parameter bundle, not a stage object: it holds no behaviour, mints no ids,
/// and performs no I/O — the same three rules `turn_context.rs` states for
/// `TurnContext`, and for the same reason. A type that starts answering
/// questions becomes a second place for turn logic to live.
///
/// **REQ-606 — kept, Rule R (return position, three values).** This is returned
/// rather than passed, so clippy's argument limit never reaches it and it
/// justifies itself on width alone: at three or more heterogeneous values a
/// named struct beats a tuple, because a tuple makes the caller's destructure
/// positional and a transposition gets no diagnostic. `turn_id` also appears
/// borrowed in [`PromptRequest`] and [`AttemptInputs`] — see the duplication
/// note on [`AttemptInputs`], which covers all three occurrences.
struct ClaimedTurn {
    /// This turn's id, minted before the claim because the claim is keyed on it.
    turn_id: teton_protocol::TurnId,
    /// Held by `run_prompt_turn` for the whole turn — see the stage's docs.
    claim: crate::sessions::TurnClaim,
    /// The turn's ONE probe of that root (REQ-583 ADR-1).
    probed: ProbedRoot,
}

/// What the harness stage produces: the turn's tools and the prompt built from
/// them. A parameter bundle — no behaviour, no I/O, no ids.
///
/// **REQ-606 — kept, Rule R (return position, four values).** Returned, so the
/// argument limit does not apply; four heterogeneous values are past the width
/// where a tuple stays readable. `system` is lent onward to two stages and is
/// **not** re-derived by either: REQ-606 deleted a `PreparedAttempts` field
/// that was a clone of it (see [`DaemonRuntime::prepare_the_attempts`]).
struct AssembledHarness {
    tools: ToolRegistry,
    tool_ctx: ToolContext,
    stream_events: SessionEvents,
    system: String,
}

/// The facts the attempt loop reads and never changes.
///
/// A parameter bundle. `TurnContext` already carries the four core facts plus
/// the gate and invoker, so this is deliberately the *rest* — the second small
/// struct REQ-598 ADR-1 anticipated ("if a subset of the sites turns out to
/// want a different bundle, the answer is two small structs, not one wide
/// one"), not a widening of the first.
///
/// **REQ-606 — kept, Rule A (eight fields; collapsing puts `run_attempts` at
/// eleven arguments).** `run_attempts` takes four today. Clippy's
/// `too_many_arguments` threshold is 7 and fires at 8, and REQ-606 AC-2 forbids
/// the suppression that would silence it — so this is not a readability
/// preference, it is the only shape that compiles clean. The cluster is real.
///
/// **The duplication of `turn_id`, `route` and `probed` is deliberate**
/// (REQ-606 AC-1, third category). `turn_id` appears here, in [`ClaimedTurn`]
/// and in [`PromptRequest`]; `route` in [`ResolvedRoute`], [`TurnProducts`],
/// [`AttemptState`] and [`ExpansionInputs`]; `probed` in [`ClaimedTurn`],
/// [`SessionFacts`] and [`ExpansionInputs`]. Each occurrence is the **same
/// value at a different ownership stage** — minted or produced once, lent to
/// the stages that read it, owned by whatever outlives them. Rust cannot spell
/// "the same value, borrowed here and owned there" in one type, so the
/// repetition is what the borrow checker costs, not a second source of truth.
/// `route` is the one to watch: it is *reassigned* by the reroute arms, which
/// is why [`AttemptState`] owns it and every other bundle borrows it.
#[derive(Clone, Copy)]
struct AttemptInputs<'a> {
    turn_id: &'a teton_protocol::TurnId,
    phase: Option<ProtoPhase>,
    tools: &'a ToolRegistry,
    tool_ctx: &'a ToolContext,
    stream_events: &'a SessionEvents,
    refit_system: &'a str,
    typed_refit: usize,
    prompt_spend: Option<&'a Arc<teton_core::cost_ceiling::PromptSpend>>,
}

/// The state the attempt loop carries *across* attempts.
///
/// Separate from [`AttemptInputs`] because the split is the point: everything
/// here is rebound by a reroute arm, and everything there is not. `route` lives
/// in this half for the reason `turn_context.rs` ADR-3 keeps it out of
/// `TurnContext` — it is reassigned on every fallback reroute, and a bundle
/// that owned it alongside immutable facts would hide that.
///
/// `run_prompt_turn` owns the value and lends it, so the fields survive the
/// loop: the commit protocol below reads `conversation` and `route` after the
/// last attempt has returned.
///
/// **REQ-606 — kept, Rule I (carries an invariant).** Not a signature-width
/// device. This is the mutable state of one turn's attempt loop, `&mut`-lent
/// across iterations and then *moved* into `commit_or_abandon`, and the type is
/// what keeps that state in one place rather than in a row of out-parameters
/// that each ending would have to remember to update. Collapsing it would also
/// put `run_attempts` at ten arguments, but the invariant is the reason and the
/// arithmetic is only the confirmation.
struct AttemptState {
    attempts: u32,
    rerouted_local: bool,
    withdrew_accepted_expansion: bool,
    accepted: Option<AcceptedExpansion>,
    skill_refit: Vec<(String, String, String)>,
    conversation: CarriedTurn,
    route: crate::router::Route,
}

/// What the routing stage decides, before any `TurnContext` exists.
///
/// A parameter bundle carried across the pivot. It cannot *be* a `TurnContext`
/// and that is the invariant, not an inconvenience: `TurnContext::new` must run
/// after the warming hold rebinds `router` (REQ-598 BR-2.1), and every field
/// here is settled before that point.
///
/// **REQ-606 — kept, Rule R (return position, five values).** Returned, so the
/// argument limit does not reach it; five is well past the width at which a
/// tuple stays readable. The pre-pivot invariant in the paragraph above is the
/// stronger reason and stands on its own.
struct ResolvedRoute {
    skills: Arc<SkillRegistry>,
    skill_turn: Option<SkillTurn>,
    routed_text: String,
    router: Router,
    route: crate::router::Route,
}

/// The session facts that are settled before routing begins.
///
/// Named because the alternative was a `#[allow(clippy::too_many_arguments)]`,
/// and `suppression_ratchet.rs` refused it in as many words: "a new
/// `too_many_arguments` suppression is a new unnamed parameter cluster; name it
/// instead." It was right — these six travel together everywhere before the
/// pivot, which is the definition of a cluster.
///
/// **REQ-606 — kept, Rule A, and the arithmetic is the loudest of the set.**
/// `resolve_the_route` takes four arguments. Collapsing this bundle alone puts
/// it at nine; collapsing it *together with* [`PromptRequest`], which is the
/// only way to remove the "which spelling is in force" complaint the REQ-606
/// spec raises, puts it at **fourteen**. That is why REQ-600 split them into
/// two rather than widening one, and re-measuring has not changed the answer.
///
/// The six are also **not** a re-spelling of `TurnContext` that could be
/// deleted: no `TurnContext` exists at this point in the turn. `TurnContext::new`
/// must run after the warming hold rebinds `router` (REQ-598 BR-2.1 / ADR-3),
/// and every field here is settled before that. The duplication is real, it is
/// deliberate, and this is its reason.
#[derive(Clone, Copy)]
struct SessionFacts<'a> {
    events: &'a Arc<EventBus>,
    sessions: &'a SessionRegistry,
    session_id: &'a SessionId,
    config: &'a Config,
    gate: &'a Arc<PermissionGate>,
    probed: &'a ProbedRoot,
}

/// What this particular prompt asked for.
///
/// Separate from [`SessionFacts`] because the split is real: everything here
/// arrived on the wire with this one request, and everything there belongs to
/// the session and outlives it.
///
/// **REQ-606 — kept, Rule A (six fields; collapsing puts `resolve_the_route` at
/// nine arguments, or fourteen together with [`SessionFacts`]).**
///
/// **Renamed from `TurnRequest`, on a defect found while classifying.** This
/// module opens `use super::*;`, and `runtime/mod.rs` imports
/// `teton_providers::TurnRequest` — the provider-facing request type. A locally
/// declared item shadows a glob import **silently**: no warning, no error. So
/// inside the turn path `TurnRequest` meant this six-field bundle and the
/// provider type was unreachable by its own name. `PromptRequest` is also the
/// more accurate name — a "turn request" is what goes *to a provider*, and this
/// is what one prompt asked for.
#[derive(Clone, Copy)]
struct PromptRequest<'a> {
    turn_id: &'a teton_protocol::TurnId,
    prompt: &'a str,
    skill: Option<&'a SkillInvocation>,
    mode: SessionMode,
    phase: Option<ProtoPhase>,
    invoker: Option<ConnectionId>,
}

/// What the expansion stage needs beyond the turn context.
///
/// Named rather than suppressed: `suppression_ratchet.rs` treats a new
/// `too_many_arguments` allow as a new unnamed parameter cluster, and it is
/// right that these five travel together.
///
/// **REQ-606 — kept, Rule A, and it is the narrowest margin in the set.**
/// `settle_expansion` takes four arguments; collapsing this puts it at
/// **eight**, one over the threshold that fires at 8. One field short of
/// collapsible is still not collapsible, and AC-2 forbids buying the difference
/// with a suppression.
#[derive(Clone, Copy)]
struct ExpansionInputs<'a> {
    sessions: &'a SessionRegistry,
    probed: &'a ProbedRoot,
    route: &'a crate::router::Route,
    routed_text: &'a str,
    system: &'a str,
}

/// What the routing and expansion stages produced, moved into the loop.
///
/// **REQ-606 — kept, Rule A, and this row refutes the prediction that filed the
/// REQ.** REQ-606's Description calls this one transport: "named as an output
/// but is an *input* bundle, built from four loose locals at the call site and
/// destructured on the callee's first line." Both halves are true and the
/// conclusion does not follow.
///
/// `prepare_the_attempts` takes six arguments. Collapsing these four puts it at
/// **nine**. The best available reduction — passing `TurnContext` and taking
/// `session_id` and `config` off it, the move `assemble_harness` already makes
/// — reaches **eight**, still over a threshold that fires at 8. So the cluster
/// is real and the bundle earns its name, which is the outcome REQ-606's
/// Assumptions anticipated for exactly this case.
///
/// The name is the half of the complaint that was right, and it is left alone
/// deliberately: these *are* the products of the routing and expansion stages,
/// named from where they come rather than where they go, and renaming it to
/// suit the callee would make the two producing stages harder to trace.
struct TurnProducts {
    route: crate::router::Route,
    skill_turn: Option<SkillTurn>,
    prompt: String,
    accepted: Option<AcceptedExpansion>,
}

impl DaemonRuntime {
    /// Run one prompt turn for `session`, streaming events over `events` and
    /// returning the turn result.
    ///
    /// This is the daemon-side integration seam: it resolves the route (structured
    /// phase policy or freeform heuristic), builds the appropriate
    /// [`crate::harness::CompletionSource`] (local engine or a remote provider
    /// through the egress choke point), runs the unified turn loop, and — on a
    /// remote failure — falls back per the router (AC-7).
    ///
    /// ## A turn that meets a warming tier waits for it (REQ-580)
    ///
    /// A turn whose route has nowhere to run **only** because the local tier is
    /// still coming up — its weights downloading, or installed and mid-load —
    /// is held here until the tier settles, then routed afresh and run exactly
    /// as if it had been sent that moment. The hold is announced on the bus as
    /// `turn_queued` and taken *before* the turn's tools, head or conversation
    /// exist, so a held turn has spent nothing; a settled absence is refused
    /// immediately, exactly as before. `presence` is what ends a hold early: a
    /// client that disconnects while its turn waits gets no ghost turn run on
    /// its behalf once the tier opens (see [`ClientPresence`]).
    ///
    /// ## A `/name` invocation is expanded here, and expanded FIRST (REQ-585)
    ///
    /// `skill` is the invocation as it crossed the wire — a name and the rest of
    /// the typed line, never an expansion (ADR-3) — and `prompt` is empty
    /// whenever it is `Some`. The expansion is built before
    /// [`Self::dispatch_route`] and before [`Self::spawn_title_session`],
    /// because both of those read the prompt *text*: a skill turn expanded after
    /// them would be classified from `""` and would spend the session's one
    /// naming attempt on `""`. From there the order is BR-8's, and BR-8(c) is a
    /// statement about a single line — [`CarriedTurn::begin`] both pushes the
    /// user block and arms the drop-commit, so a check placed after it has
    /// already committed the expansion:
    ///
    /// ```text
    /// probe root → expand → route + route.budget → Stage A
    ///            → (TASK-205: consent + commands) → Stage B → CarriedTurn::begin
    /// ```
    ///
    /// # Errors
    /// Returns a [`RpcError`] when no provider can serve the turn, an
    /// unrecoverable provider failure occurs, the named skill is not one this
    /// session dispatches ([`error_code::INVALID_PARAMS`]), or its expansion does
    /// not fit the route's budget ([`error_code::SKILL_EXPANSION_TOO_LARGE`]).
    // The parameters are the session's own facts, passed individually because
    // that is how the caller reads them off `session/prompt` — the same shape
    // `run_one_attempt` already carries below. `session_cwd` is the caller's
    // pre-claim snapshot of the root; the turn re-reads the registry once it
    // holds the claim and uses the snapshot only if the session is gone.
    //
    // `invoker` is the connection that sent this prompt, and it is carried for
    // exactly one purpose: a skill's dynamic-context consent is **addressed** to
    // it and answerable by nobody else (REQ-585 ADR-7). `None` is a caller with
    // no connection at all — an internal driver or a fixture — and it asks
    // nobody, which is the gate's own fail-closed answer rather than a fallback
    // to the bus.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_prompt_turn(
        self: &Arc<Self>,
        events: &Arc<EventBus>,
        sessions: &SessionRegistry,
        session_id: SessionId,
        mode: SessionMode,
        phase: Option<ProtoPhase>,
        session_cwd: Option<PathBuf>,
        prompt: String,
        skill: Option<SkillInvocation>,
        invoker: Option<ConnectionId>,
        mut presence: ClientPresence,
    ) -> Result<PromptTurnResult, RpcError> {
        let ClaimedTurn {
            turn_id,
            claim: _claim,
            probed,
        } = self.claim_the_turn(sessions, &session_id, session_cwd)?;

        // REQ-611 BR-4 / AC-2: the prompt, to the transcript, here.
        //
        // **After the claim**, because the record carries `turn_id` and the id
        // is minted with the claim — reading a session's facts before holding
        // its claim is what LESSON-539 is about. **Before the route**, because
        // `prompt_submitted` preceding `route_decided` is one of the three
        // orderings AC-2 asserts, and `resolve_the_route` below publishes that
        // event. A refused turn — a skill this session does not dispatch, an
        // expansion that does not fit — still has its prompt in the file, which
        // is the honest record of what was asked.
        //
        // A short-lived emitter rather than the turn's `stream_events`: that one
        // is assembled with the tools, well below the route, and the ordering
        // above is the whole point. It is two `Arc` clones on a path that is
        // about to run a model.
        SessionEvents::new(Arc::clone(events), session_id.clone())
            .with_sink(self.transcript())
            .prompt_submitted(&turn_id, &transcript_prompt_blocks(&prompt), skill.as_ref());

        // REQ-585 BR-4/ADR-3: **expand before routing.** A skill turn's `prompt`
        // is empty by ADR-3 — the invocation crosses the wire as a name — and
        // both readers of the prompt text are below this line: `dispatch_route`
        // runs the freeform classifier over it, and `spawn_title_session` spends
        // the session's one naming attempt on it. Expanding after either would
        // classify and name every invocation from `""`: on a machine with
        // per-category bindings that is a route chosen from nothing, and it is a
        // session left unnamed for its whole life.
        //
        // Refusing here is free in BR-8(c)'s sense: nothing has been seeded, no
        // route has been decided, and no event has been published.

        // This turn's ONE config snapshot, taken before the expansion rather
        // than after it because the gate below reads it and the expansion below
        // needs the gate (REQ-589 ADR-10). Handed on rather than re-read: the
        // route, the gate, the registry and the capability clause further down
        // are then four readings of one config, so a commit that lands mid-turn
        // moves the next turn instead of leaving this one's prompt disagreeing
        // with its own tool set (REQ-572 verify).
        let config = self.config.lock().expect("config mutex poisoned").clone();
        // Fetched before the tools, because the web tool holds it: that tool
        // raises its own per-tier prompt inside its run rather than being
        // authorized by name at dispatch (REQ-563 BR-3/BR-12). Fetched rather
        // than built, because a gate rebuilt per turn forgets every
        // "allow for this session" answer at the end of the turn that earned it.
        //
        // **And now before the expansion too** (REQ-589 BR-6 / ADR-10): a typed
        // project skill's trust question is asked inside `accept_invocation`,
        // which is the first thing this turn does with the invocation. The fetch
        // is a per-session cache lookup, so asking for it earlier reads the same
        // gate with the same remembered answers — only sooner.
        let gate = self.permission_gate_for(&session_id, events, &config);
        let ResolvedRoute {
            skills,
            mut skill_turn,
            routed_text,
            router,
            mut route,
        } = self
            .resolve_the_route(
                SessionFacts {
                    events,
                    sessions,
                    session_id: &session_id,
                    config: &config,
                    gate: &gate,
                    probed: &probed,
                },
                PromptRequest {
                    turn_id: &turn_id,
                    prompt: &prompt,
                    skill: skill.as_ref(),
                    mode,
                    phase,
                    invoker,
                },
                &mut presence,
            )
            .await?;
        let tctx = TurnContext::new(events, &session_id, &config, &router, &gate, invoker);

        self.spawn_the_naming_duty(tctx, sessions, &skill_turn, &routed_text);
        // REQ-563 BR-3: every URL in this prompt joins the session's user-pasted
        // set **before** the turn runs, so a message that pastes a link and asks
        // about it in one breath classifies as `UserPasted` — the ordinary
        // shape of the request. This is the one ingestion point, and the text it
        // reads is the user's own (see `record_user_prompt_urls`).
        //
        // REQ-585: `prompt`, deliberately — **not** `routed_text`. A skill body
        // is file-authored, and feeding it here would let a file on disk author
        // its own authorization by writing a URL into the set the web tool then
        // trusts. A skill turn's `prompt` is empty, so an invocation contributes
        // nothing, which is the correct answer: nobody pasted anything.
        self.record_user_prompt_urls(&session_id, &prompt);
        // The gate is the one fetched above the expansion (REQ-589 ADR-10). It
        // is still fetched before the tools, which is what the web tool needs:
        // that tool raises its own per-tier prompt inside its run rather than
        // being authorized by name at dispatch (REQ-563 BR-3/BR-12).
        //
        // This turn's config snapshot is handed on rather than re-read: the
        // route, the gate, the registry and the capability clause below are then
        // four readings of ONE config, so a commit that lands mid-turn moves the
        // next turn instead of leaving this one's prompt disagreeing with its
        // own tool set (REQ-572 verify).
        let AssembledHarness {
            tools,
            tool_ctx,
            stream_events,
            system,
        } = self
            .assemble_harness(tctx, sessions, &skills, &probed, &mut route)
            .await;

        let accepted = self
            .settle_expansion(
                tctx,
                ExpansionInputs {
                    sessions,
                    probed: &probed,
                    route: &route,
                    routed_text: &routed_text,
                    system: &system,
                },
                &mut skill_turn,
            )
            .await?;
        let (mut st, typed_refit) = self.prepare_the_attempts(
            sessions,
            &session_id,
            &config,
            &system,
            TurnProducts {
                route,
                skill_turn,
                prompt,
                accepted,
            },
        );
        // Whether an accepted expansion was withdrawn from this conversation
        // because the tier refused it at the window (REQ-589 BR-14.1).
        //
        // It travels out of the loop rather than being acted on inside it
        // because it decides which of the two outcomes below this turn takes,
        // and the loop's whole shape is that every ending funnels through that
        // one seam. See the commit/abandon match for what it changes and why.
        // The loop is labelled and its endings `break` a value rather than
        // returning one, so every one of them funnels through the single
        // commit/abandon below instead of each exit remembering to disarm —
        // BR-6's atomicity is a property of the shape, not of ten call sites
        // agreeing.
        // REQ-588 ADR-1: the prompt's spend accumulator, created **here** —
        // before the attempt loop — because "per prompt" is its lifetime, not a
        // key looked up somewhere. Every attempt, every fallback reroute and
        // every duty of this prompt is handed this same `Arc`, so they add into
        // one total; the next prompt gets a new one and therefore starts at
        // zero, without anything having to remember to reset it.
        //
        // `None` when no ceiling is configured, which is what makes the whole
        // feature cost nothing when it is off (ADR-6): the check at the choke
        // point needs both this and a ceiling, so neither the accumulator nor
        // the pricing lookup exists on an un-opted-in machine.
        let prompt_spend = config
            .cost
            .ceiling_micro_cents()
            .map(|_| Arc::new(teton_core::cost_ceiling::PromptSpend::default()));

        let outcome = self
            .run_attempts(
                tctx,
                AttemptInputs {
                    turn_id: &turn_id,
                    phase,
                    tools: &tools,
                    tool_ctx: &tool_ctx,
                    stream_events: &stream_events,
                    // `system` itself — see `prepare_the_attempts` (REQ-606).
                    refit_system: &system,
                    typed_refit,
                    prompt_spend: prompt_spend.as_ref(),
                },
                &mut st,
            )
            .await;

        self.commit_or_abandon(outcome, st, &stream_events)
    }

    /// **Stage 1 — claim the turn, then read the session state under it.**
    ///
    /// The claim is *returned* rather than held here, and that is the whole
    /// subtlety of this stage: `run_prompt_turn` binds it as its first local so
    /// it outlives the conversation guard bound later, because locals drop in
    /// reverse. A stage that held the claim would release it at its own `}`,
    /// and a waiting client could slip a turn between this turn's commit and
    /// its release.
    ///
    /// The ordering inside — claim, *then* re-read — is LESSON-539 and is
    /// pinned by `the_claim_is_taken_before_the_registry_is_re_read`, which
    /// scopes itself to this function.
    fn claim_the_turn(
        &self,
        sessions: &SessionRegistry,
        session_id: &SessionId,
        session_cwd: Option<PathBuf>,
    ) -> Result<ClaimedTurn, RpcError> {
        let turn_id = teton_protocol::TurnId::from(format!(
            "turn-{}",
            self.turn_counter.fetch_add(1, Ordering::SeqCst)
        ));

        // REQ-567 BR-5 / D-3: claim the session before ANY of this turn's work.
        // Two `session/prompt` calls on one session can be in flight at once —
        // each runs on its own task — and both would replay the same snapshot
        // and both commit, so the second commit would erase the first turn's
        // blocks wholesale. Refused rather than queued, with the turn already
        // running named in the sentence.
        //
        // Placed first so a refused prompt spends nothing: no classifier call,
        // no title duty, no tool registry. The claim releases on drop, so every
        // exit below — including the task abort that has no code to run — frees
        // the session.
        //
        // Bound for the whole function on purpose, and declared BEFORE the
        // conversation guard below so it outlives it: locals drop in reverse,
        // which means the commit happens while this session is still claimed and
        // a waiting client cannot slip a turn between the write and the release.
        let _claim = sessions
            .try_begin_turn(session_id, &turn_id)
            .map_err(|err| refused_claim_error(&err))?;

        // REQ-583 verify: the `session_cwd` parameter was read off the registry
        // BEFORE the claim above was taken (`spawn_prompt_turn` snapshots the
        // summary, then spawns). A `session/set_cwd` that landed in that window
        // moved the root and cleared the conversation — and a turn built on the
        // stale snapshot would run jailed to the old root, state that root in
        // its environment block, and commit its blocks into the just-cleared
        // conversation. Now that the claim is held no move can land, so the
        // registry's path is authoritative for the whole turn and is re-read
        // here; the snapshot stands in only for a session the registry no
        // longer has, which the claim a moment ago says is not this one.
        let session_cwd = sessions
            .get(session_id)
            .map(|summary| summary.cwd)
            .unwrap_or(session_cwd);

        // REQ-583 ADR-1: the root is probed once per turn, from the registry's
        // path, and the ONE probe feeds every consumer — the jail
        // (`ToolContext::for_root`), the prompt's environment block
        // (`route.harness.session_root`) and, since REQ-585, the identity a
        // skill turn's user block is pinned to. (The skill file's *display*
        // spelling is discovery's, not this probe's — it needs the skill's
        // source as well, which only the registry has; BUG-187.)
        // It is read here rather than beside the jail below because the
        // expansion needs it and the expansion runs before the route (see the
        // comment at `accept_invocation`'s call site).
        let probed = self.session_root_for(session_cwd.as_deref());
        Ok(ClaimedTurn {
            turn_id,
            claim: _claim,
            probed,
        })
    }

    /// **Stage — resolve the route, expanding any skill first.**
    ///
    /// Everything here happens *before* `TurnContext` exists, because the
    /// warming hold at the end of it rebinds `router` and BR-2.1 requires the
    /// context be built after the last rebinding of every field it captures.
    /// The stage therefore takes its inputs explicitly; that asymmetry with the
    /// stages after the pivot is the invariant showing through the types.
    async fn resolve_the_route(
        self: &Arc<Self>,
        facts: SessionFacts<'_>,
        request: PromptRequest<'_>,
        presence: &mut ClientPresence,
    ) -> Result<ResolvedRoute, RpcError> {
        let SessionFacts {
            events,
            sessions,
            session_id,
            config,
            gate,
            probed,
        } = facts;
        let PromptRequest {
            turn_id,
            prompt,
            skill,
            mode,
            phase,
            invoker,
        } = request;
        // **The turn's one registry snapshot**, taken here under the claim and
        // read by both consumers: the `/name` resolution just below, and the
        // `skill` tool `build_tools` registers further down (REQ-587 ADR-3,
        // ADR-5). An `Arc` clone off the session registry, not a discovery —
        // `discovery_is_paid_at_create_and_at_cd_and_never_per_turn` pins that
        // no turn opens a directory, and this opens none.
        //
        // One turn, one snapshot, for the reason the config above is one
        // snapshot: a `/cd` landing between two reads would leave the roster
        // the model was shown and the registry its call resolves against
        // describing two different roots. The claim is held, so no move can
        // land — which makes the single read a *statement* rather than a race
        // this happens to win.
        let skills = sessions.skills(session_id);
        let skill_turn = match &skill {
            // `invoker` is carried in for REQ-589 ADR-10's acknowledgment: the
            // question is **addressed** to the connection that typed the `/name`
            // and answerable by nobody else, exactly as the dynamic-context
            // question below it is (REQ-585 ADR-7).
            Some(invocation) => Some(
                self.accept_invocation(&skills, probed, invocation, gate, invoker)
                    .await?,
            ),
            None => None,
        };
        // The one reading of "this turn's prompt text". For a skill turn it is
        // the **body-only** expansion — the dynamic-context output is not folded
        // in yet, and that is deliberate rather than incidental: the classifier
        // reads the skill's instructions, and the alternative would make the
        // route depend on output that the route's own permission level decides
        // whether to produce.
        // Owned rather than borrowed: `settle_expansion` below lends
        // `skill_turn` mutably, and a `&str` into it would hold an immutable
        // borrow across that call. One `String` per turn, and it buys the seam.
        let routed_text: String = skill_turn
            .as_ref()
            .map_or_else(|| prompt.to_owned(), |skill| skill.text.clone());

        let router = self.turn_router(config, session_id);

        let core_phase = phase.map(to_core_phase);
        let mut route = self
            .dispatch_route(&router, session_id, mode, core_phase, &routed_text)
            .await;

        // REQ-580 BR-1/BR-3: a turn with nowhere to run *only* because the
        // local tier is still coming up is held here, and then routed afresh.
        //
        // Here and not at the `NoTierAvailable` arm below, because everything
        // between — the tools, the system head, the carried conversation — is
        // built from the route, and a turn served after the wait must be
        // built from the route it is served *by* (REQ-567 BR-7 for a single
        // turn: the head is this turn's, not a stale one's). Nothing above this
        // line has spent anything a held turn should not spend: the session
        // claim (which is the point — a second prompt on this session while
        // this one waits is `SESSION_BUSY`, naming it), and a `dispatch_route`
        // that the warming tier caused to bypass its classifier.
        //
        // The predicate is the attempt's own (`attempt_source`, the reading
        // `run_one_attempt` refuses on) crossed with the tier's state
        // (`local_tier_hold`, the reading `unserved_turn_error` codes
        // `TIER_WARMING` on) — so a turn is held on exactly the two facts that
        // would otherwise have refused it with "retry in a moment", and no
        // other. A route the router sent somewhere that can serve (a remote
        // provider) is not held: the tier's state is not that turn's concern
        // (REQ-547 D-3 — the gate withholds the tier, never the session).
        let router = match self.hold_for(config, &route) {
            Some((waiting_on, model_id)) => {
                events.publish(
                    Some(session_id.clone()),
                    Event::TurnQueued(TurnQueued {
                        turn_id: turn_id.clone(),
                        model_id,
                        waiting_on,
                    }),
                );
                tokio::select! {
                    () = self.await_local_tier() => {}
                    // The client left while the turn was still held. Nothing
                    // was spent and nobody is listening: end the turn with the
                    // refusal it would have carried without the hold — the same
                    // classifier, the same sentence — rather than run a ghost
                    // turn on the tier when it opens (ADR-3).
                    () = presence.gone() => {
                        let category = route.resolution.as_ref().map(|r| r.category);
                        return Err(unserved_turn_sentence(
                            &route,
                            self.unserved_turn_error(config, category),
                        ));
                    }
                }
                // Fresh, from the tier's settled state: the router now sees the
                // tier as it is, and the classifier — bypassed while the tier
                // was down — runs for real. If the tier failed instead of
                // opening, this route lands on the `NoTierAvailable` arm below,
                // where the classifier now says something settled.
                let router = self.turn_router(config, session_id);
                route = self
                    .dispatch_route(&router, session_id, mode, core_phase, &routed_text)
                    .await;
                router
            }
            None => router,
        };

        // REQ-598 ADR-4 / BR-2.1: **this** is the earliest point a
        // `TurnContext` may be built, and the line above is why.
        //
        // BR-2 asks for construction after the turn is claimed, and the claim is
        // ~150 lines above. That is necessary and not sufficient. BR-2 names one
        // instance of a class: *a context must not be constructed before any
        // point that rebinds a field it captures.* `router` is bound before
        // `dispatch_route`, and then **shadow-rebound by the hold above** when
        // the local tier was still warming — rebuilt from the settled tier state
        // so the classifier that was bypassed while the tier was down runs for
        // real.
        //
        // A context built at the first `router` binding satisfies BR-2, passes
        // this file's whole suite, and hands every consumer below a router
        // describing a tier state that no longer exists — silently breaking
        // REQ-580's guarantee that a turn served after the wait is built from
        // the route it is served *by*. The guard is mechanical, not this
        // comment: see the BR-2.1 warming-tier test in this module.

        Ok(ResolvedRoute {
            skills,
            skill_turn,
            routed_text,
            router,
            route,
        })
    }

    /// **Stage — spend the session's one naming attempt, on a detached task.**
    ///
    /// Only for a typed turn: a skill invocation crosses the wire as a name and
    /// would have the session named from `""`.
    ///
    /// The duty it spawns publishes its own `route_decided` from inside
    /// `tokio::spawn`, which is why the golden event fixture discriminates that
    /// event by category rather than by position (LESSON-591).
    fn spawn_the_naming_duty(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        sessions: &SessionRegistry,
        skill_turn: &Option<SkillTurn>,
        routed_text: &str,
    ) {
        // REQ-561 TASK-062: name the session, at most once for its whole life.
        // Ahead of the turn rather than after it, for two reasons: the name is
        // derived from the prompt, which is already in hand, so a client can
        // label the session the moment the user hits enter rather than a whole
        // answer later; and this is a point on the path that every turn
        // reaches, where the turn's own maze of early returns is still ahead.
        //
        // After the hold rather than before it (REQ-580): the title's route
        // reads the same tier state the turn's does, and a title requested of a
        // tier that is still loading spends the session's one naming attempt on
        // a duty that cannot run — the first prompt of every session that
        // started during a load stayed untitled for its whole life. One
        // classification's latency later is the price, and it is paid on the
        // local model.
        //
        // **Started here, not awaited here.** The turn proceeds while the
        // naming runs on its own task; the handle is dropped because nothing
        // below reads a title, and a session that is not named yet is a session
        // with no title — BR-3's degraded state. It cannot fail the turn — see
        // `spawn_title_session`.
        //
        // REQ-585: `routed_text`, not `prompt` — the naming attempt is spent
        // once per session, and a skill turn's `prompt` is empty.
        //
        // **A skill turn names later**, below Stage A. The naming duty is a
        // model call: on a machine with `reflex` bound remotely it puts a
        // bounded copy of its input on the wire. Spending it here would make
        // BR-8's refusal sentence — *"Nothing was sent and no provider saw this
        // turn"* — true of the turn and false of the machine, and would spend
        // the session's one naming attempt on an expansion that never ran. A
        // typed prompt still names exactly here, so REQ-561's "label the
        // session the moment the user hits enter" is unchanged for every turn
        // that is not a skill invocation.
        if skill_turn.is_none() {
            let _ = self.spawn_title_session(
                tctx.core,
                sessions,
                routed_text,
                // A typed prompt is the user's own bytes, read from no file, so
                // there is nothing for a boundary to be compared against. This
                // is the one call site for which the empty value is a statement
                // rather than an omission — the skill call site below is the
                // other one, and it says something different.
                Provenance::empty(),
            );
        }
    }

    // Assemble the harness context, tools, and the permission gate once; a
    // fallback re-runs the loop against the same accumulated context.
    //
    // REQ-544 (known limitation, deliberately deferred): the retry/fallback
    // path below re-runs the loop against this *same* `ctx`, which by design
    // preserves completed work (file reads/edits done before a mid-turn
    // transient failure). The trade-off is that the accumulated context is
    // re-sent to the retry/fallback provider and thus re-billed as input
    // tokens — a mid-turn transient failure re-bills the partial progress.
    // A clean fix (snapshot `ctx` here and restore it before a retry, or drive
    // retries at single-call granularity so only the failed call is re-issued)
    // changes the "continue vs. restart" semantics and needs a product call on
    // whether a fallback should preserve or discard partial work; it is out of
    // scope for this correctness pass. `ContextManager` is `Clone`, so the
    // snapshot itself is cheap when that decision is made.
    // TODO(REQ-544 followup): make retries cost-neutral once continue-vs-restart
    // semantics are decided.
    /// **Stage — assemble the harness: tools, jail, stream and system prompt.**
    ///
    /// Everything here reads the turn's single `ProbedRoot` and its single
    /// config snapshot. It mutates `route.harness` in place rather than
    /// returning a new one, because `route` is rebound on every fallback
    /// reroute and a stage that returned a fresh harness would hand the loop a
    /// second thing to keep in step (`turn_context.rs` ADR-3).
    async fn assemble_harness(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        sessions: &SessionRegistry,
        skills: &Arc<SkillRegistry>,
        probed: &ProbedRoot,
        route: &mut crate::router::Route,
    ) -> AssembledHarness {
        // `events`, `session_id` and `config` come off the context rather than
        // being passed again beside it — three parameters that were already in
        // the bundle sitting next to them.
        let (events, session_id, config) =
            (tctx.core.events, tctx.core.session_id, tctx.core.config);
        let tools = self
            // REQ-587 ADR-3: the connection that submitted this turn is the
            // addressee of any consent the `skill` tool raises — it now travels
            // on `tctx` with the rest of the turn's facts (REQ-598). `ConnectionId`
            // is `Copy`, so the seam below still consumes its own.
            .build_tools(tctx, Arc::clone(skills))
            .await;
        // BUG-147: jail this session's tools to the CLIENT's working directory.
        // The daemon-global `repo_root` is only a fallback for clients that did
        // not send one — under launchd it is `/`, which is what had every tool
        // call running against the filesystem root.
        //
        // REQ-583 ADR-1: the root is probed here, per turn, from the registry's
        // path — never cached, never client-derived — and the ONE probe feeds
        // both consumers: the jail (`ToolContext::for_root`, whose refusals name
        // the display) and the prompt (`route.harness.session_root`, the
        // environment block). Both are built from one `ProbedRoot`, so the jail
        // path and the probed view cannot come from two readings. Probing per
        // turn is what keeps the branch honest after a checkout between turns
        // and moves every consumer the turn after a `/cd` rewrote the path.
        //
        // REQ-585 moved the probe itself to the top of the turn — the expansion
        // needs it, and the expansion runs before the route — so `probed` here
        // is that same single reading, not a second one.
        //
        // REQ-611 BR-8 / ADR-7: and the session's transcript directory is
        // denied to every file tool the turn runs, at both seams the jail and
        // the walkers give a path. Composed on **every** turn, not only while a
        // transcript is being recorded: `enabled` is a fact about what the sink
        // writes next, and last week's file is still on disk after
        // `/transcript off`. Nothing here reads `transcript.enabled`, and that
        // is the point.
        let tool_ctx = ToolContext::for_root(probed)
            .with_denied_prefix(effective_transcript_dir(&config.transcript));
        // REQ-611 BR-4: the turn's streaming surface also carries the sink, so
        // the tool input and the tool result — neither of which the bus has ever
        // carried — reach the transcript in-process from the two points in the
        // loop that hold them.
        let stream_events =
            SessionEvents::new(events.clone(), session_id.clone()).with_sink(self.transcript());

        // REQ-572 BR-3: the prompt's capability clause reads the same classifier
        // that decides tool exposure — stated here, where both inputs live, so
        // the SearchUnavailable clause can reach a session (the registry
        // fallback alone cannot distinguish it from Ready).
        route.harness.web_capability = Some(web_capability_state(
            &config.web,
            self.local_model_present(),
        ));
        // REQ-583 BR-1: the same probed root the jail above was built from — so
        // the environment block and the jail's refusals print one spelling.
        route.harness.session_root = Some(probed.view.clone());
        // REQ-584 BR-7: known project names for a NON-project root, ranked by
        // `last_seen` and bounded here, where the registry is. The composer
        // places them and decides how many fit; deriving them there would put a
        // filesystem-backed read inside a pure renderer.
        //
        // **Reads the stored snapshot only — it never scans** (BR-3): this runs
        // on every turn, and a turn that did not ask for projects must not pay
        // for a directory walk, let alone raise the macOS Documents dialog.
        route.harness.known_projects =
            if probed.view.kind == teton_protocol::methods::RootKind::Project {
                Vec::new()
            } else {
                self.projects
                    .snapshot()
                    .rank(None)
                    .iter()
                    .map(|p| {
                        teton_core::session_root::bounded_field(
                            &p.name,
                            teton_core::session_root::NAME_MAX_CHARS,
                        )
                    })
                    .collect()
            };
        // REQ-612 BR-6 / ADR-3: the repository's notes, re-checked **here** —
        // the one place per turn that has already re-derived the root, after
        // `session_root_for` and before the prompt is built. Never mid-turn: the
        // system prompt is fixed for the turn, so an edit that lands between two
        // tool iterations is resident at the *next* prompt and not inside this
        // one.
        //
        // The quiet answer costs one `stat` and no allocation (`refresh`
        // returns `None` when the key and the boundary verdict both still
        // hold), which is why it is taken inline rather than on a blocking
        // thread: one syscall does not earn a hand-off, and a turn whose notes
        // actually changed is already paying for a read.
        let enabled = self.repo_context_enabled(sessions, session_id);
        let boundaries = config.effective_boundaries();
        let current = sessions.repo_context(session_id);
        // The route's **effective** cap, not the build's ceiling (ADR-5): the
        // loader stored the file and this stage renders it, so a floored
        // 16,384-byte route carries 4 KiB of notes and the local tier the full
        // 8 KiB — one derivation, asked where the route is known.
        let cap = route.budget.repo_context_cap;
        let state = match refresh_repo_context(
            &current,
            probed,
            &boundaries,
            enabled,
            self.repo_files.as_ref(),
        ) {
            Some(fresh) => {
                // Published on the change and only on the change (BR-6's "one
                // event"), from outside the registry lock, before the prompt
                // that carries it is built.
                let event = repo_context_event(&fresh, cap);
                if sessions.set_repo_context(session_id, fresh) {
                    events.publish(Some(session_id.clone()), event);
                }
                sessions.repo_context(session_id)
            }
            None => current,
        };
        // The block, rendered from the state the two lines above settled. The
        // manager's `system_sources` follows from it through `CarriedTurn::begin`
        // — read off this same value, so the prompt's bytes and the provenance
        // of those bytes are seeded from one fact (ADR-2, REQ-585 BR-7).
        route.harness.repo_context = state.file().map(|file| RepoContextBlock::render(file, cap));
        let system = build_system_prompt(&tools, &route.harness);
        AssembledHarness {
            tools,
            tool_ctx,
            stream_events,
            system,
        }
    }

    /// **Stage — settle the skill expansion against the budget.**
    ///
    /// Stage A asks whether the body fits, the consent gate asks the user, and
    /// stage B folds the dynamic context in. All three read the same `system`
    /// prompt and the same route, and all three may leave `skill_turn` changed —
    /// which is why it is lent mutably rather than returned: a stage that
    /// returned a new `SkillTurn` would give the caller two versions to keep in
    /// step.
    async fn settle_expansion(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        inputs: ExpansionInputs<'_>,
        skill_turn: &mut Option<SkillTurn>,
    ) -> Result<Option<AcceptedExpansion>, RpcError> {
        let ExpansionInputs {
            sessions,
            probed,
            route,
            routed_text,
            system,
        } = inputs;
        // ── REQ-585 BR-8 / ADR-11: Stage A — does the BODY fit? ──────────────
        //
        // Before the user is asked to approve anything (BR-8d), and before
        // `CarriedTurn::begin` below, which both pushes the user block and arms
        // the drop-commit — so a check placed after it would have committed the
        // very expansion it is refusing (BR-8c). Nothing is derived here: the
        // budget is the one `Router::budget_for` stamped on this route, and the
        // measurement is `ContextManager::would_seed_fit`, the estimators the
        // pressure path itself runs on.
        //
        // A refused turn returns from here: no `context_pressure` of any kind,
        // no health change, no degradation, no retry — none of the machinery
        // below has run.
        //
        // ── REQ-589 BR-2/BR-3: and the refusal is now a *question* ───────────
        //
        // Over budget no longer ends the turn on its own. On the user-typed path
        // — this one, and only this one — the measurement is put to the person
        // who typed the name, who may send it anyway, take a durable fix, or
        // decline. Declining is byte-for-byte the refusal above it replaced
        // (AC-3), and every not-sent arm still returns from here with BR-11
        // intact: the naming duty is below this line, so a refused turn has
        // spent no model call.
        //
        // `SkillCaller::User` is not passed because it cannot be anything else:
        // `skill_turn` is `Some` only for a typed `/name`, and
        // `OverBudgetOffer`'s composer hardcodes it (BR-2). The model's own
        // expansions are measured by the loop's `skill_append_fit` and are never
        // offered a choice.
        //
        // The one place this turn's pressure policy can be decided, because it
        // is the one place an over-budget send is consented to (BR-12).
        //
        // **One variable, not a flag beside a payload** (REQ-589 BR-14.1). This
        // was a `bool` and an `Option<String>` set on the same two arms; the
        // withdrawal below needs the skill's *name* as well, and a third local
        // that three arms have to remember to keep in step is how one of them
        // comes to say a turn was accepted while another cannot say what was
        // accepted. `Some` **is** "this turn's over-budget send was consented
        // to", and everything downstream reads it from here.
        let mut accepted: Option<AcceptedExpansion> = None;
        if let Some(skill) = skill_turn.as_ref() {
            match self
                .offer_or_refuse_over_budget(
                    tctx,
                    route,
                    SkillStage::Body,
                    skill,
                    system,
                    // Nothing has been accepted yet: this is the turn's first
                    // question.
                    None,
                )
                .await
            {
                SkillStageVerdict::Fits => {}
                SkillStageVerdict::Accepted => {
                    // The exact bytes the answer was about, carried to Stage B
                    // so an unchanged expansion is not put to the user twice —
                    // and to the failure path below, which has to find this
                    // block in the conversation to withdraw it (BR-14.1).
                    accepted = Some(AcceptedExpansion {
                        skill: skill.name.clone(),
                        text: skill.text.clone(),
                    });
                }
                SkillStageVerdict::NotSent(message) => {
                    return Err(RpcError::new(
                        error_code::SKILL_EXPANSION_TOO_LARGE,
                        message,
                    ));
                }
            }
        }

        // Stage A said the body fits, so this turn is going to happen and the
        // session may be named after it. Deferred to here rather than run with
        // the typed prompts above, because the naming duty is a model call and
        // a refused turn must not have reached one (BR-8).
        //
        // **With the expansion's own provenance** (REQ-587 verify). `routed_text`
        // here is `SkillTurn::text` — the skill file's bytes — and the duty sends
        // a bounded copy of it to a route `title_route` resolves *remotely*
        // unless the session is already tainted, which on the session's first
        // substantive prompt it is not. `Provenance::empty()` short-circuits
        // `Egress::send` before any boundary check, so passing it here would ship
        // a `local-only` skill body (or a user skill's fail-closed `unknown`) off
        // the machine through the one duty the turn does not wait for.
        //
        // The values are read off the turn rather than recomputed, and they are
        // this text's provenance *at this point in the function*: the commands
        // have not run yet, so `routed_text` still carries
        // `PENDING_PLACEHOLDER` where their output will go and no command has
        // contributed anything for `unknown` to account for. The seam below OR's
        // in `spawned` before the user block is seeded, which is the same rule
        // applied to a longer string — not a second reading of this one.
        if let Some(skill) = skill_turn.as_ref() {
            let _ = self.spawn_title_session(
                tctx.core,
                sessions,
                routed_text,
                expansion_provenance(&skill.sources, skill.unknown),
            );
        }

        // ── REQ-585 BR-6 / BR-12: dynamic-context consent and execution ──────
        //
        // The TASK-205 seam. One `authorize_skill` for the whole invocation,
        // `run_all` in document order with the session root as cwd, the outcomes
        // folded back into `skill_turn.text`, and BR-12's `skill_invoked`
        // published — in that order, and all of it **between** the two budget
        // stages.
        //
        // Stage A is above this and must stay there: a body that cannot fit is
        // refused *before* a user is walked through approving four commands,
        // watching them run, and then being told the turn was refused (BR-8d).
        // Stage B is below it and must stay there: until this seam runs, the
        // slots hold `skills::PENDING_PLACEHOLDER` — exactly what Stage A
        // measured — and it is the fold that makes the second measurement a
        // different one.
        // ─────────────────────────────────────────────────────────────────────
        if let Some(skill) = skill_turn.as_mut() {
            self.settle_dynamic_context(
                tctx.core.events,
                tctx.core.session_id,
                tctx.gate,
                &probed.path,
                tctx.invoker,
                skill,
            )
            .await;
        }

        // ── REQ-585 BR-8 / ADR-11: Stage B — does it still fit with the
        // dynamic-context output folded in? ──────────────────────────────────
        //
        // Reached only once Stage A has answered `Fits` **or was accepted**,
        // which is what entitles this stage's sentence to say the body itself
        // fit. Still before `CarriedTurn::begin`, for Stage A's reason.
        //
        // REQ-589: this stage offers too, and the wire carries which stage
        // spoke precisely so it can. The two are different questions and the
        // sentence says so — Stage A's is about the body, Stage B's has to say
        // that the dynamic-context output is what spent the room, which is a
        // different thing for the user to act on. Leaving this one a hard
        // refusal would mean a skill with one oversized `` !`command` `` output
        // could never be sent even by someone who understood exactly what they
        // were asking for, and would leave `SkillStage::WithDynamicContext`
        // unreachable on every surface TASK-241 put it on.
        //
        // BR-11 reads differently here, and the difference is not new: the
        // naming duty and the dynamic-context commands are both *above* this
        // line, so a Stage B refusal has spent them — exactly as it did before
        // REQ-589. What is unchanged is the invariant that matters for
        // `-32023`: no provider has seen this turn.
        if let Some(skill) = skill_turn.as_ref() {
            match self
                .offer_or_refuse_over_budget(
                    tctx,
                    route,
                    SkillStage::WithDynamicContext,
                    skill,
                    system,
                    // What Stage A's answer was about, if there was one. An
                    // expansion the fold left untouched is the same question,
                    // already answered a few lines up — asking again would let
                    // a second refusal kill a turn the user had just approved.
                    accepted.as_ref().map(|a| a.text.as_str()),
                )
                .await
            {
                SkillStageVerdict::Fits => {}
                // Recorded with **this** stage's bytes, which is the point of
                // re-assigning rather than only raising a flag: the fold may
                // have grown the expansion since Stage A, and it is the folded
                // text that becomes the block a failure has to withdraw
                // (BR-14.1). A Stage-B-only acceptance — Stage A said `Fits`,
                // the dynamic-context output is what spent the room — leaves
                // this `None` if it does not write here, and the withdrawal
                // would then have nothing to look for.
                SkillStageVerdict::Accepted => {
                    accepted = Some(AcceptedExpansion {
                        skill: skill.name.clone(),
                        text: skill.text.clone(),
                    });
                }
                SkillStageVerdict::NotSent(message) => {
                    return Err(RpcError::new(
                        error_code::SKILL_EXPANSION_TOO_LARGE,
                        message,
                    ));
                }
            }
        }

        // The seed, and its provenance (BR-7, ADR-9): the expansion for a skill
        // turn, the typed text otherwise. One block either way — `push_user_from`
        // with an empty set and `unknown: false` is byte-identical to the
        // `push_user` every typed turn has always taken.
        // What a **reroute** has to re-ask, carried past the move below.
        //
        // Both stages measured against the budget of the route this turn
        // started on. A mid-turn reroute — the privacy pin, or a provider
        // fallback — swaps in a smaller one, and `refit_for_reroute` then
        // *clamps* the conversation: `truncate_to_budget` drops history until
        // one block is left and middle-elides that block, which by then is the
        // skill expansion. BR-8 says a skill turn is never middle-elided into
        // something the user did not invoke, and BR-4 says carried whole or
        // refused.
        //
        // It is not a corner: BR-7 makes the privacy pin the *expected* path
        // for any invocation that ran a dynamic command on a boundary-configured
        // machine, which is every ADLC skill. The name and the text are cloned
        // because the seed below consumes them, and a clone of one expansion is
        // the price of not shortening it behind the user's back.
        // The system prompt rides along for the same reason: it is consumed by
        // `CarriedTurn::begin` below, and the refusal has to measure the same
        // pair Stage A and Stage B did — system plus expansion.
        //
        // **REQ-587 BR-7: there is more than one of them, and REQ-585's guard
        // could not see any of the others.** This list was a single `Option`
        // built from `skill_turn`, which is `Some` only for a user-typed
        // `/name` — so `skill_would_not_survive_refit` answered `None` for
        // *every* model invocation and `refit_for_reroute` middle-elided the
        // expansion, at the one seam REQ-585 built the guard for. On a
        // boundary-configured machine that privacy pin is the expected path for
        // any invocation that ran a dynamic command, so this was the common
        // case, not a corner. The typed turn seeds the list; every expansion
        // the model commits inside the loop joins it below, and the guard
        // refuses on the first that would not survive.

        Ok(accepted)
    }

    /// **Stage — assemble the prompt and arm the conversation for attempts.**
    ///
    /// The last thing before the loop. `CarriedTurn::begin` pushes the user
    /// block and arms the guard, so nothing here may fail afterwards without
    /// the commit protocol below deciding what to do about it.
    ///
    /// **Returns a pair rather than a `PreparedAttempts` (REQ-606).** That
    /// bundle had three fields and one of them was a round-trip: it took
    /// `system: &str`, cloned it as `refit_system`, and returned the clone to a
    /// caller that still held the original unmutated. Deleting the field left
    /// two values, where a tuple is the idiomatic shape and the binding names at
    /// the one call site carry the meaning. It also removes a `String`
    /// allocation from every turn.
    fn prepare_the_attempts(
        self: &Arc<Self>,
        sessions: &SessionRegistry,
        session_id: &SessionId,
        config: &Config,
        system: &str,
        products: TurnProducts,
    ) -> (AttemptState, usize) {
        let TurnProducts {
            route,
            skill_turn,
            prompt,
            accepted,
        } = products;
        let skill_refit: Vec<(String, String, String)> = skill_turn
            .as_ref()
            .map(|skill| (skill.name.clone(), skill.text.clone(), system.to_owned()))
            .into_iter()
            .collect();
        // Where the typed seed ends and the loop's own expansions begin, so a
        // second attempt rebuilds the tail rather than appending a duplicate of
        // it: the tool's per-turn state accumulates across attempts.
        let typed_refit = skill_refit.len();

        // The seed, and its provenance (BR-7, ADR-9): the expansion for a skill
        // turn, the typed text otherwise. One block either way — `push_user_from`
        // with an empty set and `unknown: false` is byte-identical to the
        // `push_user` every typed turn has always taken.
        let (prompt, prompt_sources, prompt_unknown) = match skill_turn {
            Some(skill) => (skill.text, skill.sources, skill.unknown),
            None => (prompt, BTreeSet::new(), false),
        };

        // REQ-567 BR-1: this turn begins from what the session has already said.
        // The head was rebuilt from *this* turn's tools and route, and the
        // carried blocks are replayed under it — so a mid-session head change
        // re-renders the same conversation rather than fossilizing an old head
        // (BR-7). From here the manager is the conversation-in-progress:
        // whichever outcome arrives — completed, failed, the task being dropped,
        // or a panic — decides what the session keeps (see [`CarriedTurn`]),
        // and there is exactly ONE place below where a turn's outcome becomes
        // that decision.
        let conversation = CarriedTurn::begin(
            sessions,
            session_id,
            system,
            &route.harness,
            Arc::clone(&self.session_taint),
            config.effective_boundaries(),
            prompt,
            // REQ-585 BR-7: a typed prompt is drawn from no file; a skill turn
            // carries the skill file's id, or the unpinnable marker for a user
            // skill outside the root. It is the same block either way.
            prompt_sources,
            prompt_unknown,
        );

        // The loop's carried state, bundled so the stage below takes three
        // parameters instead of twenty (REQ-598 ADR-1: "two small structs, not
        // one wide one"). `run_prompt_turn` owns it, so `conversation` and
        // `route` survive the last attempt for the commit protocol to read.
        let st = AttemptState {
            attempts: 0,
            rerouted_local: false,
            withdrew_accepted_expansion: false,
            accepted,
            skill_refit,
            conversation,
            route,
        };

        (st, typed_refit)
    }

    /// **Stage — the attempt loop.**
    ///
    /// One `run_one_attempt` per iteration, with the reroute arms at the bottom
    /// deciding whether to try again. Extracted whole rather than in pieces:
    /// its `break 'turn` exits are the turn's exits, and splitting the loop
    /// from the arms that break out of it would replace a label the compiler
    /// checks with a convention a reader has to hold.
    async fn run_attempts(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        inputs: AttemptInputs<'_>,
        st: &mut AttemptState,
    ) -> Result<PromptTurnResult, RpcError> {
        'turn: loop {
            tctx.core.router.emit_route_decided(
                tctx.core.events,
                Some(tctx.core.session_id.clone()),
                &st.route,
            );
            let provider_id = st.route.provider_id.clone();

            let result = self
                .run_one_attempt(
                    tctx,
                    // Passed apart from `tctx` on purpose: `st.route` is reassigned
                    // by the reroute arms at the foot of this loop, and a
                    // context owning it would go stale (ADR-3).
                    &st.route,
                    inputs.phase,
                    inputs.tools,
                    inputs.tool_ctx,
                    inputs.stream_events,
                    st.conversation.ctx_mut(),
                    inputs.prompt_spend,
                    // REQ-589 BR-12 / ADR-8: the one turn whose top-of-loop
                    // pressure gate is suspended is the one whose over-budget
                    // measurement a human was shown and accepted. Built fresh
                    // per attempt because `PressurePolicy` is consumed by the
                    // call — which is the type carrying the "exactly one
                    // iteration" rule rather than a flag someone has to reset —
                    // and because a fallback attempt re-assembles the *same*
                    // consented prompt, so it is owed the same suspension.
                    if st.accepted.is_some() {
                        PressurePolicy::SuspendedForAcceptedTurn
                    } else {
                        PressurePolicy::Enforced
                    },
                )
                .await;

            // REQ-587 BR-7: what the *loop* committed is only knowable now.
            // Both reroute arms below sit under this `Err`, so the list is
            // refreshed here — once, from the tool's own per-turn record of
            // what it folded — rather than at each guard. The tail is rebuilt
            // from `inputs.typed_refit` because that state accumulates across attempts.
            if result.is_err() {
                st.skill_refit.truncate(inputs.typed_refit);
                st.skill_refit
                    .extend(model_invoked_expansions(inputs.tools, inputs.refit_system));
            }

            // REQ-544 M-1: a privacy block is NOT a transient failure. It must
            // never be retried against the blocked provider (which would emit
            // duplicate `privacy_block` tctx.core.events and never reroute). Taint the
            // session and re-run this same turn on the local tier — reusing the
            // C-2 taint→local mechanism — so there is exactly one block event and
            // one reroute. The egress choke point already emitted the single
            // authoritative `privacy_block`.
            if let Err(err) = &result {
                // REQ-562 BR-3: the *cause* travels with the signal, so all
                // three sentences below name which inspection refused the turn.
                // Read as one value rather than asked twice — a block with no
                // detail is not a block (see `HarnessError::privacy_block_detail`).
                if let Some(detail) = err.privacy_block_detail() {
                    if taints_the_session(detail) && self.session_taint.mark(tctx.core.session_id) {
                        eprintln!("{}", taint_pin_line(taint_detail_word(detail)));
                    }
                    if !self.engine.present() {
                        break 'turn Err(RpcError::new(
                            error_code::PRIVACY_BLOCKED,
                            unrerouteable_block_sentence(detail),
                        ));
                    }
                    if st.rerouted_local {
                        // Already rerouted to local (which has no egress and so
                        // cannot privacy-block) — never loop.
                        break 'turn Err(RpcError::new(
                            error_code::PRIVACY_BLOCKED,
                            failed_reroute_block_sentence(detail),
                        ));
                    }
                    // REQ-586 BR-1: the budget follows the route. The local
                    // pin's window is a fraction of the remote one this turn
                    // was assembled against, so the context is re-fitted here —
                    // after the route is chosen, before the retry — rather than
                    // arriving over-window at a tier that has no fallback left.
                    let previous = st.route.budget.clone();
                    st.route = tctx
                        .core
                        .router
                        .resolve_local_pin(reroute_after_block_reason(detail));
                    if let Some(refusal) = skill_would_not_survive_refit(
                        &st.skill_refit,
                        inputs.typed_refit,
                        &st.route,
                    ) {
                        if !relay_refit_refusal(
                            &refusal,
                            &mut st.conversation,
                            inputs.tools,
                            &mut st.skill_refit,
                        ) {
                            break 'turn Err(RpcError::new(
                                error_code::SKILL_EXPANSION_TOO_LARGE,
                                refusal.message,
                            ));
                        }
                    }
                    refit_for_reroute(
                        &mut st.conversation,
                        inputs.stream_events,
                        &previous,
                        &st.route.budget,
                    );
                    st.rerouted_local = true;
                    continue;
                }
            }

            // ── REQ-589 BR-14.1 / D-8: an approval must not leave the session
            // hitting the same wall ──────────────────────────────────────────
            //
            // The tier refused the very bytes a human just approved. The turn
            // is over either way — nothing about resending them can succeed —
            // but the *session* must not be left holding the expansion that
            // earned the refusal, because the budget the next turn is measured
            // against is the one that already said these bytes fit. A window
            // that disagrees with the stamped budget disagrees with it again on
            // the next turn, and the next, which is the circle the reported
            // `/analyze` failure walked.
            //
            // **Read through `context_refusal`, never by matching the two
            // variants** (ADR-3). The local engine's refusal is its own variant
            // and it is the tier the reported failure ran on; a predicate that
            // matched `ContextLengthExceeded` here would handle the remote half
            // and quietly miss the one that matters, exactly as the arm below
            // used to.
            //
            // Sited here rather than in that arm for the same reason the
            // privacy block above is: this is where `result` is still a
            // `HarnessError` and `st.conversation` is still writable. The arm
            // below composes the user's sentence from the same projection, so
            // the words the model reads in the withdrawn block and the words
            // the user reads in the error are one composer's (BR-5).
            if let (Some(accepted), Some(refusal)) = (
                st.accepted.as_ref(),
                result
                    .as_ref()
                    .err()
                    .and_then(HarnessError::context_refusal),
            ) {
                let sentence = refusal.sentence(&st.route.budget.window_label);
                // BR-14.2, and it is marked whether or not the withdrawal below
                // finds its block: the rejection is a thing this daemon
                // *watched happen*, and the next offer for this pair is owed it
                // regardless of what the conversation turned out to look like.
                // Keyed off the route that actually refused — a fallback may
                // have moved the turn since the offer was made, and it is the
                // refusing window the next offer's lead is a claim about.
                self.window_rejections.mark(
                    tctx.core.session_id,
                    &accepted.skill,
                    &RouteWindow::of(&st.route.harness.budget),
                );
                st.withdrew_accepted_expansion =
                    withdraw_accepted_expansion(&mut st.conversation, &accepted.text, &sentence);
            }

            match result {
                Ok(outcome) => {
                    // REQ-544 M-5: a provider that just served a turn is healthy
                    // again — clear any earlier downgrade (including a half-open
                    // re-probe that just succeeded) so a recovered provider returns
                    // to full rotation on the next turn.
                    if let Some(pid) = st.route.provider_id.as_ref() {
                        self.record_health(&pid.0, HealthRecord::healthy());
                    }
                    // REQ-544 C-2's pin — "this turn's context intersects a
                    // local-only boundary, so every later turn stays local" —
                    // is evaluated at the commit seam rather than here. It has
                    // to be: the cancellation path commits too, and a pin
                    // written only in this arm left an aborted turn's boundary
                    // content carried into the next prompt with nothing pinning
                    // the session (see [`CarriedTurn::commit_now`]).
                    break 'turn Ok(PromptTurnResult {
                        turn_id: inputs.turn_id.clone(),
                        stop_reason: outcome.stop_reason,
                    });
                }
                // REQ-586 BR-2 / ADR-8: the tier answered that the request does
                // not fit its window. A **typed outcome**, and therefore ahead
                // of every `Remote` arm below: this is not a provider failure
                // and must not be run through the machinery that treats one.
                //
                // No `record_health` — a provider that correctly reported its
                // own limit is not unhealthy, and downgrading it would move
                // *later* turns off a provider that is working. No
                // `on_provider_failure` — a fallback would send the same bytes
                // to a window that may be smaller, and would emit a
                // `provider_degraded` blaming the provider for the daemon's
                // sizing. And no retry, because nothing about resending
                // unchanged bytes can succeed.
                //
                // The sentence carries the three numbers a user can act on and
                // no response body (BR-11): who refused, what was assembled,
                // and the budget the route was running under. A wide gap
                // between the last two says the declared window is wrong; a
                // narrow one says this content tokenizes denser than the
                // estimator assumed.
                //
                // REQ-589 ADR-3: **both** window refusals arrive here, on one
                // arm. The local engine has no provider to name, so it is its
                // own variant; giving it its own arm as well would be a second
                // place for the same sentence to be edited, and the local tier
                // is precisely the one whose report was found wrong. Before
                // this, a local over-window turn fell through to the
                // `HarnessError::Engine` arm below and reached the user as
                // `INTERNAL_ERROR "the local engine could not serve the turn"`.
                Err(
                    err @ (HarnessError::ContextLengthExceeded { .. }
                    | HarnessError::LocalContextLengthExceeded { .. }),
                ) => {
                    break 'turn Err(RpcError::new(
                        error_code::CONTEXT_LENGTH_EXCEEDED,
                        // `Display` is the total fallback rather than an
                        // `expect`: the pattern above admits only the two
                        // variants `window_refusal_sentence` answers for, so
                        // the fallback is unreachable, and a future third
                        // window refusal degrades to a true sentence instead of
                        // panicking the daemon.
                        err.window_refusal_sentence(&st.route.budget.window_label)
                            .unwrap_or_else(|| err.to_string()),
                    ));
                }
                // REQ-588 BR-3 / ADR-4: the spend ceiling, answered here and
                // **before** the generic remote arm below. That arm asks for a
                // `failure_class`, and this error deliberately has none, so
                // without this branch a budget stop would fall through to
                // "provider failed unrecoverably" — a sentence that is wrong
                // about the cause, silent about the money, and names no remedy.
                //
                // The sentence is composed here rather than carried up from the
                // choke point because `TransportError` is `Copy` and cannot hold
                // one — and because every fact it needs is already in scope at
                // this point: the accumulator this prompt has been adding to,
                // the ceiling the same config supplied to the choke point, and
                // the route's provider and model. Composed through the one
                // composer the `/verbose` clause uses, so the two surfaces
                // cannot come to name different ceilings (BR-2).
                Err(HarnessError::Remote(perr)) if perr.is_spend_ceiling_reached() => {
                    use teton_core::cost_ceiling::{ceiling_refusal, unpriced_refusal, SpendBound};
                    let bound = SpendBound::PromptCeiling;
                    let ceiling = tctx.core.config.cost.ceiling_micro_cents().unwrap_or(0);
                    let spent = inputs.prompt_spend.as_ref().map_or(0, |s| s.spent());
                    // Two different problems with two different remedies, told
                    // apart by what the prompt recorded: an unpriceable call is
                    // not an overspend, and telling a user to raise a ceiling
                    // when the real fix is a missing price would send them to
                    // the wrong file.
                    let message = if inputs
                        .prompt_spend
                        .as_ref()
                        .is_some_and(|s| s.saw_unpriced())
                    {
                        unpriced_refusal(
                            provider_id.as_ref().map_or("", |p| p.0.as_str()),
                            st.route.model.as_deref().unwrap_or("(unknown model)"),
                            bound,
                        )
                    } else {
                        ceiling_refusal(spent, ceiling, bound)
                    };
                    break 'turn Err(RpcError::new(error_code::SPEND_CEILING_REACHED, message));
                }
                Err(HarnessError::Remote(perr)) if st.attempts < 2 => {
                    st.attempts += 1;
                    let Some(pid) = provider_id.as_ref() else {
                        break 'turn Err(RpcError::new(
                            error_code::INTERNAL_ERROR,
                            "remote turn failed with no provider to fall back from",
                        ));
                    };
                    let Some(class) = perr.failure_class() else {
                        break 'turn Err(RpcError::new(
                            error_code::INTERNAL_ERROR,
                            "provider failed unrecoverably",
                        ));
                    };
                    // REQ-544 M-5: persist the failed provider's health so the
                    // downgrade survives into the next turn's routing. A transient
                    // failure (Retry) leaves health untouched; a persistent one is
                    // stamped with a half-open cooldown so it recovers on its own.
                    if let Some(record) = health_record_after_failure(class, Instant::now()) {
                        self.record_health(&pid.0, record);
                    }
                    let fo = tctx
                        .core
                        .router
                        .on_provider_failure(&st.route, &pid.0, class);
                    if let Some(degraded) = fo.degraded {
                        tctx.core.router.emit_provider_degraded(
                            tctx.core.events,
                            Some(tctx.core.session_id.clone()),
                            degraded,
                        );
                    }
                    match fo.route {
                        Some(next) => {
                            // REQ-586 BR-1/ADR-2: a fallback provider may
                            // declare a smaller window than the one that just
                            // failed, so the same refit runs here. An in-place
                            // degrade arrives through this arm too and is
                            // silent by construction — it keeps the failed
                            // provider's pair (see `refit_for_reroute`).
                            let previous = st.route.budget.clone();
                            st.route = next;
                            if let Some(refusal) = skill_would_not_survive_refit(
                                &st.skill_refit,
                                inputs.typed_refit,
                                &st.route,
                            ) {
                                if !relay_refit_refusal(
                                    &refusal,
                                    &mut st.conversation,
                                    inputs.tools,
                                    &mut st.skill_refit,
                                ) {
                                    break 'turn Err(RpcError::new(
                                        error_code::SKILL_EXPANSION_TOO_LARGE,
                                        refusal.message,
                                    ));
                                }
                            }
                            refit_for_reroute(
                                &mut st.conversation,
                                inputs.stream_events,
                                &previous,
                                &st.route.budget,
                            );
                            continue;
                        }
                        None => {
                            break 'turn Err(RpcError::new(
                                error_code::UNKNOWN_PROVIDER,
                                "provider failed and no fallback is configured",
                            ));
                        }
                    }
                }
                Err(HarnessError::Remote(_)) => {
                    break 'turn Err(RpcError::new(
                        error_code::INTERNAL_ERROR,
                        "remote turn failed after exhausting fallbacks",
                    ));
                }
                // BUG-146: name what actually failed. The reason is the
                // engine's own sentence, which on this path is always a static
                // literal or an already-scrubbed backend message — never a
                // path or prompt text (BR-11).
                Err(HarnessError::Engine(e)) => {
                    break 'turn Err(RpcError::new(
                        error_code::INTERNAL_ERROR,
                        format!("the local engine could not serve the turn: {e}"),
                    ));
                }
                // BUG-146: nothing could serve the turn. The daemon knows
                // exactly why — it published the same fact on the lifecycle
                // stream moments earlier — so it says so, with the action.
                // BUG-152: and with the code that says whether there is an
                // action at all, or only a wait.
                Err(HarnessError::NoTierAvailable) => {
                    // The category the turn was routed by — read off the
                    // resolution rather than recomputed, and `None` for the taint
                    // pin, which resolved no category at all (BR-7).
                    let category = st.route.resolution.as_ref().map(|r| r.category);
                    // REQ-572 ADR-4: the same classification, plus the
                    // `capability_dead_end` announcement for the one cause that
                    // names an absent capability rather than a broken one.
                    break 'turn Err(unserved_turn_sentence(
                        &st.route,
                        self.unserved_turn_error_announcing(
                            tctx.core.config,
                            category,
                            tctx.core.events,
                            tctx.core.session_id,
                        ),
                    ));
                }
                // REQ-544 M-3: a credential that will not resolve is a config
                // problem, not a transient fault — surface it clearly (the
                // message names the reference and reason, never the secret) and
                // do not retry the same broken credential.
                Err(HarnessError::Credential(msg)) => {
                    break 'turn Err(RpcError::new(error_code::CONFIG_REJECTED, msg));
                }
            }
        }
    }

    /// **Stage — commit what the turn produced, or abandon it whole.**
    ///
    /// Every ending of the attempt loop funnels here, which is what makes BR-6's
    /// atomicity a property of the shape rather than of ten exits each
    /// remembering to disarm.
    fn commit_or_abandon(
        &self,
        outcome: Result<PromptTurnResult, RpcError>,
        st: AttemptState,
        stream_events: &SessionEvents,
    ) -> Result<PromptTurnResult, RpcError> {
        // REQ-567 D-1, the whole commit protocol in one place. A turn that
        // completed hands the session what its manager holds — the retained
        // view, post cut and post compaction, moved rather than re-derived. A
        // turn that failed writes nothing at all, which is what makes BR-6's
        // "byte-identical to the failed turn never having run" true by
        // construction: the pre-turn vector was never touched.
        //
        // # The one failure that writes, and why (REQ-589 BR-14.1 / D-8)
        //
        // BR-14.1 asks for an accepted expansion to be *withdrawn* from the
        // session when the tier refuses it at the window, so the next turn
        // assembles without it. Under abandon alone that withdrawal is
        // unobservable: the manager it edited is dropped, the pre-turn vector
        // is what the session keeps, and nothing any test can drive would tell
        // the withdrawal apart from its deletion (LESSON-544's exact shape —
        // a producer with no consumer, invisible to a green suite). So the
        // withdrawal is what this turn commits, and BR-6's "no trace" is
        // narrowed by exactly one path: an over-budget send a human approved,
        // that the tier then refused at the window, whose block was found.
        //
        // What it writes is **smaller than what it was handed** — the same
        // st.conversation with the expansion replaced by the refusal that killed
        // it, and the block's provenance absorbed rather than shed (BUG-188).
        // A user who approved once is left with a session that can take
        // another turn, which is D-8's whole ask; the alternative is a
        // withdrawal that is a comment.
        //
        // **This is a deviation from REQ-567 BR-6 and is flagged as one.** The
        // narrowness is the mitigation: every other failure — every retry
        // class, every reroute, every panic — still abandons.
        match outcome {
            Ok(result) => {
                // REQ-586 BR-10: the commit re-asserts the budget one last
                // time, and what it took is news like any other clamp. Written
                // as one call rather than two lines so that the fixture
                // standing in for this dispatch runs the same protocol — see
                // [`commit_and_publish`].
                commit_and_publish(st.conversation, stream_events, &st.route.budget);
                Ok(result)
            }
            Err(err) if st.withdrew_accepted_expansion => {
                // The withdrawal, and the turn's error, both. The commit runs
                // the same protocol the success arm does — it re-asserts the
                // budget and evaluates the taint pin — because what is being
                // handed over is a conversation the session will assemble from
                // next, not a special case of one.
                commit_and_publish(st.conversation, stream_events, &st.route.budget);
                Err(err)
            }
            Err(err) => {
                st.conversation.abandon();
                Err(err)
            }
        }
    }

    /// Why no tier could serve a turn, said so the user can act on it
    /// (BUG-146) — and coded so a client can tell "wait" from "fix something"
    /// (BUG-152).
    ///
    /// Reached only from the [`HarnessError::NoTierAvailable`] arm — the route
    /// named a provider this daemon does not have and the local slot was empty.
    /// "Nothing could serve it" is one condition with six very different
    /// causes, and the daemon can tell them apart: it published exactly this
    /// classification on the lifecycle stream at startup. The precedence below
    /// is [`startup_lifecycle`]'s, deliberately — a turn failure and the
    /// lifecycle replay describing the same machine at the same moment must
    /// not tell the user two different stories.
    ///
    /// Two of those six causes — an install in flight, and verified weights
    /// mid-load — resolve **without the user doing anything**, so they carry
    /// [`error_code::TIER_WARMING`] and a client renders them as a waiting
    /// notice rather than a failure. The other four need an answer, a command,
    /// or different hardware, and keep [`error_code::UNKNOWN_PROVIDER`]. The
    /// split is made here, next to the classification it depends on, rather
    /// than by a client re-reading the sentence for keywords — that would be a
    /// second classifier for one state, which is what LESSON-456 is about.
    ///
    /// Every branch names the model but never a path (BR-11): the two reason
    /// builders it borrows, [`loading_local_engine_reason`] and
    /// [`no_local_engine_reason`], are the same ones the lifecycle stream
    /// already publishes.
    ///
    /// Since REQ-580 the state read is [`Self::local_tier_state`] and this is
    /// its renderer: the same reading decides whether a turn is *held* rather
    /// than refused ([`Self::local_tier_hold`]), so the two consumers cannot come
    /// to read the machine in two orders.
    pub(super) fn unserved_turn_error(
        &self,
        config: &Config,
        category: Option<Category>,
    ) -> RpcError {
        // Every settled cause codes the same way; only the two transient ones
        // below override it, and each says so at the `return`.
        let settled = |reason: String| RpcError::new(error_code::UNKNOWN_PROVIDER, reason);
        // The remote half of the sentence, appended to whichever local-tier
        // reason applies below. Four states of ONE classifier, most specific
        // first — deliberately not a second classifier (REQ-557 BR-5,
        // LESSON-456): the turn-failure sentence and the lifecycle stream have
        // to keep agreeing, so the two causes REQ-557 introduces are branches
        // *here* rather than a parallel machine somewhere else.
        let unusable = config.unusable_providers();
        let has_remote = has_remote_provider(config);
        let usable_remote: Vec<&str> = config
            .providers
            .iter()
            .filter(|p| p.kind.is_remote() && !unusable.contains(&p.id))
            .map(|p| p.id.as_str())
            .collect();
        // BUG-155: arm 1 fires only when the unusable set is actually IMPLICATED
        // — either it is all we have, or the configured default is one of them.
        // It used to fire whenever any unusable provider existed anywhere, so a
        // leftover unmigrated provider hijacked the message for unrelated
        // causes: a turn that failed for want of a `[[routing]]` rule told the
        // user to re-register a provider that had nothing to do with it, and
        // doing so changed nothing.
        let default_is_unusable = config
            .default_provider
            .as_ref()
            .is_some_and(|d| unusable.contains(d));
        // The turn's own binding is the strongest signal: if THIS category routes
        // to a provider that declares no model, that provider is the cause even
        // when other providers are perfectly healthy. Without this the message
        // would tell the user their config is fine and point them at
        // `teton policy show`, while the binding is exactly what is broken.
        //
        // REQ-558: the category, not the phase — a freeform turn has a binding
        // too and never had a phase, so keying on the phase left the default
        // experience with no way to reach this arm at all. The category comes
        // from the resolution the turn was routed by, so the two cannot disagree
        // about which binding is under discussion.
        //
        // What follows is a *lookup*, not a second resolution: it selects nothing
        // and screens nothing, it only asks which ids this turn's binding names.
        //
        // The override → tier precedence is **asked of the table**
        // (`CategoryTable::binding_for`), which is the same accessor
        // `category::resolve` reads. It used to be re-spelled here as a pair of
        // `find`s, which is a second config-reading path answering a question
        // the resolver already owns — the shape BUG-155 found three of, and the
        // failure mode is quiet: the two disagree about which binding is under
        // discussion, so the message names the wrong provider.
        let binding_names_unusable = category.is_some_and(|category| {
            teton_core::category::binding_for(&config.tiers, &config.categories, category)
                .is_some_and(|row| row.names(|id| unusable.iter().any(|u| u == id)))
        });
        let unusable_is_implicated = !unusable.is_empty()
            && (usable_remote.is_empty() || default_is_unusable || binding_names_unusable);
        let add_provider = if unusable_is_implicated {
            // REQ-557 ADR-E, router half. A remote provider with no declared
            // model is a *usability* condition, so the daemon started — that is
            // the whole point of keeping the rule out of `validate()`. The
            // refusal therefore has to happen at routing time, and it has to
            // name the provider and the remedy rather than report a generic
            // no-route the user cannot act on.
            format!(
                " Provider(s) {} are registered with no `model`, so they cannot serve \
                 turns — re-register with `teton provider add <id> --model <name>`.",
                unusable.join(", ")
            )
        } else if !has_remote {
            " No remote provider is configured either — `teton provider add` \
             registers one to serve turns while the local tier is unavailable."
                .to_owned()
        } else if config.default_provider.is_none() {
            // REQ-557 BR-4 / AC-4: the absence IS the cause, and it is nameable.
            // Pre-REQ this state could not arise, because the router synthesized
            // a default from array position and, failing that, the literal
            // "local" — which is exactly how an unconfigured install came to
            // announce a route to a provider registered nowhere (BUG-146 root
            // cause #1). Keeping the absence in the type is what makes this
            // sentence possible.
            format!(
                " A remote provider is configured but no `default_provider` is set, so a \
                 turn with no matching policy has no remote to route to; set \
                 `default_provider` to one of: {}.",
                usable_remote.join(", ")
            )
        } else {
            // A remote provider IS configured and a default IS set, so the route
            // resolving to a missing one is a routing/config mismatch rather
            // than an empty machine — say that instead of telling them to add
            // what they have.
            " A remote provider is configured but this turn did not route to it; \
             check `teton policy show` and the provider id in `teton provider list`."
                .to_owned()
        };

        match self.local_tier_state() {
            // A machine below the hardware floor has no local tier to wait for.
            LocalTierState::BelowFloor { reason } => settled(format!("{reason}{add_provider}")),
            // BR-4: a settled, deliberate absence — not something to wait for.
            LocalTierState::Declined => settled(format!(
                "the local tier was declined, so it will not serve turns; \
                 `teton model set <name>` changes that.{add_provider}"
            )),
            // Transient (BUG-152): the download finishing is the only thing this
            // turn was waiting for.
            LocalTierState::Installing { model_id } => RpcError::new(
                error_code::TIER_WARMING,
                format!("{}{add_provider}", installing_local_model_reason(&model_id)),
            ),
            // BR-1: proposed and unanswered. The session runs, the tier does not.
            LocalTierState::AwaitingDecision { model_id } => settled(format!(
                "{model_id} is proposed for this machine but has not been \
                 answered yet, so the local tier is withheld — answer the \
                 prompt (or `teton model list`) to open it.{add_provider}"
            )),
            // A load that already failed is settled: retrying the turn meets
            // the same dead engine, so this is not a "wait" state.
            LocalTierState::LoadFailed { reason } => settled(format!("{reason}{add_provider}")),
            // Transient (BUG-152): the load completing is the only thing this
            // turn was waiting for. Since REQ-580 a turn in this state is held
            // rather than refused, so this sentence reaches a user only from
            // the paths the hold does not cover (a fallback that landed on the
            // warming tier after a remote primary failed) — where "retry" is
            // still the honest advice.
            LocalTierState::Loading { model_id } => RpcError::new(
                error_code::TIER_WARMING,
                format!(
                    "{} Retry in a moment.{add_provider}",
                    loading_local_engine_reason(&model_id)
                ),
            ),
            LocalTierState::NoEngine { model_id } => settled(format!(
                "{}{add_provider}",
                no_local_engine_reason(&model_id)
            )),
        }
    }

    /// [`Self::unserved_turn_error`], plus the one dead end the daemon can
    /// actually see it standing in (REQ-572 ADR-4, AC-2).
    ///
    /// The classifier itself is untouched: every code and every sentence
    /// BUG-152 settled comes back exactly as it was, and this adds an
    /// **announcement** beside it rather than a fifth arm inside it. The
    /// announcement is made only where both halves of "this is a dead end"
    /// hold, and each half is a guard against a way the event would lie:
    ///
    /// - **the classification is settled** (`UNKNOWN_PROVIDER`). A
    ///   `TIER_WARMING` turn is not dead-ended on anything — its tier is
    ///   finishing a download or a load — and telling that user to configure a
    ///   capability would be advice to act on a state that is about to resolve
    ///   itself. This is the BUG-152 split being *consumed*, which is the point
    ///   of having made it.
    /// - **no remote provider is configured at all**. That is the one arm of
    ///   the remote half naming an absent capability; a provider registered
    ///   with no model, an unset `default_provider` and a routing mismatch are
    ///   all *configured* remote tiers whose remedy the turn's own sentence
    ///   already carries, and `capability_dead_end` is not the vocabulary for
    ///   a misconfiguration. Read through [`has_remote_provider`], the same
    ///   function the classifier's own arm reads.
    ///
    /// Session-scoped, like every event a user's own turn produces.
    pub(super) fn unserved_turn_error_announcing(
        &self,
        config: &Config,
        category: Option<Category>,
        events: &Arc<EventBus>,
        session_id: &SessionId,
    ) -> RpcError {
        let error = self.unserved_turn_error(config, category);
        if error.code == error_code::UNKNOWN_PROVIDER && !has_remote_provider(config) {
            events.publish(
                Some(session_id.clone()),
                Event::CapabilityDeadEnd(CapabilityDeadEnd {
                    capability: CapabilityDeadEnd::REMOTE_PROVIDER.to_owned(),
                }),
            );
        }
        error
    }

    /// The router a prompt turn resolves by, built from the tier and health as
    /// they stand *right now*.
    ///
    /// Called once per turn — twice for a turn that was held (REQ-580), because
    /// the whole point of the hold is that the tier's availability changed
    /// underneath the first reading. Both readings are this one function so
    /// they cannot disagree about anything but the tier.
    fn turn_router(&self, config: &Config, session_id: &SessionId) -> Router {
        // REQ-544 M-5: seed the router from the daemon-wide health map so a
        // provider marked Unavailable on an earlier turn stays Unavailable here —
        // UNLESS its half-open cooldown has elapsed, in which case it is offered as
        // Healthy so this turn re-probes it (the recovery path that keeps a single
        // transient failure from stranding a provider daemon-wide until restart).
        let health_snapshot = self.health_snapshot();
        build_router(
            config,
            // REQ-547 BR-1/D-3: a tier awaiting a consent decision is withheld
            // here, so this turn routes remote-only instead of blocking on the
            // answer.
            self.local_tier_available(),
            &health_snapshot,
        )
        // REQ-559 BR-12 / ADR-F: a provider that refused the effort field
        // earlier in THIS session resolves to `Omit(RefusedThisSession)` rather
        // than being asked again. Seeded per turn from the session-scoped memo,
        // so it is honoured by the route, the `route_decided` event, the request
        // and the `teton effort` surface alike — one resolution, every reader.
        .with_effort_refusals(self.effort_refusals.for_session(session_id))
    }

    /// Whether `route` should be held for the local tier rather than attempted
    /// (REQ-580 BR-1), and if so what the tier is doing and to which model.
    ///
    /// Two readings, both borrowed rather than re-derived: the tier's
    /// ([`Self::local_tier_hold`]) and the attempt's ([`attempt_source`], with
    /// its own one read of the slot). `Some` only when the tier is genuinely
    /// warming **and** this route has nowhere else to run — a route the router
    /// resolved to a servable remote is attempted, whatever the tier is doing.
    fn hold_for(
        &self,
        config: &Config,
        route: &crate::router::Route,
    ) -> Option<(TierWarming, String)> {
        let hold = self.local_tier_hold()?;
        match attempt_source(config, route, self.engine.get_with_format()) {
            Err(Unservable::LocalTierDown) => Some(hold),
            // Servable now (a remote), or unservable for a reason the tier's
            // arrival would not fix — neither is a wait.
            Ok(_) | Err(Unservable::RemoteWithoutModel) => None,
        }
    }

    /// Resolve one `/name` invocation against **this daemon's** registry and
    /// expand it (REQ-585 BR-4, ADR-3, ADR-9).
    ///
    /// # The client's snapshot is a convenience, not the authority
    ///
    /// `classify` runs client-side over a snapshot of this registry (ADR-13), so
    /// in the ordinary case the name arriving here is one the client already
    /// matched. That is not a reason to trust it: the snapshot is refreshed on
    /// an event, a third-party client need not hold one at all, and the registry
    /// moves under `/cd`. A check that lived only on the far side of the wire is
    /// LESSON-520's shape — so the name is resolved again here, against the
    /// registry that will actually be dispatched from, and an unknown or
    /// **shadowed** name is refused. `SkillRegistry::dispatchable` is what
    /// decides both: a shadowed row exists, is listed by `/help`, and never
    /// dispatches (BR-2).
    ///
    /// # Nothing unvalidated reaches the sentence
    ///
    /// The name is checked against [`crate::skills::is_valid_skill_name`]
    /// *before* it is echoed. A registered skill's name already satisfies it —
    /// discovery would not have registered it otherwise — so this guard only
    /// ever fires on a name that came off the wire and matched nothing, which is
    /// exactly the string that must not be reflected verbatim into a message a
    /// terminal renders (LESSON-517). `raw_arguments` is never echoed at all: it
    /// reaches the model through [`crate::skills::expand`], which defuses it.
    ///
    /// # A project skill is acknowledged here, and nowhere else on this path
    ///
    /// REQ-589 BR-6 / ADR-10 / D-10. REQ-585 BR-4's acknowledgment had exactly
    /// one production caller — `harness::tools::skill`'s **model**-invoked tool
    /// — so until this function grew an `await`, a user who typed `/name` ran a
    /// project-authored body with nothing asked. The gate is
    /// [`PermissionGate::authorize_project_skill_trust`] verbatim, under the key
    /// [`teton_protocol::methods::project_skill_trust_key`] mints, so one answer
    /// covers both callers for the session and neither can drift onto a key
    /// family of its own (ADR-7).
    ///
    /// It is asked **before the expansion**, which puts it before everything
    /// `run_prompt_turn` does with the expansion: the route, the naming attempt,
    /// Stage A's budget question and BR-8's refusal. That ordering is BR-6's
    /// whole point — a user asked "may this oversized body be sent?" before "do
    /// you trust this repository?" would be authorizing an over-budget send of
    /// bytes from a repository they have not said they trust, and a file on disk
    /// would be choosing when it gets a consent prompt. A declined trust returns
    /// from here, so no budget offer is composed and no budget sentence is
    /// rendered.
    ///
    /// **`async` is the forcing function.** The signature change is what makes
    /// a caller that skips the gate a compile error rather than a silent
    /// regression — the LESSON-508 shape this REQ found in the first place.
    async fn accept_invocation(
        &self,
        registry: &SkillRegistry,
        probed: &ProbedRoot,
        invocation: &SkillInvocation,
        gate: &PermissionGate,
        invoker: Option<ConnectionId>,
    ) -> Result<SkillTurn, RpcError> {
        if !crate::skills::is_valid_skill_name(&invocation.name) {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                "`skill.name` is not a skill name: a skill dispatches under \
                 `^[a-z0-9][a-z0-9_-]{0,63}$`, and nothing else is registered"
                    .to_owned(),
            ));
        }
        // `dispatchable_by_user`, which is this caller's question: `skill/invoke`
        // is the *user* typing `/name`, so REQ-587 BR-3's `user-invocable:
        // false` is refused here and not only by the client that usually
        // refuses it first (ADR-1 — a rule enforced only in the client is a
        // rule the next client does not have).
        let Some(skill) = registry.dispatchable_by_user(&invocation.name) else {
            return Err(RpcError::new(
                error_code::INVALID_PARAMS,
                format!(
                    "no skill `/{}` you can dispatch in this session — `skills/list` is \
                     what this session dispatches, and a name it does not list, lists as \
                     shadowed, or lists as `user-invocable: false` (model-only) is not \
                     one of them",
                    invocation.name
                ),
            ));
        };

        // One reading of "does a project skill take this name from a user
        // skill?", two consumers: the trust gate's `shadows_user_skill`
        // override, which is what makes BR-4 ask about the swap even at `full`,
        // and BR-9's rendered fact on the `SkillInvoked` line below. Two
        // readings of one registry would agree only by accident — and the
        // registry is fixed for this turn (the claim is held), so asking once is
        // a statement rather than a race this happens to win.
        let shadows = crate::harness::tools::skill::shadows_user_skill(registry, &skill.name);

        // REQ-589 BR-6 / ADR-10: the acknowledgment, before the expansion and
        // therefore before the route, the naming attempt and both budget stages.
        // A user-authored skill raises nothing — BR-4's question is about
        // *repository* text reaching the model labelled instructions, and the
        // current order stands for a file the user installed themselves.
        if skill.source == SkillSource::Project {
            // The **untruncated, faithful** home-relative root name, from the
            // one minter both callers use (`tools::skill::trust_root_name`):
            // untruncated because a key is matched and never read, home-relative
            // because the subject reaches a client that may render it, and
            // faithful because `display_for` alone collapses two distinct roots
            // onto one name — and one name is one grant.
            let root =
                crate::harness::tools::skill::trust_root_name(&probed.path, home().as_deref());
            // REQ-589 D-13, and a **deliberate security widening**: the same
            // root, named canonically, is what `[skills] trusted_project_roots`
            // is written and matched under. It is a second name rather than the
            // one above because a row in config outlives the session that wrote
            // it and is matched against a path that has had every chance to
            // change what it points at — a symlink dropped at a listed path
            // would otherwise hand an unacknowledged repository the trust of an
            // acknowledged one. See `durable_trust_root_name`.
            //
            // From `registry.read_under()` and **not** from `probed.path`. The
            // probe is re-derived every turn and never resolves anything; the
            // registry is the frozen snapshot these bodies were read into, and
            // it carries the one resolution they were read under. Naming the
            // path as spelled here is how an unattended session came to spend a
            // listed tree's row on a body from an unlisted one, because a link
            // may be re-pointed at any time in a session that reads its skills
            // exactly twice.
            let durable_root = registry
                .read_under()
                .map(crate::harness::tools::skill::durable_trust_root_name);
            let consent = match invoker {
                Some(connection) => {
                    gate.authorize_project_skill_trust(
                        &teton_protocol::methods::project_skill_trust_key(
                            teton_protocol::events::InvokedBy::User,
                            &root,
                        ),
                        crate::harness::permissions::TrustRoot {
                            display: &root,
                            durable: durable_root.as_deref(),
                        },
                        // The project's whole model-invocable set, which is what
                        // the user is being asked about; the door bounds the
                        // list it renders.
                        &crate::harness::tools::skill::project_trust_entries(registry),
                        shadows,
                        // TASK-261: **the user typed this**. The door's other
                        // caller is the model's tool, and until this argument
                        // existed the prompt said so unconditionally — so a
                        // human who typed `/analyze` was told "the model wants
                        // to run this repository's skills as instructions" by
                        // the one prompt whose job is letting them decide
                        // whether to trust the repository.
                        //
                        // Since REQ-591 D-7 it decides the **key** as well as
                        // the sentence: the answer is remembered under
                        // `project_skill_trust:user:<root>`, and the model's
                        // door at this very root keeps asking. That is D-2's
                        // rule about a durable row, applied to the session
                        // answer that is the same question at a shorter range.
                        teton_protocol::events::InvokedBy::User,
                        connection,
                    )
                    .await
                }
                // No addressable connection — an internal driver, or a fixture.
                // The question cannot be *put* to anyone, which is the gate's
                // own fail-closed answer, so it is spelled with the gate's word
                // rather than with a second one (`settle_dynamic_context` says
                // the same thing at its own door).
                None => SkillConsent::Unanswerable,
            };
            // `closed_door` because it is the reader **both** callers share:
            // the model's tool and this path must not come to disagree about
            // which settlement is a decline and which is a refusal (LESSON-528).
            if let Some(door) = closed_door(consent) {
                return Err(project_trust_refusal(
                    &skill.name,
                    &root,
                    durable_root.as_deref(),
                    door,
                ));
            }
        }

        // BR-4's preamble names the file, never absolutely: an absolute path
        // carries a username — or the location of the user's working tree —
        // into a transcript and into every remote payload this turn produces.
        // Taken from the registry row rather than re-derived here, because the
        // rule needs the skill's *source* and the session root as well as
        // `HOME`, and discovery is the one place that holds all three
        // (BUG-187); `expand` stays pure either way (BR-14).
        let display = skill.path_display.clone();
        let expansion = crate::skills::expand(skill, &invocation.raw_arguments, &display);
        // REQ-587 ADR-6: the frame line is the caller's, and this caller is the
        // user path — so BR-4's line is supplied here and composed *inside* the
        // string below. Prepending it around `pending_text` would leave Stage A
        // and Stage B measuring a string this turn does not send, short by the
        // frame's length, and `truncate_to_budget` middle-elides what BR-8 says
        // is carried whole or refused.
        let frame = expansion.user_frame();
        let text = expansion.pending_text(&frame);

        // ADR-9's id-minting gap, decided rather than papered over: a project
        // skill is under the root and mints; a user skill at
        // `~/.claude/skills/x/SKILL.md` in a repo-rooted session has no
        // repo-relative identity, and the minter refuses rather than inventing
        // one. `unknown` is what carries that refusal forward.
        //
        // Through `skills::provenance_of`, not `ProvenanceId::from_resolved`
        // directly (REQ-587 verify): `from_resolved` takes a **canonical** path
        // and `skill.path` is the path discovery walked to the file, which a
        // symlinked-but-in-repo project root leaves non-canonical. That helper
        // is the one home for resolving both sides, and it fails closed.
        let (sources, unknown) = match crate::skills::provenance_of(&probed.path, skill) {
            Some(id) => (BTreeSet::from([id]), false),
            None => (BTreeSet::new(), true),
        };

        Ok(SkillTurn {
            name: skill.name.clone(),
            text,
            // REQ-587 BR-5: **the** mint, from the expansion, before the value
            // is moved into the turn. `permission_key_for(skill.source, …)` is
            // the tempting spelling and it is the silent one — the gate accepts
            // either and pins whichever it is given, so a plain key here would
            // keep REQ-585's behaviour (one answer covering every argument list
            // this skill is ever invoked with) with nothing red. The two facts a
            // caller cannot supply correctly on its own — the *substituted*
            // command set, and whether the arguments had a hand in it — live on
            // the expansion, which is why the mint does. The `Skill` method that
            // used to offer the tempting spelling on the row itself is gone.
            permission_key: expansion.grant_key(skill.source),
            expansion: Some(expansion),
            source: skill.source,
            // BR-9's three rendered facts, read off the registry row here
            // because this is the surface that has it: the client's snapshot
            // lives on its `UiContext` and `render_event` cannot see it, and by
            // the time it could the registry may have moved under a `/cd`.
            // `shadows` is the one reading taken above, which the trust gate
            // also asked its question with.
            shadows_user_skill: shadows,
            model_invocable: skill.model_invocable,
            user_invocable: skill.user_invocable,
            // Bounded here, at the surface, for the reason the preamble's copy
            // is bounded in `expand`: BR-12's event goes to every attached
            // client and into transcripts (ADR-15). The *rule* is discovery's
            // (BUG-187); the ceiling is the renderer's.
            path_display: teton_core::session_root::bounded_field(
                &display,
                teton_core::session_root::DISPLAY_MAX_CHARS,
            ),
            body_bytes: skill.body.len() as u64,
            ignored_keys: skill.ignored_keys.clone(),
            name_note: skill.name_note.clone(),
            sources,
            unknown,
        })
    }

    /// Ask once, run in document order, fold the outcomes back into the
    /// expansion, and publish the invocation's record (REQ-585 BR-6, BR-12;
    /// ADR-7, ADR-14, ADR-15).
    ///
    /// # One consent, every command
    ///
    /// [`PermissionGate::authorize_skill`] is called **once per invocation**,
    /// with the whole command list in document order and already substituted, so
    /// the prompt shows what will run. A prompt per command is REQ-560 BR-2's
    /// named anti-pattern — the shipped ADLC corpus has skills with four `` !`…` ``
    /// slots, and four prompts for one typed `/name` is a session nobody uses
    /// twice.
    ///
    /// The key is the skill's own (`skill:<source>:<name>`, ADR-6), minted with
    /// the registry row in hand at [`Self::accept_invocation`] and carried here
    /// — never `shell`, or one "allow for this session" answered at a skill
    /// prompt would free every later model-issued shell call (LESSON-495).
    ///
    /// # Nothing is re-read across the await
    ///
    /// The permission `await` releases this turn to the event loop, and nothing
    /// below it re-reads a fact the gate settled above it: `decide` snapshots the
    /// level at the top of its own body for exactly that reason (REQ-560 BR-7),
    /// and the root, the commands and the expansion were all fixed before the
    /// question was asked. A `/permissions` landing mid-prompt moves the *next*
    /// turn.
    ///
    /// # The event is published here, and not one line later
    ///
    /// BR-12 says *every* invocation echoes one line, so the publish is the last
    /// thing this function does and the Stage B check is the first thing after
    /// it. A turn where the user approved four commands, watched them run, and
    /// was then refused for size is the turn whose record matters most; a publish
    /// on the far side of that refusal would leave it with no echo line and no
    /// `/verbose` outcomes (ADR-15).
    ///
    /// # No model call happens here (BR-4)
    ///
    /// [`crate::skills::run_all`] is `run_bounded`'s second caller, not
    /// `ShellTool::run`'s — so `Tool::refine`, which fires the `shell` duty and
    /// *is* a model call, is not on this path (ADR-14).
    async fn settle_dynamic_context(
        self: &Arc<Self>,
        events: &Arc<EventBus>,
        session_id: &SessionId,
        gate: &PermissionGate,
        root: &Path,
        invoker: Option<ConnectionId>,
        skill: &mut SkillTurn,
    ) {
        // Taken, not borrowed: `fold` consumes, so this value can be spent once
        // and the type says so (see [`SkillTurn::expansion`]). It is `Some` on
        // every path into this function, which runs once per turn.
        let Some(expansion) = skill.expansion.take() else {
            return;
        };
        let commands = expansion.commands().to_vec();

        // A skill with no dynamic context asks nothing. There is no question to
        // put — a prompt listing zero commands is a prompt about nothing — and
        // BR-12's line still gets published below with an empty outcome list,
        // which is a real state ("0 dynamic commands") rather than a missing one.
        let door = if commands.is_empty() {
            None
        } else {
            let consent = match invoker {
                Some(connection) => {
                    gate.authorize_skill(
                        &skill.permission_key,
                        &skill.name,
                        skill.source,
                        commands.iter().map(|c| c.as_str().to_owned()).collect(),
                        // This path is REQ-585's user-typed `/name`, and only
                        // that: `SkillTurn` is `Some` for a slash command and
                        // nothing else. The model's invocations reach the gate
                        // from inside the turn loop (TASK-217/TASK-218) and pass
                        // `InvokedBy::Model` there.
                        teton_protocol::events::InvokedBy::User,
                        connection,
                    )
                    .await
                }
                // No addressable connection — an internal caller, or a fixture.
                // The question cannot be *put* to anyone, which is precisely the
                // gate's own fail-closed answer, so it is spelled with the gate's
                // word rather than with a second one.
                None => SkillConsent::Unanswerable,
            };
            closed_door(consent)
        };

        let outcomes = match door {
            // Consent given. Sequential, in document order, with the session
            // root as cwd and the `shell` tool's jail, composed environment
            // (REQ-596: an allowlist, not a scrub), PATH floor, process group
            // and deadline (ADR-14).
            None if !commands.is_empty() => {
                let root = root.to_path_buf();
                let to_run = commands.clone();
                let timeout_ms = self.skill_command_timeout_ms;
                // On the blocking pool: `run_bounded` waits on a child process
                // for up to the deadline, per command, and a turn that parked an
                // async worker for that long would stall every other session on
                // it. A panic inside propagates exactly as an inline call's
                // would — which is the only way this join can fail, since a
                // `spawn_blocking` task is never cancelled.
                tokio::task::spawn_blocking(move || {
                    crate::skills::run_all(&root, &to_run, timeout_ms)
                })
                .await
                .expect("the dynamic-context runner does not panic")
            }
            None => Vec::new(),
            // A closed door is the same answer for every command of the
            // invocation, because the question was asked once about all of them.
            Some(reason) => vec![door_outcome(reason); commands.len()],
        };

        // The fold is where the output becomes prompt text: a ran slot enters
        // inside `frame_untrusted_builtin("skill:<name>", …)` — the same
        // envelope every built-in tool result gets, which neutralizes envelope
        // tags in its payload — and every other slot becomes an explicit
        // ``[dynamic context not run: `<cmd>` — <reason>]`` (BR-6).
        //
        // The frame is the same line Stage A measured, from the same composer
        // (REQ-587 ADR-6): this fold changes the slots and nothing else, which
        // is what entitles Stage B's sentence to say the body itself already
        // fit.
        let frame = expansion.user_frame();
        skill.text = expansion.fold(&frame, &outcomes);

        // BR-7: anything that came from a command carries what `shell` output
        // carries — nothing that can be pinned. On a boundary-configured machine
        // that fails closed, so an invocation that **spawned** any command pins
        // its turn to the local tier, exactly as a `shell` result does. Recorded
        // on the block the seed below builds, which is what egress inspects.
        //
        // `spawned`, not `did_run`: a command that ran and exited non-zero, or
        // timed out, produces no output but still reports a *value the command
        // chose*, and `fold` writes that value into the prompt (`exited 2`).
        // Asking about output left a side channel that pinned nothing — one bit
        // per command about a boundary-protected file, in a turn free to route
        // remote. `ShellTool::run` tags every spawned arm the same way, and the
        // claim above is parity with it.
        skill.unknown |= outcomes.iter().any(DynamicOutcome::spawned);

        events.publish(
            Some(session_id.clone()),
            Event::SkillInvoked(SkillInvoked {
                name: skill.name.clone(),
                source: skill.source,
                path_display: skill.path_display.clone(),
                body_bytes: skill.body_bytes,
                ignored_keys: skill.ignored_keys.clone(),
                name_note: skill.name_note.clone(),
                outcomes: commands
                    .iter()
                    .zip(outcomes.iter())
                    .map(|(command, outcome)| outcome_view(command, outcome, door))
                    .collect(),
                // A literal, and it stays one: the only path that reaches here
                // is REQ-585's user-typed `/name` expansion. A model-issued
                // call never comes through this function — it publishes its own
                // record from inside the tool (REQ-587 TASK-217), which is the
                // one surface that has the turn's invocation count.
                invoked_by: teton_protocol::events::InvokedBy::User,
                shadows_user_skill: skill.shadows_user_skill,
                model_invocable: skill.model_invocable,
                user_invocable: skill.user_invocable,
                // **`None`, and that is a fact rather than an omission.** BR-6a's
                // per-turn cap bounds the *model's* invocations inside one
                // prompt turn; a human typing `/name` spends none of it, and a
                // `/verbose` line reading "1 of 12" here would name a budget
                // the user is not drawing on.
                turn_invocations: None,
                // **Never `Some` on this path.** A typed `/name` that reaches
                // this publish has expanded; BR-8's two stages refuse a turn
                // that cannot fit by *returning* — Stage A before this runs at
                // all, Stage B after it — so there is no refused invocation for
                // this site to describe. The model path is where a refusal has
                // a record to be attached to, because its turn survives it.
                refused: None,
            }),
        );
    }

    /// Measure one of BR-8's two budget stages for a **typed** skill turn and,
    /// where the expansion does not fit, put BR-3's question to the user
    /// instead of refusing it (REQ-589 BR-2, BR-3, BR-4, BR-11; ADR-1, ADR-13,
    /// ADR-15, ADR-16).
    ///
    /// # BR-2: the offer is the typed caller's alone
    ///
    /// Both call sites are in [`Self::run_prompt_turn`], which runs only for a
    /// user-typed `/name`. A model-issued `skill` call is measured mid-loop by
    /// `skill_append_fit` and keeps today's refusal verbatim — there is no
    /// human inside a tool call to answer per-invocation, and a consent nobody
    /// could give is not one to ask for. The reroute guard below is refusal-only
    /// for the same reason plus one more: by then the block is already in the
    /// conversation, and the choice there is between refusing whole and
    /// middle-eliding, which BR-8 and BR-4 both answer without asking.
    /// [`OverBudgetOffer`]'s own composer hardcodes `SkillCaller::User`, so the
    /// Model arm's sentence is not merely unused here — it is unreachable.
    ///
    /// # The measurement is the one estimator, called once
    ///
    /// [`ContextManager::would_seed_fit`] is what `skill_fit` calls, against the
    /// budget `Router::budget_for` stamped on this route. Nothing here derives a
    /// budget and nothing re-counts the text. `skill_fit` itself is not called
    /// on this path because it consumes the [`Fit`](crate::harness::context::Fit)
    /// and hands back only a sentence, and the offer needs the pair — ADR-11's
    /// rule is one *estimator*, not one caller of it. The sentence a decline
    /// produces is [`OverBudgetOffer::decline_refusal`], which composes through
    /// the identical `skill_refusal` arm with the identical arguments, so AC-3's
    /// "byte-identical to today's refusal" holds by construction rather than by
    /// two sentences being kept in step.
    ///
    /// # `.window`, never `.cap`
    ///
    /// [`Router::budget_inputs_for`] hands back the provider's **raw declared**
    /// `capabilities.max_context`, and that is what the verdict is measured
    /// against (ADR-15): the cap is *this daemon's* policy exactly as the
    /// generation reservation is, while the window is the provider's own bound.
    /// Passing `.cap` here would collapse the reachable `UserCap` + `FitsWindow`
    /// row — a measurement can be over budget, over the cap, and still
    /// legitimately inside the window — and would tell a user their send will
    /// blow a window it fits.
    ///
    /// # Nothing is remembered, and nothing remembered is read
    ///
    /// `PermissionGate::authorize_skill_over_budget` consults no grant and
    /// records none (BR-10, ADR-14). What *is* read is
    /// [`ObservedWindowRejections`] — a rejection this daemon watched a provider
    /// perform — and it reaches the composed sentence and nothing else: it does
    /// not suppress the question and it does not pre-answer it (BR-14.2, AC-23).
    ///
    /// # BR-11 holds on every not-sent path
    ///
    /// Every arm that returns [`SkillStageVerdict::NotSent`] returns *from the
    /// caller* before a dispatch: no provider is reached, no `context_pressure`
    /// is emitted, no health changes, and — at Stage A — the session-naming duty
    /// is still below the gate, so a refused turn has not spent it.
    async fn offer_or_refuse_over_budget(
        &self,
        tctx: TurnContext<'_>,
        route: &crate::router::Route,
        stage: SkillStage,
        skill: &SkillTurn,
        system: &str,
        already_accepted: Option<&str>,
    ) -> SkillStageVerdict {
        // Destructured to the names the REQ-589 body below already used, so
        // that body stays byte-identical (BR-1). `gate` narrows to
        // `&PermissionGate` here — this consumer never needed the `Arc`.
        let TurnContext {
            core:
                TurnCore {
                    events,
                    session_id,
                    config,
                    router,
                },
            gate,
            invoker,
        } = tctx;
        let budget = &route.harness.budget;
        let measured = ContextManager::would_seed_fit(
            system,
            &skill.text,
            budget.budget_tokens,
            budget.budget_bytes,
        );
        if measured.fits {
            return SkillStageVerdict::Fits;
        }

        // **One question per expansion, not one per stage.** BR-8's two stages
        // are two questions only when the fold between them changed something:
        // a skill with no `` !`command` `` slots — or one whose commands all
        // failed closed — reaches Stage B with byte-identical text, and asking
        // again there would put the *same* measurement of the *same* bytes in
        // front of the same person twice in one turn, with the second refusal
        // able to kill a turn they had just approved.
        //
        // Compared as **text**, not as a measured pair: two different
        // expansions can measure the same figures, and this decides whether a
        // human is asked about bytes they have not seen. This is not BR-10's
        // remembering, either — nothing survives the invocation; what is carried
        // is *this turn's* answer to *this turn's* question, one line up the
        // same function.
        if already_accepted == Some(skill.text.as_str()) {
            return SkillStageVerdict::Accepted;
        }

        // The route's own inputs, for the two facts the *budget* does not carry:
        // the declared window the verdict is measured against, and the shape
        // `proposed_window` substitutes a vendor recipe into. Read, never
        // re-derived — `budget_for` stays the single `derive` caller.
        let inputs = router.budget_inputs_for(route.provider_id.as_ref().map(|id| id.0.as_str()));
        let mut offer = OverBudgetOffer::new(
            &skill.name,
            stage,
            measured,
            budget,
            inputs.window,
            // BR-7c: the shipped catalog's figure or nothing. Keyed off the
            // route's **model**, never its provider id — ids are the user's
            // namespace (ADR-6).
            proposed_window(route.model.as_deref(), inputs, measured),
            // ADR-12's payload for the `LocalEngine` row: which tier a
            // `BindTierRemote` remedy would rebind, read off the resolution the
            // route was built from rather than re-resolved.
            route.resolution.as_ref().map(|r| r.tier),
        );

        // No connection to address the question to — an internal driver, or a
        // fixture. The question cannot be *put* to anyone, which is not the same
        // as being declined; what the user gets is BR-4's sentence, which is
        // today's refusal, and no `skill_over_budget_offered` is published
        // because no offer was made. `settle_dynamic_context` fails closed at
        // its own door the same way, with `SkillConsent::Unanswerable`.
        let Some(connection) = invoker else {
            return SkillStageVerdict::NotSent(offer.decline_refusal());
        };

        // BR-14.2: a rejection this daemon *observed*, not a consent it
        // remembered. It leads the sentence and changes nothing else.
        let prior = if self.window_rejections.was_observed(
            session_id,
            &skill.name,
            &RouteWindow::of(budget),
        ) {
            PriorWindowRejection::Observed
        } else {
            PriorWindowRejection::None
        };

        // Published when the offer is **raised**, not when it is answered: that
        // is what makes "asked and declined" distinguishable from "nobody could
        // be reached", which is REQ-585 AC-9's distinction and the reason
        // `OVER_BUDGET_REASON` needs no second refusal token.
        events.publish(
            Some(session_id.clone()),
            Event::SkillOverBudgetOffered(SkillOverBudgetOffered {
                skill: skill.name.clone(),
                source: skill.source,
                stage: wire_skill_stage(stage),
                measured_tokens: measured.tokens as u64,
                measured_bytes: measured.bytes as u64,
                budget_tokens: budget.budget_tokens as u64,
                budget_bytes: budget.budget_bytes as u64,
                bound: budget.bound,
                window_verdict: offer.window_verdict,
                // The fact a later reader cannot recover: the option list is
                // gone by then, and "this bound had no remedy" is what explains
                // an offer that presented the one-time override alone.
                remedy_kind: offer.remedy.kind(),
            }),
        );

        // **The write is planned before the question is asked**, and the plan
        // is what decides whether the two remedy-bearing options appear at all
        // (ADR-1). A `None` here is not a failure: BR-7c ships no figure for a
        // provider matching no recipe, and ADR-12 will not choose between two
        // configured remotes — in both cases the honest offer is the one-time
        // override and the decline, with the sentence still naming the durable
        // fix and asking for what the daemon does not have. What is *not*
        // acceptable is an option a human can select that writes nothing, and
        // [`RemedyPlan`]'s doc records why that is the same defect
        // `enable_permanent` shipped once already.
        let plan = plan_over_budget_remedy(config, router, &offer.remedy, measured);

        // **The plan is the fact, and it is told to the offer once** (ADR-1).
        //
        // Both halves of what a reader meets — which option rows are drawn, and
        // which closing question the sentence ends on — are read off
        // `OverBudgetOffer::remedy_offer` from here. They used to be decided
        // separately: the rows from this plan, the closing from
        // `Remedy::is_offered`. Those disagree exactly where BR-7c ships no
        // figure, where ADR-6 rule 2 rejects a cap, and where ADR-12 withholds
        // the rebind — and the offer then closed *"take the durable fix"* above
        // a prompt with no row for one.
        if plan.is_none() {
            offer.withhold_remedy();
        }

        // **BR-9's provider, named** (ADR-1; ADR-18 item 2). The rebind is the
        // one remedy whose target `Remedy::for_bound` could not know: it is
        // keyed on the bound, and the only provider id in its hand names the
        // route being *left*. The planner above is where the choice was made —
        // ADR-12's exactly-one case — so the name is taken from that plan and
        // from nowhere else, which is what makes the sentence's provider and the
        // written provider the same value rather than two lookups that agree
        // today. Where ADR-12 withheld the choice there is no plan and no name,
        // and the clause states BR-9's fix without inventing one.
        //
        // Before the labels and the sentence, both of which read the remedy as
        // it stands.
        if let Some(target) = plan.as_ref().and_then(|plan| plan.rebind_target.clone()) {
            offer.name_rebind_target(target);
        }

        // No second decision here: `option_labels` reads the same
        // `remedy_offer` the closing question does.
        let labels = offer.option_labels();

        let answer = gate
            .authorize_skill_over_budget(
                // The **plain** key `skill:<source>:<name>`, never the digest
                // spelling `SkillTurn::permission_key` carries: a digest exists
                // so a remembered grant follows its substituted commands, and
                // this door remembers nothing for it to follow (ADR-14). The
                // gate asserts this exact spelling.
                &crate::skills::permission_key_for(skill.source, &skill.name),
                PermissionSubject::SkillOverBudget {
                    skill: skill.name.clone(),
                    source: skill.source,
                    stage: wire_skill_stage(stage),
                    measured_tokens: measured.tokens as u64,
                    measured_bytes: measured.bytes as u64,
                    budget_tokens: budget.budget_tokens as u64,
                    budget_bytes: budget.budget_bytes as u64,
                    bound: budget.bound,
                    window_verdict: offer.window_verdict,
                    // ADR-16: the daemon words it, the client renders it
                    // verbatim. The structure around it is for presentation and
                    // for the `Unknown` hedge — not for a second composer.
                    sentence: offer.question(skill.source, prior),
                    // The id the **budget** carries, which is the one the remedy
                    // was addressed to — not a second opinion about who declared
                    // this window, and already sanitized by `derive`.
                    provider_id: budget.provider_id.as_deref().map(ProviderId::from),
                },
                // The gate words nothing (BR-5): finished text arrives, and what
                // it decides is which options reach the prompt.
                labels,
                connection,
            )
            .await;

        // Read **both**, and independently: `remedy_only` is a legitimate answer
        // — fix the limit, do not send this turn — and `proceed_once` is the
        // other (AC-7b). Applied before the accept is announced because the
        // remedy is the going-forward half and holds whichever way the send went.
        if answer.apply_remedy() {
            match plan {
                Some(plan) => self.apply_over_budget_remedy(events, session_id, plan, connection),
                // Unreachable, and stated rather than assumed away: the two
                // remedy ids are denied by `interpret_over_budget` unless the
                // option list carried them, and it carried them only where a
                // plan existed. A client that answered one anyway gets the
                // decline it was given, and the daemon says so — the one thing
                // that must not happen quietly here is nothing.
                None => eprintln!(
                    "tetond: an over-budget answer authorized a durable write this build \
                     planned none for — nothing was written, the limit still stands, and \
                     the next invocation will meet the same measurement"
                ),
            }
        }

        if answer.consent().is_allowed() {
            events.publish(
                Some(session_id.clone()),
                Event::SkillOverBudgetAccepted(SkillOverBudgetAccepted {
                    skill: skill.name.clone(),
                    source: skill.source,
                    stage: wire_skill_stage(stage),
                    // BR-1's "whole" is these numbers: what goes out is what was
                    // measured, and nothing on this path shortens it.
                    measured_tokens: measured.tokens as u64,
                    measured_bytes: measured.bytes as u64,
                    budget_tokens: budget.budget_tokens as u64,
                    budget_bytes: budget.budget_bytes as u64,
                    // What the user was told before they answered.
                    window_verdict: offer.window_verdict,
                }),
            );
            // BR-5's third sentence, which the refusal's *"no provider saw this
            // turn"* clause cannot be borrowed for: it becomes false the moment a
            // human proceeds, and that clause is what makes `-32023` different
            // from `-32022`. Written to the daemon's own record channel, as the
            // taint pin and the degraded-duty lines are, because an accepted turn
            // has no refusal frame to carry it and the typed
            // `skill_over_budget_accepted` above deliberately holds no prose.
            eprintln!("tetond: {}", offer.accepted_record(skill.source));
            SkillStageVerdict::Accepted
        } else {
            SkillStageVerdict::NotSent(offer.decline_refusal())
        }
    }

    /// Perform the offer's **going-forward** remedy, through the one
    /// durable-write path this daemon has (REQ-589 BR-7, BR-8, BR-9; ADR-4,
    /// ADR-5).
    ///
    /// [`Self::apply_config_update`] is `config/set`'s own body, so this
    /// inherits that method's posture verbatim — its validation, its atomic
    /// persist, its refusals — rather than minting a second way to write the
    /// same class of fact. **No new authority is created here**: a change
    /// `config/set` would refuse is refused identically, and the event below is
    /// the announcement rather than the permission.
    ///
    /// # What it writes is not decided here
    ///
    /// [`plan_over_budget_remedy`] decided it, *before* the question was asked,
    /// and the same plan is what put the remedy options on the prompt at all.
    /// That is the whole guard against ADR-1's cautionary precedent: there is
    /// no arm in this function that can accept an answer it has no write for,
    /// because a remedy with no write never became an option. A remedy this
    /// build cannot apply is not offered — never offered and then quietly
    /// dropped.
    ///
    /// # Ordering, for the one remedy that is two writes
    ///
    /// [`RemedyWrites::apply`] walks BR-9's pair in the one order that makes
    /// the forbidden state unreachable (ADR-5): the window is declared first,
    /// the tier is bound second. A failure between them leaves a declared
    /// window on a tier bound exactly where it was, which is harmless; the
    /// reverse order leaves a newly-bound remote tier with `max_context = 0`,
    /// which is the circle the reported `/analyze` failure was sitting in.
    ///
    /// A failure is stated rather than swallowed, and says which half landed:
    /// the turn's own answer already stands, and a user told their limit was
    /// raised when the write failed would meet the same refusal next turn with
    /// no explanation.
    ///
    /// # The plan is checked against the **live** config, not the one it was
    /// built from
    ///
    /// `RegisterProvider` replaces `endpoint`, `model` and `auth_ref`
    /// **wholesale** — only the two capability fields merge field-wise — and
    /// the plan captured all three from the snapshot the *question* was
    /// composed under, before an await on a human with no timeout. So an offer
    /// raised at 10:00 and answered at 10:05 would, unguarded, restore 10:00's
    /// identity over whatever `config/set` or `teton provider add` wrote at
    /// 10:02: a credential change silently reverted, and the next turn calling
    /// the old endpoint.
    ///
    /// [`Self::apply_config_update_guarded`] closes it. Every
    /// `RegisterProvider` this plan makes is admitted only if the stored
    /// provider's identity still equals what the write re-states, read under
    /// the config lock the write itself takes — so there is no instant between
    /// the check and the write. A provider that moved (or was removed) fails
    /// the remedy through the same stderr line every other failure takes,
    /// rather than being reverted; the limit still stands, and re-invoking the
    /// skill measures the provider as it now is.
    ///
    /// The **record** is read in the same guard, for the same reason: BR-7's
    /// `previous_value` is a claim about what the write replaced, and a value
    /// rendered minutes earlier is a claim about something else.
    fn apply_over_budget_remedy(
        &self,
        events: &Arc<EventBus>,
        session_id: &SessionId,
        plan: RemedyPlan,
        answered_by: ConnectionId,
    ) {
        // REQ-591 D-1, and the half of BUG-162's question REQ-589 owns.
        //
        // `config/set` runs `refuse_daemon_wide` and `refuse_unattested_commitment`
        // before it reaches `apply_config_update`; this path reaches the same
        // body from the other side, through a `permission/respond` frame that
        // `handle_permission_respond` never presence-checks. ADR-4's claim that
        // the remedy inherits `config/set`'s posture "verbatim" was true of the
        // *body* and false of the two gates around it — which is what
        // `config_set_attestation::the_remedys_gate_sits_at_its_own_door_and_not_in_the_shared_config_body`
        // pinned as a fact rather than as a footnote, and now pins the placement
        // this block chose.
        //
        // Checked **before** the writes and not inside them: gating
        // `apply_config_update` itself would put a second presence check under
        // `config/set`, which already ran one, and the seam a test drives
        // directly would stop being the seam production uses. The subject is the
        // connection that *answered the offer* — the addressee, not the
        // submitter, because those can differ and BR-10(b) asks about the actor
        // who chose the durable option.
        //
        // A refusal leaves the turn's own answer standing, for
        // `PermissionGate::persist_project_trust`'s reason: the human at the
        // prompt decided whether to send *this* expansion, and only the
        // going-forward half is a machine-wide fact. It is stated rather than
        // swallowed, in the same shape a failed write is, because a user told
        // their limit was raised when nothing was written meets the same
        // refusal next turn with no explanation.
        if let Some(seam) = self.commitment_attestation() {
            if let Err(err) = seam.attest_daemon_wide_commitment(answered_by) {
                eprintln!(
                    "tetond: the over-budget remedy was not applied — it is a machine-wide \
                     commitment and no verified human stands behind this answer ({err}). \
                     This turn's own answer stands; the limit still stands too, and the next \
                     invocation will meet the same measurement."
                );
                return;
            }
        }
        let RemedyPlan {
            kind,
            provider_id,
            previous_value,
            new_value,
            writes,
            // Spent before this point, on the offer's own wording: the target
            // is what the *question* had to name, and by the time an answer
            // reaches here the writes carry it. Destructured rather than
            // ignored with `..` so a later field cannot slip past unread.
            rebind_target: _,
        } = plan;
        // Filled by the guard below, under the lock, on the first write — which
        // for BR-9's pair is the one that happens before the tier moves, so the
        // binding it names is the one being replaced.
        let replaced: RefCell<Option<String>> = RefCell::new(None);
        let outcome = writes.apply(|update| {
            // Read off the update before it is handed over, because the guard
            // cannot borrow a value that has been moved into the call.
            let restates = match &update {
                ConfigUpdate::RegisterProvider(pc) => Some(pc.clone()),
                ConfigUpdate::SetTierBinding(_)
                | ConfigUpdate::SetCategoryBinding(_)
                | ConfigUpdate::SetPrivacyBoundary(_)
                | ConfigUpdate::SetEffort(_)
                | ConfigUpdate::SetTranscriptEnabled { .. }
                | ConfigUpdate::SetRepoContextEnabled { .. } => None,
            };
            self.apply_config_update_guarded(update, |config| {
                if let Some(restates) = &restates {
                    provider_identity_unchanged(config, restates)?;
                }
                // One borrow, held across the test and the write: two would be
                // a `RefCell` double-borrow waiting on a temporary-lifetime
                // rule, in a consent path where the cost of being wrong is a
                // panic.
                let mut slot = replaced.borrow_mut();
                if slot.is_none() {
                    *slot = Some(previous_value.read(config, &provider_id.0));
                }
                Ok(())
            })
        });
        // `None` is unreachable on the `Ok` arm — the guard runs before any
        // mutation, so a write that landed ran it — and is answered rather than
        // unwrapped, because the alternative is a panic in a consent path.
        let previous_value = replaced
            .into_inner()
            .unwrap_or_else(|| "unrecorded".to_owned());
        match outcome {
            Ok(()) => events.publish(
                Some(session_id.clone()),
                Event::SkillOverBudgetRemedyApplied(SkillOverBudgetRemedyApplied {
                    // Never `NotOffered` on a published event: that value means
                    // no fix existed to take, and nothing reaches this line
                    // without one having been written.
                    remedy_kind: kind,
                    provider_id: Some(provider_id),
                    // **Both values, always.** A record naming only the new one
                    // leaves a reader unable to tell a raise from a first
                    // declaration — which is the difference between
                    // `RaiseWindow` and `DeclareWindow`, and between a fix and a
                    // surprise.
                    previous_value,
                    new_value,
                }),
            ),
            Err((err, applied)) => eprintln!(
                "tetond: the over-budget remedy for `{}` could not be written ({}); {} and \
                 the limit still stands",
                provider_id.0, err.message, applied
            ),
        }
    }

    /// The route this turn takes, chosen before the harness runs (REQ-558 BR-1).
    ///
    /// Three layers, outermost first.
    ///
    /// 1. **Session taint** (REQ-544 C-2 / BR-7). A session whose context has
    ///    touched `local-only` or unknown-provenance content is pinned to the
    ///    local tier for every subsequent turn regardless of what any binding
    ///    resolves to. It is evaluated before a category is even chosen, so a
    ///    tainted turn issues no classification call either: category routing is a
    ///    cost decision, the boundary is a privacy guarantee, and the two
    ///    deliberately do not compose (LESSON-432).
    /// 2. **The category.** One dispatch key in both session modes; what differs
    ///    is only where the category comes from. A **structured** turn maps it
    ///    from the phase it is already in — a total function, no model call
    ///    (ADR-C). A **freeform** turn asks the `route` classifier
    ///    ([`crate::classify`]), which reads the prompt this function never hands
    ///    to the router.
    /// 3. **The resolver**, through [`Router::resolve`] / [`Router::resolve_judgment`]
    ///    — the same table, the same precedence, both modes (BR-1).
    ///
    /// The phase is stamped on **after** the decision (BR-11, AC-9): it is a
    /// cost-attribution fact and the resolver never saw it. A freeform session has
    /// no lifecycle position, so it attributes none — it never has (ADR-G).
    pub(super) async fn dispatch_route(
        &self,
        router: &Router,
        session_id: &SessionId,
        mode: SessionMode,
        core_phase: Option<CorePhase>,
        prompt: &str,
    ) -> crate::router::Route {
        if self.session_taint.is_tainted(session_id) {
            return router.resolve_local_pin(taint_pin_reason("this turn"));
        }

        match mode {
            SessionMode::Structured => {
                let ph = core_phase.unwrap_or(CorePhase::Implement);
                let mut resolved = router.resolve(category_for_phase(ph));
                resolved.phase = Some(to_protocol_phase(ph));
                resolved
            }
            SessionMode::Freeform => {
                router.resolve_judgment(&self.classify_freeform(router, prompt).await)
            }
        }
    }

    /// Classify a freeform prompt into a judgment category, or bypass (BR-3, BR-5).
    ///
    /// The bypass question is answered by **the resolver**, not here: `route` has
    /// no `ConfigurableCategory` counterpart, so `category::resolve` reaches it
    /// through the branch that consults no binding and yields the local tier or
    /// nothing. Asking a locality question at this call site would be a guard
    /// placed where it is convenient rather than where the decision is made
    /// (LESSON-484) — and it would be a *second* answer to a question the resolver
    /// has already answered (BR-6).
    ///
    /// What this function owns is the read of the engine slot, taken once for the
    /// turn exactly as [`Self::run_one_attempt`] does, with the format read
    /// alongside the handle so the async path never locks the engine for metadata
    /// (LESSON-448).
    async fn classify_freeform(&self, router: &Router, prompt: &str) -> Classification {
        let plan = crate::classify::plan(
            &router.resolution_for(Category::Route),
            self.engine.get_with_format(),
        );
        crate::classify::run(plan, prompt, router.judgment_default()).await
    }

    /// Build the tool registry for a turn: the built-ins, any registered MCP
    /// server tools (ADR-003, namespaced and egress-gated), — **only** on a
    /// machine that opted in — the web tool (REQ-563 D-1), and — **only** where
    /// the session's registry holds a model-invocable skill — the `skill` tool
    /// (REQ-587 BR-2, ADR-3, ADR-4).
    ///
    /// The web tool goes on **last** among the capped tools, after the MCP
    /// tools, and that ordering is the BR-6 charter rule expressed as insertion
    /// order rather than as a special case: [`ToolRegistry::exposed_names`] caps
    /// from the front, so a degraded provider's `max_tools` cuts the opt-in
    /// capability before it cuts a server the user configured. The `skill` tool
    /// is registered cap-**exempt** (ADR-4) and so sits outside that argument
    /// entirely.
    ///
    /// ## `invoker` is the whole of REQ-587 ADR-3, and its absence is silent
    ///
    /// `invoker` is the connection that submitted **this turn**, and it is the
    /// addressee of any consent the `skill` tool raises. It is threaded here
    /// rather than stored on [`ToolContext`] — the jail type, which dozens of
    /// fixtures construct and whose subject is the root — or on
    /// [`PermissionGate`], which is per *session*, so a connection kept there
    /// is whichever one created the session rather than the one that sent this
    /// prompt.
    ///
    /// Dropping it does not break a build and does not redden a suite: the tool
    /// takes `authorize_skill`'s `None => Unanswerable` arm and produces
    /// placeholders byte-identical to REQ-585's tested piped-refusal path,
    /// because that arm is already correct for an internal caller. What guards
    /// it is a test that drives a model-issued call and reads the addressee off
    /// the consent double (`skill_turn.rs`).
    ///
    /// ## One turn, one registry snapshot
    ///
    /// `skills` is the caller's snapshot — the same `Arc` [`Self::accept_
    /// invocation`] resolved this turn's `/name` against, taken once in
    /// [`Self::run_prompt_turn`] — and not a second read of the session
    /// registry. That is what makes ADR-5's claim true: the roster the tool
    /// renders into its description and the registry a call resolves against
    /// are provably one value, so a `/cd` cannot leave the model reading one
    /// root's names and reaching another root's files. Discovery is not paid
    /// here and must not be (`discovery_is_paid_at_create_and_at_cd_and_never_
    /// per_turn`): this is an `Arc` clone, not a walk.
    ///
    /// `config` is the **caller's** snapshot, not a second read of the mutex
    /// (REQ-572 verify): [`Self::run_prompt_turn`] already clones the config to
    /// build its route, its gate and its capability clause, and a `web/setup_
    /// commit` landing between that clone and this one gave the turn a prompt
    /// that said the capability was off while the registry it was handed had the
    /// tool in it. One turn, one snapshot — which is also what makes ADR-1's
    /// "the config **is** the flow state" true per turn rather than per read.
    pub(super) async fn build_tools(
        self: &Arc<Self>,
        tctx: TurnContext<'_>,
        skills: Arc<SkillRegistry>,
    ) -> ToolRegistry {
        // Destructured to the six names the body already used, rather than
        // reaching through `tctx` at each site: everything below is REQ-571 /
        // REQ-572 / REQ-585 registry logic this REQ must leave byte-identical
        // (BR-1), and in particular the cap-exempt versus optional ordering
        // BR-7 asks to keep visible.
        let TurnContext {
            core:
                TurnCore {
                    events,
                    session_id,
                    config,
                    router,
                },
            gate,
            invoker,
        } = tctx;
        let mut tools = ToolRegistry::with_builtins();
        if !self.mcp_servers.is_empty() {
            if let Ok(transport) = HttpTransport::new() {
                let egress =
                    Arc::new(self.mcp_egress(transport, router, config, events, session_id));
                let registry =
                    Arc::new(
                        McpRegistry::with_egress(
                            egress as Arc<dyn crate::mcp::EgressGate>,
                            Some(session_id.clone()),
                            self.mcp_servers.clone(),
                        )
                        // REQ-571 ADR-D: an MCP argument asserting a path the daemon
                        // cannot mint taints the call unknown, and the user is told
                        // which argument did it rather than left with a session that
                        // silently went local.
                        .with_event_sink(
                            Arc::clone(events) as Arc<dyn crate::egress::PrivacyEventSink>
                        ),
                    );
                crate::harness::tools::mcp::register_mcp_tools(
                    &mut tools,
                    registry,
                    tokio::runtime::Handle::current(),
                )
                .await;
            }
        }
        // REQ-584 BR-6. **Unconditional**, unlike the skill tool's "at least one
        // model-invocable skill": an empty registry is a meaningful answer here
        // and the one a new machine gives ("no known projects; looked in: …"),
        // so withholding the tool would send the model back to the disk walk
        // this exists to replace.
        //
        // Registered here — beside the built-ins, before the two conditional
        // tools — because it is a static knowledge tool in `teton_docs`' class,
        // and because REQ-563 requires `web` to be registered **last** so it
        // reads after the built-ins and MCP in the exposed tool docs.
        crate::harness::tools::register_projects_tool(
            &mut tools,
            Arc::clone(self.projects()),
            home(),
            // BR-11's hand-off record goes to the session that asked.
            //
            // REQ-611: carries the sink like every other production
            // `SessionEvents`, so that "an emitter without a sink is a test
            // fixture" stays a rule with no exceptions. This one only
            // publishes — the tap records what it publishes — so the sink is
            // never used here; the alternative is a second shape of emitter
            // whose difference a reader has to work out.
            Some(
                SessionEvents::new(Arc::clone(events), session_id.clone())
                    .with_sink(self.transcript()),
            ),
        );
        // REQ-563 BR-1: `register_web_tool` is the one place the "tier is above
        // off" condition is expressed, so a machine that never opted in has no
        // web tool rather than a web tool behind a flag.
        let web = config.web.clone();
        register_web_tool(
            &mut tools,
            &web,
            WebCache::from_config(&self.data_dir, &web),
            self.user_urls_for(session_id),
            Arc::clone(gate),
            Arc::new(RuntimeLookupSeam {
                runtime: Arc::clone(self),
                router: router.clone(),
                // The seam outlives this call, so it takes an owned copy — of
                // the caller's snapshot, which is the whole point.
                config: config.clone(),
                events: Arc::clone(events),
                session_id: session_id.clone(),
            }),
            tokio::runtime::Handle::current(),
        );
        // REQ-587 BR-2/ADR-4: `register_skill_tool` is the one place the "at
        // least one model-invocable skill" condition is expressed, so a session
        // whose roots hold none has no `skill` tool rather than a tool with an
        // empty roster. After the built-ins and outside `with_builtins()`,
        // which `docs_are_capped_by_max_tools_for_degraded_providers` asserts
        // by equality.
        register_skill_tool(
            &mut tools,
            skills,
            Arc::clone(gate),
            // ADR-3. See this function's doc: the parameter is the feature.
            invoker,
            tokio::runtime::Handle::current(),
            self.skill_command_timeout_ms,
        );
        tools
    }

    /// This session's permission gate, created on first use (REQ-563 verify,
    /// M-5).
    ///
    /// One gate per **session**, not per turn. The gate is where `*_always`
    /// answers live, and the promise attached to "Allow for this session" is
    /// that it holds for the session — a gate rebuilt on every prompt turn kept
    /// that promise for exactly one turn, with the CLI's own grant cache hiding
    /// the re-prompt from the one client that happened to have it.
    ///
    /// ## The config read happens once, at creation
    ///
    /// `[web] permission_allow` is folded into the policy table here, so a
    /// session started after `enable_permanent` listed a tier does not prompt for
    /// *that* tier, and one started before it keeps the posture it began with. That is the same
    /// stability every other session-scoped fact has (a grant, the taint flag),
    /// and the alternative — re-reading config per turn — would let a config
    /// edit silently *narrow* a session mid-conversation, which is the one
    /// direction a user has no way to observe.
    ///
    /// The gate is **not** pruned, exactly like [`Self::session_user_urls`]: the
    /// map is keyed by a monotonically-minted `SessionId`, so an entry cannot be
    /// resurrected by a later session, and the memory is a policy table plus a
    /// small grant map.
    pub(super) fn permission_gate_for(
        self: &Arc<Self>,
        session_id: &SessionId,
        events: &Arc<EventBus>,
        config: &Config,
    ) -> Arc<PermissionGate> {
        let mut gates = self
            .session_gates
            .lock()
            .expect("session gate mutex poisoned");
        Arc::clone(gates.entry(session_id.clone()).or_insert_with(|| {
            let mut gate =
                // REQ-560: the gate is created at a *level*, not at a built
                // table, because the level is what the user can change
                // mid-session — a table snapshotted here would go stale the
                // instant they typed `/permissions`. The level starts at the
                // configured default and is never written back (BR-6).
                //
                // REQ-563 BR-4: `[web] permission_allow` is what an
                // `enable_permanent` answer becomes on disk, and the gate folds
                // it onto every table the level produces — one listed tier, one
                // key, and (since REQ-560) only ever relaxing an `ask`.
                PermissionGate::with_level(
                    session_id.clone(),
                    self.default_permission_level,
                    config.web.permission_allow.clone(),
                    events.clone(),
                    self.pending.clone(),
                )
                // REQ-563 BR-4: the one consent answer that outlives the session
                // writes through the daemon, never the client. The gate holds
                // the seam and this is the only place it is filled in.
                .with_web_persistence(Arc::clone(self) as Arc<dyn WebTierPersistence>)
                // REQ-589 D-13, and the second such answer — a **deliberate
                // security widening**. `[skills] trusted_project_roots` is read
                // here, once, for the same reason `permission_allow` is: a
                // session keeps the posture it started with. What it buys is
                // the only unattended answer D-10's trust gate has; what it
                // costs is that a project-authored body can now reach the model
                // in a session where nobody acknowledged it, on the strength of
                // a row a human wrote earlier and out of band.
                .with_trusted_project_roots(config.skills.trusted_project_roots.clone())
                .with_project_trust_persistence(
                    Arc::clone(self) as Arc<dyn ProjectTrustPersistence>
                );
            // REQ-585 ADR-7: and the route a skill's dynamic-context consent is
            // *addressed* on. Filled in here for the same reason the web sink is
            // — the gate is built in one place and is cached for the session's
            // life — and left empty on a runtime nobody wired one into, which
            // asks nobody rather than falling back to the bus.
            if let Some(route) = self.addressed_delivery.get() {
                gate = gate.with_addressed_delivery(Arc::clone(route));
            }
            // REQ-591 D-1: and what proves a human behind the durable half of a
            // `p` answer. Wired here rather than folded into the sink above
            // because the *same* seam gates the over-budget remedy, which does
            // not go through this gate at all — one answer to BUG-162's
            // question, consulted from both places that ask it.
            if let Some(seam) = self.commitment_attestation() {
                gate = gate.with_commitment_attestation(seam);
            }
            Arc::new(gate)
        }))
    }

    /// This session's set of user-pasted URLs, created on first use (BR-3).
    pub(super) fn user_urls_for(&self, session_id: &SessionId) -> Arc<Mutex<UserUrls>> {
        Arc::clone(
            self.session_user_urls
                .lock()
                .expect("session user url mutex poisoned")
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(UserUrls::new()))),
        )
    }

    /// Record every URL in one **user-authored** prompt as user-pasted (BR-3).
    ///
    /// Called at prompt ingestion, before the turn runs, so a message that
    /// pastes a URL *and* asks about it in one breath classifies as
    /// `UserPasted` — the ordinary shape of the request, and one that would
    /// otherwise need two turns to be granted at the floor tier.
    ///
    /// The "user-authored" qualifier is the whole safety argument: feeding this
    /// a model turn, a tool result or a fetched page would let the model author
    /// its own authorization by writing a URL into content this set then
    /// trusts. There is exactly one caller, and it holds the `session/prompt`
    /// text.
    pub(super) fn record_user_prompt_urls(&self, session_id: &SessionId, prompt: &str) {
        self.user_urls_for(session_id)
            .lock()
            .expect("user url set mutex poisoned")
            .record_user_message(prompt);
    }

    /// Run one turn attempt against the route's provider (local or remote).
    #[allow(clippy::too_many_arguments)]
    async fn run_one_attempt(
        &self,
        tctx: TurnContext<'_>,
        route: &crate::router::Route,
        phase: Option<ProtoPhase>,
        tools: &ToolRegistry,
        tool_ctx: &ToolContext,
        stream_events: &SessionEvents,
        ctx: &mut ContextManager,
        prompt_spend: Option<&Arc<teton_core::cost_ceiling::PromptSpend>>,
        pressure: PressurePolicy,
    ) -> Result<crate::harness::TurnOutcome, HarnessError> {
        // Destructured to the names the body already used, so the REQ-558 /
        // REQ-561 / REQ-589 logic below stays byte-identical (BR-1). `route` is
        // deliberately **not** on the context: it is reassigned on every
        // fallback reroute in the caller's `'turn:` loop, and keeping it a
        // parameter is what keeps that reroute visible (ADR-3, BR-7).
        let TurnContext {
            core:
                TurnCore {
                    events,
                    session_id,
                    config,
                    router,
                },
            gate,
            // Discarded, and not an oversight: an attempt raises no consent of
            // its own. The `skill` tool's addressee is bound into the registry
            // when `build_tools` runs, once per turn, so by the time an attempt
            // is running the invoker has already been baked in where it is
            // needed. `run_one_attempt` took no invoker before this REQ either
            // (BR-1).
            invoker: _,
        } = tctx;
        let mut hook = NoopProvenanceHook;

        // One read of the slot for the whole attempt: the engine this turn runs
        // on is the engine that was live when the turn started, even if a
        // consent outcome swaps the slot mid-turn.
        // Handle AND format from the slot in one read: the format was resolved
        // at install time, so no engine lock is needed on this async path
        // (LESSON-448, REQ-554 verify).
        let local_engine = self.engine.get_with_format();
        // REQ-598 ADR-1: the duty bundle for this attempt, derived from the
        // turn's own context plus the two facts that travel with every duty
        // resolution — the one engine-slot read above, and the prompt's spend
        // accumulator. The four calls below passed these six arguments
        // identically before this REQ; that repetition is what named the bundle.
        // The gate is dropped on the way through: a duty route authorizes
        // nothing.
        let dctx = tctx.duties(local_engine.as_ref(), prompt_spend);

        // REQ-558 TASK-054: the `digest` duty resolves through its **own**
        // category, independently of the turn's. A turn on a frontier `think`
        // provider still summarizes through whatever `scan` is bound to, and a
        // turn on the local tier can digest remotely — the two decisions are not
        // the same decision, which is the whole premise of dispatching on purpose.
        let digest = self.digest_route(dctx);
        // REQ-561 TASK-060: and so does `triage`, the duty the `grep` tool owns.
        // Resolved here beside `digest` because both need the engine slot read
        // once for the attempt, and independently of it because two categories
        // are two decisions.
        let triage = self.triage_route(dctx);
        // REQ-561 TASK-061: and so does `shell`, the duty the `shell` tool owns.
        // It is a `build` duty where `triage` is a `scan` one, which is the point
        // of resolving them separately: interpreting a failed build is worth a
        // stronger model than ordering a list of grep hits.
        let shell = self.shell_route(dctx);
        // REQ-561 TASK-063: and `compact`, which belongs to no tool at all — the
        // thing that knows a conversation no longer fits is the context manager.
        // Resolved here with the others and passed separately, because
        // `ToolDuties` is the tools' own struct.
        let compact = self.compact_route(dctx);
        let duties = ToolDuties {
            triage: &triage,
            shell: &shell,
        };

        // Which source this attempt runs on — or `NoTierAvailable`, which the
        // caller classifies from state rather than reporting as an engine fault
        // (BUG-146). The three unservable shapes are spelled out on
        // `attempt_source`; the REQ-580 hold asks it the same question ahead of
        // the turn, which is why the decision lives there and not here.
        let (provider_cfg, model) = match attempt_source(config, route, local_engine)? {
            AttemptSource::Local(engine, format) => {
                let mut source = LocalEngineSource::new(engine, format, session_id.clone())
                    .metered(Arc::new(self.ledger.clone()));
                return run_session_turn_with_pressure_policy(
                    &mut source,
                    tools,
                    tool_ctx,
                    gate,
                    stream_events,
                    ctx,
                    &route.harness,
                    &mut hook,
                    &digest,
                    &compact,
                    &duties,
                    pressure,
                )
                .await;
            }
            // BUG-155 / REQ-557 BR-1: a remote route with no model does NOT
            // fall back to the provider id. That fallback was `billing_model`'s,
            // it was supposed to be deleted rather than relocated, and it was
            // live: a provider the router deliberately refused to register
            // could still be reached through `default_provider`, through a
            // policy `fallback_id`, or through `config/set register_provider` —
            // and this then put the provider's own id on the wire as the model,
            // billed it, and named it in `teton cost` as a model needing a
            // price. The route not carrying a model means no usable provider
            // was selected, which is exactly `NoTierAvailable`'s meaning — so
            // `attempt_source` refuses it and the user gets the sentence naming
            // the unusable provider and the `--model` remedy (BR-5).
            AttemptSource::Remote { provider, model } => (provider, model),
        };

        // Remote: build the adapter + egress choke point, then drive it.
        let caps = CapabilityProfile::from_core(provider_cfg.capabilities);
        let provider: Box<dyn Provider> = build_provider(provider_cfg, caps);

        // BR-7 / REQ-544 M-3: resolve the provider's credential from its
        // `auth_ref` and bind it to this provider's endpoint. A provider with no
        // `auth_ref` (e.g. a local mock endpoint) gets a credential-free
        // transport, exactly as before. The injected header rides only requests
        // to this endpoint's origin — never MCP, never another provider.
        let transport = build_remote_transport(provider_cfg, &self.secret_resolver)?;
        let boundaries = config.effective_boundaries();
        let mut egress = Egress::new(transport, boundaries, events.clone())
            .with_cost_meter(Arc::new(self.ledger.clone()))
            // REQ-588 BR-1/ADR-6: the user's ceiling, when they set one. Absent
            // leaves the choke point exactly as it was — no check, no pricing
            // lookup, no branch.
            .with_optional_spend_ceiling(config.cost.ceiling_micro_cents())
            .with_prompt_spend(prompt_spend.cloned());
        // REQ-562 ADR-1/ADR-2: the turn's own outbound payload is scanned here,
        // and only when the user opted in.
        if let Some(gate) = self.redaction_gate(router, config, events, session_id) {
            egress = egress.with_redaction_gate(gate);
        }

        // REQ-559 ADR-G: the effort was resolved once, at route time, by
        // `Router::effort_for`. It is READ off the route here, never recomputed
        // — the `route_decided` event already announced this exact value, and a
        // second computation is a second chance to disagree with it (AC-4).
        let mut source = RemoteProviderSource::new(
            &*provider,
            &egress,
            ProviderId::from(provider_cfg.id.as_str()),
            model,
            session_id.clone(),
            route.effective_effort(),
        );
        if let Some(ph) = phase {
            source = source.with_phase(ph);
        }
        // REQ-558 BR-11: the category the routing decision **resolved**, read
        // off the route rather than re-derived from the phase (ADR-D). Threaded
        // exactly the way the phase is, and for the same reason: without it the
        // ledger's category column is NULL for every ordinary turn — `edit`,
        // `design`, `debug`, `review` — and "what did `edit` cost me" is a
        // question a phase column cannot answer, in freeform mode most of all,
        // where there is no phase at all.
        if let Some(category) = route.resolution.as_ref().map(|r| r.category) {
            source = source.with_category(to_protocol_category(category));
        }

        let outcome = run_session_turn_with_pressure_policy(
            &mut source,
            tools,
            tool_ctx,
            gate,
            stream_events,
            ctx,
            &route.harness,
            &mut hook,
            &digest,
            &compact,
            &duties,
            pressure,
        )
        .await;

        // REQ-559 BR-12 / ADR-F: if this provider refused the effort field, the
        // source already retried once with no reasoning field — that is the
        // whole of BR-12's per-call handling. Remember it for the session so the
        // *next* call does not repeat a request known to fail.
        //
        // Read AFTER the turn and unconditionally, including on the error path:
        // a refusal happened whether or not the retried turn then succeeded for
        // some other reason, and a memo that only recorded successes would ask
        // again on the very next call.
        if source.effort_was_refused() && self.effort_refusals.mark(session_id, &provider_cfg.id) {
            // Announced once per (session, provider), like the taint pin: a
            // degradation nothing says out loud is one the user discovers as
            // "why did my effort setting stop working". The `teton effort` /
            // `/effort` surfaces carry the standing state; this is the moment it
            // changed.
            eprintln!(
                "teton: '{}' refused the reasoning-effort field; this session will \
                 send none to it (the next session tries again).",
                provider_cfg.id,
            );
        }
        outcome
    }
}
