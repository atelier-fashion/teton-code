//! The commit protocol that carries one session's conversation from prompt to
//! prompt (REQ-567 D-1).
//!
//! One type — [`CarriedTurn`] — owns the whole of it: seed a fresh
//! [`ContextManager`] from what the session has retained, hold it for the turn,
//! and decide on exactly one of three outcomes what the session keeps.
//!
//! It lives in its own module, above [`crate::runtime`]'s dispatch and reachable
//! from an integration test, for a reason LESSON-451 records: the acceptance
//! fixture for carry used to re-implement the seed/commit sequence by hand, so a
//! dispatch that stopped seeding left the fixture green. There is now one
//! implementation and both callers consume it, which makes that mutation
//! impossible to hide.

use std::io::Write;
use std::sync::Arc;

use teton_core::entities::PrivacyBoundary;
use teton_protocol::SessionId;

use crate::harness::context::{BlockRole, ContextManager, Provenance, RetainedContext};
use crate::harness::reply::prose_before_tool_call;
use crate::harness::turn_loop::HarnessConfig;
use crate::runtime::{context_is_sensitive, taint_pin_line, SessionTaint, TAINT_BY_CONTEXT};
use crate::sessions::SessionRegistry;

/// One turn's [`ContextManager`], plus the promise to write what it holds back
/// to the session's conversation (REQ-567 D-1).
///
/// The manager is owned here rather than beside the guard because the third
/// outcome — cancellation — has no code of its own to run. A prompt's task is
/// drained when its client disconnects and *aborted* if it outlasts the drain
/// (`server.rs`, REQ-565), and a panicking turn unwinds; in both cases the
/// future stops without reaching either the success or the failure branch, and
/// `Drop` is the only thing that executes. A guard that merely *watched* a
/// manager it did not own would be dropped alongside blocks it could no longer
/// read.
///
/// So the outcomes are:
///
/// - **completed** ([`Self::commit`]): disarm, then replace the session's
///   conversation with what the manager holds. That vector *is* the retained
///   view — post containment cut (BUG-147), post compaction (BR-4) — so the
///   commit is a move rather than a re-derivation.
/// - **failed** ([`Self::abandon`]): disarm and write nothing. Commit is the
///   only writer, so a failed turn rolls back by never having written (BR-6).
/// - **cancelled** (armed [`Drop`], not panicking): commit what the manager
///   holds, minus a tool call that was parked and never dispatched (OQ-1 — see
///   [`Self::commit_now`]).
/// - **panicked** (armed [`Drop`] while unwinding): write nothing. A panicking
///   turn is a *failed* turn, and BR-6 says a failed turn leaves no trace.
///
/// It holds a registry *handle*, never a lock: the mutex is taken for the one
/// vector write and released, so nothing here blocks the async path for the
/// length of a turn (LESSON-448).
pub struct CarriedTurn {
    /// The turn's context. `Some` for the guard's whole life; taken only by the
    /// method (or the `Drop`) that consumes it, so the two can never both write.
    ctx: Option<ContextManager>,
    sessions: SessionRegistry,
    session_id: SessionId,
    /// The session-taint set this turn's context may have to pin (REQ-544 C-2).
    ///
    /// Held here rather than evaluated by the caller because the evaluation has
    /// to happen wherever the commit does, and one of the commits is a `Drop`
    /// with no caller to run it. See [`Self::commit_now`].
    taint: Arc<SessionTaint>,
    /// The boundaries the pin is measured against, as this turn's config
    /// declared them.
    boundaries: Vec<PrivacyBoundary>,
    /// Whether a drop from here would be a cancellation. Set once the new user
    /// message is in the manager and cleared by whichever outcome arrives first.
    armed: bool,
}

