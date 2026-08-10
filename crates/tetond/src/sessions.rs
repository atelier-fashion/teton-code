//! The session registry — the daemon's authoritative list of sessions.
//!
//! Sessions live here, in daemon-owned shared state, not in any client
//! connection. They therefore outlive the clients that create them (BR-4): a
//! client can disconnect and reconnect, or a second client can attach, and the
//! session list stays identical for everyone. This module is the skeleton's
//! session store; prompt-turn and phase-gate machinery land in later tasks.
//!
//! # The conversation lives here too (REQ-567)
//!
//! A session's [`Conversation`] — the ordered blocks the harness retained
//! across every completed turn — is canonical session state, the thing ACP says
//! a session *is*, so it belongs in the session store rather than in a sixth
//! per-session side-map beside the runtime's cross-cutting ones (`session_taint`,
//! `session_gates`, `session_user_urls`). Three properties make it safe to keep
//! it behind the same `std::sync::Mutex` as the summaries:
//!
//! - Every critical section is a vector move or a clone; **the lock is never
//!   held across a turn or an `.await`** (LESSON-448). A turn's exclusivity is
//!   carried by a [`TurnClaim`] — a flag on the record, released on drop — not
//!   by a held lock.
//! - A commit is a **whole-vector replacement** (BR-6): there is no partial
//!   write for a failed turn to leave behind.
//! - The store is keyed by session and read only by that session's dispatch, so
//!   BR-2's isolation is structural rather than checked.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use teton_protocol::methods::SessionSummary;
use teton_protocol::{Phase, SessionId, SessionMode, TurnId};

use crate::harness::context::ContextBlock;

/// One session as the registry holds it: what clients see, plus what only the
/// registry needs to know.
///
/// [`SessionSummary`] is a **wire type** — it is what `session/list` returns — so
/// bookkeeping that exists to stop the daemon spending money twice does not
/// belong on it. `title_attempted` is exactly that: a fact about what the daemon
/// has already tried, not a fact about the session a client is looking at.
struct SessionRecord {
    summary: SessionSummary,
    /// Whether this session's **one** `title` duty attempt has been claimed
    /// (REQ-561 TASK-062).
    ///
    /// Separate from `summary.title.is_some()` on purpose, and the separation is
    /// the whole cost argument. "Name a session that has no title" and "name a
    /// session that has not been named yet" differ only on the sessions where
    /// the duty *failed* — and on those, the first rule fires again on every
    /// subsequent turn, which turns the cheapest category in the table into a
    /// per-turn model call precisely on the machines where the calls are already
    /// not working. So the attempt is spent when it is made, not when it
    /// succeeds.
    title_attempted: bool,
    /// What this session has said so far (REQ-567 BR-1) — replayed into the next
    /// turn's [`ContextManager`](crate::harness::context::ContextManager) and
    /// replaced wholesale when that turn completes.
    conversation: Conversation,
    /// The turn currently holding this session, if any (REQ-567 BR-5).
    ///
    /// Named rather than a bare `bool` because the refusal has to say *which*
    /// turn is in flight: a busy error that cannot name its cause is the generic
    /// turn failure LESSON-456 is about.
    in_flight_turn: Option<TurnId>,
}

/// One session's conversation: the ordered blocks the harness retained, turn
/// after turn (REQ-567 BR-1).
///
/// The blocks are stored **exactly as the harness kept them** — post containment
/// cut (BUG-147), post compaction — with the per-block role and egress
/// provenance the [`ContextBlock`] already carries. There is no parallel
/// conversation type: a second spelling of "who said this and where did it come
/// from" is how one of them ends up laxer than the other at the egress choke
/// point (BR-3).
///
/// **The system head is never in here.** Heads are rebuilt per prompt from the
/// current tools and route, and a fossilized head replayed under a fresh one
/// would put two system prompts in the context and make a mid-session head
/// change unrepresentable — which is exactly what BR-7's cache-independence
/// asks for. [`ContextManager::into_blocks`](crate::harness::context::ContextManager::into_blocks)
/// excludes it by construction: the head was never a block.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    blocks: Vec<ContextBlock>,
}

impl Conversation {
    /// The blocks, in the order they happened.
    #[must_use]
    pub fn blocks(&self) -> &[ContextBlock] {
        &self.blocks
    }

