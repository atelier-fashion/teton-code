//! The session registry — the daemon's authoritative list of sessions.
//!
//! Sessions live here, in daemon-owned shared state, not in any client
//! connection. They therefore outlive the clients that create them (BR-4): a
//! client can disconnect and reconnect, or a second client can attach, and the
//! session list stays identical for everyone. This module is the skeleton's
//! session store; prompt-turn and phase-gate machinery land in later tasks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use teton_protocol::methods::SessionSummary;
use teton_protocol::{Phase, SessionId, SessionMode};

/// A thread-safe registry of live sessions, newest tracked last.
pub struct SessionRegistry {
    sessions: Mutex<Vec<SessionSummary>>,
    counter: AtomicU64,
}

impl SessionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Creates a session and returns its summary.
    ///
    /// Structured sessions are pinned to a phase; freeform sessions carry no
    /// phase regardless of any phase passed in (BR-3).
    ///
    /// # Errors
    ///
    /// Returns an error message when a structured session is requested without
    /// a starting phase (the protocol requires one).
    pub fn create(
        &self,
        mode: SessionMode,
        phase: Option<Phase>,
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
        };
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .push(summary.clone());
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
            .cloned()
            .collect()
    }

    /// Looks up a session by id.
    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<SessionSummary> {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .iter()
            .find(|s| &s.session_id == id)
            .cloned()
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
        assert!(reg.create(SessionMode::Structured, None).is_err());

        let s = reg
            .create(SessionMode::Structured, Some(Phase::Spec))
            .unwrap();
        assert_eq!(s.mode, SessionMode::Structured);
        assert_eq!(s.phase, Some(Phase::Spec));
    }

    #[test]
    fn freeform_session_never_carries_a_phase() {
        let reg = SessionRegistry::new();
        let s = reg
            .create(SessionMode::Freeform, Some(Phase::Spec))
            .unwrap();
        assert_eq!(s.phase, None);
    }

    #[test]
    fn list_is_newest_first_and_get_finds_by_id() {
        let reg = SessionRegistry::new();
        assert!(reg.is_empty());

        let a = reg.create(SessionMode::Freeform, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None).unwrap();

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, b.session_id);
        assert_eq!(list[1].session_id, a.session_id);

        assert_eq!(reg.get(&a.session_id).unwrap().session_id, a.session_id);
        assert!(reg.get(&SessionId::from("does-not-exist")).is_none());
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn session_ids_are_unique() {
        let reg = SessionRegistry::new();
        let a = reg.create(SessionMode::Freeform, None).unwrap();
        let b = reg.create(SessionMode::Freeform, None).unwrap();
        assert_ne!(a.session_id, b.session_id);
    }
}