impl CarriedTurn {
    /// Seed a turn from what `session_id` has retained and arm the commit
    /// (REQ-567 BR-1).
    ///
    /// `system` is **this** turn's head, rebuilt from the current tools and
    /// route by the caller; the carried blocks are replayed under it, so a
    /// mid-session head change re-renders the same conversation rather than
    /// fossilizing an old head (BR-7). The new user message goes in last,
    /// because the transcript is in turn order and this prompt is the newest
    /// thing in it — and the arming happens after it, so a cancelled turn
    /// retains the message the user sent (OQ-1).
    ///
    /// The budgets come from `harness` — the route's own degradation-derived
    /// profile — rather than as two loose numbers, so a turn on a weak tier is
    /// seeded against the window that tier actually has (BR-4). It is the same
    /// value the turn loop is then run under.
    ///
    /// This is the **only** seeding path. `run_prompt_turn` calls it and so does
    /// the acceptance fixture, which is what stops the fixture from drifting
    /// into an agreeing re-implementation of a dispatch that has changed
    /// (LESSON-451).
    #[must_use]
    pub fn begin(
        sessions: &SessionRegistry,
        session_id: &SessionId,
        system: impl Into<String>,
        harness: &HarnessConfig,
        taint: Arc<SessionTaint>,
        boundaries: Vec<PrivacyBoundary>,
        prompt: impl Into<String>,
    ) -> Self {
        let mut ctx = ContextManager::new(system, harness.context_budget_tokens)
            .with_budget_bytes(harness.context_budget_bytes);
        ctx.replay(sessions.conversation_snapshot(session_id).into_retained());
        ctx.push_user(prompt);
        Self {
            ctx: Some(ctx),
            sessions: sessions.clone(),
            session_id: session_id.clone(),
            taint,
            boundaries,
            armed: true,
        }
    }

    /// The turn's context.
    ///
    /// # Panics
    ///
    /// If the context has already been consumed by a commit, which cannot happen
    /// while the caller still holds the guard.
    #[must_use]
    pub fn ctx(&self) -> &ContextManager {
        self.ctx.as_ref().expect("the turn context was taken")
    }

    /// The turn's context, mutably — what the turn loop appends to.
    ///
    /// # Panics
    ///
    /// As [`Self::ctx`].
    pub fn ctx_mut(&mut self) -> &mut ContextManager {
        self.ctx.as_mut().expect("the turn context was taken")
    }

    /// The turn completed: the conversation becomes what the manager holds.
    pub fn commit(mut self) {
        self.armed = false;
        self.commit_now(false, false);
    }

    /// The turn failed: the conversation stays exactly as the turn found it.
    pub fn abandon(mut self) {
        self.armed = false;
    }

