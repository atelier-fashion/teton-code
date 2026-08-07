//! The session registry — the daemon's authoritative list of sessions.
//!
//! Sessions live here, in daemon-owned shared state, not in any client
//! connection. They therefore outlive the clients that create them (BR-4): a
//! client can disconnect and reconnect, or a second client can attach, and the
//! session list stays identical for everyone. This module is the skeleton's
//! session store; prompt-turn and phase-gate machinery land in later tasks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use teton_protocol::methods::SessionSummary;
use teton_protocol::{Phase, SessionId, SessionMode};

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
}