    /// How many blocks the conversation holds — the count `context_cleared`
    /// reports (BR-8).
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether this session has retained anything yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// The refusal a second concurrent turn on one session gets (REQ-567 BR-5, D-3).
///
/// Typed, and it names the turn already running: BR-5 requires that a refusal
/// "names its cause truthfully rather than surfacing as a generic turn error",
/// which is LESSON-456's rule applied to a state a client can legitimately hit
/// and legitimately retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TurnClaimError {
    /// Another turn holds this session. The conversation is linear by refusing,
    /// not by interleaving (D-3).
    #[error(
        "session {session_id} is already running turn {turn_id}; \
         one session runs one turn at a time — retry when it finishes"
    )]
    InFlight {
        /// The session that is busy.
        session_id: SessionId,
        /// The turn holding it.
        turn_id: TurnId,
    },
    /// There is no such session to claim.
    ///
    /// A second variant rather than a granted claim over nothing: a claim that
    /// guards no record would let a turn run — and later commit — against a
    /// session the registry does not have, which is a silent no-op dressed as a
    /// turn. Callers reach this only if a session vanishes between lookup and
    /// claim, which nothing does today.
    #[error("no such session {session_id}")]
    NoSuchSession {
        /// The id that matched no record.
        session_id: SessionId,
    },
}

/// Exclusive hold on one session's conversation for the length of one turn
/// (REQ-567 BR-5, D-3).
///
/// **Released by `Drop`, never by an explicit call** — the discipline
/// [`crate::lifetime`] and [`crate::model_consent`]'s `InFlightGuard` already
/// keep, for the same reason: a turn can panic, and the server aborts a turn's
/// task outright when its client disconnects (`server.rs`). Either of those
/// leaking a claim would wedge the session so that every later prompt on it is
/// refused for a turn that no longer exists.
///
/// It holds a registry *handle*, not a lock guard: the mutex is taken for the
/// two vector writes and released immediately, so nothing here blocks the async
/// path for the length of a turn (LESSON-448).
pub struct TurnClaim {
    sessions: Arc<Mutex<Vec<SessionRecord>>>,
    session_id: SessionId,
    turn_id: TurnId,
}

impl TurnClaim {
    /// The turn this claim was taken for.
    #[must_use]
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// The session it holds.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Prints what the claim *is* — which session, which turn — and not the
/// registry handle it releases into.
///
/// Deriving would print every session's conversation through the handle, which
/// is prompt text and tool output in whatever log the claim was formatted into
/// (conventions: no file content or prompt text in logs).
impl fmt::Debug for TurnClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnClaim")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .finish_non_exhaustive()
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        let Ok(mut sessions) = self.sessions.lock() else {
            // A poisoned registry means some other critical section panicked;
            // there is nothing useful to release into and unwinding here would
            // replace that panic with this one.
            return;
        };
        let Some(record) = sessions
            .iter_mut()
            .find(|record| record.summary.session_id == self.session_id)
        else {
            return;
        };
        // Only ever release *this* claim. Two claims cannot coexist today, so
        // the comparison is a guard against a future that changes that rather
        // than a live case — but "the guard released whatever it found" is the
        // shape that turns such a change into a silent double-admission.
        if record.in_flight_turn.as_ref() == Some(&self.turn_id) {
            record.in_flight_turn = None;
        }
    }
}

/// A thread-safe registry of live sessions, newest tracked last.
///
/// **`Clone` yields another handle to the *same* registry**, not a copy of it —
/// the state is behind an `Arc`, in the shape a `tokio::sync` primitive or a
/// `reqwest::Client` uses. That exists so work which outlives a request handler
/// can still write back: the `title` duty (REQ-561 TASK-062) is detached from
/// the turn that triggers it precisely so the turn does not wait on it, and a
/// detached task needs an owned `'static` handle rather than a borrow of the
/// daemon's field.
#[derive(Clone)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<Vec<SessionRecord>>>,
    counter: Arc<AtomicU64>,
}