    /// The one write, shared by the explicit commit and the armed `Drop`
    /// (REQ-567 verify: single writer).
    ///
    /// Two things happen here and both have to happen on **every** path that
    /// writes, which is exactly why they live here rather than at a call site:
    ///
    /// ## The taint pin (REQ-544 C-2)
    ///
    /// A turn whose context intersects a `local-only` boundary — or carries
    /// unknown provenance — pins its session to the local tier for every later
    /// turn. That is the backstop for a *later* model paraphrase of what this
    /// turn read, and it is owed by any turn whose content survives into the
    /// conversation. Evaluating it in the caller's success arm left the
    /// cancellation path unpinned while still committing the boundary content it
    /// read: the next prompt would carry the content and route remote. So the
    /// evaluation is bound to the commit, fail-closed, and a `Drop` gets it too.
    ///
    /// ## The budget (BR-4)
    ///
    /// The context is measured one last time before it is handed over, so that
    /// **a stored `Conversation` fits the budget** is an invariant of the store
    /// rather than a property the last turn happened to leave behind. The loop's
    /// own gate runs at the top of each iteration, which covers every
    /// conversation a *completed* turn commits; a cancelled one can be dropped
    /// after a fold and before the next iteration measures it, and the block it
    /// grew by would otherwise be replayed into the next prompt unmeasured.
    ///
    /// ## The undispatched-call trim (OQ-1)
    ///
    /// The loop pushes the model's text — which, on a *local* tool-calling
    /// reply, contains the call — before it awaits the permission gate. A turn
    /// cancelled while parked at that gate therefore holds a trailing assistant
    /// block whose call was never answered, and committing it would leave the
    /// next prompt opening on a question the transcript never resolves. OQ-1's
    /// product decision is "retain prose, drop incomplete tool work", so on the
    /// cancellation path the call is cut off the end of that block
    /// ([`prose_before_tool_call`]); what the model wrote *before* the call is
    /// completed prose and stays, and a block left with nothing but the call is
    /// dropped whole.
    ///
    /// ### It fires only where the loop says there is a call to cut
    ///
    /// The trim is gated on
    /// [`ContextManager::pending_tool_call`](crate::harness::context::ContextManager::pending_tool_call),
    /// which the turn loop sets when it pushes a block whose text embeds an
    /// undispatched call and clears the moment the tool returns. It is
    /// deliberately **not** re-derived here by asking whether the text contains
    /// something call-shaped, because that question has two wrong answers:
    ///
    /// - A turn's prose may *quote* something call-shaped —
    ///   `{"name": "serde", "version": "1"}` — and a remote provider's real
    ///   call is rendered onto the end of exactly such prose (BUG-178). Reading
    ///   the first call-shaped object as "the call" would truncate a cancelled
    ///   remote turn at the quote, discarding it and everything after — content
    ///   the user watched stream. The trim therefore cuts only the **trailing**
    ///   call ([`prose_before_tool_call`]), which is where the loop puts it for
    ///   every source.
    /// - A call whose tool **already ran** is not incomplete work. A
    ///   cancellation landing in the refine or digest awaits that follow
    ///   dispatch commits the call block as it stands: an `edit` that reached
    ///   the disk reached it, and a conversation that denies having asked for it
    ///   is a worse trace than one holding a call whose result never arrived.
    ///   Such a conversation can therefore carry a dispatched-but-unfolded call
    ///   — the honest record of exactly what happened. OQ-1's "incomplete tool
    ///   work" means work that never ran.
    ///
    /// The committed conversation therefore ends at the last complete exchange —
    /// possibly on the user's own message, which is correct and stays: the user
    /// really did send it, and dropping it would make the next prompt's context
    /// disagree with the transcript the client already rendered.
    ///
    /// `panicking` is the one state that writes nothing: a panicking turn is a
    /// failed turn (BR-6), not a cancelled one.
    fn commit_now(&mut self, cancelled: bool, panicking: bool) {
        let Some(mut ctx) = self.ctx.take() else {
            return;
        };
        if panicking {
            return;
        }
        // Read before the manager is shrunk or consumed, and before any trim:
        // the pin is about what this turn's context *was*, not about what
        // survives OQ-1's edit — a call the user never answered was still
        // assembled from, and shown, whatever the conversation had read.
        //
        // `try_mark` and a swallowed write, not `mark` and `eprintln!`: this
        // runs from `Drop`, and a panic raised inside a drop that is itself
        // unwinding aborts the daemon. A poisoned taint mutex and a closed
        // stderr are both survivable; neither is worth the whole process.
        if context_is_sensitive(&ctx, &self.boundaries) && self.taint.try_mark(&self.session_id) {
            let _ = writeln!(std::io::stderr(), "{}", taint_pin_line(TAINT_BY_CONTEXT));
        }
        // BR-4, at the seam that makes it an invariant of the store rather than
        // of the last writer. Unconditional and ahead of the trim, for the same
        // reason the loop's own gate is unconditional: what enforces a budget is
        // never allowed to be skipped by a path.
        // TASK-189 returns this to the runtime, which publishes the
        // between-turns drop as `context_pressure` (BR-10): the commit runs
        // from `Drop` and holds no `SessionEvents`, so the seam re-asserts the
        // invariant and the news is published where the handle lives
        // (LESSON-501).
        let _pressure = ctx.truncate_to_budget();
        let pending_call = ctx.pending_tool_call();
        let mut retained = ctx.into_retained();
        if cancelled && pending_call {
            trim_dangling_tool_call(&mut retained);
        }
        // The non-panicking twin, because this is also the drop path: a panic
        // raised inside a drop that is running because of a panic aborts the
        // whole daemon.
        self.sessions
            .try_commit_conversation(&self.session_id, retained);
    }
}

