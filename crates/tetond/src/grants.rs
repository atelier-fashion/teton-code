//! Session grants: who may attach to a session, and who may declare `monitor`.
//!
//! REQ-569 raises session access from "ambient to the uid" to "granted". A
//! connection may attach only to a session it created or holds an
//! **attach-scope** grant for, and `monitor` — a claim over every session's
//! events — needs its own **monitor-scope** grant (BR-1/BR-2).
//!
//! ## Two halves, deliberately
//!
//! - [`may_attach`] and [`may_monitor`] are **pure**: plain functions over a
//!   connection id, a session name, the sessions that connection created, and
//!   a set of grants. That is what makes the authorization rule table-testable
//!   without a socket, a daemon, or a process — the decision is checkable
//!   directly instead of being inferred from what a handler happened to answer.
//! - [`GrantRegistry`] is the **mechanism**: the shared, locked state the
//!   daemon holds, plus the connection-id namespace. It answers by calling the
//!   pure functions above, so there is exactly one definition of the rule
//!   (LESSON-484 — enforce where the decision is made, and make that one
//!   place).
//!
//! ## Scope never leaks across (LESSON-495)
//!
//! [`GrantScope`] is part of the key, not a note attached to it: an
//! attach-scope grant for a session answers *only* "may this connection attach
//! to that session", and a monitor-scope grant answers *only* the monitor
//! question. Neither implies the other in either direction. This is the same
//! split REQ-568 already made between `may_receive` and `may_drive` — a
//! remembered grant answers every question its key matches, so the key has to
//! encode the whole question.
//!
//! ## Lifetime (ADR-C)
//!
//! A grant lives in this registry for as long as the connection that holds it,
//! and **nothing is persisted** — not to disk, not to the keychain. Persistence
//! would buy one avoided prompt and cost a stored credential whose storage
//! becomes attack surface, which is a bad trade for a control whose entire
//! purpose is that possession of local state must not confer access (BR-3).
//! [`GrantRegistry::release`] is called when a connection ends, so a
//! connection id can never be inherited along with the grants that were keyed
//! to it.
//!
//! That bounds each **entry**, not the capability. Consent arm 1 admits any
//! connection attached to the target as an approver, so a grant holder can
//! approve a second connection, leave, and hand the approver role on — the row
//! dies with its subject while the access propagates past it. Recorded honestly
//! rather than implied away: see architecture.md ADR-C's correction, open as a
//! residual owned by REQ-570.
//!
//! ## What can mint one
//!
//! Nothing here mints a grant on its own, and nothing derives one from the
//! environment, the socket path, or the filesystem (BR-3). Creating a session
//! is standing without a registry entry at all — it is the creator's own
//! [`may_attach`] argument, not a [`Grant`].
//!
//! Registry entries come from exactly **one** production caller: the
//! `session/attach` handler, on a [`crate::consent::ConsentOutcome::Granted`]
//! (`crate::server`). It mints [`GrantScope::Attach`] and nothing else.
//!
//! [`GrantScope::Monitor`] therefore has **no socket-reachable minter at all**
//! (REQ-569 verify, F1). It had one — an `attach/consent` answered by any
//! attached peer — and that was mintable by a single attacker holding two
//! connections, because `session/create` is ungated: connection A created a
//! throwaway session, which made A an attached surface, which made A the
//! approver for connection B's monitor declaration, which A answered. The path
//! is removed rather than re-predicated; `may_monitor` stays, so the scope is
//! still the whole answer to the monitor question, and the question's answer is
//! now always "no" over the socket.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use teton_protocol::SessionId;

/// Identifies one client connection for exactly as long as that connection
/// lives.
///
/// The grant *subject* (ADR-D). A connection is the right subject because it is
/// the finest-grained thing the daemon can actually attest: the peer
/// credentials are a property of the connected socket, so a connection cannot
/// hand its identity to another process the way a token or a file could. It
/// also gives grants their lifetime for free — the subject stops existing, so
/// the grant does.
///
/// Opaque on purpose: nothing outside this module may construct one from a
/// number, so a grant cannot be keyed to a connection id someone guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

