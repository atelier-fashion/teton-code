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

use std::sync::Arc;

use teton_core::entities::PrivacyBoundary;
use teton_protocol::SessionId;

use crate::harness::context::{BlockRole, ContextBlock, ContextManager, Provenance};
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
///   holds, minus any dangling tool call (OQ-1 — see [`Self::commit_now`]).
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
    /// ## The dangling-call trim (OQ-1)
    ///
    /// The loop pushes the model's text — which, on a tool-calling reply, *is*
    /// the call — before it awaits the permission gate. A turn cancelled while
    /// parked at that gate therefore holds a trailing assistant block containing
    /// a call that was never answered, and committing it would leave the next
    /// prompt opening on a question the transcript never resolves. OQ-1's
    /// product decision is "retain prose, drop incomplete tool work", so on the
    /// cancellation path the call is cut off the end of that block through the
    /// same parser the loop dispatches with ([`prose_before_tool_call`]); what
    /// the model wrote *before* the call is completed prose and stays, and a
    /// block left with nothing but the call is dropped whole.
    ///
    /// The committed conversation therefore ends at the last complete exchange —
    /// possibly on the user's own message, which is correct and stays: the user
    /// really did send it, and dropping it would make the next prompt's context
    /// disagree with the transcript the client already rendered.
    ///
    /// `panicking` is the one state that writes nothing: a panicking turn is a
    /// failed turn (BR-6), not a cancelled one.
    fn commit_now(&mut self, cancelled: bool, panicking: bool) {
        let Some(ctx) = self.ctx.take() else {
            return;
        };
        if panicking {
            return;
        }
        // Read before the manager is consumed, and before any trim: the pin is
        // about what this turn's context *was*, not about what survives OQ-1's
        // edit — a call the user never answered was still assembled from, and
        // shown, whatever the conversation had read.
        if context_is_sensitive(&ctx, &self.boundaries) && self.taint.mark(&self.session_id) {
            eprintln!("{}", taint_pin_line(TAINT_BY_CONTEXT));
        }
        let mut retained = ctx.into_retained();
        if cancelled {
            let mut blocks = retained.blocks().to_vec();
            if trim_dangling_tool_call(&mut blocks) {
                retained.set_blocks(blocks);
            }
        }
        // The non-panicking twin, because this is also the drop path: a panic
        // raised inside a drop that is running because of a panic aborts the
        // whole daemon.
        self.sessions
            .try_commit_conversation(&self.session_id, retained);
    }
}

/// Cut a trailing, unanswered tool call off the end of `blocks`, reporting
/// whether anything changed (REQ-567 OQ-1).
///
/// Only the **last** block is considered, and only when it is the assistant's:
/// a call anywhere earlier was answered by the tool block after it, and a
/// trailing user or tool block is complete work by construction.
fn trim_dangling_tool_call(blocks: &mut Vec<ContextBlock>) -> bool {
    let Some(last) = blocks.last_mut() else {
        return false;
    };
    if last.role != BlockRole::Assistant || last.provenance != Provenance::Model {
        return false;
    }
    let Some(prose) = prose_before_tool_call(&last.text) else {
        return false;
    };
    let prose = prose.trim_end().to_owned();
    if prose.is_empty() {
        blocks.pop();
    } else {
        last.text = prose;
    }
    true
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

    /// OQ-1's two halves in one edit: the prose the model completed before the
    /// call stays, the call goes.
    #[test]
    fn a_trailing_call_is_cut_off_the_prose_that_preceded_it() {
        let mut blocks = vec![
            user("run the tests"),
            model(r#"I will run them. {"tool":"shell","arguments":{"command":"cargo test"}}"#),
        ];
        assert!(trim_dangling_tool_call(&mut blocks));
        assert_eq!(texts(&blocks), ["run the tests", "I will run them."]);
    }

    /// A reply that was nothing but the call leaves no prose behind, so the
    /// block goes with it rather than committing an empty assistant turn — and
    /// the conversation ends on the user's own message, which is correct: the
    /// user really did send it, and the client has already rendered it.
    #[test]
    fn a_bare_trailing_call_takes_its_block_with_it() {
        let mut blocks = vec![
            user("run the tests"),
            model(r#"{"tool":"shell","arguments":{"command":"cargo test"}}"#),
        ];
        assert!(trim_dangling_tool_call(&mut blocks));
        assert_eq!(texts(&blocks), ["run the tests"]);
    }

    /// A call that was *answered* is complete work and is left alone: the tool
    /// block after it is the answer, so the assistant block is no longer last.
    #[test]
    fn an_answered_call_is_not_a_dangling_one() {
        let mut blocks = vec![
            user("read a.rs"),
            model(r#"{"tool":"read","arguments":{"path":"a.rs"}}"#),
            tool("fn main() {}"),
        ];
        assert!(!trim_dangling_tool_call(&mut blocks));
        assert_eq!(blocks.len(), 3);
    }

    /// A completed answer is not trimmed, and neither is a trailing user or
    /// tool block. Without these the trim would quietly eat the last thing a
    /// cancelled turn actually finished.
    #[test]
    fn completed_work_survives_the_trim() {
        for mut blocks in [
            vec![user("hi"), model("The retry budget is three.")],
            vec![model("Done."), user("thanks")],
            vec![model(r#"{"tool":"read","arguments":{}}"#), tool("ok")],
            Vec::new(),
        ] {
            let before = blocks.clone();
            assert!(!trim_dangling_tool_call(&mut blocks));
            assert_eq!(blocks, before);
        }
    }
}