/// Cut an undispatched tool call off the end of `retained` (REQ-567 OQ-1).
///
/// The caller has already established that there *is* one — the turn loop said
/// so when it pushed the block — so this only has to find where the call starts
/// and edit around it. Only the **last** block is touched: a call anywhere
/// earlier was answered by the tool block after it.
///
/// The role/provenance guard is a consistency check on that claim rather than
/// the decision itself, and the block's provenance is absorbed into the retained
/// [`DroppedProvenance`](crate::harness::context::DroppedProvenance) either way.
/// That absorb is a no-op for the `Model` block this always finds today, which
/// is the point: it means nothing here depends on the guard being right, so a
/// future call shape that *does* carry provenance cannot be laundered out of the
/// conversation by this edit (BR-3).
fn trim_dangling_tool_call(retained: &mut RetainedContext) {
    let mut blocks = retained.blocks().to_vec();
    let Some(last) = blocks.last_mut() else {
        return;
    };
    if last.role != BlockRole::Assistant || last.provenance != Provenance::Model {
        return;
    }
    // A trailing block whose text no longer parses as a call — a budget clamp
    // that landed in the middle of the JSON is the way this happens — is left
    // exactly as it is. There is nothing to cut around, and guessing would edit
    // prose.
    let Some(prose) = prose_before_tool_call(&last.text) else {
        return;
    };
    let prose = prose.trim_end().to_owned();
    let provenance = last.provenance.clone();
    if prose.is_empty() {
        blocks.pop();
    } else {
        last.text = prose;
    }
    retained.absorb_dropped(&provenance);
    retained.set_blocks(blocks);
}

impl Drop for CarriedTurn {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A panicking turn is a FAILED turn, and BR-6 says a failed turn leaves
        // the conversation exactly as it found it. Only a task abort — a drop
        // with no panic in flight — is the cancellation OQ-1 retains work for.
        // Without this check the two are indistinguishable from inside `Drop`,
        // and a turn that panicked halfway through assembling a tool result
        // would commit that half as though the user had walked away from it.
        let panicking = std::thread::panicking();
        self.commit_now(true, panicking);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use teton_protocol::SessionMode;

    use crate::harness::context::ContextBlock;