/// What a grant permits. Never both, never implicitly either (LESSON-495).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrantScope {
    /// `session/attach` against the named session.
    Attach,
    /// The `monitor` declaration — sight of every session's events, present and
    /// future. Strictly broader than [`Attach`](Self::Attach), and strictly
    /// separate from it: a monitor grant confers no attach right, and no number
    /// of attach grants adds up to one.
    Monitor,
}

/// One registry entry: `connection` may do `scope` to `session_id`.
///
/// The whole struct is the key — all three fields participate in `Hash`/`Eq` —
/// which is what makes "does this connection hold *this* right over *this*
/// session" a set membership test rather than a search with a predicate someone
/// has to get right at each call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Grant {
    /// The connection the grant was minted for.
    pub connection: ConnectionId,
    /// The session it opens.
    pub session_id: SessionId,
    /// What it opens the session *for*.
    pub scope: GrantScope,
}

impl Grant {
    /// An attach-scope grant.
    #[must_use]
    pub fn attach(connection: ConnectionId, session_id: SessionId) -> Self {
        Self {
            connection,
            session_id,
            scope: GrantScope::Attach,
        }
    }

    /// A monitor-scope grant.
    #[must_use]
    pub fn monitor(connection: ConnectionId, session_id: SessionId) -> Self {
        Self {
            connection,
            session_id,
            scope: GrantScope::Monitor,
        }
    }
}

/// May `connection` attach to `session_id`? (BR-1)
///
/// Two standings, and only two: the connection created the session, or it holds
/// an attach-scope grant for it. Everything else — knowing the id, having
/// listed it, running as the same uid, sitting on the same socket — is refused,
/// which is the whole point of the REQ (BR-3/BR-8: ids are names, grants are
/// credentials).
///
/// `created` is passed in rather than stored here because it is a fact about
/// the *connection*, not about the grant registry, and duplicating it into this
/// module would create a second copy to drift out of step with the one the
/// connection already keeps.
#[must_use]
pub fn may_attach(
    connection: ConnectionId,
    session_id: &SessionId,
    created: &HashSet<SessionId>,
    grants: &HashSet<Grant>,
) -> bool {
    created.contains(session_id) || grants.contains(&Grant::attach(connection, session_id.clone()))
}

/// May `connection` declare `monitor`? (BR-2)
///
/// Deliberately *not* a per-session question, because the declaration is not a
/// per-session claim: a monitor asks for every session's events, including
/// sessions that do not exist yet. So what satisfies it is a monitor-scope
/// grant held by this connection — the scope is the whole answer, and the
/// session recorded in the key is the session whose consent minted it.
///
/// Reading this off the attach evidence instead — "it holds a grant, so let it
/// monitor" — is precisely LESSON-495's failure: the key would stop encoding
/// the question, and every connection allowed into one session would silently
/// become an observer of all of them.
///
/// Creating a session confers *nothing* here, which is the asymmetry with
/// [`may_attach`]: having made one session is standing to attach to that
/// session, never standing to watch every other one.
#[must_use]
pub fn may_monitor(connection: ConnectionId, grants: &HashSet<Grant>) -> bool {
    grants
        .iter()
        .any(|grant| grant.connection == connection && grant.scope == GrantScope::Monitor)
}

/// The session recorded on a **monitor**-scope grant.
///
/// A monitor grant is not about a session, and [`may_monitor`] proves it: the
/// predicate matches on connection and scope and never looks at this field. So
/// per LESSON-495 the whole key for the monitor question is `(connection,
/// scope)`, and the `session_id` slot is a witness rather than part of the
/// question.
///
/// Given that, a sentinel is the honest value — better than the approver's
/// session, which would read as though the grant were somehow scoped to it.
/// REQ-569 BR-8 made real session ids 128 random bits, so this cannot collide
/// with one.
#[must_use]
pub fn monitor_witness() -> SessionId {
    SessionId::from("daemon-wide")
}