impl SessionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Creates a session and returns its summary.
    ///
    /// Structured sessions are pinned to a phase; freeform sessions carry no
    /// phase regardless of any phase passed in (BR-3). `cwd` is the client's
    /// working directory — the session's tool jail (BUG-147); `None` falls back
    /// to the daemon's env-derived root at turn time.
    ///
    /// # Errors
    ///
    /// Returns an error message when a structured session is requested without
    /// a starting phase (the protocol requires one).
    pub fn create(
        &self,
        mode: SessionMode,
        phase: Option<Phase>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionSummary, &'static str> {
        let phase = match mode {
            SessionMode::Structured => match phase {
                Some(phase) => Some(phase),
                None => return Err("structured session requires a starting phase"),
            },
            SessionMode::Freeform => None,
        };

        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let summary = SessionSummary {
            session_id: SessionId::from(format!("sess-{n}")),
            mode,
            phase,
            title: None,
            cwd,
        };
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .push(SessionRecord {
                summary: summary.clone(),
                title_attempted: false,
                conversation: Conversation::default(),
                in_flight_turn: None,
            });
        Ok(summary)
    }

    /// Every live session, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<SessionSummary> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .rev()
            .map(|record| record.summary.clone())
            .collect()
    }

    /// Looks up a session by id.
    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<SessionSummary> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map(|record| record.summary.clone())
    }

    /// Claim this session's **one** `title` duty attempt (REQ-561 TASK-062).
    ///
    /// Returns `true` at most once per session, and only while the session has
    /// no title. The caller runs the duty exactly when this says `true`; every
    /// later turn of the same session gets `false` and issues no model call,
    /// which is how AC-6's "requested exactly once" is a property of the
    /// registry rather than of every call site remembering.
    ///
    /// ## The claim is taken *before* the call, not after it
    ///
    /// A guard keyed only on `title.is_none()` would re-fire on every turn of
    /// every session whose duty failed — an unresolvable binding, a local engine
    /// that will not load, a model that answered with nothing. That is the
    /// cheapest category in the table becoming a per-turn model call on exactly
    /// the machines where the calls are not working. So the attempt is spent
    /// here, at claim time; a failed title leaves the session unnamed, which is
    /// the state every session was in before this REQ (BR-3).
    ///
    /// ## Why it is one method and not a read plus a write
    ///
    /// Two turns of one session can be in flight at once (each `session/prompt`
    /// runs on its own task), and a `has_title()` followed by a `mark()` would
    /// let both read `false` and both call the model. The check and the mark
    /// happen under one lock, so the claim is genuinely exclusive.
    pub fn claim_title(&self, id: &SessionId) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if record.title_attempted || record.summary.title.is_some() {
            return false;
        }
        record.title_attempted = true;
        true
    }

    /// Give the session named by `id` its title, if it does not already have one.
    ///
    /// Returns whether the title landed. **An existing title is never
    /// overwritten** (REQ-561 BR-9): a session is a thing a person has already
    /// learned to recognize by name, and a second derivation that renamed it
    /// would move it in their list for no reason they asked for. The guard is
    /// keyed on `title.is_none()` and lives here rather than at the call site,
    /// so a future second caller inherits it instead of having to remember it.
    ///
    /// A `false` return is also what keeps `session_titled` honest: the caller
    /// publishes only when the title actually landed, so the event stream
    /// carries at most one naming per session (AC-15).
    pub fn set_title(&self, id: &SessionId, title: &str) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return false;
        };
        if record.summary.title.is_some() {
            return false;
        }
        record.summary.title = Some(title.to_owned());
        true
    }

    /// This session's retained conversation, as the next turn replays it
    /// (REQ-567 BR-1).
    ///
    /// A clone rather than a borrow, deliberately: the caller replays these
    /// blocks into a `ContextManager` and then runs a whole turn against it, and
    /// handing out a borrow would mean handing out the lock for that long
    /// (LESSON-448). The copy costs one pass over the retained blocks per prompt,
    /// against a turn that is about to spend seconds in a model.
    ///
    /// A session the registry does not have snapshots as **empty**, which is the
    /// honest answer — a conversation that does not exist has said nothing — and
    /// keeps the caller's seeding path free of a "session vanished" branch it
    /// cannot act on anyway.
    #[must_use]
    pub fn conversation_snapshot(&self, id: &SessionId) -> Vec<ContextBlock> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|record| &record.summary.session_id == id)
            .map(|record| record.conversation.blocks.clone())
            .unwrap_or_default()
    }

    /// Replace this session's conversation with `blocks` — **the whole vector**,
    /// which is the atomic unit of REQ-567 BR-6.
    ///
    /// ## Why replacement and never an append
    ///
    /// What the turn's manager holds when the turn ends *is* the retained view:
    /// the model text as the containment cut kept it (BUG-147), the blocks a
    /// mid-turn compaction rewrote (BR-4), the tool results as they folded in.
    /// Committing that vector is a move, not a re-derivation — so the store can
    /// never disagree with the harness about what the conversation is. An append
    /// API would have to be told which blocks are new, and a compaction that
    /// rewrote the history it is appending to has no such answer.
    ///
    /// ## Rollback is by omission, not by undo
    ///
    /// This is the **only** writer, so a turn that fails simply never calls it
    /// and the pre-turn vector stands untouched (BR-6/AC-5). There is no partial
    /// state to roll back because there was never a partial write; the atomicity
    /// is a property of the API's shape rather than of a call site remembering to
    /// restore something.
    ///
    /// A commit for a session the registry does not have stores nothing — there
    /// is no record to hold it, and inventing one would resurrect a session
    /// whose id nothing will ever look up again.
    pub fn commit_conversation(&self, id: &SessionId, blocks: Vec<ContextBlock>) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        if let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        {
            record.conversation.blocks = blocks;
        }
    }

    /// Empty this session's conversation and report how many blocks went
    /// (REQ-567 BR-8).
    ///
    /// The count is the `context_cleared` payload — a clear that reported
    /// nothing would leave the user unable to tell "cleared a long session" from
    /// "cleared a session that was already empty", and those are the two things
    /// they are most likely to want to know.
    ///
    /// **Only the conversation** (OQ-4): session taint, the user-pasted-URL set,
    /// and remembered permission grants all survive, because a routinely-typed
    /// clear must never silently widen egress or consent (LESSON-495).
    pub fn clear_conversation(&self, id: &SessionId) -> usize {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return 0;
        };
        let dropped = record.conversation.blocks.len();
        record.conversation.blocks.clear();
        dropped
    }

    /// Claim this session for `turn_id`, or refuse and name the turn already
    /// running (REQ-567 BR-5, D-3).
    ///
    /// Two `session/prompt` calls on one session can be in flight at once — each
    /// runs on its own task — and both replaying the same snapshot and then both
    /// committing would fork the conversation: the second commit would erase the
    /// first turn's blocks wholesale. This is the gate that makes the transcript
    /// linear, and it refuses rather than queues (D-3): a refusal is immediate
    /// and retryable where a queue is silent and unbounded, and the CLI prompter
    /// is sequential, so a single well-behaved client never sees it.
    ///
    /// ## The check and the mark are one lock, for `claim_title`'s reason
    ///
    /// A `is_busy()` followed by a `mark_busy()` would let both turns read
    /// "free" and both proceed — the exact race this exists to close. So the
    /// read and the write happen under one lock and the claim is genuinely
    /// exclusive.
    ///
    /// ## The lock is not what is held for the turn
    ///
    /// The returned [`TurnClaim`] holds a flag on the record, released on drop,
    /// while the mutex is released before this function returns. Holding the
    /// registry lock across a turn would block every other session's `list`,
    /// `get`, and title claim on one model call (LESSON-448).
    ///
    /// # Errors
    ///
    /// [`TurnClaimError::InFlight`] when another turn holds the session, and
    /// [`TurnClaimError::NoSuchSession`] when there is no record to claim.
    pub fn try_begin_turn(
        &self,
        id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<TurnClaim, TurnClaimError> {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry mutex poisoned");
        let Some(record) = sessions
            .iter_mut()
            .find(|record| &record.summary.session_id == id)
        else {
            return Err(TurnClaimError::NoSuchSession {
                session_id: id.clone(),
            });
        };
        if let Some(in_flight) = &record.in_flight_turn {
            return Err(TurnClaimError::InFlight {
                session_id: id.clone(),
                turn_id: in_flight.clone(),
            });
        }
        record.in_flight_turn = Some(turn_id.clone());
        Ok(TurnClaim {
            sessions: Arc::clone(&self.sessions),
            session_id: id.clone(),
            turn_id: turn_id.clone(),
        })
    }

    /// Number of live sessions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .len()
    }

    /// Whether the registry holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_session_requires_a_phase() {
        let reg = SessionRegistry::new();
        assert!(reg.create(SessionMode::Structured, None, None).is_err());

        let s = reg
            .create(SessionMode::Structured, Some(Phase::Spec), None)
            .unwrap();
        assert_eq!(s.mode, SessionMode::Structured);
        assert_eq!(s.phase, Some(Phase::Spec));
    }

    #[test]
    fn freeform_session_never_carries_a_phase() {
        let reg = SessionRegistry::new();
        let s = reg
            .create(SessionMode::Freeform, Some(Phase::Spec), None)
            .unwrap();
        assert_eq!(s.phase, None);
    }

    #[test]
    fn list_is_newest_first_and_get_finds_by_id() {
        let reg = SessionRegistry::new();
        assert!(reg.is_empty());

        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, b.session_id);
        assert_eq!(list[1].session_id, a.session_id);

        assert_eq!(reg.get(&a.session_id).unwrap().session_id, a.session_id);
        assert!(reg.get(&SessionId::from("does-not-exist")).is_none());
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn a_session_remembers_its_cwd() {
        // BUG-147: the cwd is the session's tool jail — it must survive from
        // session/create through get() to the prompt turn.
        let reg = SessionRegistry::new();
        let s = reg
            .create(
                SessionMode::Freeform,
                None,
                Some(PathBuf::from("/Users/dev/my-repo")),
            )
            .unwrap();
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/Users/dev/my-repo"))
        );
        assert_eq!(
            reg.get(&s.session_id).unwrap().cwd,
            Some(PathBuf::from("/Users/dev/my-repo"))
        );
        // A client that sends none stores none (daemon-root fallback applies).
        let bare = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert_eq!(bare.cwd, None);
    }

    #[test]
    fn session_ids_are_unique() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert_ne!(a.session_id, b.session_id);
    }

    // -- the `title` duty's once-only guard (REQ-561 TASK-062) ---------------

    /// **AC-6 at its source.** The claim is granted exactly once, however many
    /// turns a session runs — and the second caller is refused *before* it can
    /// reach a model, which is what makes "requested once" a property of the
    /// registry rather than of every call site remembering.
    #[test]
    fn a_session_grants_its_title_attempt_exactly_once() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&s.session_id), "the first turn claims it");
        for turn in 2..=5 {
            assert!(
                !reg.claim_title(&s.session_id),
                "turn {turn} claimed a second title attempt"
            );
        }
    }

    /// **The cost trap, at the layer that decides it.** A claim that is spent and
    /// then *fails* leaves the session unnamed — and must not be handed back on
    /// the next turn. Keying the guard on `title.is_none()` alone would do
    /// exactly that, and would keep doing it for the life of the session.
    #[test]
    fn a_claim_that_produced_no_title_is_not_granted_again() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&s.session_id));
        // The duty failed: nothing was stored. The session is still untitled...
        assert_eq!(reg.get(&s.session_id).unwrap().title, None);
        // ...and that is not a reason to pay for another call.
        assert!(
            !reg.claim_title(&s.session_id),
            "a failed title must not re-fire on every subsequent turn"
        );
    }

    /// A session that already carries a title is never a candidate at all, so a
    /// restored or client-supplied name costs nothing to keep.
    #[test]
    fn a_session_that_already_has_a_title_claims_nothing() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert!(reg.set_title(&s.session_id, "Already named"));

        assert!(
            !reg.claim_title(&s.session_id),
            "a titled session must not buy a naming call"
        );
    }

    /// **BR-9.** An existing title is never overwritten, and the caller can tell:
    /// the `false` return is what stops a second `session_titled` reaching the
    /// wire.
    #[test]
    fn an_existing_title_is_never_overwritten() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.set_title(&s.session_id, "Retry the download client"));
        assert!(!reg.set_title(&s.session_id, "Something else entirely"));
        assert_eq!(
            reg.get(&s.session_id).unwrap().title.as_deref(),
            Some("Retry the download client")
        );
        // And the stored title is what `session/list` shows, not a second field.
        assert_eq!(
            reg.list()[0].title.as_deref(),
            Some("Retry the download client")
        );
    }

    /// A session the registry never had claims nothing and stores nothing —
    /// there is no record to spend or to name.
    #[test]
    fn an_unknown_session_neither_claims_nor_stores_a_title() {
        let reg = SessionRegistry::new();
        let ghost = SessionId::from("never-created");
        assert!(!reg.claim_title(&ghost));
        assert!(!reg.set_title(&ghost, "A name for nothing"));
    }

    /// Two sessions are two claims: the guard is per session, not per daemon.
    #[test]
    fn each_session_gets_its_own_title_claim() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        assert!(reg.claim_title(&a.session_id));
        assert!(reg.claim_title(&b.session_id));
        assert!(!reg.claim_title(&a.session_id));
    }

    // -- the session's conversation (REQ-567 TASK-092) ------------------------

    use crate::harness::context::{BlockRole, Provenance, ToolProvenance};

    fn user_block(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::User,
            text: text.to_owned(),
            provenance: Provenance::User,
        }
    }

    fn model_block(text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Assistant,
            text: text.to_owned(),
            provenance: Provenance::Model,
        }
    }

    fn tool_block(tool: &str, path: &str, text: &str) -> ContextBlock {
        ContextBlock {
            role: BlockRole::Tool,
            text: text.to_owned(),
            provenance: Provenance::Tool {
                tool: tool.to_owned(),
                provenance: ToolProvenance::path(path),
            },
        }
    }

    fn texts(blocks: &[ContextBlock]) -> Vec<&str> {
        blocks.iter().map(|b| b.text.as_str()).collect()
    }

    /// **BR-1 at its source.** A session starts with nothing to say, and what a
    /// turn commits is exactly what the next turn snapshots — same blocks, same
    /// order, same roles and provenance.
    #[test]
    fn what_a_turn_commits_is_what_the_next_turn_snapshots() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        assert!(
            reg.conversation_snapshot(&s.session_id).is_empty(),
            "a new session has said nothing"
        );

        let committed = vec![
            user_block("what does sessions.rs do?"),
            model_block("it is the daemon's session store"),
            tool_block("read", "crates/tetond/src/sessions.rs", "//! The session…"),
        ];
        reg.commit_conversation(&s.session_id, committed.clone());

        assert_eq!(reg.conversation_snapshot(&s.session_id), committed);
    }

    /// **BR-6, both halves, in the one place that can hold them.** A turn mutates
    /// a snapshot it took — that is what a turn *is* — and none of it reaches the
    /// registry until the turn commits. A turn that fails never commits, so the
    /// next prompt sees the pre-turn conversation byte for byte (AC-5).
    ///
    /// And the commit that does land is a **whole-vector replacement**, not an
    /// append: it is the manager's post-turn view, which a mid-turn compaction is
    /// free to have rewritten.
    #[test]
    fn only_a_commit_changes_the_conversation_and_it_replaces_the_whole_vector() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        let before = vec![user_block("prompt one"), model_block("answer one")];
        reg.commit_conversation(&s.session_id, before.clone());

        // A turn that errors: it read the conversation, grew it, and died.
        let mut failed_turn = reg.conversation_snapshot(&s.session_id);
        failed_turn.push(user_block("prompt two"));
        failed_turn.push(tool_block("read", "a.rs", "half a file"));
        drop(failed_turn);

        assert_eq!(
            reg.conversation_snapshot(&s.session_id),
            before,
            "a turn that never committed left blocks behind"
        );

        // A turn that completes, having compacted its history mid-turn: the
        // vector it commits does not contain the blocks it replaced.
        let compacted = vec![
            tool_block("compact", "a.rs", "[earlier conversation compacted]"),
            user_block("prompt two"),
            model_block("answer two"),
        ];
        reg.commit_conversation(&s.session_id, compacted.clone());

        assert_eq!(
            reg.conversation_snapshot(&s.session_id),
            compacted,
            "a commit must replace the conversation, never extend it"
        );
    }

    /// **BR-8.** Clearing empties the conversation and reports what went — the
    /// number `context_cleared` carries — and clearing again reports nothing,
    /// because nothing was there.
    #[test]
    fn clearing_empties_the_conversation_and_reports_what_went() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        reg.commit_conversation(
            &s.session_id,
            vec![
                user_block("one"),
                model_block("two"),
                tool_block("read", "a.rs", "three"),
            ],
        );

        assert_eq!(reg.clear_conversation(&s.session_id), 3);
        assert!(reg.conversation_snapshot(&s.session_id).is_empty());
        assert_eq!(
            reg.clear_conversation(&s.session_id),
            0,
            "clearing an empty conversation drops nothing"
        );
    }

    /// **BR-2 / AC-12.** Two sessions interleaving their turns each carry only
    /// their own blocks. The isolation is the key, not a filter someone applies:
    /// a snapshot reads one record.
    #[test]
    fn interleaved_sessions_never_see_each_others_blocks() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        reg.commit_conversation(&a.session_id, vec![user_block("A's secret plan")]);
        reg.commit_conversation(&b.session_id, vec![user_block("B's unrelated bug")]);
        reg.commit_conversation(
            &a.session_id,
            vec![user_block("A's secret plan"), model_block("A's answer")],
        );

        assert_eq!(
            texts(&reg.conversation_snapshot(&a.session_id)),
            ["A's secret plan", "A's answer"]
        );
        assert_eq!(
            texts(&reg.conversation_snapshot(&b.session_id)),
            ["B's unrelated bug"]
        );
        // And clearing one session leaves the other's conversation alone.
        assert_eq!(reg.clear_conversation(&a.session_id), 2);
        assert_eq!(
            texts(&reg.conversation_snapshot(&b.session_id)),
            ["B's unrelated bug"]
        );
    }

    /// **BR-5 / D-3.** One session runs one turn: the second claim is refused
    /// while the first is live, and the refusal names the turn holding it rather
    /// than surfacing as a generic failure (LESSON-456). When the claim drops —
    /// including on an aborted or panicking turn, which is the whole reason it is
    /// a guard — the session is admitted again.
    #[test]
    fn a_second_turn_is_refused_while_one_is_in_flight_and_admitted_after_it_drops() {
        let reg = SessionRegistry::new();
        let s = reg.create(SessionMode::Freeform, None, None).unwrap();
        let first = TurnId::from("turn-1");
        let second = TurnId::from("turn-2");

        let claim = reg.try_begin_turn(&s.session_id, &first).unwrap();
        assert_eq!(claim.turn_id(), &first);
        assert_eq!(claim.session_id(), &s.session_id);

        let refused = reg.try_begin_turn(&s.session_id, &second).unwrap_err();
        assert_eq!(
            refused,
            TurnClaimError::InFlight {
                session_id: s.session_id.clone(),
                turn_id: first.clone(),
            }
        );
        assert!(
            refused.to_string().contains("turn-1"),
            "the refusal must name the in-flight turn, not just report busy: {refused}"
        );

        drop(claim);
        let readmitted = reg
            .try_begin_turn(&s.session_id, &second)
            .expect("a released session must admit the next turn");
        assert_eq!(readmitted.turn_id(), &second);
    }

    /// The claim is per session, like the title claim: one busy session does not
    /// stop the daemon's other sessions from running turns (BR-2).
    #[test]
    fn each_session_is_claimed_independently() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None, None).unwrap();

        let _a_turn = reg.try_begin_turn(&a.session_id, &TurnId::from("turn-1"));
        assert!(reg
            .try_begin_turn(&b.session_id, &TurnId::from("turn-2"))
            .is_ok());
        assert!(reg
            .try_begin_turn(&a.session_id, &TurnId::from("turn-3"))
            .is_err());
    }

    /// A session the registry never had carries no conversation and grants no
    /// claim — and says which of the two failures it is, rather than handing back
    /// a claim over nothing.
    #[test]
    fn an_unknown_session_carries_no_conversation_and_claims_no_turn() {
        let reg = SessionRegistry::new();
        let ghost = SessionId::from("never-created");

        assert!(reg.conversation_snapshot(&ghost).is_empty());
        reg.commit_conversation(&ghost, vec![user_block("into the void")]);
        assert!(
            reg.conversation_snapshot(&ghost).is_empty(),
            "a commit must not resurrect a session that does not exist"
        );
        assert_eq!(reg.clear_conversation(&ghost), 0);
        assert_eq!(
            reg.try_begin_turn(&ghost, &TurnId::from("turn-1"))
                .unwrap_err(),
            TurnClaimError::NoSuchSession {
                session_id: ghost.clone(),
            }
        );
    }
}