    fn model(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Assistant,
            text: text.to_owned(),
            provenance: Provenance::Model,
        }
    }

    fn user(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::User,
            text: text.to_owned(),
            provenance: Provenance::User,
        }
    }

    fn tool(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Tool,
            text: text.to_owned(),
            provenance: Provenance::Tool {
                tool: "read".to_owned(),
                provenance: crate::harness::context::ToolProvenance::none(),
            },
        }
    }

    fn texts(blocks: &[ContextBlock]) -> Vec<&str> {
        blocks.iter().map(|b| b.text.as_str()).collect()
    }

    /// Run the trim over `blocks` and hand back what it left.
    fn trimmed(blocks: Vec<ContextBlock>) -> Vec<ContextBlock> {
        let mut retained = RetainedContext::from_blocks(blocks);
        trim_dangling_tool_call(&mut retained);
        retained.into_blocks()
    }

    /// OQ-1's two halves in one edit: the prose the model completed before the
    /// call stays, the call goes.
    #[test]
    fn a_trailing_call_is_cut_off_the_prose_that_preceded_it() {
        let blocks = trimmed(vec![
            user("run the tests"),
            model(r#"I will run them. {"tool":"shell","arguments":{"command":"cargo test"}}"#),
        ]);
        assert_eq!(texts(&blocks), ["run the tests", "I will run them."]);
    }

    /// A reply that was nothing but the call leaves no prose behind, so the
    /// block goes with it rather than committing an empty assistant turn — and
    /// the conversation ends on the user's own message, which is correct: the
    /// user really did send it, and the client has already rendered it.
    #[test]
    fn a_bare_trailing_call_takes_its_block_with_it() {
        let blocks = trimmed(vec![
            user("run the tests"),
            model(r#"{"tool":"shell","arguments":{"command":"cargo test"}}"#),
        ]);
        assert_eq!(texts(&blocks), ["run the tests"]);
    }

    /// The structural guard, independent of the caller's flag: a trailing user
    /// or tool block, and an assistant block that is no longer last, are all
    /// complete work by construction and are left alone even if the trim is
    /// reached with them on the end.
    #[test]
    fn completed_work_survives_the_trim() {
        for blocks in [
            vec![user("hi"), model("The retry budget is three.")],
            vec![model("Done."), user("thanks")],
            vec![model(r#"{"tool":"read","arguments":{}}"#), tool("ok")],
            vec![
                user("read a.rs"),
                model(r#"{"tool":"read","arguments":{"path":"a.rs"}}"#),
                tool("fn main() {}"),
            ],
            Vec::new(),
        ] {
            assert_eq!(trimmed(blocks.clone()), blocks);
        }
    }

    // -- the gate: what a cancelled turn actually commits ---------------------

    /// A turn on a fresh session, armed exactly as dispatch arms it.
    fn begin_turn(sessions: &SessionRegistry, session_id: &SessionId) -> CarriedTurn {
        CarriedTurn::begin(
            sessions,
            session_id,
            "sys",
            &HarnessConfig::default(),
            Arc::new(SessionTaint::new()),
            Vec::new(),
            "do the thing",
        )
    }

    /// A registry holding one freeform session.
    fn one_session() -> (SessionRegistry, SessionId) {
        let sessions = SessionRegistry::new();
        let summary = sessions
            .create(SessionMode::Freeform, None, None)
            .expect("a freeform session needs no phase");
        let id = summary.session_id.clone();
        (sessions, id)
    }

    /// **The local path, at the gate.** The loop pushed the reply through
    /// `push_model_call` because the call was parsed out of that very text, and
    /// then parked. The cancellation keeps the prose and cuts the call.
    #[test]
    fn a_turn_cancelled_at_the_gate_commits_its_prose_without_the_call() {
        let (sessions, session_id) = one_session();
        {
            let mut turn = begin_turn(&sessions, &session_id);
            turn.ctx_mut().push_model_call(
                r#"Now I will run the tests. {"tool":"shell","arguments":{"command":"echo hi"}}"#,
            );
            // Dropped armed and not panicking: a cancelled turn.
        }
        let conversation = sessions.conversation_snapshot(&session_id);
        assert_eq!(
            texts(conversation.blocks()),
            ["do the thing", "Now I will run the tests."],
            "the cancelled turn did not commit prose-minus-call"
        );
    }

    /// **The remote path, at the same gate — the regression this gate exists
    /// for.** A remote provider delivers its call as a structured event; the
    /// loop renders it onto the end of the prose and pushes that as the
    /// pending block (BUG-178). The prose may itself quote tool-call-*shaped*
    /// JSON, and it is ordinary content the user watched stream: a trim that
    /// took the *first* call-shaped object for "the call" would truncate the
    /// block at the quote and discard the rest. The trailing call is what goes.
    #[test]
    fn a_cancelled_remote_turn_keeps_prose_that_merely_looks_like_a_call() {
        let (sessions, session_id) = one_session();
        const PROSE: &str =
            r#"The manifest pins {"name": "serde", "version": "1"}, which the lockfile agrees on."#;
        {
            let mut turn = begin_turn(&sessions, &session_id);
            // Exactly what the loop does for `call_in_text == false`.
            turn.ctx_mut()
                .push_model_call(crate::harness::reply::append_tool_call(
                    PROSE,
                    "read",
                    &serde_json::json!({ "path": "Cargo.toml" }),
                ));
        }
        let conversation = sessions.conversation_snapshot(&session_id);
        assert_eq!(
            texts(conversation.blocks()),
            ["do the thing", PROSE],
            "a cancelled remote turn's prose was mutilated at the JSON it quoted, or \
             committed with the call it never ran"
        );
    }

    /// **BUG-178, at the gate.** The commonest remote tool-call turn has no
    /// prose at all. Its pending block is the bare rendered call, and a
    /// cancellation drops that block whole — never a blank assistant turn,
    /// which the next request would carry to a provider that refuses it.
    #[test]
    fn a_cancelled_remote_call_with_no_prose_leaves_no_blank_turn_behind() {
        let (sessions, session_id) = one_session();
        {
            let mut turn = begin_turn(&sessions, &session_id);
            turn.ctx_mut()
                .push_model_call(crate::harness::reply::append_tool_call(
                    "",
                    "shell",
                    &serde_json::json!({ "command": "ls" }),
                ));
        }
        let conversation = sessions.conversation_snapshot(&session_id);
        assert_eq!(
            texts(conversation.blocks()),
            ["do the thing"],
            "the cancelled call left an assistant turn behind"
        );
        assert!(
            conversation.blocks().iter().all(|b| !b.text.is_empty()),
            "no committed block may be empty"
        );
    }

    /// **After dispatch.** The tool ran; the cancellation landed in the refine
    /// or digest await that follows. The call block stays as the honest trace of
    /// what happened — an edit that reached the disk is on the disk, and a
    /// conversation denying it is worse than one holding a call whose result
    /// never arrived.
    #[test]
    fn a_turn_cancelled_after_dispatch_keeps_the_call_it_actually_ran() {
        let (sessions, session_id) = one_session();
        const CALL: &str = r#"{"tool":"edit","arguments":{"path":"a.rs","new":"fn main() {}"}}"#;
        {
            let mut turn = begin_turn(&sessions, &session_id);
            turn.ctx_mut().push_model_call(CALL);
            // `tools.dispatch` returned — the tool ran.
            turn.ctx_mut().resolve_pending_call();
        }
        let conversation = sessions.conversation_snapshot(&session_id);
        assert_eq!(
            texts(conversation.blocks()),
            ["do the thing", CALL],
            "the call of a tool that actually ran was erased from the conversation"
        );
    }

    /// The three outcomes that are not a cancellation still behave: a completed
    /// turn commits the call block untouched (it is not cancelled, whatever the
    /// flag says), and a failed one commits nothing at all.
    #[test]
    fn only_a_cancellation_trims_and_only_a_commit_writes() {
        const CALL: &str = r#"Checking. {"tool":"read","arguments":{"path":"a.rs"}}"#;

        let (sessions, session_id) = one_session();
        let mut turn = begin_turn(&sessions, &session_id);
        turn.ctx_mut().push_model_call(CALL);
        turn.commit();
        assert_eq!(
            texts(sessions.conversation_snapshot(&session_id).blocks()),
            ["do the thing", CALL],
            "a completed turn's blocks must reach the store verbatim"
        );

        let (sessions, session_id) = one_session();
        let mut turn = begin_turn(&sessions, &session_id);
        turn.ctx_mut().push_model_call(CALL);
        turn.abandon();
        assert!(
            sessions.conversation_snapshot(&session_id).is_empty(),
            "a failed turn wrote to the conversation (BR-6)"
        );
    }

    /// **BR-4 as a store invariant.** A cancellation can land after a fold and
    /// before the loop's next iteration measures anything, so the commit
    /// measures. Without it the over-budget vector is replayed into the next
    /// prompt, which is the wedge BR-4 forbids.
    #[test]
    fn a_cancelled_turn_commits_a_conversation_that_fits_the_budget() {
        const BUDGET_BYTES: usize = 4_000;
        let harness = HarnessConfig {
            context_budget_bytes: BUDGET_BYTES,
            ..HarnessConfig::default()
        };
        let (sessions, session_id) = one_session();
        {
            let mut turn = CarriedTurn::begin(
                &sessions,
                &session_id,
                "sys",
                &harness,
                Arc::new(SessionTaint::new()),
                Vec::new(),
                "do the thing",
            );
            for i in 0..6 {
                turn.ctx_mut()
                    .push_model(format!("block {i} {}", "x".repeat(1_000)));
            }
            assert!(
                turn.ctx().estimated_bytes() > BUDGET_BYTES,
                "non-vacuity: the turn must be over budget, or the gate has nothing to do"
            );
        }
        let conversation = sessions.conversation_snapshot(&session_id);
        let bytes: usize = conversation.blocks().iter().map(|b| b.text.len()).sum();
        assert!(
            bytes <= BUDGET_BYTES,
            "a cancelled turn stored {bytes} bytes of blocks against a {BUDGET_BYTES}-byte budget"
        );
        assert!(
            conversation.retained().was_truncated(),
            "the conversation was cut without the honesty note that says so"
        );
    }
}