/// The daemon's live grants, and the connection-id namespace they are keyed by.
///
/// In-memory and daemon-lifetime (ADR-C). Held in the [`crate::server::Daemon`]
/// beside the session registry and the event bus, so every client task consults
/// one set rather than a copy each.
///
/// The id minter lives here rather than in the server because this is the
/// module that *keys* things by connection id: whoever owns the key space owns
/// the guarantee that two live connections never share a key (LESSON-503 —
/// mint at the scope that resolves).
#[derive(Debug)]
pub struct GrantRegistry {
    /// The next connection id. Monotonic and daemon-wide; it never wraps in any
    /// realistic daemon lifetime (a `u64` at one connection per nanosecond
    /// lasts ~584 years), so a released id is never handed out again and a
    /// grant cannot outlive its subject by being re-keyed to a successor.
    next_connection: AtomicU64,
    /// Every live grant. A `HashSet` because the whole [`Grant`] is the key.
    granted: RwLock<HashSet<Grant>>,
}

impl GrantRegistry {
    /// An empty registry with a fresh id namespace.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_connection: AtomicU64::new(0),
            granted: RwLock::new(HashSet::new()),
        }
    }

    /// Mints an id for a new connection.
    #[must_use]
    pub fn next_connection_id(&self) -> ConnectionId {
        ConnectionId(self.next_connection.fetch_add(1, Ordering::Relaxed))
    }

    /// Records `grant`. Idempotent — a set, so re-granting changes nothing.
    ///
    /// The only caller that may reach this is an explicit user-consent decision
    /// (`session/attach`'s `Granted` arm, TASK-108). Nothing derives a grant
    /// from the environment, the socket path, or filesystem state (BR-3), and
    /// nothing in production reaches it with [`GrantScope::Monitor`] — see the
    /// module docs.
    pub fn grant(&self, grant: Grant) {
        self.granted
            .write()
            .expect("grant registry lock poisoned")
            .insert(grant);
    }

    /// Drops exactly `grant`, if it is held. Returns whether it was.
    ///
    /// The narrow counterpart to [`Self::release`], and narrow on purpose: the
    /// whole [`Grant`] is the key, so this can retract one entry without
    /// touching another the same connection holds over a different session or at
    /// a different scope.
    ///
    /// **Not a revocation mechanism** — mid-session revocation is out of scope
    /// for REQ-569 (requirement.md, Out of Scope), and no user-reachable path
    /// calls this. Its one caller is `session/attach` undoing a grant it had to
    /// mint before it was allowed to learn that the session does not exist
    /// (`crate::server`, R2): the consent must run in full for an id that names
    /// nothing, or the handler becomes the existence oracle BR-8 forbids, and
    /// the entry it leaves behind would otherwise be permanent and keyed by a
    /// string the requester chose.
    pub fn revoke(&self, grant: &Grant) -> bool {
        self.granted
            .write()
            .expect("grant registry lock poisoned")
            .remove(grant)
    }

    /// [`may_attach`] against the live registry.
    #[must_use]
    pub fn may_attach(
        &self,
        connection: ConnectionId,
        session_id: &SessionId,
        created: &HashSet<SessionId>,
    ) -> bool {
        let granted = self.granted.read().expect("grant registry lock poisoned");
        may_attach(connection, session_id, created, &granted)
    }

    /// [`may_monitor`] against the live registry.
    #[must_use]
    pub fn may_monitor(&self, connection: ConnectionId) -> bool {
        let granted = self.granted.read().expect("grant registry lock poisoned");
        may_monitor(connection, &granted)
    }

    /// Drops every grant held by `connection` — called when it ends (ADR-C).
    ///
    /// Unconditional rather than conditional on having granted anything: the
    /// rule is "grants die with the connection", and a release that only ran
    /// where the caller believed a grant existed would be a rule enforced by
    /// the caller's bookkeeping instead of by the registry's.
    pub fn release(&self, connection: ConnectionId) {
        self.granted
            .write()
            .expect("grant registry lock poisoned")
            .retain(|grant| grant.connection != connection);
    }

    /// Every grant `connection` currently holds.
    ///
    /// Exists so a test can assert an *absence* — "a denied consent left no
    /// grant behind" (BR-7, TASK-108) — by inspecting the registry rather than
    /// inferring it from an error code.
    #[must_use]
    pub fn held_by(&self, connection: ConnectionId) -> Vec<Grant> {
        self.granted
            .read()
            .expect("grant registry lock poisoned")
            .iter()
            .filter(|grant| grant.connection == connection)
            .cloned()
            .collect()
    }

    /// How many grants are live, across every connection.
    #[must_use]
    pub fn len(&self) -> usize {
        self.granted
            .read()
            .expect("grant registry lock poisoned")
            .len()
    }

    /// Whether any grant is live at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for GrantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection id without a registry, for the pure-function tables.
    fn conn(n: u64) -> ConnectionId {
        ConnectionId(n)
    }

    fn session(name: &str) -> SessionId {
        SessionId::from(name)
    }

    /// BR-1/BR-2 and LESSON-495 in one table: every combination of standing a
    /// connection can have, crossed with both questions.
    ///
    /// The rows that matter most are the *cross* ones — an attach grant asked
    /// the monitor question, and a monitor grant asked the attach question.
    /// Those are the two cells a scope-blind key would get wrong, and they are
    /// the only cells that distinguish "the scope is part of the key" from "the
    /// scope is a label on an entry nobody reads".
    #[test]
    fn attach_and_monitor_are_answered_by_their_own_scope_and_no_other() {
        let me = conn(1);
        let other = conn(2);
        let mine = session("sess-mine");
        let theirs = session("sess-theirs");

        // (case, sessions created, grants held, attach target, may_attach, may_monitor)
        type Case = (
            &'static str,
            Vec<SessionId>,
            Vec<Grant>,
            SessionId,
            bool,
            bool,
        );
        let cases: Vec<Case> = vec![
            (
                "no standing at all",
                vec![],
                vec![],
                theirs.clone(),
                false,
                false,
            ),
            (
                "the creator may attach to what it made",
                vec![mine.clone()],
                vec![],
                mine.clone(),
                true,
                false,
            ),
            (
                "creating one session is no standing over another",
                vec![mine.clone()],
                vec![],
                theirs.clone(),
                false,
                false,
            ),
            (
                "an attach grant opens the session it names",
                vec![],
                vec![Grant::attach(me, theirs.clone())],
                theirs.clone(),
                true,
                false,
            ),
            (
                "an attach grant opens no other session",
                vec![],
                vec![Grant::attach(me, theirs.clone())],
                mine.clone(),
                false,
                false,
            ),
            // The LESSON-495 pair. An attach grant must not answer the monitor
            // question (row above: `may_monitor` false while `may_attach` is
            // true), and a monitor grant must not answer the attach one.
            (
                "a monitor grant permits monitoring and nothing else",
                vec![],
                vec![Grant::monitor(me, theirs.clone())],
                theirs.clone(),
                false,
                true,
            ),
            (
                "a monitor grant over one session does not open another",
                vec![],
                vec![Grant::monitor(me, theirs.clone())],
                mine.clone(),
                false,
                true,
            ),
            // The subject is part of the key too: another connection's grants
            // are not this connection's standing.
            (
                "another connection's attach grant is not ours",
                vec![],
                vec![Grant::attach(other, theirs.clone())],
                theirs.clone(),
                false,
                false,
            ),
            (
                "another connection's monitor grant is not ours",
                vec![],
                vec![Grant::monitor(other, theirs.clone())],
                theirs.clone(),
                false,
                false,
            ),
            (
                "both scopes held: both answers are yes, each from its own key",
                vec![],
                vec![
                    Grant::attach(me, theirs.clone()),
                    Grant::monitor(me, theirs.clone()),
                ],
                theirs.clone(),
                true,
                true,
            ),
        ];

        for (case, created, grants, target, attach, monitor) in cases {
            let created: HashSet<SessionId> = created.into_iter().collect();
            let grants: HashSet<Grant> = grants.into_iter().collect();
            assert_eq!(
                may_attach(me, &target, &created, &grants),
                attach,
                "{case}: may_attach"
            );
            assert_eq!(may_monitor(me, &grants), monitor, "{case}: may_monitor");
        }
    }

    /// Creating a session is standing to attach to *it* — never standing to
    /// monitor everything.
    ///
    /// Called out on its own because it is the one asymmetry between the two
    /// predicates, and because "the creator may monitor" is the tempting
    /// shortcut: it would hand every ordinary CLI, which creates a session as
    /// its first act, sight of every other session on the machine.
    #[test]
    fn creating_a_session_is_never_standing_to_monitor() {
        let me = conn(7);
        let mine = session("sess-mine");
        let created: HashSet<SessionId> = [mine.clone()].into_iter().collect();
        let none = HashSet::new();

        assert!(may_attach(me, &mine, &created, &none));
        assert!(!may_monitor(me, &none));
    }

    #[test]
    fn a_registry_answers_with_the_same_rule_it_stores() {
        let registry = GrantRegistry::new();
        let me = registry.next_connection_id();
        let target = session("sess-theirs");
        let nothing_created = HashSet::new();

        assert!(!registry.may_attach(me, &target, &nothing_created));
        assert!(!registry.may_monitor(me));

        registry.grant(Grant::attach(me, target.clone()));
        assert!(registry.may_attach(me, &target, &nothing_created));
        assert!(
            !registry.may_monitor(me),
            "an attach grant must not answer the monitor question through the registry either"
        );

        registry.grant(Grant::monitor(me, target.clone()));
        assert!(registry.may_monitor(me));

        // Idempotent: re-granting is a set insert, not a second entry.
        let before = registry.len();
        registry.grant(Grant::attach(me, target.clone()));
        assert_eq!(registry.len(), before);
    }

    /// ADR-C: grants die with the connection, and only with *that* connection.
    #[test]
    fn releasing_a_connection_drops_its_grants_and_leaves_every_other_alone() {
        let registry = GrantRegistry::new();
        let leaving = registry.next_connection_id();
        let staying = registry.next_connection_id();
        let target = session("sess-shared");
        let nothing_created = HashSet::new();

        registry.grant(Grant::attach(leaving, target.clone()));
        registry.grant(Grant::monitor(leaving, target.clone()));
        registry.grant(Grant::attach(staying, target.clone()));
        assert_eq!(registry.held_by(leaving).len(), 2);

        registry.release(leaving);
        assert!(
            registry.held_by(leaving).is_empty(),
            "a departed connection must leave no grant behind"
        );
        assert!(!registry.may_attach(leaving, &target, &nothing_created));
        assert!(!registry.may_monitor(leaving));
        assert!(
            registry.may_attach(staying, &target, &nothing_created),
            "one connection leaving must not revoke another's grant"
        );

        // Releasing a connection that holds nothing is a no-op, not a panic:
        // every connection is released at teardown, most of them never having
        // been granted anything.
        registry.release(registry.next_connection_id());
        assert_eq!(registry.len(), 1);
    }

    /// The id namespace is the reason a released grant cannot be inherited: no
    /// two connections ever share a subject, so a fresh connection starts with
    /// exactly no standing (LESSON-503 — mint at the scope that resolves).
    #[test]
    fn connection_ids_are_unique_and_never_reused() {
        let registry = GrantRegistry::new();
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(registry.next_connection_id()),
                "a connection id was handed out twice"
            );
        }

        // And a release does not put the id back in circulation.
        let released = *seen.iter().next().unwrap();
        registry.release(released);
        for _ in 0..10 {
            assert!(seen.insert(registry.next_connection_id()));
        }
    }

    /// The registry is shared across client tasks, so its own state has to hold
    /// up under concurrent writers rather than relying on the caller to
    /// serialize them.
    #[test]
    fn concurrent_grants_and_releases_leave_a_consistent_registry() {
        use std::sync::Arc;

        let registry = Arc::new(GrantRegistry::new());
        let target = session("sess-busy");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let registry = Arc::clone(&registry);
            let target = target.clone();
            handles.push(std::thread::spawn(move || {
                let me = registry.next_connection_id();
                registry.grant(Grant::attach(me, target.clone()));
                assert!(registry.may_attach(me, &target, &HashSet::new()));
                registry.release(me);
                assert!(registry.held_by(me).is_empty());
            }));
        }
        for handle in handles {
            handle.join().expect("no thread may panic");
        }
        assert!(
            registry.is_empty(),
            "every connection released, so nothing may remain: {} left",
            registry.len()
        );
    }
}
